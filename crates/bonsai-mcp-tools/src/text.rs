//! API text on its way to a model: what a cluster's name or a search term holds
//! is whatever someone typed.
//!
//! Control characters and Unicode format characters (bidi overrides, zero-width
//! characters, the BOM) are replaced in error text and written as `\uXXXX` in
//! JSON, where the document still parses to the same value. The same filter as
//! the CLI uses for a terminal.

use std::borrow::Cow;
use std::io::Write;

use serde_json::Value;

const REPLACEMENT: char = '\u{FFFD}';

/// Whether `c` is a control (C0, DEL, C1) or a Unicode format (`Cf`) character,
/// bidi controls included.
fn is_unsafe(c: char) -> bool {
    c.is_control() || is_format(c)
}

/// `s` with every unsafe character but a newline replaced; borrowed when
/// nothing was.
pub fn sanitize(s: &str) -> Cow<'_, str> {
    let unsafe_char = |c: char| c != '\n' && is_unsafe(c);
    if !s.chars().any(unsafe_char) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(
        s.chars()
            .map(|c| if unsafe_char(c) { REPLACEMENT } else { c })
            .collect(),
    )
}

/// `v` as compact JSON, each unsafe character a `\uXXXX` escape (a surrogate
/// pair above the BMP), in keys and values alike.
pub fn json_string(v: &Value) -> String {
    let mut out = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut out, Escaping);
    serde::Serialize::serialize(v, &mut ser).expect("writing to a Vec doesn't fail");
    String::from_utf8(out).expect("serde_json writes UTF-8")
}

/// serde_json's compact formatter, but string fragments it would pass through
/// unescaped get `\uXXXX` for each unsafe character. serde_json escapes `"`,
/// `\\` and C0 controls itself.
struct Escaping;

impl serde_json::ser::Formatter for Escaping {
    fn write_string_fragment<W: ?Sized + Write>(
        &mut self,
        w: &mut W,
        fragment: &str,
    ) -> std::io::Result<()> {
        let mut start = 0;
        for (i, c) in fragment.char_indices() {
            if !is_unsafe(c) {
                continue;
            }
            w.write_all(&fragment.as_bytes()[start..i])?;
            let mut units = [0u16; 2];
            for unit in c.encode_utf16(&mut units) {
                write!(w, "\\u{unit:04x}")?;
            }
            start = i + c.len_utf8();
        }
        w.write_all(&fragment.as_bytes()[start..])
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
    use serde_json::json;

    #[test]
    fn sanitize_replaces_controls_and_bidi_but_keeps_newlines() {
        assert_eq!(
            sanitize("a\u{1b}[31mb\u{202e}c\nd"),
            "a\u{fffd}[31mb\u{fffd}c\nd"
        );
        assert!(matches!(sanitize("plain\ntext"), Cow::Borrowed(_)));
    }

    #[test]
    fn json_escapes_unsafe_characters_and_parses_back_to_the_same_value() {
        let value = json!({"na\u{200b}me": "a\u{202e}b\u{9b}c \u{e0041} ok"});

        let text = json_string(&value);

        assert!(text.is_ascii() || !text.chars().any(is_unsafe), "{text}");
        assert!(
            text.contains("\\u202e") && text.contains("\\udb40\\udc41"),
            "{text}"
        );
        assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), value);
    }
}
