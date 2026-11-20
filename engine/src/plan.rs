// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Dynamic segmentation, as Internet Download Manager describes it.
//!
//! The file is not cut into fixed parts up front. Whenever a connection is
//! free, it takes a segment nobody is working on; if there is none, the
//! segment with the most bytes left is split in half and the free connection
//! starts at the midpoint. A connection that finishes early therefore helps
//! the slowest one. Segments too small to be worth a connection are not split.
//!
//! Pure functions over a list of segments: no I/O, no clock, tested alone.

use serde::{Deserialize, Serialize};

/// Never split a segment with fewer than `2 × MIN_SPLIT` bytes left: below
/// that, opening a connection costs more than it saves.
pub const MIN_SPLIT: u64 = 256 * 1024;

/// `[start, end)`, with `start..pos` already on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Segment {
    /// Stable for the life of a download, so a connection can name its segment
    /// while others are split around it. Not persisted.
    #[serde(skip)]
    pub id: u32,
    pub start: u64,
    pub pos: u64,
    pub end: u64,
    /// The connection working on it, if any. Not persisted.
    #[serde(skip)]
    pub owner: Option<u32>,
}

impl Segment {
    pub fn left(&self) -> u64 {
        self.end.saturating_sub(self.pos)
    }
}

/// What a free connection should do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Assignment {
    /// Take this segment, which nobody is working on.
    Take(u32),
    /// Split the segment `id` at `at`; the new half `[at, end)` is the
    /// connection's.
    Split { id: u32, at: u64 },
}

/// A fresh plan for `length` bytes.
pub fn initial(length: u64) -> Vec<Segment> {
    vec![Segment {
        id: 0,
        start: 0,
        pos: 0,
        end: length,
        owner: None,
    }]
}

/// Number the segments of a plan read back from disk, and check it still
/// describes `[0, length)` exactly once. `None` means the plan cannot be
/// trusted and the download must start again.
pub fn restore(mut segments: Vec<Segment>, length: u64) -> Option<Vec<Segment>> {
    segments.sort_by_key(|s| s.start);
    let mut cursor = 0;
    for (i, s) in segments.iter_mut().enumerate() {
        if s.start != cursor || s.pos < s.start || s.pos > s.end {
            return None;
        }
        cursor = s.end;
        s.id = u32::try_from(i).ok()?;
        s.owner = None;
    }
    (cursor == length && !segments.is_empty()).then_some(segments)
}

/// The next thing a free connection should do, or `None` when nothing is worth
/// doing (everything is owned and nothing is big enough to split).
pub fn next(segments: &[Segment]) -> Option<Assignment> {
    if let Some(s) = segments.iter().find(|s| s.owner.is_none() && s.left() > 0) {
        return Some(Assignment::Take(s.id));
    }
    let largest = segments
        .iter()
        .filter(|s| s.owner.is_some())
        .max_by_key(|s| (s.left(), std::cmp::Reverse(s.start)))?;
    let left = largest.left();
    (left >= 2 * MIN_SPLIT).then(|| Assignment::Split {
        id: largest.id,
        at: largest.pos + left / 2,
    })
}

/// Carry out a split: the segment keeps `[start, at)`, a new one gets
/// `[at, end)`. Returns the new segment's id. Kept in file order.
pub fn split(segments: &mut Vec<Segment>, id: u32, at: u64) -> Option<u32> {
    let index = segments.iter().position(|s| s.id == id)?;
    let old = &mut segments[index];
    if at <= old.pos || at >= old.end {
        return None;
    }
    let end = old.end;
    old.end = at;
    let new_id = segments.iter().map(|s| s.id).max().unwrap_or(0) + 1;
    segments.insert(
        index + 1,
        Segment {
            id: new_id,
            start: at,
            pos: at,
            end,
            owner: None,
        },
    );
    Some(new_id)
}

pub fn done_bytes(segments: &[Segment]) -> u64 {
    segments.iter().map(|s| s.pos - s.start).sum()
}

