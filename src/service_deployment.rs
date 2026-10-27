//! Reviewable service definitions for foreground `.program` services.
//!
//! Rendering is pure with respect to the host service manager: this module
//! never writes a unit file, registers a job, or starts/stops a service.

use crate::ast::{InputSource, ListenSource, Trigger};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Supported per-user service definition formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceFormat {
    Systemd,
    SystemdTimer,
    Launchd,
}

impl ServiceFormat {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "systemd" => Ok(Self::Systemd),
            "systemd-timer" => Ok(Self::SystemdTimer),
            "launchd" => Ok(Self::Launchd),
            _ => bail!("service format must be `systemd`, `systemd-timer`, or `launchd`"),
        }
    }
}

/// Inputs used to render a reviewable user service definition.
///
/// `inputs` and `program_arguments` become separate literal argv entries. No
/// shell is involved in either generated format.
pub struct WorkflowServiceRequest<'a> {
    pub format: ServiceFormat,
    pub workflow_path: &'a Path,
    pub executable: &'a Path,
    pub working_directory: &'a Path,
    pub manifest_directory: Option<&'a Path>,
    pub capability_policy: Option<&'a Path>,
    pub data_flow_policy: Option<&'a Path>,
    /// Optional host child environment allowlist; an empty string clears it.
    pub child_environment_allowlist: Option<&'a str>,
    /// Private JSON answers file forwarded to `run` by path.
    pub answers_file: Option<&'a Path>,
    pub label: Option<&'a str>,
    pub inputs: &'a BTreeMap<String, String>,
    pub program_arguments: &'a [String],
    /// systemd-timer only: `Persistent=true` runs activations missed while the host was off.
    pub persistent: bool,
}

/// Render a service definition without installing or activating it.
pub fn render_for_workflow(request: &WorkflowServiceRequest<'_>) -> Result<String> {
    render_for_workflow_with_launchd_home(request, None)
}

/// Render a service that explicitly uses one packed manifest catalog.
///
/// The generated command stores the canonical catalog file path, so it does
/// not depend on the caller's working directory when the service starts.
pub fn render_for_workflow_with_manifest_catalog(
    request: &WorkflowServiceRequest<'_>,
    manifest_catalog: &Path,
) -> Result<String> {
    render_for_workflow_with_launchd_home_and_catalog(request, None, Some(manifest_catalog))
}

fn render_for_workflow_with_launchd_home(
    request: &WorkflowServiceRequest<'_>,
    launchd_home: Option<&Path>,
) -> Result<String> {
    render_for_workflow_with_launchd_home_and_catalog(request, launchd_home, None)
}

