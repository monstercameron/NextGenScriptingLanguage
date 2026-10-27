//! Prompts for declared inputs: `accept NAME as TYPE from prompt "QUESTION"`.
//!
//! A prompt is a declaration, so `explain` and `inspect` list it, and a run can be
//! pre-answered: with `--input NAME=VALUE`, or with an answers file (`--answers FILE`)
//! that is a JSON object. Supplied answers always win over the terminal, and a
//! prompt that has no answer and no terminal fails the run before any step starts.
//!
//! A secret prompt reads its answer with echo off. A secret answer is never printed,
//! recorded, or shown in an error: its value is withheld in `inspect`, and its
//! record entry is replaced by a marker.

use crate::ast::{InputDeclaration, InputSource, Value};
use crate::terminal_text::escape_terminal_field;
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::path::Path;

/// The longest answer a prompt accepts, in bytes.
pub const MAX_ANSWER_BYTES: usize = 4096;
/// The largest answers file that is read.
const MAX_ANSWERS_FILE_BYTES: u64 = 64 * 1024;
const NEWLINE: u8 = b'\n';
const RETURN: u8 = b'\r';

fn write_question(output: &mut impl Write, question: &str) -> std::io::Result<()> {
    write!(output, "{}: ", escape_terminal_field(question))
}

/// Ask one question. `Ok(None)` means there is no terminal to ask on.
pub type Ask<'a> = dyn FnMut(&str, bool) -> Result<Option<String>> + 'a;

/// Fill declared prompt inputs that have no supplied value. An empty answer takes the
/// input's default when it has one. Answers are checked like command-line values.
pub fn fill_missing(
    declarations: &[InputDeclaration],
    values: &mut BTreeMap<String, Value>,
    ask: &mut Ask<'_>,
) -> Result<()> {
    for declaration in declarations {
        let Some(InputSource::Prompt { question, secret }) = &declaration.source else {
            continue;
        };
        if values.contains_key(&declaration.name) {
            continue;
        }
        // No terminal: the missing answer is refused when the run resolves its inputs.
        let Some(answer) = ask(question, *secret)? else {
            continue;
        };
        if answer.is_empty() {
            if let Some(default) = &declaration.default {
                values.insert(declaration.name.clone(), default.clone());
                continue;
            }
            bail!(
                "line {}: input {:?} needs an answer to the prompt {question:?}; an empty answer was given",
                declaration.line,
                declaration.name
            );
        }
        let value = declaration.parse_cli(&answer).with_context(|| {
            format!(
                "line {}: invalid answer for input {:?}",
                declaration.line, declaration.name
            )
        })?;
        values.insert(declaration.name.clone(), value);
    }
    Ok(())
}

/// The answers in a JSON object file, as text values for `--input`-style parsing.
/// Strings are used as they are; numbers and booleans are written as their JSON text.
pub fn answers_from_file(path: &Path) -> Result<BTreeMap<String, String>> {
    #[cfg(not(any(unix, windows)))]
    bail!(
        "cannot verify that answers file {} is private on this platform",
        path.display()
    );
    #[cfg(any(unix, windows))]
    let file = crate::security::open_private_file(path)
        .with_context(|| format!("opening private answers file {}", path.display()))?;
    #[cfg(windows)]
    crate::security::validate_private_windows_acl(&file, path)?;
    let mut text = String::new();
    #[cfg(any(unix, windows))]
    file.take(MAX_ANSWERS_FILE_BYTES + 1)
        .read_to_string(&mut text)
        .with_context(|| format!("reading answers file {}", path.display()))?;
    #[cfg(any(unix, windows))]
    if text.len() as u64 > MAX_ANSWERS_FILE_BYTES {
        bail!("answers file {} must be at most 64 KiB", path.display());
    }
    let object: serde_json::Map<String, serde_json::Value> =
        serde_json::from_str(&text).context("the answers file must be a JSON object")?;
    let mut answers = BTreeMap::new();
    for (name, value) in object {
        let text = match value {
            serde_json::Value::String(text) => text,
            serde_json::Value::Number(_) | serde_json::Value::Bool(_) => value.to_string(),
            _ => bail!("the answer for {name:?} must be text, a number, or a boolean"),
        };
        answers.insert(name, text);
    }
    Ok(answers)
}

/// Read one answer line from a terminal, byte by byte, so a secret is never buffered
/// past its newline. `Ok(None)` means the terminal ended before any byte arrived.
fn read_answer(reader: &mut impl Read) -> Result<Option<String>> {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        match reader.read(&mut byte) {
            Ok(0) if line.is_empty() => return Ok(None),
            Ok(0) => break,
            Ok(_) if byte[0] == NEWLINE => break,
            Ok(_) => {
                if line.len() >= MAX_ANSWER_BYTES {
                    bail!("the answer is longer than {MAX_ANSWER_BYTES} bytes");
                }
                line.push(byte[0]);
            }
            Err(error) => return Err(error).context("reading the answer"),
        }
    }
    if line.last() == Some(&RETURN) {
        line.pop();
    }
    String::from_utf8(line)
        .map(Some)
        .map_err(|_| anyhow::anyhow!("the answer is not UTF-8 text"))
}

