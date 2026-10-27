use flow::applets::{execute, AppletError, AppletErrorKind};
use serde::Deserialize;
use serde_json::{Map, Value};
use std::io::{self, BufRead, Read, Write};

const MAX_WORKER_LINE: usize = 64 * 1024 * 1024;
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

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkerRequestV2 {
    version: u32,
    id: u64,
    arguments: Vec<String>,
    input_base64: String,
    #[serde(default)]
    value: Option<Value>,
    json_output: bool,
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
    let mut v2_negotiated = false;
    loop {
        let Some(line) = read_bounded_line(reader, MAX_WORKER_LINE)? else {
            return Ok(());
        };
        let line = line.strip_suffix(b"\n").unwrap_or(&line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let parsed = if v2_negotiated {
            flow::contracts::parse_json_strict(line).map_err(|error| error.to_string())
        } else {
            serde_json::from_slice(line).map_err(|error| error.to_string())
        };
        let Some(request_value) = parsed.ok() else {
            let encoded = if v2_negotiated {
                serde_json::to_vec(&worker_response_v2_error(
                    0,
                    false,
                    "invalid v2 request JSON or duplicate object key",
                ))
            } else {
                serde_json::to_vec(&serde_json::json!({
                    "id": 0,
                    "status": 2,
                    "output": [],
                    "stderr": "execute-applet: invalid_worker_request: invalid JSON\n"
                }))
            }
            .map_err(io::Error::other)?;
            writer.write_all(&encoded)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            continue;
        };
        let is_handshake = request_value.get("type").and_then(Value::as_str) == Some("handshake")
            && request_value.get("protocol").and_then(Value::as_str)
                == Some("ngsl.json-lines-worker")
            && request_value
                .get("versions")
                .and_then(Value::as_array)
                .is_some()
            && !request_value.as_object().is_some_and(|object| {
                object.contains_key("id") || object.contains_key("arguments")
            });
        if is_handshake {
            if v2_negotiated {
                let encoded = serde_json::to_vec(&serde_json::json!({
                    "type":"handshake",
                    "protocol":"ngsl.json-lines-worker",
                    "error":"protocol version cannot be renegotiated"
                }))
                .map_err(io::Error::other)?;
                writer.write_all(&encoded)?;
                writer.write_all(b"\n")?;
                writer.flush()?;
                continue;
            }
            let strict_handshake = flow::contracts::parse_json_strict(line).is_ok();
            let valid = request_value.get("protocol").and_then(Value::as_str)
                == Some("ngsl.json-lines-worker")
                && request_value
                    .get("versions")
                    .and_then(Value::as_array)
                    .is_some_and(|versions| {
                        versions.iter().any(|version| version.as_u64() == Some(2))
                    })
                && strict_handshake;
            v2_negotiated |= valid;
            let encoded = if valid {
                serde_json::to_vec(&serde_json::json!({
                    "type":"handshake",
                    "protocol":"ngsl.json-lines-worker",
                    "version":2
                }))
            } else {
                serde_json::to_vec(&serde_json::json!({"type":"handshake","version":1}))
            }
            .map_err(io::Error::other)?;
            writer.write_all(&encoded)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            continue;
        }
        let request_version = request_value.get("version").and_then(Value::as_u64);
        if v2_negotiated && request_version != Some(2) {
            let id = request_value.get("id").and_then(Value::as_u64).unwrap_or(0);
            let json_output = request_value
                .get("json_output")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let encoded = serde_json::to_vec(&worker_response_v2_error(
                id,
                json_output,
                "frame version is missing or unsupported after v2 handshake",
            ))
            .map_err(io::Error::other)?;
            writer.write_all(&encoded)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
            continue;
        }
        let is_v2 = v2_negotiated && request_version == Some(2);
        let (mut encoded, fallback) = if is_v2 {
            let request = serde_json::from_value::<WorkerRequestV2>(request_value.clone());
            let response = match request {
                Ok(request) if request.version == 2 => worker_response_v2(request),
                Ok(_) => worker_response_v2_error(0, false, "unsupported frame version"),
                Err(error) => {
                    let id = request_value.get("id").and_then(Value::as_u64).unwrap_or(0);
                    worker_response_v2_error(id, false, &format!("invalid worker request: {error}"))
                }
            };
            let encoded = serde_json::to_vec(&response).map_err(io::Error::other)?;
            let id = response.get("id").and_then(Value::as_u64).unwrap_or(0);
            let json_output = response.get("value").is_some();
            let fallback = serde_json::to_vec(&worker_response_v2_error(
                id,
                json_output,
                "output_too_large",
            ))
            .map_err(io::Error::other)?;
            (encoded, fallback)
        } else {
            let response = match serde_json::from_slice::<WorkerRequest>(line) {
                Ok(request) => worker_response(request),
                Err(error) => {
                    let id = request_value.get("id").and_then(Value::as_u64).unwrap_or(0);
                    WorkerResponse {
                        id,
                        status: 2,
                        output: Vec::new(),
                        stderr: Some(format!("execute-applet: invalid_worker_request: {error}\n")),
                    }
                }
            };
            let id = response.id;
            let encoded = serde_json::to_vec(&response).map_err(io::Error::other)?;
            let fallback = WorkerResponse {
                id,
                status: 2,
                output: Vec::new(),
                stderr: Some(format!(
                    "execute-applet: output_too_large: worker response exceeds {MAX_WORKER_LINE} bytes\n"
                )),
            };
            (
                encoded,
                serde_json::to_vec(&fallback).map_err(io::Error::other)?,
            )
        };
        if encoded.len() + 1 > MAX_WORKER_LINE {
            encoded = fallback;
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

fn worker_response_v2(request: WorkerRequestV2) -> Value {
    if request.version != 2 {
        return worker_response_v2_error(
            request.id,
            request.json_output,
            "unsupported frame version",
        );
    }
    let input = match base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        &request.input_base64,
    ) {
        Ok(input) => input,
        Err(error) => {
            return worker_response_v2_error(
                request.id,
                request.json_output,
                &format!("invalid input_base64: {error}"),
            );
        }
    };
    let invocation = parse_invocation_v2(&request.arguments, request.value);
    match invocation.and_then(|(operation, args)| execute(&operation, &args, &input)) {
        Ok(value) if request.json_output => serde_json::json!({
            "version":2,
            "id":request.id,
            "status":0,
            "value":value
        }),
        Ok(value) => match encode_output(value) {
            Ok(output) => serde_json::json!({
                "version":2,
                "id":request.id,
                "status":0,
                "output_base64":base64::Engine::encode(&base64::engine::general_purpose::STANDARD, output)
            }),
            Err(error) => worker_response_v2_error(
                request.id,
                request.json_output,
                &format!("{}: {}", error.code, error.message),
            ),
        },
        Err(error) => worker_response_v2_error(
            request.id,
            request.json_output,
            &format!("{}: {}", error.code, error.message),
        ),
    }
}

fn parse_invocation_v2(
    arguments: &[String],
    value: Option<Value>,
) -> Result<(String, Map<String, Value>), AppletError> {
    let Some(value) = value else {
        return parse_invocation(arguments);
    };
    let Some(operation) = arguments.first() else {
        return Err(request_error(
            "invalid_arguments",
            "v2 typed JSON value requires an operation argument",
        ));
    };
    let trailing = &arguments[1..];
    if !trailing.is_empty() && !(trailing.len() == 1 && trailing[0] == "--args") {
        return Err(request_error(
            "invalid_arguments",
            "v2 typed JSON value cannot be combined with additional positional arguments",
        ));
    }
    let object = value.as_object().ok_or_else(|| {
        request_error("invalid_arguments", "v2 typed JSON value must be an object")
    })?;
    Ok((operation.clone(), object.clone()))
}

fn worker_response_v2_error(id: u64, json_output: bool, message: &str) -> Value {
    let mut response = serde_json::json!({
        "version":2,
        "id":id,
        "status":2,
        "stderr":format!("execute-applet: {message}\n")
    });
    if json_output {
        response["value"] = Value::Null;
    } else {
        response["output_base64"] = Value::String(String::new());
    }
    response
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
     text.normalize, text.trim, text.length, text.substring, text.pad, text.repeat,\n\
     text.starts_with, text.ends_with, text.contains_any, text.truncate, text.concat,\n\
     text.matches, number.abs, number.ceil, number.clamp, number.divmod, number.floor,\n\
     number.format, number.format_size, number.int_divide, number.max, number.min,\n\
     number.modulo, number.round, number.sign, number.trunc, number.add, number.subtract,\n\
     number.multiply, number.divide, number.compare, number.parse_integer,\n\
     number.parse_integer.result, number.to_text, color.rgb_to_hex, color.parse_rgb,\n\
     color.parse_rgb.result, json.parse, json.stringify, json.get, json.set,\n\
     json.map, json.filter, json.concat, json.delete, json.entries, json.first,\n\
     json.group_by, json.has, json.keys, json.last, json.length, json.merge,\n\
     json.omit, json.pick, json.rename_keys, json.sort_by, json.sum, json.unique_by,\n\
     json.values, csv.parse, csv.stringify, jsonc.edit_content,\n\
     text.lines, text.split, text.default_if_empty, text.replace,\n\
     text.strip_prefix, text.strip_suffix, text.lower, text.upper, text.join,\n\
     path.join, text.render, list.chunk, list.concat, list.enumerate, list.first,\n\
     list.flatten, list.last, list.range, list.rest, list.reverse, list.slice, list.zip,\n\
     hash.sha256, hash.sha512, encode.base64, encode.form, encode.hex, encode.url,\n\
     decode.base64, decode.hex, decode.url, time.add, time.diff, time.format, time.now,\n\
     time.parse, time.sleep, wait.until, yaml.parse, yaml.stringify, toml.parse, toml.stringify,\n\
     ini.parse, ini.stringify, env.load, fs.test, fs.glob, fs.ensure_file, fs.ensure_line,\n\
     fs.ensure_symlink, fs.ensure_dir,\n\
     fs.read, fs.snapshot,\n\
     fs.write_atomic, fs.unlink_checked, fs.transaction, fs.transaction.recover,\n\
     jsonc.set,\n\
     jsonc.remove.\n\
     Persistent workers use --worker and newline-delimited JSON frames."
}

#[cfg(test)]
mod worker_protocol_tests {
    use super::*;
    use std::io::Cursor;

    fn handshake() -> Value {
        serde_json::json!({
            "type": "handshake",
            "protocol": "ngsl.json-lines-worker",
            "versions": [2]
        })
    }

    fn legacy_effectful_request(id: u64, path: &std::path::Path) -> Value {
        let arguments = serde_json::json!({
            "path": path,
            "content": "must-not-be-written"
        });
        serde_json::json!({
            "id": id,
            "arguments": [
                "fs.ensure_file",
                "--args",
                arguments.to_string()
            ],
            "input": []
        })
    }

    fn typed_effectful_request(id: u64, path: &std::path::Path) -> Value {
        serde_json::json!({
            "version": 2,
            "id": id,
            "arguments": ["fs.ensure_file", "--args"],
            "input_base64": "",
            "value": {
                "path": path,
                "content": "written-after-valid-v2-frame"
            },
            "json_output": true
        })
    }

    fn run_frames(frames: &[Value]) -> Vec<Value> {
        let mut input = Vec::new();
        for frame in frames {
            input.extend(serde_json::to_vec(frame).expect("request frame JSON"));
            input.push(b'\n');
        }
        let mut output = Vec::new();
        run_worker(&mut Cursor::new(input), &mut output).expect("worker frames should be handled");
        output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).expect("response frame JSON"))
            .collect()
    }

    #[test]
    fn unnegotiated_version_field_keeps_legacy_frame_interpretation() {
        let mut request =
            br#"{"version":2,"id":7,"arguments":["unknown.operation","--args","{}"],"input":[]}"#
                .to_vec();
        request.push(b'\n');
        let mut reader = Cursor::new(request);
        let mut response = Vec::new();
        run_worker(&mut reader, &mut response).expect("worker request should be handled");

        let response: Value = serde_json::from_slice(&response).expect("legacy response JSON");
        assert_eq!(response["id"], 7);
        assert_eq!(response["status"], 2);
        assert!(response["output"].is_array());
        assert!(response.get("version").is_none());
    }

    #[test]
    fn handshake_extension_fields_do_not_capture_legacy_requests() {
        let mut request = br#"{"type":"handshake","protocol":"ngsl.json-lines-worker","versions":[2],"id":8,"arguments":["unknown.operation","--args","{}"],"input":[]}"#
            .to_vec();
        request.push(b'\n');
        let mut reader = Cursor::new(request);
        let mut response = Vec::new();
        run_worker(&mut reader, &mut response).expect("legacy worker request should be handled");

        let response: Value = serde_json::from_slice(&response).expect("legacy response JSON");
        assert_eq!(response["id"], 8);
        assert_eq!(response["status"], 2);
        assert!(response["output"].is_array());
        assert!(response.get("version").is_none());
    }

    #[test]
    fn duplicate_legacy_id_and_arguments_are_rejected_before_effects() {
        let directory = tempfile::tempdir().expect("temporary legacy effect directory");
        let duplicate_id_path = directory.path().join("duplicate-id-must-not-write");
        let duplicate_arguments_path = directory.path().join("duplicate-arguments-must-not-write");
        let arguments_for = |path: &std::path::Path| {
            serde_json::to_string(&vec![
                "fs.ensure_file".to_owned(),
                "--args".to_owned(),
                serde_json::json!({
                    "path": path,
                    "content": "must-not-be-written"
                })
                .to_string(),
            ])
            .expect("legacy arguments JSON")
        };
        let duplicate_id_arguments = arguments_for(&duplicate_id_path);
        let duplicate_arguments_arguments = arguments_for(&duplicate_arguments_path);
        let duplicate_id =
            format!(r#"{{"id":40,"id":41,"arguments":{duplicate_id_arguments},"input":[]}}"#);
        let duplicate_arguments = format!(
            r#"{{"id":42,"arguments":["unknown.operation","--args","{{}}"],"arguments":{duplicate_arguments_arguments},"input":[]}}"#
        );
        let mut input = Vec::new();
        for request in [duplicate_id, duplicate_arguments] {
            input.extend(request.as_bytes());
            input.push(b'\n');
        }
        let mut output = Vec::new();
        run_worker(&mut Cursor::new(input), &mut output)
            .expect("legacy requests should produce errors");
        let responses = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).expect("legacy response JSON"))
            .collect::<Vec<_>>();

        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["id"], 41);
        assert_eq!(responses[1]["id"], 42);
        for response in &responses {
            assert_eq!(response["status"], 2);
            assert_eq!(response["output"], serde_json::json!([]));
            assert!(response.get("version").is_none());
        }
        assert!(!duplicate_id_path.exists());
        assert!(!duplicate_arguments_path.exists());
    }

    #[test]
    fn negotiated_v2_rejects_missing_and_wrong_versions_before_effects() {
        let directory = tempfile::tempdir().expect("temporary effect directory");
        let missing_version_path = directory.path().join("missing-version");
        let wrong_version_path = directory.path().join("wrong-version");
        let mut wrong_version = legacy_effectful_request(11, &wrong_version_path);
        wrong_version["version"] = serde_json::json!(1);

        let responses = run_frames(&[
            handshake(),
            legacy_effectful_request(10, &missing_version_path),
            wrong_version,
        ]);

        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0]["version"], 2);
        for response in &responses[1..] {
            assert_eq!(response["version"], 2);
            assert_eq!(response["status"], 2);
        }
        assert!(!missing_version_path.exists());
        assert!(!wrong_version_path.exists());
    }

    #[test]
    fn negotiated_v2_cannot_be_downgraded_by_a_later_handshake() {
        let directory = tempfile::tempdir().expect("temporary downgrade directory");
        let rejected_path = directory.path().join("rejected-legacy-frame");
        let accepted_path = directory.path().join("accepted-v2-frame");
        let renegotiation = serde_json::json!({
            "type": "handshake",
            "protocol": "ngsl.json-lines-worker",
            "versions": [1]
        });

        let responses = run_frames(&[
            handshake(),
            renegotiation,
            legacy_effectful_request(12, &rejected_path),
            typed_effectful_request(13, &accepted_path),
        ]);

        assert_eq!(responses.len(), 4);
        assert_eq!(responses[0]["version"], 2);
        assert!(responses[1].get("error").is_some());
        assert_eq!(responses[2]["version"], 2);
        assert_eq!(responses[2]["status"], 2);
        assert_eq!(responses[3]["version"], 2);
        assert_eq!(responses[3]["status"], 0);
        assert!(!rejected_path.exists());
        assert_eq!(
            std::fs::read_to_string(accepted_path).expect("valid v2 frame effect"),
            "written-after-valid-v2-frame"
        );
    }

    #[test]
    fn negotiated_v2_rejects_duplicate_json_keys_before_effects() {
        let directory = tempfile::tempdir().expect("temporary duplicate-key directory");
        let path = directory.path().join("duplicate-key-must-not-write");
        let path_json = serde_json::to_string(&path).expect("escaped temporary path");
        let mut input = serde_json::to_vec(&handshake()).expect("handshake frame");
        input.push(b'\n');
        input.extend(
            format!(
                r#"{{"version":2,"id":14,"arguments":["fs.ensure_file","--args"],"input_base64":"","value":{{"path":{path_json},"path":{path_json},"content":"must-not-be-written"}},"json_output":true}}"#
            )
            .as_bytes(),
        );
        input.push(b'\n');
        let mut output = Vec::new();
        run_worker(&mut Cursor::new(input), &mut output).expect("worker frames should be handled");
        let responses = output
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice::<Value>(line).expect("response frame JSON"))
            .collect::<Vec<_>>();

        assert_eq!(responses.len(), 2);
        assert_eq!(responses[1]["version"], 2);
        assert_eq!(responses[1]["status"], 2);
        assert!(!path.exists());
    }

    #[test]
    fn worker_reader_accepts_lines_over_sixteen_mib_within_advertised_cap() {
        assert_eq!(MAX_WORKER_LINE, 64 * 1024 * 1024);
        let mut input = vec![b'x'; 16 * 1024 * 1024 + 1];
        input.push(b'\n');
        let line = read_bounded_line(&mut Cursor::new(input), MAX_WORKER_LINE)
            .expect("line below the advertised 64 MiB limit")
            .expect("line present");
        assert_eq!(line.len(), 16 * 1024 * 1024 + 2);
    }
}
