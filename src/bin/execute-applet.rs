use flow::applets::{execute, AppletError, AppletErrorKind};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::io::{self, BufRead, Read, Write};

const MAX_WORKER_LINE: usize = 16 * 1024 * 1024;
const MAX_STDIN_BYTES: usize = 16 * 1024 * 1024;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.as_slice() == ["--help"] || args.as_slice() == ["-h"] {
        println!("{}", help());
        return;
    }
    if args.as_slice() == ["--version"] {
        println!("execute-applet {}", env!("CARGO_PKG_VERSION"));
        return;
    }
    if args.as_slice() == ["--worker"] {
        if let Err(error) = run_worker(&mut io::stdin().lock(), &mut io::stdout().lock()) {
            eprintln!("execute-applet: worker_protocol: {error}");
            std::process::exit(2);
        }
        return;
    }

    let result = (|| {
        let (operation, args) = parse_invocation(&args)?;
        let mut input = Vec::new();
        io::stdin()
            .take((MAX_STDIN_BYTES + 1) as u64)
            .read_to_end(&mut input)
            .map_err(|error| request_error("stdin_error", error.to_string()))?;
        if input.len() > MAX_STDIN_BYTES {
            return Err(request_error(
                "input_too_large",
                format!("stdin exceeds {MAX_STDIN_BYTES} bytes"),
            ));
        }
        encode_output(execute(&operation, &args, &input)?)
    })();
    match result {
        Ok(output) => {
            if let Err(error) = io::stdout().lock().write_all(&output) {
                eprintln!("execute-applet: stdout_error: {error}");
                std::process::exit(75);
            }
        }
        Err(error) => {
            eprintln!("execute-applet: {}: {}", error.code, error.message);
            std::process::exit(error.exit_status());
        }
    }
}

fn parse_invocation(args: &[String]) -> Result<(String, Map<String, Value>), AppletError> {
    let [operation, flag, json] = args else {
        return Err(request_error(
            "invalid_request",
            "usage: execute-applet OPERATION --args JSON_OBJECT",
        ));
    };
    if flag != "--args" {
        return Err(request_error(
            "invalid_request",
            "expected --args followed by one JSON object",
        ));
    }
    let value: Value = serde_json::from_str(json).map_err(|error| {
        request_error("invalid_arguments", format!("invalid JSON args: {error}"))
    })?;
    let object = value
        .as_object()
        .ok_or_else(|| request_error("invalid_arguments", "--args value must be a JSON object"))?;
    Ok((operation.clone(), object.clone()))
}

fn encode_output(value: Value) -> Result<Vec<u8>, AppletError> {
    let mut bytes = serde_json::to_vec(&value)
        .map_err(|error| request_error("serialization_error", error.to_string()))?;
    bytes.push(b'\n');
    Ok(bytes)
}

#[derive(Deserialize)]
struct WorkerRequest {
    id: u64,
    arguments: Vec<String>,
    #[serde(default)]
    input: Vec<u8>,
}

#[derive(serde::Serialize)]
struct WorkerResponse {
    id: u64,
    status: i32,
    output: Vec<u8>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stderr: Option<String>,
}

fn run_worker<R: BufRead, W: Write>(reader: &mut R, writer: &mut W) -> io::Result<()> {
    loop {
        let Some(line) = read_bounded_line(reader, MAX_WORKER_LINE)? else {
            return Ok(());
        };
        let line = line.strip_suffix(b"\n").unwrap_or(&line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let response = match serde_json::from_slice::<WorkerRequest>(line) {
            Ok(request) => worker_response(request),
            Err(error) => {
                let id = serde_json::from_slice::<Value>(line)
                    .ok()
                    .and_then(|value| value.get("id").and_then(Value::as_u64))
                    .unwrap_or(0);
                WorkerResponse {
                    id,
                    status: 2,
                    output: Vec::new(),
                    stderr: Some(format!("execute-applet: invalid_worker_request: {error}\n")),
                }
            }
        };
        let mut encoded = serde_json::to_vec(&response).map_err(io::Error::other)?;
        if encoded.len() + 1 > MAX_WORKER_LINE {
            let response = WorkerResponse {
                id: response.id,
                status: 2,
                output: Vec::new(),
                stderr: Some(format!(
                    "execute-applet: output_too_large: worker response exceeds {MAX_WORKER_LINE} bytes\n"
                )),
            };
            encoded = serde_json::to_vec(&response).map_err(io::Error::other)?;
        }
        writer.write_all(&encoded)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
}

fn worker_response(request: WorkerRequest) -> WorkerResponse {
    match parse_invocation(&request.arguments)
        .and_then(|(operation, args)| execute(&operation, &args, &request.input))
        .and_then(encode_output)
    {
        Ok(output) => WorkerResponse {
            id: request.id,
            status: 0,
            output,
            stderr: None,
        },
        Err(error) => WorkerResponse {
            id: request.id,
            status: error.exit_status(),
            output: Vec::new(),
            stderr: Some(format!(
                "execute-applet: {}: {}\n",
                error.code, error.message
            )),
        },
    }
}

fn read_bounded_line<R: BufRead>(reader: &mut R, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut result = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if result.is_empty() {
                Ok(None)
            } else {
                Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "worker frame is missing its final newline",
                ))
            };
        }
        let end = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if result.len().saturating_add(end) > limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("worker line exceeds {limit} bytes"),
            ));
        }
        result.extend_from_slice(&available[..end]);
        reader.consume(end);
        if result.last() == Some(&b'\n') {
            return Ok(Some(result));
        }
    }
}

fn request_error(code: &'static str, message: impl Into<String>) -> AppletError {
    AppletError {
        code,
        kind: AppletErrorKind::Permanent,
        message: message.into(),
    }
}

fn help() -> &'static str {
    "execute-applet OPERATION --args JSON_OBJECT\n\
     Reads optional raw bytes from stdin and writes one JSON value plus LF.\n\
     Operations: path.dirname, path.basename, path.remove_suffix, text.trim_final_lf,\n\
     text.normalize, text.concat, text.matches, number.add, number.subtract,\n\
     number.multiply, number.divide, number.compare, number.parse_integer,\n\
     number.parse_integer.result, number.to_text, color.rgb_to_hex, color.parse_rgb,\n\
     color.parse_rgb.result, json.parse, json.stringify, json.get, json.set,\n\
     json.map, json.filter, csv.parse, csv.stringify, jsonc.edit_content,\n\
     fs.read, fs.snapshot,\n\
     fs.write_atomic, fs.unlink_checked, fs.transaction, fs.transaction.recover,\n\
     jsonc.set,\n\
     jsonc.remove.\n\
     Persistent workers use --worker and newline-delimited JSON frames."
}