/// Ask on the controlling terminal. Returns `None` when there is none.
#[cfg(unix)]
pub fn terminal_ask(question: &str, secret: bool) -> Result<Option<String>> {
    if prompts_disabled() {
        return Ok(None);
    }
    use std::os::unix::io::AsFd;
    let Ok(mut tty) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
    else {
        return Ok(None);
    };
    write_question(&mut tty, question).context("writing the prompt")?;
    tty.flush().context("writing the prompt")?;
    // The echo guard borrows its own duplicate of the terminal, so `tty` stays free for
    // reading. Both descriptors name the same terminal, and tcsetattr applies to it.
    let echo_fd = secret
        .then(|| tty.try_clone())
        .transpose()
        .context("duplicating the terminal for echo control")?;
    let echo = echo_fd
        .as_ref()
        .map(|fd| EchoOff::new(fd.as_fd()))
        .transpose()?;
    let answer = read_answer(&mut tty);
    drop(echo);
    if secret {
        writeln!(tty).ok();
    }
    answer
}

/// Ask on the console. Returns `None` when there is no console.
#[cfg(windows)]
pub fn terminal_ask(question: &str, secret: bool) -> Result<Option<String>> {
    if prompts_disabled() {
        return Ok(None);
    }
    use std::os::windows::io::AsRawHandle;
    use windows_sys::Win32::System::Console::{GetConsoleMode, SetConsoleMode, ENABLE_ECHO_INPUT};
    let Ok(mut input) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONIN$")
    else {
        return Ok(None);
    };
    let Ok(mut output) = std::fs::OpenOptions::new().write(true).open("CONOUT$") else {
        return Ok(None);
    };
    write_question(&mut output, question).context("writing the prompt")?;
    output.flush().context("writing the prompt")?;
    let handle = input.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
    let mut mode = 0u32;
    let mut saved = None;
    if secret {
        // A secret answer is never read with echo on. If the console mode cannot be
        // changed, refuse the prompt rather than show the answer.
        // SAFETY: `handle` is the raw handle of `input`, a File this function owns and keeps alive
        // until after the mode calls. `&mut mode` is a valid, exclusive pointer to a local u32
        // that GetConsoleMode writes through. A zero return is checked before `mode`.
        if unsafe { GetConsoleMode(handle, &mut mode) } == 0
            // SAFETY: this call runs only when GetConsoleMode above returned nonzero, so `mode`
            // holds a console mode the API returned. Clearing ENABLE_ECHO_INPUT changes only that
            // flag.
            || unsafe { SetConsoleMode(handle, mode & !ENABLE_ECHO_INPUT) } == 0
        {
            return Err(anyhow::anyhow!(
                "refusing a secret prompt: the console would show the answer"
            ));
        }
        saved = Some(mode);
    }
    let answer = read_answer(&mut input);
    if let Some(mode) = saved {
        // SAFETY: `handle` is still the live handle of `input`, which is in scope here, and `mode`
        // is the value GetConsoleMode returned (`saved` is set only after that call succeeded).
        // The return value is ignored, so the restore is not confirmed.
        unsafe { SetConsoleMode(handle, mode) };
        writeln!(output).ok();
    }
    answer
}

/// `EXECUTE_NO_PROMPT=1` turns terminal prompts off, so an unattended run fails before
/// any effect when an answer is missing. Automation and tests use it.
fn prompts_disabled() -> bool {
    std::env::var_os("EXECUTE_NO_PROMPT").is_some_and(|value| value == "1")
}

/// Without a terminal API, no prompt can be answered interactively.
#[cfg(not(any(unix, windows)))]
pub fn terminal_ask(_question: &str, _secret: bool) -> Result<Option<String>> {
    Ok(None)
}

/// Turns terminal echo off for one borrowed descriptor and restores it when dropped. The
/// borrow keeps the descriptor open for as long as the guard exists.
#[cfg(unix)]
struct EchoOff<'a> {
    fd: std::os::unix::io::BorrowedFd<'a>,
    saved: libc::termios,
}

#[cfg(unix)]
impl<'a> EchoOff<'a> {
    fn new(fd: std::os::unix::io::BorrowedFd<'a>) -> Result<Self> {
        use std::os::unix::io::AsRawFd;
        let raw = fd.as_raw_fd();
        // SAFETY: termios is plain data, and tcgetattr fills it in for a valid fd.
        let mut saved: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `saved` is a live, zero-initialized termios, and tcgetattr writes only through
        // the pointer. `raw` is open because `fd` borrows it. A non-terminal descriptor makes
        // tcgetattr return -1, which is checked.
        if unsafe { libc::tcgetattr(raw, &mut saved) } != 0 {
            return Err(std::io::Error::last_os_error()).context("reading terminal settings");
        }
        let mut quiet = saved;
        quiet.c_lflag &= !libc::ECHO;
        // SAFETY: `quiet` is a copy of `saved` that tcgetattr filled in, with ECHO cleared, and
        // `raw` is open for the whole call because `fd` borrows it. TCSANOW only reads `quiet`.
        if unsafe { libc::tcsetattr(raw, libc::TCSANOW, &quiet) } != 0 {
            return Err(std::io::Error::last_os_error()).context("turning echo off");
        }
        Ok(Self { fd, saved })
    }
}

