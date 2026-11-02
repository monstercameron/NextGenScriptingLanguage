//! Terminal-safe rendering of text that can carry untrusted bytes: file names, applet
//! output and error messages. Control characters, including ESC (which starts terminal
//! sequences), CSI, OSC and the C1 range, become visible `\u{..}` escapes. Newlines and
//! tabs stay in rendered messages so intentional layout keeps its shape.

use std::fmt::Write;

/// Escape control characters, except newline and tab, as visible `\u{..}` sequences.
pub fn escape_controls(text: &str) -> String {
    escape(text, true)
}

/// Escape controls and line separators in one untrusted field, including newlines and tabs, so
/// the field cannot inject terminal rows or columns into surrounding output.
pub fn escape_terminal_field(text: &str) -> String {
    escape(text, false)
}

/// Whether pipeline output is bound for a terminal: the actual stdout sink,
/// not the content source. Model text becomes an ordinary pipeline value, so
/// the sink cannot tell model bytes apart later; every stdout sink below
/// reads this one decision. Pipes and files keep exact bytes; TTYs get
/// escaped text.
pub fn stdout_is_terminal() -> bool {
    std::io::IsTerminal::is_terminal(&std::io::stdout())
}

/// Encode pipeline bytes for the stdout sink. Pipes and files keep the exact
/// bytes (H17 pipe guarantee); a TTY sink gets [`escape_controls`] over the
/// text so OSC/CSI/C1 escapes and bidi overrides in untrusted model output
/// cannot rewrite the terminal. Invalid UTF-8 renders lossily on a TTY: it
/// cannot display as text there, while pipes still carry the original bytes.
/// The `stdout_is_terminal` flag is a parameter (rather than read here) so the
/// sink decision stays testable without a real TTY.
pub fn encode_pipeline_for_stdout(bytes: &[u8], stdout_is_terminal: bool) -> Vec<u8> {
    if !stdout_is_terminal {
        return bytes.to_vec();
    }
    escape_controls(&String::from_utf8_lossy(bytes)).into_bytes()
}

fn escape(text: &str, preserve_layout: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for character in text.chars() {
        if (preserve_layout && (character == '\n' || character == '\t'))
            || !is_unsafe_terminal_character(character)
        {
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
                | 0x2028..=0x2029
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

    #[test]
    fn pipeline_sink_escapes_model_escapes_on_a_tty() {
        use super::encode_pipeline_for_stdout;
        // OSC title-set, CSI erase, C1 CSI and a bidi override: the model
        // payload from #522 must not reach a terminal writer raw.
        let hostile = "go\u{1b}]0;pwned\u{7}now\u{1b}[2Jthen\u{9b}31mswap\u{202e}end\nnext\tcol";
        let rendered = encode_pipeline_for_stdout(hostile.as_bytes(), true);
        let rendered = String::from_utf8(rendered).expect("escaped output is UTF-8");
        assert!(
            !rendered.contains('\u{1b}'),
            "raw ESC reached the TTY encoding: {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{9b}'),
            "raw C1 CSI reached the TTY encoding: {rendered:?}"
        );
        assert!(
            !rendered.contains('\u{202e}'),
            "raw bidi override reached the TTY encoding: {rendered:?}"
        );
        assert_eq!(
            rendered,
            "go\\u{1b}]0;pwned\\u{7}now\\u{1b}[2Jthen\\u{9b}31mswap\\u{202e}end\nnext\tcol"
        );
    }

    #[test]
    fn pipeline_sink_keeps_pipe_bytes_exact() {
        use super::encode_pipeline_for_stdout;
        let hostile = "go\u{1b}]0;pwned\u{7}now\u{1b}[2Jthen\u{9b}31mswap\u{202e}end\ncaf\u{e9}\n";
        assert_eq!(
            encode_pipeline_for_stdout(hostile.as_bytes(), false),
            hostile.as_bytes()
        );
    }

    #[test]
    fn pipeline_sink_preserves_valid_text_and_shows_invalid_lossily_on_a_tty() {
        use super::encode_pipeline_for_stdout;
        // Valid UTF-8 (including non-ASCII) survives TTY escaping unchanged.
        assert_eq!(
            encode_pipeline_for_stdout("caf\u{e9} \u{6f22}\u{5b57}\n".as_bytes(), true),
            "caf\u{e9} \u{6f22}\u{5b57}\n".as_bytes()
        );
        // Invalid bytes cannot render as terminal text; they become a visible
        // placeholder instead of passing through raw or vanishing.
        let rendered = encode_pipeline_for_stdout(b"a\xffb", true);
        assert_eq!(rendered, "a\u{fffd}b".as_bytes());
        assert_eq!(
            encode_pipeline_for_stdout(b"a\xffb", false),
            b"a\xffb".to_vec(),
            "pipes keep even invalid bytes exact"
        );
    }

    #[test]
    fn field_escaping_turns_layout_controls_and_unicode_separators_visible() {
        assert_eq!(
            super::escape_terminal_field("line\nnext\tcol\u{2028}row\u{2029}paragraph"),
            r"line\u{a}next\u{9}col\u{2028}row\u{2029}paragraph"
        );
        assert_eq!(
            escape_controls("line\u{2028}next\u{2029}paragraph"),
            r"line\u{2028}next\u{2029}paragraph"
        );
    }
}
