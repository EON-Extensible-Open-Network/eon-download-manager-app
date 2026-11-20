// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 EON contributors
//
// Additional permission under GNU GPL version 3 section 7:
// see LICENSE-EXCEPTION.md (EON Module ABI Exception 1.0).

//! Numbered series: `https://example.org/photo[001-120].jpg` becomes 120
//! addresses, zero-padded as written. Letters work too: `part[a-f].bin`.

/// At most this many addresses from one pattern.
pub const LIMIT: usize = 10_000;

/// The addresses a pattern stands for; the text itself when it has no
/// `[from-to]`. `Err` says what is wrong in words a person can act on.
pub fn expand(pattern: &str) -> Result<Vec<String>, String> {
    let Some(open) = pattern.find('[') else {
        return Ok(vec![pattern.to_owned()]);
    };
    let close = pattern[open..]
        .find(']')
        .map(|i| open + i)
        .ok_or_else(|| "a [ without its ]".to_owned())?;
    let inner = &pattern[open + 1..close];
    let (from, to) = inner
        .split_once('-')
        .ok_or_else(|| format!("[{inner}] needs a range like [1-20]"))?;
    let (prefix, suffix) = (&pattern[..open], &pattern[close + 1..]);
    if suffix.contains('[') {
        return Err("one [range] per address".into());
    }
    let items: Vec<String> = if let (Ok(a), Ok(b)) = (from.parse::<u64>(), to.parse::<u64>()) {
        if a > b {
            return Err(format!("[{inner}] counts down"));
        }
        if b - a >= LIMIT as u64 {
            return Err(format!("more than {LIMIT} addresses"));
        }
        let width = if from.starts_with('0') && from.len() > 1 {
            from.len()
        } else {
            0
        };
        (a..=b).map(|n| format!("{n:0width$}")).collect()
    } else {
        let (Some(a), Some(b)) = (single(from), single(to)) else {
            return Err(format!("[{inner}] is neither numbers nor single letters"));
        };
        if a > b {
            return Err(format!("[{inner}] counts down"));
        }
        (a..=b).map(String::from).collect()
    };
    Ok(items
        .into_iter()
        .map(|i| format!("{prefix}{i}{suffix}"))
        .collect())
}

fn single(text: &str) -> Option<char> {
    let mut chars = text.chars();
    let c = chars.next()?;
    (chars.next().is_none() && c.is_ascii_alphabetic()).then_some(c)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn numbers_keep_their_padding() {
        let all = expand("https://x.org/p[008-011].jpg").unwrap();
        assert_eq!(
            all,
            [
                "https://x.org/p008.jpg",
                "https://x.org/p009.jpg",
                "https://x.org/p010.jpg",
                "https://x.org/p011.jpg"
            ]
        );
        assert_eq!(
            expand("https://x.org/e[9-10]").unwrap(),
            ["https://x.org/e9", "https://x.org/e10"]
        );
    }

    #[test]
    fn letters_and_plain_addresses() {
        assert_eq!(expand("https://x.org/[a-c].bin").unwrap().len(), 3);
        assert_eq!(
            expand("https://x.org/a.bin").unwrap(),
            ["https://x.org/a.bin"]
        );
    }

    #[test]
    fn mistakes_are_explained() {
        assert!(expand("https://x.org/[5-1]")
            .unwrap_err()
            .contains("counts down"));
        assert!(expand("https://x.org/[1-99999]")
            .unwrap_err()
            .contains("more than"));
        assert!(expand("https://x.org/[1-").is_err());
        assert!(expand("https://x.org/[aa-bb]").is_err());
    }
}