fn render_for_workflow_with_launchd_home_and_catalog(
    request: &WorkflowServiceRequest<'_>,
    launchd_home: Option<&Path>,
    manifest_catalog: Option<&Path>,
) -> Result<String> {
    if manifest_catalog.is_some() && request.manifest_directory.is_some() {
        bail!("manifest directory and packed manifest catalog are mutually exclusive");
    }
    let workflow = canonical_file(request.workflow_path, "workflow")?;
    let executable = canonical_file(request.executable, "execute binary")?;
    let working_directory = canonical_directory(request.working_directory, "working directory")?;
    let manifest_directory = request
        .manifest_directory
        .map(|path| canonical_directory(path, "manifest directory"))
        .transpose()?;
    let manifest_catalog = manifest_catalog
        .map(|path| canonical_file(path, "manifest catalog"))
        .transpose()?;
    let capability_policy = request
        .capability_policy
        .map(|path| canonical_file(path, "capability policy"))
        .transpose()?;
    let data_flow_policy = request
        .data_flow_policy
        .map(|path| canonical_file(path, "data-flow policy"))
        .transpose()?;
    let answers_file = request
        .answers_file
        .map(validate_private_answers_file)
        .transpose()?;
    let label = request
        .label
        .map(str::to_owned)
        .unwrap_or_else(|| default_label(&workflow));
    validate_label(&label)?;

    let source = fs::read_to_string(&workflow)
        .with_context(|| format!("reading workflow {}", workflow.display()))?;
    let program = crate::parser::parse(&source)
        .with_context(|| format!("parsing workflow {}", workflow.display()))?;
    let trigger = program
        .trigger
        .as_ref()
        .ok_or_else(|| anyhow!("service generation requires a triggered workflow"))?;
    if matches!(
        trigger,
        Trigger::Listen {
            source: ListenSource::Stdin,
            ..
        }
    ) {
        bail!(
            "cannot generate a managed service for a stdin listener; the service manager does not provide a durable event producer"
        );
    }

    if answers_file.is_none() {
        validate_explicit_environment_inputs(&program.inputs, request.inputs)?;
    }
    let supplied = crate::inputs::parse_cli_arguments(
        &program.inputs,
        request.inputs,
        request.program_arguments,
    )?;
    for declaration in &program.inputs {
        if answers_file.is_some() {
            continue;
        }
        if (crate::ast::is_sensitive(declaration)
            || matches!(declaration.source, Some(InputSource::Environment(_))))
            && request.inputs.contains_key(&declaration.name)
        {
            bail!(
                "service generation refuses to embed sensitive or environment input {:?} in the unit command line; secure --answers file support is required",
                declaration.name
            );
        }
    }
    validate_service_inputs(&program.inputs, &supplied, request.program_arguments.len())?;

    if request.persistent && request.format != ServiceFormat::SystemdTimer {
        bail!("persistent catch-up applies only to the systemd-timer format");
    }
    // A timer activation runs the handler once. `run --once` executes the body
    // immediately instead of waiting for the next calendar tick.
    let calendar = match request.format {
        ServiceFormat::SystemdTimer => match trigger {
            Trigger::Cron { expression, .. } => calendar_specs(expression)?,
            _ => bail!(
                "systemd-timer requires a workflow scheduled with `on schedule \"<cron expression>\"`"
            ),
        },
        ServiceFormat::Systemd | ServiceFormat::Launchd => Vec::new(),
    };
    let mut argv = vec![utf8_path(&executable, "execute binary")?, "run".into()];
    if request.format == ServiceFormat::SystemdTimer {
        argv.push("--once".into());
    }
    if let Some(path) = manifest_directory {
        argv.extend([
            "--manifest-dir".into(),
            utf8_path(&path, "manifest directory")?,
        ]);
    }
    if let Some(path) = manifest_catalog {
        argv.extend([
            "--manifest-catalog".into(),
            utf8_path(&path, "manifest catalog")?,
        ]);
    }
    if let Some(path) = capability_policy {
        argv.extend([
            "--capability-policy".into(),
            utf8_path(&path, "capability policy")?,
        ]);
    }
    if let Some(path) = data_flow_policy {
        argv.extend([
            "--data-flow-policy".into(),
            utf8_path(&path, "data-flow policy")?,
        ]);
    }
    if let Some(value) = request.child_environment_allowlist {
        let names = crate::capabilities::parse_child_environment_allowlist(value)?;
        if names.is_empty() {
            argv.push("--clear-env".into());
        } else {
            argv.push("--env-allow".into());
            argv.push(names.join(","));
        }
    }
    if let Some(path) = answers_file {
        argv.extend(["--answers".into(), utf8_path(&path, "answers file")?]);
    } else {
        for (name, value) in request.inputs {
            argv.push("--input".into());
            argv.push(format!("{name}={value}"));
        }
    }
    argv.push(utf8_path(&workflow, "workflow")?);
    if !request.program_arguments.is_empty() {
        argv.push("--".into());
        argv.extend(request.program_arguments.iter().cloned());
    }
    validate_arguments(&argv)?;

    match request.format {
        ServiceFormat::Systemd => render_systemd(&label, &working_directory, &argv),
        ServiceFormat::SystemdTimer => render_systemd_timer(
            &label,
            &working_directory,
            &argv,
            &calendar,
            request.persistent,
        ),
        ServiceFormat::Launchd => render_launchd(&label, &working_directory, &argv, launchd_home),
    }
}

