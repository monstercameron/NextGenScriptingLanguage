//! Terminal-safe rendering of text that can carry untrusted bytes: file names, applet
//! output and error messages. Control characters, including ESC (which starts terminal
//! sequences), CSI, OSC and the C1 range, become visible `\u{..}` escapes. Newlines and
//! tabs stay, so multi-line messages keep their shape.

use std::fmt::Write;

/// Escape control characters, except newline and tab, as visible `\u{..}` sequences.
pub fn escape_controls(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\n' || character == '\t' || !character.is_control() {
            out.push(character);
        } else {
            let _ = write!(out, "\\u{{{:x}}}", u32::from(character));
        }
    }
    out
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
}
