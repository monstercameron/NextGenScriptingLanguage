//! Persistent, side-effect-free parser/validator probe for differential testing.
//!
//! Build with `cargo build --release --example parse_probe`, then pass an
//! explicit manifest directory. The process accepts one JSON request per input
//! line and emits exactly one JSON response per request line. It deliberately
//! calls only parsers and static validators; it never starts an applet or runs
//! a workflow.

use anyhow::{anyhow, bail, Context, Result};
use flow::{manifest::Registry, parser, runtime, ui::syntax};
use serde::Deserialize;
use serde_json::{json, Value as Json};
use std::env;
use std::io::{self, BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::ExitCode;

const MAX_REQUEST_BYTES: usize = 2 * 1024 * 1024;
const MAX_SOURCE_BYTES: usize = 256 * 1024;
const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    source: String,
    #[serde(default)]
    mode: Mode,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum Mode {
    #[default]
    Auto,
    Program,
    Ui,
}

enum RequestLine {
    Bytes(Vec<u8>),
    TooLarge,
}

fn manifest_directory() -> Result<PathBuf> {
    let mut args = env::args_os().skip(1);
    let flag = args
        .next()
        .ok_or_else(|| anyhow!("usage: parse_probe --manifest-dir <explicit-directory-or-file>"))?;
    if flag != "--manifest-dir" {
        bail!("usage: parse_probe --manifest-dir <explicit-directory-or-file>");
    }
    let path = args
        .next()
        .ok_or_else(|| anyhow!("--manifest-dir needs a path"))?;
    if args.next().is_some() {
        bail!("unexpected argument; usage: parse_probe --manifest-dir <path>");
    }
    Ok(PathBuf::from(path))
}

/// Read and discard an overlong physical line without allowing it to grow the
/// request buffer. Returning one TooLarge item keeps the following request in
/// sync with the one-request/one-response protocol.
fn read_request<R: BufRead>(
    reader: &mut R,
    bytes: &mut Vec<u8>,
) -> io::Result<Option<RequestLine>> {
    bytes.clear();
    let mut saw_bytes = false;
    let mut too_large = false;
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if !saw_bytes {
                Ok(None)
            } else if too_large {
                Ok(Some(RequestLine::TooLarge))
            } else {
                Ok(Some(RequestLine::Bytes(std::mem::take(bytes))))
            };
        }

        saw_bytes = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let count = newline.map_or(available.len(), |index| index + 1);
        if !too_large {
            if bytes.len().saturating_add(count) > MAX_REQUEST_BYTES {
                too_large = true;
                bytes.clear();
            } else {
                bytes.extend_from_slice(&available[..count]);
            }
        }
        reader.consume(count);

        if newline.is_some() {
            if too_large {
                return Ok(Some(RequestLine::TooLarge));
            }
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            return Ok(Some(RequestLine::Bytes(std::mem::take(bytes))));
        }
    }
}

fn validation_result(result: Result<()>) -> Json {
    match result {
        Ok(()) => json!({"ok": true}),
        Err(error) => json!({"ok": false, "error": format!("{error:#}")}),
    }
}

fn process_request(request: Request, registry: &Registry) -> Json {
    if request.source.len() > MAX_SOURCE_BYTES {
        return json!({
            "type": "limit_error",
            "error": format!("source exceeds {MAX_SOURCE_BYTES} bytes"),
        });
    }

    let mode = match request.mode {
        Mode::Auto if syntax::is_screen(&request.source) => Mode::Ui,
        Mode::Auto => Mode::Program,
        mode => mode,
    };
    match mode {
        Mode::Program => match parser::parse(&request.source) {
            Err(error) => json!({
                "type": "program_parse_error",
                "error": format!("{error:#}"),
            }),
            Ok(program) => match serde_json::to_value(&program) {
                Err(error) => json!({
                    "type": "internal_error",
                    "error": format!("serializing program AST: {error}"),
                }),
                Ok(ast) => json!({
                    "type": "program",
                    "ast": ast,
                    "validation": validation_result(runtime::validate(&program, registry)),
                }),
            },
        },
        Mode::Ui => match syntax::parse(&request.source) {
            Err(error) => json!({
                "type": "ui_parse_error",
                "error": format!("{error:#}"),
            }),
            Ok(screen) => json!({
                "type": "ui",
                "ast_debug": format!("{screen:#?}"),
                "validation": validation_result(syntax::validate(&screen, registry)),
            }),
        },
        Mode::Auto => unreachable!("auto mode is resolved above"),
    }
}

fn request_response(line: RequestLine, registry: &Registry) -> Json {
    match line {
        RequestLine::TooLarge => json!({
            "type": "request_error",
            "error": format!("request line exceeds {MAX_REQUEST_BYTES} bytes"),
        }),
        RequestLine::Bytes(bytes) => match serde_json::from_slice::<Request>(&bytes) {
            Err(error) => json!({
                "type": "request_error",
                "error": error.to_string(),
            }),
            Ok(request) => process_request(request, registry),
        },
    }
}

fn write_response<W: Write>(writer: &mut W, response: Json) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(&response).map_err(io::Error::other)?;
    if bytes.len() > MAX_RESPONSE_BYTES {
        bytes = serde_json::to_vec(&json!({
            "type": "limit_error",
            "error": format!("response exceeds {MAX_RESPONSE_BYTES} bytes"),
        }))
        .map_err(io::Error::other)?;
    }
    bytes.push(b'\n');
    writer.write_all(&bytes)?;
    writer.flush()
}

fn run() -> Result<()> {
    let registry =
        Registry::load(&manifest_directory()?).context("loading the explicit manifest catalog")?;
    let stdin = io::stdin();
    let mut reader = BufReader::new(stdin.lock());
    let stdout = io::stdout();
    let mut writer = stdout.lock();
    let mut buffer = Vec::with_capacity(4096);

    while let Some(line) = read_request(&mut reader, &mut buffer)? {
        write_response(&mut writer, request_response(line, &registry))?;
    }
    Ok(())
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(io::stderr().lock(), "parse_probe: {error:#}");
            ExitCode::FAILURE
        }
    }
}
