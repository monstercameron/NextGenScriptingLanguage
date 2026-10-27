//! Argument parsing: the option table that turns the command line into `Args`.

use super::args::Args;
use crate::{print_help, usage};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::io::{self, Write};
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;
use std::path::PathBuf;

/// Parse command line arguments. `None` means help or version was printed.
pub(crate) fn parse_args(arguments: &[String]) -> Result<Option<Args>> {
    if arguments.is_empty() {
        print_help()?;
        bail!("a command is required");
    }
    let (command, first_argument) = match arguments.first().map(String::as_str) {
        Some("check") => ("check", 1),
        Some("explain") => ("explain", 1),
        Some("inspect") => ("inspect", 1),
        Some("run") => ("run", 1),
        Some("resume") => ("resume", 1),
        Some(_) => ("run", 0),
        None => {
            print_help()?;
            bail!("a workflow file is required");
        }
    };

    let mut file = None;
    let mut json = false;
    let mut diagnostic_explain = None;
    let mut explain_version = None;
    #[cfg(feature = "agent-advisory")]
    let mut allow = Vec::new();
    #[cfg(not(feature = "agent-advisory"))]
    let allow = Vec::new();
    let mut what_if = false;
    let mut confirm = None;
    let mut policy_header = false;
    let mut single_instance = None;
    let mut github = false;
    let mut inputs = BTreeMap::new();
    let mut answers: Option<PathBuf> = None;
    let mut program_arguments = Vec::new();
    let mut program_arguments_literal = false;
    let mut manifest_dir = None;
    let mut manifest_catalog = None;
    let mut capability_policy = None;
    let mut execution_mode = None;
    let mut data_flow_policy = None;
    let mut cache_dir = None;
    let mut checkpoint_dir = None;
    let mut record_dir = None;
    let mut retry_uncertain = false;
    let mut once = false;
    let mut trace = false;
    let mut headless = false;
    let mut max_output_bytes = 16 * 1024 * 1024;
    let mut max_output_seen = false;
    let mut max_loop_iterations = flow::runtime::DEFAULT_MAX_LOOP_ITERATIONS;
    let mut max_loop_seen = false;
    let mut max_runs = None;
    let mut missed_ticks = None;
    let mut max_catch_up = None;
    let mut calendar_dedup = None;
    let mut command_timeout = None;
    let mut drain_timeout = None;
    let mut total_timeout = None;
    let mut max_record_bytes = 65_536;
    let mut max_record_bytes_seen = false;
    let mut control = false;
    let mut break_at: Vec<usize> = Vec::new();
    let mut index = first_argument;
    while index < arguments.len() {
        let argument = &arguments[index];
        match argument.as_str() {
            "--" => {
                index += 1;
                if file.is_none() {
                    let argument = arguments
                        .get(index)
                        .context("missing workflow file after --")?;
                    file = Some(workflow_path(argument)?);
                    index += 1;
                    // Shebang form: `--` came from the interpreter line. A second
                    // `--` makes the following arguments literal, including `--help`.
                    if arguments.get(index).is_some_and(|a| a == "--") {
                        program_arguments_literal = true;
                        index += 1;
                    }
                } else {
                    // An explicit separator after the file: arguments are literal.
                    program_arguments_literal = true;
                }
                program_arguments.extend_from_slice(&arguments[index..]);
                break;
            }
            "--json" if command == "check" || command == "explain" => {
                if json {
                    bail!("--json may be specified only once");
                }
                json = true;
            }
            "--explain" if command == "check" => {
                if diagnostic_explain.is_some() {
                    bail!("--explain may be specified only once");
                }
                diagnostic_explain = Some(take_option_value(
                    arguments,
                    &mut index,
                    "--explain",
                    "a diagnostic code",
                )?);
            }
            "--explain-version" if command == "explain" || command == "run" => {
                if explain_version.is_some() {
                    bail!("--explain-version may be specified only once");
                }
                let version =
                    take_option_value(arguments, &mut index, "--explain-version", "1 or 2")?;
                explain_version = Some(match version.as_str() {
                    "1" => 1,
                    "2" => 2,
                    _ => bail!("--explain-version accepts 1 or 2"),
                });
            }
            "--policy-header" if command == "explain" => {
                if policy_header {
                    bail!("--policy-header may be specified only once");
                }
                policy_header = true;
            }
            "--single-instance" if command == "run" => {
                if single_instance.is_some() {
                    bail!("--single-instance may be specified only once");
                }
                single_instance = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--single-instance",
                    "a lock file path",
                )?));
            }
            "--what-if" if command == "run" => {
                if what_if {
                    bail!("--what-if may be specified only once");
                }
                what_if = true;
            }
            "--confirm" if command == "run" => {
                if confirm.is_some() {
                    bail!("--confirm may be specified only once");
                }
                confirm = Some(take_option_value(
                    arguments,
                    &mut index,
                    "--confirm",
                    "a plan fingerprint",
                )?);
            }
            "--allow" if command == "explain" => {
                #[cfg(feature = "agent-advisory")]
                {
                    let category =
                        take_option_value(arguments, &mut index, "--allow", "a category")?;
                    allow.push(flow::agent::gate::parse_category(&category)?.to_owned());
                }
                #[cfg(not(feature = "agent-advisory"))]
                bail!("explain --json effect gates require the agent-advisory feature; rebuild with --features agent-advisory");
            }
            "--format" if command == "check" => {
                let format = take_option_value(arguments, &mut index, "--format", "github|json")?;
                match &*format {
                    "github" => github = true,
                    "json" => {}
                    other => bail!("--format accepts github or json, not {other:?}"),
                }
                json = true;
            }
            "--input" => {
                let pair = take_option_value(arguments, &mut index, "--input", "NAME=VALUE")?;
                let (name, value) = pair
                    .split_once('=')
                    .context("--input requires NAME=VALUE")?;
                if inputs.insert(name.to_owned(), value.to_owned()).is_some() {
                    bail!("duplicate program input {name:?}");
                }
            }
            "--answers" => {
                let path = take_option_value(arguments, &mut index, "--answers", "FILE")?;
                if answers.replace(PathBuf::from(path)).is_some() {
                    bail!("--answers may be specified only once");
                }
            }
            "--help" | "-h" => {
                print_help()?;
                return Ok(None);
            }
            "--version" | "-V" => {
                writeln!(io::stdout(), "execute {}", env!("CARGO_PKG_VERSION"))?;
                return Ok(None);
            }
            "--manifest-dir" => {
                if manifest_dir.is_some() {
                    bail!("--manifest-dir may be specified only once");
                }
                if manifest_catalog.is_some() {
                    bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
                }
                manifest_dir = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--manifest-dir",
                    "a directory",
                )?));
            }
            "--manifest-catalog" => {
                if manifest_catalog.is_some() {
                    bail!("--manifest-catalog may be specified only once");
                }
                if manifest_dir.is_some() {
                    bail!("--manifest-dir and --manifest-catalog are mutually exclusive");
                }
                manifest_catalog = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--manifest-catalog",
                    "a packed manifest catalog file",
                )?));
            }
            "--execution-mode" => {
                if execution_mode.is_some() {
                    bail!("--execution-mode may be specified only once");
                }
                let value = take_option_value(
                    arguments,
                    &mut index,
                    "--execution-mode",
                    "trusted or untrusted",
                )?;
                execution_mode = Some(match value.as_str() {
                    "trusted" => flow::capabilities::ExecutionMode::Trusted,
                    "untrusted" => flow::capabilities::ExecutionMode::Untrusted,
                    _ => bail!("--execution-mode must be trusted or untrusted"),
                });
            }
            "--capability-policy"
                if matches!(command, "check" | "explain" | "inspect" | "run" | "resume") =>
            {
                if capability_policy.is_some() {
                    bail!("--capability-policy may be specified only once");
                }
                capability_policy = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--capability-policy",
                    "a policy file",
                )?));
            }
            "--data-flow-policy"
                if matches!(command, "check" | "explain" | "inspect" | "run" | "resume") =>
            {
                if data_flow_policy.is_some() {
                    bail!("--data-flow-policy may be specified only once");
                }
                data_flow_policy = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--data-flow-policy",
                    "a policy file",
                )?));
            }
            "--record" if command == "run" => {
                if record_dir.is_some() {
                    bail!("--record may be specified only once");
                }
                record_dir = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--record",
                    "a new or empty directory",
                )?));
            }
            "--cache-dir" if matches!(command, "run" | "resume") => {
                if cache_dir.is_some() {
                    bail!("--cache-dir may be specified only once");
                }
                cache_dir = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--cache-dir",
                    "a directory",
                )?));
            }
            "--checkpoint" if matches!(command, "run" | "resume") => {
                if checkpoint_dir.is_some() {
                    bail!("--checkpoint may be specified only once");
                }
                checkpoint_dir = Some(PathBuf::from(take_option_value(
                    arguments,
                    &mut index,
                    "--checkpoint",
                    "a directory",
                )?));
            }
            "--retry-uncertain" if command == "resume" => {
                if retry_uncertain {
                    bail!("--retry-uncertain may be specified only once");
                }
                retry_uncertain = true;
            }
            "--once" if command == "run" => {
                if once {
                    bail!("--once may be specified only once");
                }
                once = true;
            }
            "--headless" if command == "run" => {
                if headless {
                    bail!("--headless may be specified only once");
                }
                headless = true;
            }
            "--trace" if matches!(command, "run" | "resume") => {
                if trace {
                    bail!("--trace may be specified only once");
                }
                trace = true;
            }
            "--max-output-bytes" if matches!(command, "run" | "resume") => {
                if max_output_seen {
                    bail!("--max-output-bytes may be specified only once");
                }
                let value = take_option_value(
                    arguments,
                    &mut index,
                    "--max-output-bytes",
                    "a non-negative integer",
                )?;
                max_output_bytes = value
                    .parse::<usize>()
                    .map_err(|_| anyhow!("--max-output-bytes must be a non-negative integer"))?;
                max_output_seen = true;
            }
            "--max-loop-iterations" if matches!(command, "run" | "resume") => {
                if max_loop_seen {
                    bail!("--max-loop-iterations may be specified only once");
                }
                max_loop_iterations =
                    parse_positive_usize(arguments, &mut index, "--max-loop-iterations")?;
                max_loop_seen = true;
            }
            "--command-timeout-ms" if matches!(command, "run" | "resume") => {
                if command_timeout.is_some() {
                    bail!("--command-timeout-ms may be specified only once");
                }
                let value = parse_positive_usize(arguments, &mut index, "--command-timeout-ms")?;
                let duration = std::time::Duration::from_millis(u64::try_from(value)?);
                std::time::Instant::now()
                    .checked_add(duration)
                    .context("command timeout is too large")?;
                command_timeout = Some(duration);
            }
            "--drain-timeout-ms" if command == "run" => {
                if drain_timeout.is_some() {
                    bail!("--drain-timeout-ms may be specified only once");
                }
                let value = parse_positive_usize(arguments, &mut index, "--drain-timeout-ms")?;
                drain_timeout = Some(std::time::Duration::from_millis(u64::try_from(value)?));
            }
            "--total-timeout-ms" if matches!(command, "run" | "resume") => {
                if total_timeout.is_some() {
                    bail!("--total-timeout-ms may be specified only once");
                }
                let value = take_option_value(
                    arguments,
                    &mut index,
                    "--total-timeout-ms",
                    "a positive integer",
                )?;
                let millis = value
                    .parse::<u64>()
                    .map_err(|_| anyhow!("--total-timeout-ms must be a positive integer"))?;
                if millis == 0 {
                    bail!("--total-timeout-ms must be greater than zero");
                }
                let duration = std::time::Duration::from_millis(millis);
                std::time::Instant::now()
                    .checked_add(duration)
                    .context("total timeout is too large")?;
                total_timeout = Some(duration);
            }
            "--max-runs" if matches!(command, "run" | "resume") => {
                if max_runs.is_some() {
                    bail!("--max-runs may be specified only once");
                }
                max_runs = Some(parse_positive_usize(arguments, &mut index, "--max-runs")?);
            }
            "--missed-ticks" if matches!(command, "run" | "resume") => {
                if missed_ticks.is_some() {
                    bail!("--missed-ticks may be specified only once");
                }
                missed_ticks = Some(take_option_value(
                    arguments,
                    &mut index,
                    "--missed-ticks",
                    "skip, coalesce, or catch-up",
                )?);
            }
            "--max-catch-up" if matches!(command, "run" | "resume") => {
                if max_catch_up.is_some() {
                    bail!("--max-catch-up may be specified only once");
                }
                let value = parse_positive_usize(arguments, &mut index, "--max-catch-up")?;
                if value > 64 {
                    bail!("--max-catch-up must be between 1 and 64");
                }
                max_catch_up = Some(value);
            }
            "--calendar-dedup" if matches!(command, "run" | "resume") => {
                if calendar_dedup.is_some() {
                    bail!("--calendar-dedup may be specified only once");
                }
                calendar_dedup = Some(take_option_value(
                    arguments,
                    &mut index,
                    "--calendar-dedup",
                    "instant or local",
                )?);
            }
            "--max-record-bytes" if matches!(command, "run" | "resume") => {
                if max_record_bytes_seen {
                    bail!("--max-record-bytes may be specified only once");
                }
                max_record_bytes =
                    parse_positive_usize(arguments, &mut index, "--max-record-bytes")?;
                max_record_bytes_seen = true;
            }
            "--control" if command == "run" => {
                if control {
                    bail!("--control may be specified only once");
                }
                control = true;
            }
            // Taken before parsing: see daemon_forward.
            "--daemon" if command == "run" => {}
            "--break-at" if command == "run" => {
                let line = parse_positive_usize(arguments, &mut index, "--break-at")?;
                break_at.push(line);
            }
            value if value.starts_with('-') => bail!("unknown or unsupported option `{value}`"),
            _ => {
                if file.replace(workflow_path(argument)?).is_some() {
                    bail!("only one workflow file may be specified");
                }
            }
        }
        index += 1;
    }

    if diagnostic_explain.is_some() && (arguments.len() != 3 || file.is_some() || json) {
        bail!("check --explain CODE accepts only a diagnostic code");
    }
    let file = match (file, diagnostic_explain.is_some()) {
        (Some(file), _) => file,
        (None, true) => PathBuf::new(),
        (None, false) => return Err(anyhow!("missing workflow file\n{}", usage())),
    };
    if command == "resume" && checkpoint_dir.is_none() {
        bail!("resume requires --checkpoint DIR");
    }
    if !break_at.is_empty() && !control {
        bail!("--break-at requires --control");
    }
    if explain_version.is_some()
        && !(command == "explain" && json || command == "run" && (what_if || confirm.is_some()))
    {
        bail!("--explain-version requires explain --json, run --what-if, or run --confirm");
    }
    Ok(Some(Args {
        inputs,
        answers,
        program_arguments,
        program_arguments_literal,
        json,
        diagnostic_explain,
        explain_version,
        allow,
        what_if,
        confirm,
        policy_header,
        single_instance,
        github,
        command,
        file,
        manifest_dir,
        manifest_catalog,
        capability_policy,
        execution_mode,
        data_flow_policy,
        cache_dir,
        checkpoint_dir,
        retry_uncertain,
        once,
        trace,
        headless,
        max_output_bytes,
        max_loop_iterations,
        max_runs,
        missed_ticks,
        max_catch_up,
        calendar_dedup,
        command_timeout,
        drain_timeout,
        total_timeout,
        max_record_bytes,
        max_record_bytes_set: max_record_bytes_seen,
        record_dir,
        control,
        break_at,
    }))
}

