//! Escape untrusted terminal text before adding trusted formatting sequences.

use std::borrow::Cow;

/// C0, DEL and C1 controls become visible escapes. Ordinary Unicode and
/// backslashes remain unchanged; already-safe text needs no allocation.
pub(crate) fn terminal_text(text: &str) -> Cow<'_, str> {
    if !text.chars().any(char::is_control) {
        return Cow::Borrowed(text);
    }
    let mut output = String::with_capacity(text.len());
    for character in text.chars() {
        if character.is_control() {
            output.extend(character.escape_default());
        } else {
            output.push(character);
        }
    }
    Cow::Owned(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_terminal_control_ranges_are_visible_without_changing_unicode() {
        let controls: String = (0..=0x1f)
            .chain(0x7f..=0x9f)
            .filter_map(char::from_u32)
            .collect();
        let escaped = terminal_text(&controls);
        assert!(!escaped.chars().any(char::is_control));
        for expected in [
            "\\u{0}", "\\t", "\\n", "\\r", "\\u{1b}", "\\u{7f}", "\\u{9b}", "\\u{9d}",
        ] {
            assert!(escaped.contains(expected), "missing {expected}: {escaped}");
        }
        let unicode = "λ café 中文 🦀 \\n";
        assert!(matches!(terminal_text(unicode), Cow::Borrowed(value) if value == unicode));
        assert_eq!(
            terminal_text("λ\x1b]52;c;payload\x07中文"),
            "λ\\u{1b}]52;c;payload\\u{7}中文"
        );
    }
}
