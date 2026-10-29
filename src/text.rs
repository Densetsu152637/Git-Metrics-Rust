//! Text helpers that reproduce the string semantics of the original Haskell
//! service (`lines`, `words`, `isSpace`, `show`, order-preserving `nub`).

use std::collections::HashSet;

/// Haskell's `isSpace`: Unicode `Space` category plus ASCII/Latin-1 whitespace.
/// It excludes U+0085, U+2028 and U+2029, which Rust treats as whitespace.
pub fn is_space(value: char) -> bool {
    value.is_whitespace() && !matches!(value, '\u{85}' | '\u{2028}' | '\u{2029}')
}

/// Haskell's `toLower` (simple, single-character case mapping).
pub fn to_lower(value: char) -> char {
    value.to_lowercase().next().unwrap_or(value)
}

/// Haskell's `lines`: splits on `\n` only and drops one trailing empty line.
pub fn lines(text: &str) -> Vec<&str> {
    let mut parts: Vec<&str> = text.split('\n').collect();
    if parts.last() == Some(&"") {
        parts.pop();
    }
    parts
}

/// Haskell's `words`: splits on [`is_space`] and drops empty fragments.
pub fn words(text: &str) -> Vec<&str> {
    text.split(is_space)
        .filter(|word| !word.is_empty())
        .collect()
}

/// Removes leading and trailing Haskell whitespace.
pub fn trim(text: &str) -> &str {
    text.trim_matches(is_space)
}

/// Removes only leading Haskell whitespace.
pub fn trim_start(text: &str) -> &str {
    text.trim_start_matches(is_space)
}

/// Deduplicates while keeping the first occurrence of each value (`nub`).
pub fn nub(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut seen = HashSet::new();
    values
        .into_iter()
        .filter(|value| seen.insert(value.clone()))
        .collect()
}

/// Deduplicates and sorts (`Map.keys . Map.fromList`).
pub fn unique(values: impl IntoIterator<Item = String>) -> Vec<String> {
    let mut sorted: Vec<String> = values.into_iter().collect();
    sorted.sort();
    sorted.dedup();
    sorted
}

const ASCII_NAMES: [&str; 32] = [
    "NUL", "SOH", "STX", "ETX", "EOT", "ENQ", "ACK", "a", "b", "t", "n", "v", "f", "r", "SO", "SI",
    "DLE", "DC1", "DC2", "DC3", "DC4", "NAK", "SYN", "ETB", "CAN", "EM", "SUB", "ESC", "FS", "GS",
    "RS", "US",
];

/// Haskell's `show` for `String`, used in diagnostics that embed commands.
pub fn show_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    let mut chars = text.chars().peekable();
    while let Some(value) = chars.next() {
        let next_is_digit = chars.peek().is_some_and(|next| next.is_ascii_digit());
        match value {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\u{7f}' => out.push_str("\\DEL"),
            value if (value as u32) < 32 => {
                out.push('\\');
                out.push_str(ASCII_NAMES[value as usize]);
                if value == '\u{e}' && chars.peek() == Some(&'H') {
                    out.push_str("\\&");
                }
            }
            value if (value as u32) > 126 => {
                out.push('\\');
                out.push_str(&(value as u32).to_string());
                if next_is_digit {
                    out.push_str("\\&");
                }
            }
            value => out.push(value),
        }
    }
    out.push('"');
    out
}

/// Haskell's `show` for `[String]`.
pub fn show_string_list<S: AsRef<str>>(values: &[S]) -> String {
    let rendered: Vec<String> = values
        .iter()
        .map(|value| show_string(value.as_ref()))
        .collect();
    format!("[{}]", rendered.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_match_haskell() {
        assert_eq!(lines(""), Vec::<&str>::new());
        assert_eq!(lines("a\n"), vec!["a"]);
        assert_eq!(lines("\n"), vec![""]);
        assert_eq!(lines("a\n\nb"), vec!["a", "", "b"]);
        assert_eq!(lines("a\r\nb"), vec!["a\r", "b"]);
    }

    #[test]
    fn space_excludes_line_separators() {
        assert!(is_space('\u{a0}'));
        assert!(!is_space('\u{85}'));
        assert!(!is_space('\u{2028}'));
        assert_eq!(words("R100\told  new"), vec!["R100", "old", "new"]);
    }

    #[test]
    fn show_escapes_like_haskell() {
        assert_eq!(show_string("a\"b\\c\n"), "\"a\\\"b\\\\c\\n\"");
        assert_eq!(show_string("é1"), "\"\\233\\&1\"");
        assert_eq!(show_string("\u{e}H"), "\"\\SO\\&H\"");
        assert_eq!(show_string_list(&["x", "y"]), "[\"x\",\"y\"]");
    }

    #[test]
    fn unique_sorts_and_nub_keeps_order() {
        assert_eq!(unique(["b".into(), "a".into(), "b".into()]), vec!["a", "b"]);
        assert_eq!(nub(["b".into(), "a".into(), "b".into()]), vec!["b", "a"]);
    }
}