#[cfg(unix)]
impl Drop for EchoOff<'_> {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        // SAFETY: `self.fd` borrows a descriptor that is open for this guard's lifetime, and
        // this restores the settings that tcgetattr returned for that same descriptor.
        unsafe { libc::tcsetattr(self.fd.as_raw_fd(), libc::TCSANOW, &self.saved) };
    }
}

#[cfg(test)]
#[path = "../tests/support/private_answers.rs"]
mod private_answers;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::InputType;

    #[test]
    fn terminal_prompt_escapes_controls_and_bidi_before_writing() {
        let mut output = Vec::new();
        write_question(
            &mut output,
            "Who\u{001b}[2J\u{009b}31m\u{202e}\n\t\u{2028}\u{2029}?",
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            r"Who\u{1b}[2J\u{9b}31m\u{202e}\u{a}\u{9}\u{2028}\u{2029}?: "
        );
    }

    fn prompt(
        name: &str,
        kind: InputType,
        secret: bool,
        default: Option<Value>,
    ) -> InputDeclaration {
        InputDeclaration {
            name: name.into(),
            kind,
            default,
            source: Some(InputSource::Prompt {
                question: format!("{name}?"),
                secret,
            }),
            line: 1,
            description: None,
            choices: None,
        }
    }

    #[test]
    fn a_prompt_fills_only_the_inputs_without_a_supplied_value() {
        let declarations = [
            prompt("name", InputType::Text, false, None),
            prompt("count", InputType::Integer, false, None),
        ];
        let mut values = BTreeMap::from([("name".to_owned(), Value::String("given".into()))]);
        let mut asked = Vec::new();
        fill_missing(&declarations, &mut values, &mut |question, _| {
            asked.push(question.to_owned());
            Ok(Some("7".to_owned()))
        })
        .unwrap();
        assert_eq!(
            asked,
            vec!["count?".to_owned()],
            "supplied values are never asked"
        );
        assert_eq!(values["count"], Value::Integer(7));
        assert_eq!(values["name"], Value::String("given".into()));
    }

    #[test]
    fn an_empty_answer_takes_the_default_and_a_bad_answer_is_refused() {
        let declarations = [prompt(
            "mode",
            InputType::Text,
            false,
            Some(Value::String("light".into())),
        )];
        let mut values = BTreeMap::new();
        fill_missing(&declarations, &mut values, &mut |_, _| {
            Ok(Some(String::new()))
        })
        .unwrap();
        assert_eq!(values["mode"], Value::String("light".into()));

        let declarations = [prompt("count", InputType::Integer, false, None)];
        let mut values = BTreeMap::new();
        let error = fill_missing(&declarations, &mut values, &mut |_, _| {
            Ok(Some("oops".into()))
        })
        .expect_err("a non-integer answer must be refused");
        assert!(format!("{error:#}").contains("invalid answer"), "{error:#}");
    }

    #[test]
    fn no_terminal_leaves_the_input_missing_for_the_run_to_refuse() {
        let declarations = [prompt("token", InputType::Text, true, None)];
        let mut values = BTreeMap::new();
        fill_missing(&declarations, &mut values, &mut |_, _| Ok(None)).unwrap();
        assert!(values.is_empty());
    }

    #[test]
    fn a_terminal_line_ends_at_the_newline_and_drops_a_return() {
        let mut bytes: &[u8] = b"first\r\nsecond\n";
        assert_eq!(read_answer(&mut bytes).unwrap().as_deref(), Some("first"));
        assert_eq!(read_answer(&mut bytes).unwrap().as_deref(), Some("second"));
        assert_eq!(read_answer(&mut bytes).unwrap(), None);
    }

    #[test]
    fn an_answers_file_must_be_a_json_object_of_scalars() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("answers.json");
        std::fs::write(&path, r#"{"name":"ada","count":3,"ok":true}"#).unwrap();
        super::private_answers::make_private(&path);
        let answers = answers_from_file(&path).unwrap();
        assert_eq!(answers["name"], "ada");
        assert_eq!(answers["count"], "3");
        assert_eq!(answers["ok"], "true");

        std::fs::write(&path, r#"["not","an","object"]"#).unwrap();
        assert!(answers_from_file(&path).is_err());
        std::fs::write(&path, r#"{"nested":{"a":1}}"#).unwrap();
        let error = answers_from_file(&path).expect_err("an object answer must be refused");
        assert!(!format!("{error:#}").contains("\"a\""));
    }
}