/// Convert a local file URI without changing ordinary path interpretation.
/// Percent escapes decode to Unix filename bytes, not form-encoded text.
pub(crate) fn workflow_path(argument: &str) -> Result<PathBuf> {
    if !argument
        .get(..5)
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("file:"))
    {
        return Ok(PathBuf::from(argument));
    }
    let rest = &argument[5..];
    let path = if let Some(authority_path) = rest.strip_prefix("//") {
        let (authority, path) = authority_path
            .split_once('/')
            .context("file URI requires an absolute path")?;
        if !authority.is_empty() && !authority.eq_ignore_ascii_case("localhost") {
            bail!(
                "file URI must refer to this computer; remote host {authority:?} is not supported"
            );
        }
        format!("/{path}")
    } else {
        rest.to_owned()
    };
    if !path.starts_with('/') {
        bail!("file URI requires an absolute path");
    }
    if path.contains(['?', '#']) {
        bail!(
            "file URI queries and fragments are not supported; encode filename characters as %3F and %23"
        );
    }
    let mut decoded = Vec::with_capacity(path.len());
    let bytes = path.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let value = if bytes[index] == b'%' {
            let high = bytes.get(index + 1).and_then(|v| (*v as char).to_digit(16));
            let low = bytes.get(index + 2).and_then(|v| (*v as char).to_digit(16));
            match (high, low) {
                (Some(high), Some(low)) => {
                    index += 3;
                    (high * 16 + low) as u8
                }
                _ => bail!("invalid percent escape in file URI"),
            }
        } else {
            let value = bytes[index];
            index += 1;
            value
        };
        if value == 0 {
            bail!("file URI cannot contain a NUL filename byte");
        }
        decoded.push(value);
    }
    #[cfg(unix)]
    return Ok(PathBuf::from(std::ffi::OsString::from_vec(decoded)));
    #[cfg(windows)]
    {
        let path = String::from_utf8(decoded).context("Windows file URI must contain UTF-8")?;
        let path = path
            .strip_prefix('/')
            .filter(|p| p.as_bytes().get(1) == Some(&b':'))
            .unwrap_or(&path);
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            bail!("Windows file URI requires an absolute drive path");
        }
        Ok(path)
    }
}