pub fn complete(segments: &[Segment]) -> bool {
    segments.iter().all(|s| s.pos >= s.end)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const MB: u64 = 1024 * 1024;

    fn take(segments: &mut [Segment], id: u32, conn: u32) {
        segments.iter_mut().find(|s| s.id == id).unwrap().owner = Some(conn);
    }

    #[test]
    fn the_first_connection_takes_the_whole_file() {
        let plan = initial(100 * MB);
        assert_eq!(next(&plan), Some(Assignment::Take(0)));
    }

    #[test]
    fn a_free_connection_halves_the_largest_remaining_segment() {
        let mut plan = initial(100 * MB);
        take(&mut plan, 0, 1);
        plan[0].pos = 20 * MB;
        // 80 MB left in segment 0: split at its middle, 60 MB.
        assert_eq!(next(&plan), Some(Assignment::Split { id: 0, at: 60 * MB }));
        let new = split(&mut plan, 0, 60 * MB).unwrap();
        take(&mut plan, new, 2);
        assert_eq!(
            (plan[0].end, plan[1].start, plan[1].end),
            (60 * MB, 60 * MB, 100 * MB)
        );
        // Now two 40 MB halves; the earlier one in the file is split first.
        assert_eq!(next(&plan), Some(Assignment::Split { id: 0, at: 40 * MB }));
    }

    #[test]
    fn an_abandoned_segment_is_taken_before_anything_is_split() {
        let mut plan = initial(100 * MB);
        take(&mut plan, 0, 1);
        let new = split(&mut plan, 0, 50 * MB).unwrap();
        // Its connection failed: owner cleared, bytes left.
        assert_eq!(next(&plan), Some(Assignment::Take(new)));
    }

    #[test]
    fn small_segments_are_not_split() {
        let mut plan = initial(2 * MIN_SPLIT - 1);
        take(&mut plan, 0, 1);
        assert_eq!(next(&plan), None);
        let mut plan = initial(2 * MIN_SPLIT);
        take(&mut plan, 0, 1);
        assert!(next(&plan).is_some());
    }

    #[test]
    fn splits_never_lose_or_duplicate_a_byte() {
        let length = 1000 * MB + 12_345;
        let mut plan = initial(length);
        let mut seed = 7u64;
        let mut rand = || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            seed >> 33
        };
        for conn in 0..200 {
            // Progress somewhere, then ask for work.
            let i = (rand() as usize) % plan.len();
            let step = rand() % (plan[i].left() + 1);
            plan[i].pos += step;
            match next(&plan) {
                Some(Assignment::Take(id)) => take(&mut plan, id, conn),
                Some(Assignment::Split { id, at }) => {
                    let new = split(&mut plan, id, at).unwrap();
                    take(&mut plan, new, conn);
                }
                None => {}
            }
            let mut cursor = 0;
            for s in &plan {
                assert_eq!(s.start, cursor);
                assert!(s.start <= s.pos && s.pos <= s.end);
                cursor = s.end;
            }
            assert_eq!(cursor, length);
        }
    }

    #[test]
    fn a_plan_from_disk_is_checked_before_it_is_trusted() {
        let good = vec![
            Segment {
                id: 0,
                start: 50,
                pos: 70,
                end: 100,
                owner: None,
            },
            Segment {
                id: 0,
                start: 0,
                pos: 10,
                end: 50,
                owner: None,
            },
        ];
        let restored = restore(good, 100).unwrap();
        assert_eq!((restored[0].start, restored[1].id), (0, 1));
        assert_eq!(done_bytes(&restored), 30);
        let gap = vec![Segment {
            id: 0,
            start: 0,
            pos: 0,
            end: 40,
            owner: None,
        }];
        assert!(restore(gap, 100).is_none());
        let past_end = vec![Segment {
            id: 0,
            start: 0,
            pos: 120,
            end: 100,
            owner: None,
        }];
        assert!(restore(past_end, 100).is_none());
    }
}
