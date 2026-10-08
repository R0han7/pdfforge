//! User-facing page ranges: one-based, e.g. `"1-3, 5, 8-"`, `"last"`, `"odd"`, `"even"`.

use crate::{Error, Result};

/// Parse a page range expression into zero-based indices, preserving order (so `"3,1,2"` can be
/// used as a new page order) and allowing duplicates.
///
/// Grammar (comma-separated, whitespace ignored, case-insensitive):
/// - `N` — a single page; `last` (or `end`) is the final page
/// - `A-B` — inclusive range; `B < A` counts down (`"5-1"` reverses)
/// - `A-` — from `A` to the end; `-B` — from the first page to `B`
/// - `all`, `odd`, `even`
pub fn parse_page_ranges(spec: &str, page_count: usize) -> Result<Vec<usize>> {
    let bad = |m: String| Error::InvalidPages(m);
    if page_count == 0 {
        return Err(bad("document has no pages".into()));
    }
    let num = |s: &str| -> Result<usize> {
        let s = s.trim();
        let n = match s.to_ascii_lowercase().as_str() {
            "last" | "end" => page_count,
            _ => s.parse::<usize>().map_err(|_| bad(format!("'{s}' is not a page number")))?,
        };
        if n == 0 || n > page_count {
            return Err(bad(format!("page {n} is out of range 1-{page_count}")));
        }
        Ok(n - 1)
    };
    let mut out = Vec::new();
    for part in spec.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.to_ascii_lowercase().as_str() {
            "all" => out.extend(0..page_count),
            "odd" => out.extend((0..page_count).step_by(2)),
            "even" => out.extend((1..page_count).step_by(2)),
            _ => match part.split_once('-') {
                None => out.push(num(part)?),
                Some((a, b)) => {
                    let a = if a.trim().is_empty() { 0 } else { num(a)? };
                    let b = if b.trim().is_empty() { page_count - 1 } else { num(b)? };
                    if a <= b { out.extend(a..=b) } else { out.extend((b..=a).rev()) }
                }
            },
        }
    }
    if out.is_empty() {
        return Err(bad("no pages selected".into()));
    }
    Ok(out)
}

/// Format zero-based indices compactly as one-based ranges, e.g. `[0,1,2,4]` → `"1-3, 5"`.
pub fn format_page_ranges(pages: &[usize]) -> String {
    let mut parts = Vec::new();
    let mut i = 0;
    while i < pages.len() {
        let mut j = i;
        while j + 1 < pages.len() && pages[j + 1] == pages[j] + 1 {
            j += 1;
        }
        parts.push(if i == j { format!("{}", pages[i] + 1) } else { format!("{}-{}", pages[i] + 1, pages[j] + 1) });
        i = j + 1;
    }
    parts.join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ranges() {
        assert_eq!(parse_page_ranges("1-3, 5", 10).unwrap(), vec![0, 1, 2, 4]);
        assert_eq!(parse_page_ranges("8-", 10).unwrap(), vec![7, 8, 9]);
        assert_eq!(parse_page_ranges("-2", 10).unwrap(), vec![0, 1]);
        assert_eq!(parse_page_ranges("3,1,2", 3).unwrap(), vec![2, 0, 1]);
        assert_eq!(parse_page_ranges("last-1", 3).unwrap(), vec![2, 1, 0]);
        assert_eq!(parse_page_ranges("odd", 5).unwrap(), vec![0, 2, 4]);
        assert_eq!(parse_page_ranges("EVEN", 5).unwrap(), vec![1, 3]);
        assert_eq!(parse_page_ranges("all", 2).unwrap(), vec![0, 1]);
    }

    #[test]
    fn rejects_bad_ranges() {
        assert!(parse_page_ranges("0", 5).is_err());
        assert!(parse_page_ranges("6", 5).is_err());
        assert!(parse_page_ranges("abc", 5).is_err());
        assert!(parse_page_ranges("", 5).is_err());
        assert!(parse_page_ranges("1", 0).is_err());
    }

    #[test]
    fn formats_ranges() {
        assert_eq!(format_page_ranges(&[0, 1, 2, 4, 6, 7]), "1-3, 5, 7-8");
        assert_eq!(format_page_ranges(&[]), "");
    }
}