/// Resolve and verify that an unattended answers file is private to the caller.
pub fn validate_private_answers_file(path: &Path) -> Result<PathBuf> {
    let path = canonical_file(path, "answers file")?;
    let metadata = fs::metadata(&path)
        .with_context(|| format!("reading answers file metadata {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // SAFETY: geteuid reads the effective user id and has no pointer arguments or side effects.
        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o777 != 0o600
        {
            bail!(
                "answers file {} must be owned by the current user and have mode 0600",
                path.display()
            );
        }
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        let file = crate::security::open_private_file(&path)
            .with_context(|| format!("opening answers file {}", path.display()))?;
        crate::security::validate_private_windows_acl(&file, &path)?;
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = metadata;
        bail!(
            "cannot verify that answers file {} is private on this platform",
            path.display()
        );
    }
    #[cfg(unix)]
    {
        Ok(path)
    }
    #[cfg(windows)]
    {
        Ok(path)
    }
}

fn validate_explicit_environment_inputs(
    declarations: &[crate::ast::InputDeclaration],
    explicit: &BTreeMap<String, String>,
) -> Result<()> {
    for declaration in declarations {
        if matches!(declaration.source, Some(InputSource::Environment(_)))
            && !explicit.contains_key(&declaration.name)
        {
            bail!(
                "workflow input {:?} is sourced from the caller environment; supply it explicitly with `--input {}=VALUE` so the generated service does not depend on an implicit login environment",
                declaration.name,
                declaration.name
            );
        }
    }
    Ok(())
}

fn validate_service_inputs(
    declarations: &[crate::ast::InputDeclaration],
    supplied: &BTreeMap<String, crate::ast::Value>,
    positional_count: usize,
) -> Result<()> {
    for declaration in declarations {
        if supplied.contains_key(&declaration.name) || declaration.default.is_some() {
            continue;
        }
        match declaration.source.as_ref() {
            Some(InputSource::Environment(_)) => bail!(
                "workflow input {:?} is sourced from the caller environment; supply it explicitly with `--input {}=VALUE` so the generated service does not depend on an implicit login environment",
                declaration.name,
                declaration.name
            ),
            Some(InputSource::Arguments) => {}
            // The loader always supplies a location default, so this arm is unreachable
            // for loaded programs; a service reads its own program file's location.
            Some(InputSource::ProgramDirectory | InputSource::ProgramPath) => {}
            // An absent flag is false, so a service needs no value for it.
            Some(InputSource::Flag { .. }) => {}
            Some(InputSource::Option { .. }) => bail!(
                "workflow option {:?} has no value for the generated service; supply `--input {}=VALUE` or give the option a default",
                declaration.name,
                declaration.name
            ),
            Some(InputSource::Prompt { .. }) => bail!(
                "workflow input {:?} is asked on a terminal; supply it with `--input {}=VALUE` or a default so the generated service needs no terminal",
                declaration.name,
                declaration.name
            ),
            Some(InputSource::Argument(position)) if *position <= positional_count => {}
            Some(InputSource::Argument(position)) => bail!(
                "workflow needs positional input {:?} at argument {position}; supply it after `--`",
                declaration.name
            ),
            None => bail!(
                "workflow needs input {:?}; supply `--input {}=VALUE`",
                declaration.name,
                declaration.name
            ),
        }
    }
    Ok(())
}

fn validate_arguments(arguments: &[String]) -> Result<()> {
    for argument in arguments {
        if argument
            .bytes()
            .any(|byte| byte == 0 || byte.is_ascii_control())
        {
            bail!("service argv cannot contain NUL or control characters");
        }
    }
    Ok(())
}

fn canonical_existing(path: &Path, label: &str) -> Result<PathBuf> {
    let path =
        fs::canonicalize(path).with_context(|| format!("resolving {label} {}", path.display()))?;
    Ok(path)
}

fn canonical_file(path: &Path, label: &str) -> Result<PathBuf> {
    let path = canonical_existing(path, label)?;
    if !path.is_file() {
        bail!("{label} must be a file: {}", path.display());
    }
    Ok(path)
}

fn canonical_directory(path: &Path, label: &str) -> Result<PathBuf> {
    let path = canonical_existing(path, label)?;
    if !path.is_dir() {
        bail!("{label} must be a directory: {}", path.display());
    }
    Ok(path)
}

fn utf8_path(path: &Path, label: &str) -> Result<String> {
    path.to_str().map(str::to_owned).ok_or_else(|| {
        anyhow!("{label} path cannot be represented as UTF-8 for a service definition")
    })
}

fn default_label(workflow: &Path) -> String {
    let stem = workflow
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("workflow");
    let mut slug = String::with_capacity(stem.len());
    let mut separator = false;
    for character in stem.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            if separator && !slug.is_empty() {
                slug.push('-');
            }
            slug.push(character);
            separator = false;
        } else {
            separator = true;
        }
    }
    if slug.is_empty() {
        slug.push_str("workflow");
    }
    format!("com.execute.{slug}")
}

fn validate_label(label: &str) -> Result<()> {
    let valid_chars = label
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'));
    if label.len() > 255
        || !valid_chars
        || !label.contains('.')
        || label.split('.').any(str::is_empty)
        || label.bytes().any(|byte| byte.is_ascii_control())
    {
        bail!(
            "service label must be a dotted identifier using only ASCII letters, digits, dots, and hyphens"
        );
    }
    Ok(())
}

