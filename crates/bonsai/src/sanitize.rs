//! The filter for untrusted API text in human output.
//!
//! No common crate is a security sanitiser: `anstream`'s `strip_str` keeps `\r`, and
//! none removes bidi overrides. This one replaces, with U+FFFD:
//!
//! - every `char::is_control()` character but `\n` (and `\t` when asked), so ESC,
//!   `\r`, BEL and C1 controls such as CSI (U+009B) can't move the cursor or start
//!   an escape sequence;
//! - every Unicode format (`Cf`) character, which includes the bidi controls
//!   (U+061C, U+200E, U+200F, U+202A..=U+202E, U+2066..=U+2069), zero-width
//!   characters, the BOM and the soft hyphen.
//!
//! JSON output keeps every character but writes these as `\uXXXX` escapes
//! ([`is_unsafe`], used by `output::write_json`), so the value is unchanged.

use std::borrow::Cow;

const REPLACEMENT: char = '\u{FFFD}';

/// Whether `c` is a control (C0, DEL, C1) or a Unicode format (`Cf`) character,
/// bidi controls included.
pub fn is_unsafe(c: char) -> bool {
    c.is_control() || is_format(c)
}

/// `s` with every unsafe character replaced; borrowed when nothing was.
pub fn sanitize(s: &str, keep_tab: bool) -> Cow<'_, str> {
    let unsafe_char = |c: char| match c {
        '\n' => false,
        '\t' => !keep_tab,
        c => is_unsafe(c),
    };
    if !s.chars().any(unsafe_char) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.chars()
            .map(|c| if unsafe_char(c) { REPLACEMENT } else { c })
            .collect(),
    )
}

/// [`sanitize`] for text that must stay one line and keep its shape, such as an end
/// user's search term: newlines and tabs are replaced too, so `red\nshoes` doesn't
/// read as `red shoes`.
pub fn sanitize_line(s: &str) -> Cow<'_, str> {
    match sanitize(s, false) {
        Cow::Borrowed(b) if !b.contains('\n') => Cow::Borrowed(b),
        c => Cow::Owned(c.replace('\n', &REPLACEMENT.to_string())),
    }
}

/// Unicode's general category `Cf` (format), as of Unicode 16.
fn is_format(c: char) -> bool {
    matches!(
        c,
        '\u{00AD}'
            | '\u{0600}'..='\u{0605}'
            | '\u{061C}'
            | '\u{06DD}'
            | '\u{070F}'
            | '\u{0890}'..='\u{0891}'
            | '\u{08E2}'
            | '\u{180E}'
            | '\u{200B}'..='\u{200F}'
            | '\u{202A}'..='\u{202E}'
            | '\u{2060}'..='\u{2064}'
            | '\u{2066}'..='\u{206F}'
            | '\u{FEFF}'
            | '\u{FFF9}'..='\u{FFFB}'
            | '\u{110BD}'
            | '\u{110CD}'
            | '\u{13430}'..='\u{1343F}'
            | '\u{1BCA0}'..='\u{1BCA3}'
            | '\u{1D173}'..='\u{1D17A}'
            | '\u{E0001}'
            | '\u{E0020}'..='\u{E007F}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: char = '\u{FFFD}';

    #[test]
    fn a_line_keeps_no_newline_or_tab() {
        assert_eq!(sanitize_line("red\nshoes"), format!("red{R}shoes"));
        assert_eq!(sanitize_line("a\tb\r\u{202e}"), format!("a{R}b{R}{R}"));
        assert_eq!(sanitize_line("red shoes"), "red shoes");
        assert_ne!(sanitize_line("red\nshoes"), sanitize_line("red shoes"));
    }

    #[test]
    fn escape_sequences_lose_their_esc() {
        assert_eq!(sanitize("\x1b[31mred", false), format!("{R}[31mred"));
        assert_eq!(
            sanitize("\x1b]8;;http://x\x07link", false),
            format!("{R}]8;;http://x{R}link")
        );
    }

    #[test]
    fn carriage_return_and_other_controls_are_replaced() {
        assert_eq!(sanitize("ok\rEVIL", false), format!("ok{R}EVIL"));
        assert_eq!(
            sanitize("a\x00b\x7fc\u{85}d", false),
            format!("a{R}b{R}c{R}d")
        );
    }

    #[test]
    fn bidi_and_format_characters_are_replaced() {
        for c in [
            '\u{202E}',
            '\u{202A}',
            '\u{2066}',
            '\u{2069}',
            '\u{200E}',
            '\u{200F}',
            '\u{061C}',
            '\u{200B}',
            '\u{200D}',
            '\u{2060}',
            '\u{FEFF}',
            '\u{00AD}',
            '\u{E0041}',
        ] {
            assert_eq!(
                sanitize(&format!("a{c}b"), false),
                format!("a{R}b"),
                "{:X}",
                c as u32
            );
        }
    }

    #[test]
    fn newline_is_kept_and_tab_only_when_asked() {
        assert_eq!(sanitize("a\nb", false), "a\nb");
        assert_eq!(sanitize("a\tb", true), "a\tb");
        assert_eq!(sanitize("a\tb", false), format!("a{R}b"));
    }

    #[test]
    fn plain_text_is_borrowed() {
        assert!(matches!(
            sanitize("search-prod ✓ ünïcödé", false),
            Cow::Borrowed(_)
        ));
        assert!(matches!(sanitize("a\rb", false), Cow::Owned(_)));
    }
}