fn parse_positive_usize(arguments: &[String], index: &mut usize, option: &str) -> Result<usize> {
    let value = take_option_value(arguments, index, option, "a positive integer")?;
    let parsed = value
        .parse::<usize>()
        .map_err(|_| anyhow!("{option} must be a positive integer"))?;
    if parsed == 0 {
        bail!("{option} must be greater than zero");
    }
    Ok(parsed)
}

pub(crate) fn take_option_value(
    arguments: &[String],
    index: &mut usize,
    option: &str,
    expected: &str,
) -> Result<String> {
    *index += 1;
    let value = arguments
        .get(*index)
        .ok_or_else(|| anyhow!("{option} requires {expected}"))?;
    if value.starts_with('-') {
        bail!("{option} requires {expected}, got `{value}`");
    }
    Ok(value.clone())
}

#[cfg(test)]
mod tests {
    use super::parse_args;

    #[test]
    fn parser_accepts_borrowed_arguments_and_preserves_literal_boundary() {
        let arguments = vec![
            "run".to_owned(),
            "workflow.program".to_owned(),
            "--".to_owned(),
            "--help".to_owned(),
        ];
        let parsed = parse_args(&arguments)
            .expect("arguments parse")
            .expect("run arguments");
        assert!(parsed.program_arguments_literal);
        assert_eq!(parsed.program_arguments, ["--help"]);
    }

    #[test]
    fn check_explain_parses_without_a_workflow_file() {
        let arguments = vec![
            "check".to_owned(),
            "--explain".to_owned(),
            "check.scope".to_owned(),
        ];
        let parsed = parse_args(&arguments)
            .expect("arguments parse")
            .expect("check explain arguments");
        assert_eq!(parsed.diagnostic_explain.as_deref(), Some("check.scope"));
        assert!(parsed.file.as_os_str().is_empty());
    }

    #[test]
    fn check_explain_rejects_a_workflow_file_and_json_mode() {
        for tail in [
            vec!["check.scope", "workflow.program"],
            vec!["check.scope", "--json"],
        ] {
            let arguments = std::iter::once("check".to_owned())
                .chain(std::iter::once("--explain".to_owned()))
                .chain(tail.into_iter().map(str::to_owned))
                .collect::<Vec<_>>();
            assert!(parse_args(&arguments).is_err());
        }
    }
}