fn render_systemd(label: &str, working_directory: &Path, argv: &[String]) -> Result<String> {
    let description = unit_value(&format!("Execute workflow {label}"))?;
    let working_directory = unit_value(&utf8_path(working_directory, "working directory")?)?;
    let command = argv
        .iter()
        .map(|argument| exec_argument(argument))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    Ok(format!(
        "# Generated for review. Save as {label}.service and inspect before enabling.\n\
[Unit]\n\
Description={description}\n\
\n\
[Service]\n\
Type=simple\n\
WorkingDirectory={working_directory}\n\
ExecStart={command}\n\
Restart=on-failure\n\
RestartSec=5s\n\
KillMode=control-group\n\
StandardOutput=journal\n\
StandardError=journal\n\
\n\
[Install]\n\
WantedBy=default.target\n"
    ))
}

/// One `OnCalendar=` value. `None` selects every value of that component.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CalendarSpec {
    weekdays: Option<Vec<u32>>,
    days: Option<Vec<u32>>,
    months: Option<Vec<u32>>,
    hours: Option<Vec<u32>>,
    minutes: Option<Vec<u32>>,
}

const WEEKDAY_NAMES: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

impl CalendarSpec {
    fn render(&self) -> String {
        let mut text = String::new();
        if let Some(weekdays) = &self.weekdays {
            let names = weekdays
                .iter()
                .map(|day| WEEKDAY_NAMES[*day as usize])
                .collect::<Vec<_>>();
            text.push_str(&names.join(","));
            text.push(' ');
        }
        text.push_str(&format!(
            "*-{}-{} {}:{}:00",
            calendar_component(&self.months),
            calendar_component(&self.days),
            calendar_component(&self.hours),
            calendar_component(&self.minutes),
        ));
        text
    }
}

fn calendar_component(values: &Option<Vec<u32>>) -> String {
    match values {
        None => "*".into(),
        Some(values) => values
            .iter()
            .map(|value| format!("{value:02}"))
            .collect::<Vec<_>>()
            .join(","),
    }
}

/// Expand a validated five-field cron expression into systemd calendar specs.
///
/// Cron ANDs day-of-month and weekday when either field is a bare or stepped
/// `*`, and ORs them otherwise. Two specs express the OR case, because systemd
/// fires on the union of its `OnCalendar=` values.
fn calendar_specs(expression: &str) -> Result<Vec<CalendarSpec>> {
    crate::schedule::CronSchedule::parse(expression)
        .context("service generation needs a valid cron expression")?;
    let fields = expression.split_ascii_whitespace().collect::<Vec<_>>();
    let [minute, hour, day_of_month, month, day_of_week] = fields.as_slice() else {
        bail!("cron expression must contain exactly five fields");
    };
    let (minutes, _) = cron_values(minute, 0, 59, false)?;
    let (hours, _) = cron_values(hour, 0, 23, false)?;
    let (days, days_wildcard) = cron_values(day_of_month, 1, 31, false)?;
    let (months, _) = cron_values(month, 1, 12, false)?;
    let (weekdays, weekdays_wildcard) = cron_values(day_of_week, 0, 7, true)?;

    let minutes = keep_unless_full(minutes, 0, 59);
    let hours = keep_unless_full(hours, 0, 23);
    let months = keep_unless_full(months, 1, 12);
    let days = keep_unless_full(days, 1, 31);
    let weekdays = keep_unless_full(weekdays, 0, 6);
    let spec = |weekdays: Option<Vec<u32>>, days: Option<Vec<u32>>| CalendarSpec {
        weekdays,
        days,
        months: months.clone(),
        hours: hours.clone(),
        minutes: minutes.clone(),
    };
    if days_wildcard || weekdays_wildcard {
        Ok(vec![spec(weekdays, days)])
    } else {
        Ok(vec![spec(None, days), spec(weekdays, None)])
    }
}

fn keep_unless_full(bits: u64, min: u32, max: u32) -> Option<Vec<u32>> {
    let values = (min..=max)
        .filter(|value| bits & (1u64 << value) != 0)
        .collect::<Vec<_>>();
    (values.len() != (max - min + 1) as usize).then_some(values)
}

/// Mirrors the field grammar of `schedule::CronSchedule`, returning the
/// selected values as a bit set plus whether the field begins with `*`.
/// Weekday 7 is folded into Sunday (bit 0).
fn cron_values(raw: &str, min: u32, max: u32, sunday_alias: bool) -> Result<(u64, bool)> {
    let wildcard = raw.starts_with('*');
    let mut values = 0u64;
    for item in raw.split(',') {
        let mut parts = item.split('/');
        let base = parts.next().unwrap_or_default();
        let step = match parts.next() {
            Some(step) => step.parse::<u32>().context("cron step")?,
            None => 1,
        };
        let (start, end) = if base == "*" {
            (min, max)
        } else if let Some((from, to)) = base.split_once('-') {
            (from.parse::<u32>()?, to.parse::<u32>()?)
        } else {
            let value = base.parse::<u32>()?;
            (value, if item.contains('/') { max } else { value })
        };
        let mut value = start;
        while value <= end {
            let selected = if sunday_alias && value == 7 { 0 } else { value };
            values |= 1u64 << selected;
            value = match value.checked_add(step) {
                Some(next) => next,
                None => break,
            };
        }
    }
    Ok((values, wildcard))
}

