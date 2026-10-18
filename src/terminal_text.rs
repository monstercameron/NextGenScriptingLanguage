//! Terminal-safe rendering of text that can carry untrusted bytes: file names, applet
//! output and error messages. Control characters, including ESC (which starts terminal
//! sequences), CSI, OSC and the C1 range, become visible `\u{..}` escapes. Newlines and
//! tabs stay, so multi-line messages keep their shape.

use std::fmt::Write;

/// Escape control characters, except newline and tab, as visible `\u{..}` sequences.
pub fn escape_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\n' || character == '\t' || !is_unsafe_terminal_character(character) {
            out.push(character);
        } else {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(character));
        }
    }
    out
}

fn is_unsafe_terminal_character(character: char) -> bool {
    character.is_control()
        || matches!(
            u32::from(character),
            0x00ad
                | 0x034f
                | 0x061c
                | 0x115f..=0x1160
                | 0x17b4..=0x17b5
                | 0x180b..=0x180f
                // Zero-width space and the LRM/RLM marks. ZWNJ and ZWJ (U+200C, U+200D)
                // and the variation selectors stay: Persian, Indic and emoji text need them.
                | 0x200b
                | 0x200e..=0x200f
                | 0x202a..=0x202e
                | 0x2060..=0x206f
                | 0x3164
                | 0xfeff
                | 0xffa0
                | 0x1bca0..=0x1bca3
                | 0x1d173..=0x1d17a
                | 0xe0000..=0xe0fff
        )
}

#[cfg(test)]
mod tests {
    use super::escape_controls;

    #[test]
    fn escape_sequences_become_visible_and_layout_survives() {
        assert_eq!(escape_controls("a\u{1b}[2Jb"), r"a\u{1b}[2Jb");
        assert_eq!(escape_controls("x\u{9b}31m"), r"x\u{9b}31m");
        assert_eq!(escape_controls("line\nnext\tcol"), "line\nnext\tcol");
        assert_eq!(escape_controls("caf\u{e9}"), "caf\u{e9}");
    }

    #[test]
    fn escapes_bidi_and_invisible_format_characters() {
        let input = [
            'a',
            char::from_u32(0x202e).unwrap(),
            'b',
            char::from_u32(0x2066).unwrap(),
            'c',
        ]
        .iter()
        .collect::<String>();
        assert_eq!(escape_controls(&input), r"a\u{202e}b\u{2066}c");
        assert_eq!(
            escape_controls(&char::from_u32(0x9b).unwrap().to_string()),
            r"\u{9b}"
        );
    }
}
