//! `execute attach SOCKET [--control ACTION]`: an agent-friendly view of a running
//! `execute live serve` instance, and a guarded control action.
//!
//! The command is glue over the public `execute live` CLI. It runs that CLI as a
//! child process, so it uses no private transport. It always prints one JSON object.
//!
//! Without `--control`, it reads `/runtime` and reports the current revision and
//! runtime state. With `--control ACTION`, it reads the revision first and then runs
//! the action with `--if-revision` set to that revision. If the instance changed in
//! between, the live server returns a conflict, and nothing is applied.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::io::{self, Write};
use std::path::Path;
use std::process::Command;

/// Protocol identifier of the attach object.
pub const PROTOCOL: &str = "execute.attach/1";

/// The control actions `execute live control` accepts. Checked here before any child
/// process starts, so an unknown action is a usage error.
const CONTROL_ACTIONS: &[&str] = &[
    "pause",
    "resume",
    "step",
    "continue",
    "cancel",
    "retry",
    "reset",
    "restart",
    "restart-source",
    "stop",
];

const USAGE: &str = "usage: execute attach SOCKET [--control ACTION]";

/// Run the `attach` subcommand and return the process exit status.
pub fn cli(arguments: &[String]) -> Result<i32> {
    let mut socket = None;
    let mut control = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--control" => {
                let action = arguments
                    .get(index + 1)
                    .ok_or_else(|| anyhow!("--control requires an action; {USAGE}"))?;
                if !CONTROL_ACTIONS.contains(&action.as_str()) {
                    bail!(
                        "unknown control action {action:?}; expected one of {}",
                        CONTROL_ACTIONS.join(", ")
                    );
                }
                if control.replace(action.clone()).is_some() {
                    bail!("--control may be specified only once");
                }
                index += 2;
            }
            other if other.starts_with("--") => bail!("unknown option {other}; {USAGE}"),
            other => {
                if socket.replace(other.to_owned()).is_some() {
                    bail!("attach takes one socket; {USAGE}");
                }
                index += 1;
            }
        }
    }
    let socket = socket.ok_or_else(|| anyhow!("{USAGE}"))?;
    let exe = std::env::current_exe().context("locating the execute executable")?;

    let (read_ok, read) = live(&exe, &["read", "/runtime", "--socket", &socket])?;
    if !read_ok {
        writeln!(io::stdout(), "{read}")?;
        return Ok(1);
    }
    let revision = read["revision"]
        .as_u64()
        .ok_or_else(|| anyhow!("the live server did not report a revision"))?;

    let Some(action) = control else {
        let object = json!({
            "protocol": PROTOCOL,
            "socket": socket,
            "revision": revision,
            "runtime": read.get("result").cloned().unwrap_or(Value::Null),
        });
        writeln!(io::stdout(), "{object}")?;
        return Ok(0);
    };

    let revision_text = revision.to_string();
    let (control_ok, response) = live(
        &exe,
        &[
            "control",
            &action,
            "--socket",
            &socket,
            "--if-revision",
            &revision_text,
        ],
    )?;
    let object = json!({
        "protocol": PROTOCOL,
        "socket": socket,
        "action": action,
        "revision_before": revision,
        "applied": control_ok,
        "response": response,
    });
    writeln!(io::stdout(), "{object}")?;
    Ok(if control_ok { 0 } else { 1 })
}

/// Run `execute live ARGS` and parse its single JSON response. The boolean is the
/// child's success status.
fn live(exe: &Path, arguments: &[&str]) -> Result<(bool, Value)> {
    let output = Command::new(exe)
        .arg("live")
        .args(arguments)
        .output()
        .with_context(|| format!("running execute live {}", arguments.first().unwrap_or(&"")))?;
    let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|_| {
        json!({
            "ok": false,
            "error": {
                "code": "invalid_response",
                "message": String::from_utf8_lossy(&output.stdout).into_owned(),
            }
        })
    });
    Ok((output.status.success(), value))
}