fn render_systemd_timer(
    label: &str,
    working_directory: &Path,
    argv: &[String],
    calendar: &[CalendarSpec],
    persistent: bool,
) -> Result<String> {
    let description = unit_value(&format!("Execute workflow {label}"))?;
    let working_directory = unit_value(&utf8_path(working_directory, "working directory")?)?;
    let command = argv
        .iter()
        .map(|argument| exec_argument(argument))
        .collect::<Result<Vec<_>>>()?
        .join(" ");
    let mut timer = format!(
        "# ---- timer unit: save as {label}.timer. Generated for review; nothing is installed.\n\
[Unit]\n\
Description={description}\n\
\n\
[Timer]\n"
    );
    for spec in calendar {
        timer.push_str(&format!("OnCalendar={}\n", spec.render()));
    }
    timer.push_str(&format!(
        "Persistent={}\n\
Unit={label}.service\n\
\n\
[Install]\n\
WantedBy=timers.target\n",
        if persistent { "true" } else { "false" }
    ));
    Ok(format!(
        "{timer}\n# ---- service unit: save as {label}.service\n\
[Unit]\n\
Description={description}\n\
\n\
[Service]\n\
Type=oneshot\n\
WorkingDirectory={working_directory}\n\
ExecStart={command}\n\
StandardOutput=journal\n\
StandardError=journal\n"
    ))
}

fn render_launchd(
    label: &str,
    working_directory: &Path,
    argv: &[String],
    home_override: Option<&Path>,
) -> Result<String> {
    let home = home_override
        .map(Path::to_path_buf)
        .or_else(|| std::env::var_os("HOME").map(PathBuf::from))
        .ok_or_else(|| anyhow!("HOME is required to choose launchd log paths"))?;
    let log_directory = home.join("Library").join("Logs");
    let stdout_path = log_directory.join(format!("{label}.stdout.log"));
    let stderr_path = log_directory.join(format!("{label}.stderr.log"));
    let mut args_xml = String::new();
    for argument in argv {
        args_xml.push_str("\n        <string>");
        args_xml.push_str(&xml_text(argument));
        args_xml.push_str("</string>");
    }
    let working_directory = xml_text(&utf8_path(working_directory, "working directory")?);
    let stdout_path = xml_text(&utf8_path(&stdout_path, "launchd stdout log")?);
    let stderr_path = xml_text(&utf8_path(&stderr_path, "launchd stderr log")?);
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n\
<!-- Generated for review only. Save as {label}.plist; this does not load the job. -->\n\
<plist version=\"1.0\">\n\
<dict>\n\
    <key>Label</key>\n\
    <string>{label}</string>\n\
    <key>ProgramArguments</key>\n\
    <array>{args_xml}\n\
    </array>\n\
    <key>WorkingDirectory</key>\n\
    <string>{working_directory}</string>\n\
    <key>RunAtLoad</key>\n\
    <true/>\n\
    <key>KeepAlive</key>\n\
    <dict>\n\
        <key>SuccessfulExit</key>\n\
        <false/>\n\
    </dict>\n\
    <key>StandardOutPath</key>\n\
    <string>{stdout_path}</string>\n\
    <key>StandardErrorPath</key>\n\
    <string>{stderr_path}</string>\n\
</dict>\n\
</plist>\n"
    ))
}

fn unit_value(value: &str) -> Result<String> {
    validate_arguments(&[value.to_owned()])?;
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '%' => escaped.push_str("%%"),
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(character),
        }
    }
    Ok(format!("\"{escaped}\""))
}

fn exec_argument(argument: &str) -> Result<String> {
    validate_arguments(&[argument.to_owned()])?;
    let mut escaped = String::with_capacity(argument.len());
    for character in argument.chars() {
        match character {
            '%' => escaped.push_str("%%"),
            '$' => escaped.push_str("$$"),
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            _ => escaped.push(character),
        }
    }
    Ok(format!("\"{escaped}\""))
}

fn xml_text(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn systemd_arguments_escape_interpolation_and_keep_spaces_literal() {
        assert_eq!(
            exec_argument("a b$HOME%f\\\"c").unwrap(),
            "\"a b$$HOME%%f\\\\\\\"c\""
        );
        assert_eq!(exec_argument("").unwrap(), "\"\"");
    }

    #[test]
    fn launchd_xml_escapes_property_text() {
        assert_eq!(xml_text("a&<b>"), "a&amp;&lt;b&gt;");
    }

    #[cfg(unix)]
    #[test]
    fn private_answers_file_requires_current_owner_and_mode_0600() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempdir().unwrap();
        let path = directory.path().join("answers.json");
        fs::write(&path, "{}\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(validate_private_answers_file(&path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(
            validate_private_answers_file(&path).unwrap(),
            fs::canonicalize(path).unwrap()
        );
    }

    #[test]
    fn generated_service_is_validated_review_only_and_preserves_argv() {
        let directory = tempdir().unwrap();
        let workflow = directory.path().join("daily task.program");
        fs::write(
            &workflow,
            "accept first as text from argument 1\naccept second as text from argument 2\nevery 5 minutes\n  then encode \"body\" as json\n",
        )
        .unwrap();
        let working_directory = directory.path().join("work");
        fs::create_dir(&working_directory).unwrap();
        let executable = std::env::current_exe().unwrap();
        let inputs = BTreeMap::new();
        let arguments = vec!["semi;colon".to_owned(), "two words".to_owned()];
        let mut request = WorkflowServiceRequest {
            format: ServiceFormat::Systemd,
            workflow_path: &workflow,
            executable: &executable,
            working_directory: &working_directory,
            manifest_directory: None,
            capability_policy: None,
            data_flow_policy: None,
            child_environment_allowlist: Some("PATH,LANG"),
            answers_file: None,
            label: Some("com.example.daily-task"),
            inputs: &inputs,
            program_arguments: &arguments,
            persistent: false,
        };
        let rendered = render_for_workflow(&request).unwrap();
        assert!(rendered.contains("KillMode=control-group"));
        assert!(rendered.contains("Restart=on-failure"));
        assert!(rendered.contains("\"two words\""));
        assert!(rendered.contains("Generated for review"));
        assert!(rendered.contains("\"--env-allow\" \"PATH,LANG\""));

        request.child_environment_allowlist = Some("");
        let cleared = render_for_workflow(&request).unwrap();
        assert!(cleared.contains("\"--clear-env\""));
    }

    #[test]
    fn generated_service_keeps_explicit_catalog_path_in_argv() {
        let directory = tempdir().unwrap();
        let workflow = directory.path().join("scheduled.program");
        fs::write(
            &workflow,
            "every 5 minutes\n  then encode \"body\" as json\n",
        )
        .unwrap();
        let catalog = directory.path().join("manifests.catalog");
        fs::write(&catalog, "packed catalog fixture").unwrap();
        let executable = std::env::current_exe().unwrap();
        let inputs = BTreeMap::new();
        let arguments = Vec::new();
        let request = WorkflowServiceRequest {
            format: ServiceFormat::Systemd,
            workflow_path: &workflow,
            executable: &executable,
            working_directory: directory.path(),
            manifest_directory: None,
            capability_policy: None,
            data_flow_policy: None,
            child_environment_allowlist: None,
            answers_file: None,
            label: Some("com.example.catalog-service"),
            inputs: &inputs,
            program_arguments: &arguments,
            persistent: false,
        };

        let rendered = render_for_workflow_with_manifest_catalog(&request, &catalog).unwrap();
        let canonical_catalog = fs::canonicalize(catalog).unwrap();
        let canonical_catalog = canonical_catalog.to_string_lossy();
        let catalog_argument = exec_argument(&canonical_catalog).unwrap();
        assert!(rendered.contains(&format!("\"--manifest-catalog\" {catalog_argument}")));
        assert!(!rendered.contains("--manifest-dir"));
    }

    #[test]
    fn launchd_restarts_failed_jobs_but_not_successful_completion() {
        let directory = tempdir().unwrap();
        let workflow = directory.path().join("scheduled.program");
        fs::write(
            &workflow,
            "every 5 minutes\n  then encode \"body\" as json\n",
        )
        .unwrap();
        let executable = std::env::current_exe().unwrap();
        let inputs = BTreeMap::new();
        let arguments = Vec::new();
        let rendered = render_for_workflow_with_launchd_home(
            &WorkflowServiceRequest {
                format: ServiceFormat::Launchd,
                workflow_path: &workflow,
                executable: &executable,
                working_directory: directory.path(),
                manifest_directory: None,
                capability_policy: None,
                data_flow_policy: None,
                child_environment_allowlist: None,
                answers_file: None,
                label: Some("com.example.scheduled"),
                inputs: &inputs,
                program_arguments: &arguments,
                persistent: false,
            },
            Some(directory.path()),
        )
        .unwrap();
        assert!(rendered.contains("<key>RunAtLoad</key>\n<true/>"));
        assert!(
            rendered.contains("<key>KeepAlive</key>\n<dict>\n<key>SuccessfulExit</key>\n<false/>")
        );
        let expected_log = directory
            .path()
            .join("Library")
            .join("Logs")
            .join("com.example.scheduled.stdout.log");
        let expected_log = xml_text(expected_log.to_string_lossy().as_ref());
        assert!(rendered.contains(&expected_log));
    }

    #[test]
    fn rejects_stdin_listeners_and_required_values_without_static_sources() {
        let directory = tempdir().unwrap();
        let workflow = directory.path().join("stdin.program");
        fs::write(
            &workflow,
            "listen to stdin\nwhenever a line arrives as line\n  then cat\n",
        )
        .unwrap();
        let working_directory = directory.path();
        let executable = std::env::current_exe().unwrap();
        let inputs = BTreeMap::new();
        let arguments = Vec::new();
        let mut request = WorkflowServiceRequest {
            format: ServiceFormat::Systemd,
            workflow_path: &workflow,
            executable: &executable,
            working_directory,
            manifest_directory: None,
            capability_policy: None,
            data_flow_policy: None,
            child_environment_allowlist: None,
            answers_file: None,
            label: None,
            inputs: &inputs,
            program_arguments: &arguments,
            persistent: false,
        };
        assert!(render_for_workflow(&request)
            .unwrap_err()
            .to_string()
            .contains("stdin listener"));

        let workflow = directory.path().join("environment.program");
        fs::write(
            &workflow,
            "accept token as text from environment \"EXECUTE_SERVICE_TOKEN\" with default \"fallback\"\nevery 1 minute\n  then encode \"body\" as json\n",
        )
        .unwrap();
        request.workflow_path = &workflow;
        assert!(render_for_workflow(&request)
            .unwrap_err()
            .to_string()
            .contains("supply it explicitly"));

        let workflow = directory.path().join("required.program");
        fs::write(
            &workflow,
            "accept token as text\nevery 1 minute\n  then encode \"body\" as json\n",
        )
        .unwrap();
        request.workflow_path = &workflow;
        assert!(render_for_workflow(&request)
            .unwrap_err()
            .to_string()
            .contains("needs input"));

        let workflow = directory.path().join("secret.program");
        fs::write(
            &workflow,
            "accept token as text from secret prompt \"Token\"\nevery 1 minute\n  then encode \"body\" as json\n",
        )
        .unwrap();
        let sensitive_inputs = BTreeMap::from([("token".to_owned(), "CANARY".to_owned())]);
        request.workflow_path = &workflow;
        request.inputs = &sensitive_inputs;
        let error = render_for_workflow(&request).unwrap_err();
        assert!(
            format!("{error:#}").contains("refuses to embed sensitive or environment input"),
            "{error:#}"
        );
        assert!(!format!("{error:#}").contains("CANARY"));
    }

    #[test]
    fn calendar_expansion_matches_common_cron_shapes() {
        let rendered = |expression: &str| {
            calendar_specs(expression)
                .unwrap()
                .iter()
                .map(CalendarSpec::render)
                .collect::<Vec<_>>()
        };
        assert_eq!(
            rendered("30 9 * * 1-5"),
            ["Mon,Tue,Wed,Thu,Fri *-*-* 09:30:00"]
        );
        assert_eq!(rendered("*/15 * * * *"), ["*-*-* *:00,15,30,45:00"]);
        assert_eq!(rendered("0 0 1,15 6 *"), ["*-06-01,15 00:00:00"]);
        assert_eq!(
            rendered("0 12 1 * 1"),
            ["*-*-01 12:00:00", "Mon *-*-* 12:00:00"]
        );
        assert_eq!(rendered("0 0 * * 7"), ["Sun *-*-* 00:00:00"]);
        assert_eq!(rendered("0 9 * * *"), ["*-*-* 09:00:00"]);
    }

    fn spec_selects(spec: &CalendarSpec, when: &chrono::DateTime<chrono::Local>) -> bool {
        use chrono::{Datelike, Timelike};
        let selected = |values: &Option<Vec<u32>>, value: u32| {
            values.as_ref().is_none_or(|values| values.contains(&value))
        };
        selected(&spec.weekdays, when.weekday().num_days_from_sunday())
            && selected(&spec.days, when.day())
            && selected(&spec.months, when.month())
            && selected(&spec.hours, when.hour())
            && selected(&spec.minutes, when.minute())
    }

    #[test]
    fn calendar_specs_select_exactly_the_minutes_cron_selects() {
        use chrono::{Local, TimeZone};
        let expressions = [
            "30 9 * * 1-5",
            "*/15 * * * *",
            "0 12 1 * 1",
            "0 0 1,15 6 *",
            "5 4 */2 * 0,6",
            "10 8 1-7 * 1",
            "0 0 31 * *",
            "59 23 * 2 7",
            "0,30 9-17/2 1-31/2 1,3,12 0,7",
        ];
        let start = Local
            .with_ymd_and_hms(2026, 1, 1, 0, 0, 0)
            .single()
            .unwrap()
            .timestamp();
        for expression in expressions {
            let cron = crate::schedule::CronSchedule::parse(expression).unwrap();
            let specs = calendar_specs(expression).unwrap();
            let mut checked = 0usize;
            for minute in 0..(366_i64 * 24 * 60) {
                let Some(when) = Local.timestamp_opt(start + minute * 60, 0).single() else {
                    continue;
                };
                let from_specs = specs.iter().any(|spec| spec_selects(spec, &when));
                assert_eq!(from_specs, cron.matches(&when), "{expression} at {when}");
                checked += 1;
            }
            assert!(checked > 500_000, "{expression} checked {checked} minutes");
        }
    }

    #[test]
    fn systemd_timer_renders_timer_and_oneshot_service_for_cron_workflows() {
        let directory = tempdir().unwrap();
        let workflow = directory.path().join("nightly.program");
        fs::write(
            &workflow,
            "on schedule \"30 9 * * 1-5\"\n  then encode \"body\" as json\n",
        )
        .unwrap();
        let working_directory = directory.path().join("work");
        fs::create_dir(&working_directory).unwrap();
        let executable = std::env::current_exe().unwrap();
        let inputs = BTreeMap::new();
        let arguments: Vec<String> = Vec::new();
        let mut request = WorkflowServiceRequest {
            format: ServiceFormat::SystemdTimer,
            workflow_path: &workflow,
            executable: &executable,
            working_directory: &working_directory,
            manifest_directory: None,
            capability_policy: None,
            data_flow_policy: None,
            child_environment_allowlist: None,
            answers_file: None,
            label: Some("com.example.nightly"),
            inputs: &inputs,
            program_arguments: &arguments,
            persistent: false,
        };
        let rendered = render_for_workflow(&request).unwrap();
        assert!(rendered.contains("OnCalendar=Mon,Tue,Wed,Thu,Fri *-*-* 09:30:00\n"));
        assert!(rendered.contains("Persistent=false\nUnit=com.example.nightly.service\n"));
        assert!(rendered.contains("WantedBy=timers.target"));
        assert!(rendered.contains("Type=oneshot"));
        assert!(!rendered.contains("Restart="));
        assert!(rendered.contains("\"run\" \"--once\""));
        assert!(rendered.contains("nothing is installed"));

        request.persistent = true;
        let persistent = render_for_workflow(&request).unwrap();
        assert!(persistent.contains("Persistent=true"));
        request.persistent = false;

        // Catch-up is a timer property: the plain systemd format refuses it.
        request.format = ServiceFormat::Systemd;
        request.persistent = true;
        let error = render_for_workflow(&request).unwrap_err();
        assert!(format!("{error:#}").contains("persistent"), "{error:#}");
    }

    #[test]
    fn systemd_timer_rejects_workflows_without_a_cron_schedule() {
        let directory = tempdir().unwrap();
        let workflow = directory.path().join("interval.program");
        fs::write(
            &workflow,
            "every 5 minutes\n  then encode \"body\" as json\n",
        )
        .unwrap();
        let working_directory = directory.path().join("work");
        fs::create_dir(&working_directory).unwrap();
        let executable = std::env::current_exe().unwrap();
        let inputs = BTreeMap::new();
        let arguments: Vec<String> = Vec::new();
        let error = render_for_workflow(&WorkflowServiceRequest {
            format: ServiceFormat::SystemdTimer,
            workflow_path: &workflow,
            executable: &executable,
            working_directory: &working_directory,
            manifest_directory: None,
            capability_policy: None,
            data_flow_policy: None,
            child_environment_allowlist: None,
            answers_file: None,
            label: Some("com.example.interval"),
            inputs: &inputs,
            program_arguments: &arguments,
            persistent: false,
        })
        .unwrap_err();
        assert!(
            format!("{error:#}").contains("systemd-timer requires"),
            "{error:#}"
        );
    }
}
