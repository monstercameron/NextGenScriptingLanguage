//! Reviewable service definitions for foreground `.program` services.
//!
//! Rendering is pure with respect to the host service manager: this module
//! never writes a unit file, registers a job, or starts/stops a service.

use crate::ast::{InputSource, ListenSource, Trigger};
use anyhow::{anyhow, bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

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
    pub execution_mode: Option<crate::capabilities::ExecutionMode>,
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

/// Render a systemd unit with an explicit hardening mode. In `Derived` mode the
/// program's effect plan is computed with `registry`; any undeclared step makes
/// the unit get the baseline, with that step named in a comment.
pub fn render_for_workflow_with_hardening(
    request: &WorkflowServiceRequest<'_>,
    registry: &crate::manifest::Registry,
    hardening: HardeningMode,
) -> Result<String> {
    render_for_workflow_with_launchd_home_and_catalog(
        request,
        None,
        None,
        Some(registry),
        hardening,
    )
}

/// Render a service that explicitly uses one packed manifest catalog.
///
/// The generated command stores the canonical catalog file path, so it does
/// not depend on the caller's working directory when the service starts.
pub fn render_for_workflow_with_manifest_catalog(
    request: &WorkflowServiceRequest<'_>,
    manifest_catalog: &Path,
) -> Result<String> {
    render_for_workflow_with_launchd_home_and_catalog(
        request,
        None,
        Some(manifest_catalog),
        None,
        HardeningMode::Derived,
    )
}

fn render_for_workflow_with_launchd_home(
    request: &WorkflowServiceRequest<'_>,
    launchd_home: Option<&Path>,
) -> Result<String> {
    render_for_workflow_with_launchd_home_and_catalog(
        request,
        launchd_home,
        None,
        None,
        HardeningMode::Derived,
    )
}

fn render_for_workflow_with_launchd_home_and_catalog(
    request: &WorkflowServiceRequest<'_>,
    launchd_home: Option<&Path>,
    manifest_catalog: Option<&Path>,
    registry: Option<&crate::manifest::Registry>,
    hardening_mode: HardeningMode,
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
    // Computed before the policy path is moved into argv below.
    let hardening = match request.format {
        ServiceFormat::Systemd | ServiceFormat::SystemdTimer => match hardening_mode {
            HardeningMode::Off => String::new(),
            HardeningMode::Baseline => format!(
                "# baseline: always-safe directives requested for this unit\n{}",
                baseline_directives()
            ),
            HardeningMode::Derived => {
                let policy = capability_policy
                    .as_deref()
                    .map(read_policy_for_review)
                    .transpose()?;
                let undeclared = match registry {
                    Some(registry) if policy.is_some() => undeclared_step_reasons(
                        &program,
                        registry,
                        &workflow.display().to_string(),
                    )?,
                    _ => Vec::new(),
                };
                derived_hardening(policy.as_ref(), &working_directory, &undeclared)?
            }
        },
        ServiceFormat::Launchd => String::new(),
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
    if let Some(mode) = request.execution_mode {
        // Fail closed like the run path: an untrusted service without a policy
        // would refuse at startup, so the generator refuses instead of
        // emitting a unit that cannot start.
        if mode == crate::capabilities::ExecutionMode::Untrusted
            && request.capability_policy.is_none()
        {
            bail!("untrusted execution requires a capability policy");
        }
        argv.extend(["--execution-mode".into(), mode.as_str().into()]);
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
        ServiceFormat::Systemd => render_systemd(&label, &working_directory, &argv, &hardening),
        ServiceFormat::SystemdTimer => render_systemd_timer(
            &label,
            &working_directory,
            &argv,
            &calendar,
            request.persistent,
            &hardening,
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

/// Parse a capability policy to derive review text from it. This does not check
/// the owner-only privacy rule that `CapabilityPolicy::load` applies: the
/// generator grants nothing and installs nothing, and the service that runs
/// this policy still loads it with that check.
fn read_policy_for_review(path: &Path) -> Result<crate::capabilities::CapabilityPolicy> {
    let source = fs::read_to_string(path)
        .with_context(|| format!("reading capability policy {}", path.display()))?;
    crate::capabilities::CapabilityPolicy::from_json(&source)
        .with_context(|| format!("loading capability policy {}", path.display()))
}

/// Which sandbox directives a systemd unit receives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardeningMode {
    /// Directives the capability policy states. A program with undeclared
    /// effects gets the baseline instead, with the steps that forced it named.
    Derived,
    /// The always-safe baseline, whether or not a policy is supplied.
    Baseline,
    /// No sandbox directives (`--hardening none`).
    Off,
}

impl HardeningMode {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "derived" => Ok(Self::Derived),
            "baseline" => Ok(Self::Baseline),
            "none" => Ok(Self::Off),
            _ => bail!("hardening must be `derived`, `baseline`, or `none`"),
        }
    }
}

/// Directives used when the program's effects cannot all be stated. Each one
/// is safe for any program, so none of them claims more than the plan supports.
const BASELINE_DIRECTIVES: &[&str] = &[
    "PrivateTmp=yes",
    "ProtectKernelTunables=yes",
    "ProtectKernelModules=yes",
    "ProtectControlGroups=yes",
    "LockPersonality=yes",
];

fn baseline_directives() -> String {
    BASELINE_DIRECTIVES
        .iter()
        .map(|directive| format!("{directive}\n"))
        .collect()
}

/// One line per undeclared step of the program's effect plan, naming the step.
/// The plan is computed from the same registry the run path would use.
fn undeclared_step_reasons(
    program: &crate::ast::Program,
    registry: &crate::manifest::Registry,
    workflow: &str,
) -> Result<Vec<String>> {
    let plan = crate::agent::explain::explain_json(program, registry, workflow)?;
    let mut reasons = Vec::new();
    for row in plan["steps"].as_array().into_iter().flatten() {
        if row["undeclared"] != true {
            continue;
        }
        let line = row["line"]
            .as_u64()
            .map_or_else(|| "?".to_owned(), |line| line.to_string());
        let what = row["binary"]
            .as_str()
            .or_else(|| row["kind"].as_str())
            .unwrap_or("step");
        reasons.push(format!("line {line}: {what} has no declared effects"));
    }
    Ok(reasons)
}

/// Sandbox directives that the capability policy itself states, each preceded
/// by a comment naming the policy fact that justifies it.
///
/// `deny_privileged` gives `NoNewPrivileges=yes`; no applet network grant gives
/// `PrivateNetwork=yes`; the filesystem write grants give `ProtectSystem=strict`
/// and one `ReadWritePaths=` per grant. Without a policy nothing is derived, and
/// a relative write grant is reported rather than derived, because it resolves
/// against a working directory that the workflow may change.
///
/// `undeclared` lists steps whose effects are not declared. When there are any,
/// the result is the baseline only, and each step is named in a comment.
fn derived_hardening(
    policy: Option<&crate::capabilities::CapabilityPolicy>,
    working_directory: &Path,
    undeclared: &[String],
) -> Result<String> {
    let Some(policy) = policy else {
        return Ok(String::new());
    };
    if !undeclared.is_empty() {
        let mut lines = String::from(
            "# baseline: the program has steps whose effects are not declared, so no more is claimed\n",
        );
        for reason in undeclared {
            // Keep each reason on one comment line, so it cannot inject a directive.
            let text: String = reason
                .chars()
                .map(|character| {
                    if character.is_control() {
                        ' '
                    } else {
                        character
                    }
                })
                .collect();
            lines.push_str(&format!("# baseline forced by: {text}\n"));
        }
        lines.push_str(&baseline_directives());
        return Ok(lines);
    }
    let mut lines = String::new();
    if policy.deny_privileged {
        lines.push_str("# derived: capability policy sets deny_privileged\n");
        lines.push_str("NoNewPrivileges=yes\n");
    }
    if !policy.applets.values().any(|grant| grant.network) {
        lines.push_str("# derived: no applet grants network access\n");
        lines.push_str("PrivateNetwork=yes\n");
    }
    let mut writes: Vec<&Path> = policy
        .applets
        .values()
        .flat_map(|grant| grant.filesystem.write.iter().map(PathBuf::as_path))
        .collect();
    if let Some(host) = &policy.host {
        writes.extend(host.filesystem.write.iter().map(PathBuf::as_path));
    }
    if writes.iter().all(|path| path.is_absolute()) {
        lines.push_str("# derived: writes are limited to the filesystem write grants\n");
        lines.push_str("ProtectSystem=strict\n");
        let mut roots = BTreeSet::new();
        for path in writes {
            let resolved = crate::capabilities::canonicalize_for_grant(path, working_directory)?;
            roots.insert(utf8_path(&resolved, "filesystem write grant")?);
        }
        for root in roots {
            lines.push_str("# derived: filesystem write grant\n");
            lines.push_str(&format!("ReadWritePaths={}\n", unit_value(&root)?));
        }
    } else {
        lines.push_str(
            "# not derived: a filesystem write grant is relative to the working directory\n",
        );
    }
    Ok(lines)
}

fn render_systemd(
    label: &str,
    working_directory: &Path,
    argv: &[String],
    hardening: &str,
) -> Result<String> {
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
{hardening}\
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
    hardening: &str,
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
StandardError=journal\n\
{hardening}"
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

/// First line of every file the install-side generators write. `is_generated`
/// keys on it, so uninstall can refuse to remove a file Execute did not create
/// and install can refuse to overwrite a foreign one.
pub const GENERATED_MARKER: &str = "# Generated by Execute. Do not edit by hand.";

/// Longest unit file name accepted, suffix included.
const MAX_UNIT_FILE_NAME: usize = 255;

/// True only when the first line of `contents` is exactly [`GENERATED_MARKER`].
pub fn is_generated(contents: &str) -> bool {
    contents.lines().next() == Some(GENERATED_MARKER)
}

/// One unit file: its name (for example `nightly-backup.timer`) and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitFile {
    pub file_name: String,
    pub contents: String,
}

/// A trigger unit (`.timer` or `.path`) and the oneshot `.service` it starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnitPair {
    pub trigger: UnitFile,
    pub service: UnitFile,
}

/// What the service of a pair runs: `execute run <program>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunUnitSpec {
    /// Unit name without a suffix, such as `nightly-backup`.
    pub name: String,
    pub description: String,
    /// Absolute path of the `execute` binary.
    pub executable: String,
    /// Absolute path of the `.program` file.
    pub program: String,
}

/// When a timer activates its service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimerTrigger {
    /// `OnCalendar=`. The expression is only checked for newlines, control
    /// characters, NUL, `%`, `\` and `"`; `systemd-analyze calendar` is the
    /// real validator.
    Calendar(String),
    /// `OnBootSec=` and `OnUnitActiveSec=`, rendered in whole seconds or nanoseconds.
    Monotonic {
        on_boot_sec: Duration,
        on_unit_active_sec: Duration,
    },
}

/// What a path pair watches; the service runs when the condition holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathCondition {
    /// `PathExists=`
    Exists(String),
    /// `PathChanged=`
    Changed(String),
    /// `PathModified=`
    Modified(String),
}

/// Render a `.timer` and the `.service` it starts. Pure: writes nothing.
pub fn render_timer_pair(run: &RunUnitSpec, trigger: &TimerTrigger) -> Result<UnitPair> {
    validate_unit_name(&run.name)?;
    let description = description_value(&run.description)?;
    let trigger_lines = match trigger {
        TimerTrigger::Calendar(expression) => {
            format!("OnCalendar={}\n", calendar_value(expression)?)
        }
        TimerTrigger::Monotonic {
            on_boot_sec,
            on_unit_active_sec,
        } => format!(
            "OnBootSec={}\nOnUnitActiveSec={}\n",
            duration_value(*on_boot_sec, "OnBootSec")?,
            duration_value(*on_unit_active_sec, "OnUnitActiveSec")?,
        ),
    };
    let name = &run.name;
    let timer = format!(
        "{GENERATED_MARKER}\n\
[Unit]\n\
Description={description}\n\
\n\
[Timer]\n\
{trigger_lines}\
Unit={name}.service\n\
\n\
[Install]\n\
WantedBy=timers.target\n"
    );
    Ok(UnitPair {
        trigger: UnitFile {
            file_name: format!("{name}.timer"),
            contents: timer,
        },
        service: service_file(run)?,
    })
}

/// Render a `.path` unit watching one absolute path, and the `.service` it
/// starts. Pure: writes nothing.
pub fn render_path_pair(run: &RunUnitSpec, condition: &PathCondition) -> Result<UnitPair> {
    validate_unit_name(&run.name)?;
    let description = description_value(&run.description)?;
    let (key, watched) = match condition {
        PathCondition::Exists(path) => ("PathExists", path),
        PathCondition::Changed(path) => ("PathChanged", path),
        PathCondition::Modified(path) => ("PathModified", path),
    };
    let watched = unit_path_value(watched, "watched path")?;
    let name = &run.name;
    let path_unit = format!(
        "{GENERATED_MARKER}\n\
[Unit]\n\
Description={description}\n\
\n\
[Path]\n\
{key}={watched}\n\
Unit={name}.service\n\
\n\
[Install]\n\
WantedBy=paths.target\n"
    );
    Ok(UnitPair {
        trigger: UnitFile {
            file_name: format!("{name}.path"),
            contents: path_unit,
        },
        service: service_file(run)?,
    })
}

/// Check a unit name stem against systemd's unit-name characters: ASCII
/// letters, digits, `:`, `-`, `_` and `.`. Backslash is excluded because
/// systemd reads it as an escape. Names must not start with `.` or carry a
/// unit type suffix, because Execute adds the suffix.
pub fn validate_unit_name(name: &str) -> Result<()> {
    let valid_chars = name
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_' | b'.'));
    if name.is_empty()
        || !valid_chars
        || name.starts_with('.')
        || name.len() + ".service".len() > MAX_UNIT_FILE_NAME
    {
        bail!(
            "unit name must use only ASCII letters, digits, ':', '-', '_' and '.', must not start with '.', and must fit in {MAX_UNIT_FILE_NAME} bytes"
        );
    }
    if [".service", ".timer", ".path"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
    {
        bail!("unit name must not include a unit type suffix; Execute adds it");
    }
    Ok(())
}

/// The `.service` of a pair: a oneshot that runs `execute run <program>`.
fn service_file(run: &RunUnitSpec) -> Result<UnitFile> {
    let description = description_value(&run.description)?;
    let command = run_command(&run.executable, &run.program)?;
    let name = &run.name;
    Ok(UnitFile {
        file_name: format!("{name}.service"),
        contents: format!(
            "{GENERATED_MARKER}\n\
[Unit]\n\
Description={description}\n\
\n\
[Service]\n\
Type=oneshot\n\
ExecStart={command}\n\
StandardOutput=journal\n\
StandardError=journal\n"
        ),
    })
}

/// `ExecStart=` line: both paths must be absolute POSIX paths. Each argument
/// goes through `exec_argument`, which quotes it and doubles `%` and `$`.
fn run_command(executable: &str, program: &str) -> Result<String> {
    if !executable.starts_with('/') {
        bail!("execute binary path must be absolute");
    }
    if !program.starts_with('/') {
        bail!("program path must be absolute");
    }
    Ok(format!(
        "{} {} {}",
        exec_argument(executable)?,
        exec_argument("run")?,
        exec_argument(program)?
    ))
}

/// Shared checks for a single-value setting (`Description=`, `OnCalendar=`,
/// `PathChanged=`). systemd does not unquote these, so a value is written
/// as-is. Newlines, control characters and NUL are refused, and so are `\`
/// (a trailing one continues the line) and `"`.
fn check_single_value(value: &str, label: &str) -> Result<()> {
    if value.is_empty() {
        bail!("{label} must not be empty");
    }
    if value
        .bytes()
        .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        bail!("{label} cannot contain newlines, tabs, control characters or NUL");
    }
    if value.contains(['\\', '"']) {
        bail!("{label} cannot contain a backslash or double quote");
    }
    Ok(())
}

fn description_value(description: &str) -> Result<String> {
    check_single_value(description, "description")?;
    Ok(description.replace('%', "%%"))
}

fn calendar_value(expression: &str) -> Result<String> {
    check_single_value(expression, "calendar expression")?;
    if expression.contains('%') {
        bail!("calendar expression cannot contain '%'");
    }
    Ok(expression.to_owned())
}

fn unit_path_value(path: &str, label: &str) -> Result<String> {
    if !path.starts_with('/') {
        bail!("{label} must be an absolute path");
    }
    check_single_value(path, label)?;
    Ok(path.replace('%', "%%"))
}

fn duration_value(duration: Duration, label: &str) -> Result<String> {
    if duration.is_zero() {
        bail!("{label} must be greater than zero");
    }
    if duration.subsec_nanos() == 0 {
        Ok(format!("{}s", duration.as_secs()))
    } else {
        Ok(format!("{}ns", duration.as_nanos()))
    }
}

/// What a unit path holds right now, as far as install and uninstall care.
enum OnDisk {
    Absent,
    /// A regular file whose text begins with [`GENERATED_MARKER`].
    Generated(String),
    /// Anything else; the reason completes "<name> <reason>; refusing to ...".
    Foreign(&'static str),
}

/// Look at `path` without following a symbolic link.
fn inspect(path: &Path) -> Result<OnDisk> {
    inspect_with(path, is_generated)
}

/// [`inspect`] with the marker test supplied: unit files use [`is_generated`],
/// hook shims use [`is_generated_hook`].
fn inspect_with(path: &Path, is_marked: fn(&str) -> bool) -> Result<OnDisk> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(OnDisk::Absent);
        }
        Err(error) => {
            return Err(error).with_context(|| format!("inspecting {}", path.display()));
        }
    };
    if metadata.file_type().is_symlink() {
        return Ok(OnDisk::Foreign("is a symbolic link"));
    }
    if !metadata.is_file() {
        return Ok(OnDisk::Foreign("is not a regular file"));
    }
    let bytes = fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    match String::from_utf8(bytes) {
        Ok(text) if is_marked(&text) => Ok(OnDisk::Generated(text)),
        _ => Ok(OnDisk::Foreign("was not generated by Execute")),
    }
}

/// Refuse anything but `<stem>.service|.timer|.path` with a valid stem, so a
/// name cannot leave the unit directory or start with an option dash.
fn check_unit_file_name(file_name: &str) -> Result<()> {
    if file_name.contains(['/', '\\', '\0']) {
        bail!("unit file name {file_name:?} must not contain path separators");
    }
    let stem = UNIT_SUFFIXES
        .iter()
        .find_map(|suffix| file_name.strip_suffix(suffix))
        .ok_or_else(|| {
            anyhow!("unit file name {file_name:?} must end in .service, .timer or .path")
        })?;
    if stem.starts_with('-') {
        bail!("unit file name {file_name:?} must not start with '-'");
    }
    validate_unit_name(stem)
}

const UNIT_SUFFIXES: [&str; 3] = [".service", ".timer", ".path"];

/// What `plan_install` decided for one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstallAction {
    /// Absent now; will be written.
    Create,
    /// Present and byte-identical; left alone.
    Unchanged,
    /// Present, generated by Execute, different contents; rewritten.
    Replace,
    /// Present and not generated by Execute, or not a regular file, or a link.
    /// Nothing is written; the message says why.
    Refuse(String),
}

/// One file of an install plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub file_name: String,
    pub contents: String,
    pub action: InstallAction,
}

/// What installing unit files would do. Pure data: a caller can print it
/// before calling [`apply_install`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallPlan {
    pub unit_dir: PathBuf,
    pub files: Vec<PlannedFile>,
}

impl InstallPlan {
    /// True when at least one file is refused; [`apply_install`] then writes nothing.
    pub fn has_refusal(&self) -> bool {
        self.files
            .iter()
            .any(|file| matches!(file.action, InstallAction::Refuse(_)))
    }
}

/// Result of [`apply_install`]. A write failure is an `Ok` value here, not an
/// error, so callers must match on it.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub enum InstallOutcome {
    Installed {
        /// Files this call wrote.
        changed: Vec<String>,
        /// Files already identical on disk.
        unchanged: Vec<String>,
        /// Commands to run next, as argument vectors. Not run by this module.
        /// Always `systemctl --user daemon-reload` first, then `enable --now`
        /// for each `.timer` or `.path` file.
        commands: Vec<Vec<String>>,
    },
    /// A write failed. Files this call had written were put back to their
    /// previous contents, or removed when they did not exist before.
    RolledBack {
        failed_file: String,
        error: String,
        /// Files restored to the contents they had before this call.
        restored: Vec<String>,
        /// Files this call created, now removed.
        removed: Vec<String>,
        /// Files that could not be put back. They hold the new contents and
        /// need hand repair. Empty when the rollback worked.
        rollback_errors: Vec<String>,
    },
}

impl InstallOutcome {
    pub fn is_installed(&self) -> bool {
        matches!(self, Self::Installed { .. })
    }
}

/// Decide, for each generated unit file, whether to create, leave, replace or
/// refuse it. Writes nothing. `files` must come from a render function, so
/// every content begins with [`GENERATED_MARKER`].
pub fn plan_install(unit_dir: &Path, files: &[UnitFile]) -> Result<InstallPlan> {
    let mut seen = BTreeSet::new();
    let mut planned = Vec::with_capacity(files.len());
    for file in files {
        check_unit_file_name(&file.file_name)?;
        if !seen.insert(file.file_name.as_str()) {
            bail!("unit file {} is listed twice", file.file_name);
        }
        if !is_generated(&file.contents) {
            bail!(
                "unit file {} does not begin with the Execute marker",
                file.file_name
            );
        }
        let action = match inspect(&unit_dir.join(&file.file_name))? {
            OnDisk::Absent => InstallAction::Create,
            OnDisk::Generated(existing) if existing == file.contents => InstallAction::Unchanged,
            OnDisk::Generated(_) => InstallAction::Replace,
            OnDisk::Foreign(reason) => {
                InstallAction::Refuse(refusal(&file.file_name, reason, "overwrite"))
            }
        };
        planned.push(PlannedFile {
            file_name: file.file_name.clone(),
            contents: file.contents.clone(),
            action,
        });
    }
    Ok(InstallPlan {
        unit_dir: unit_dir.to_path_buf(),
        files: planned,
    })
}

fn refusal(file_name: &str, reason: &str, verb: &str) -> String {
    format!("{file_name} {reason}; refusing to {verb} it")
}

/// Write the plan. Refuses to run if the plan holds a refusal. The disk is
/// inspected again before the first write, so a file that turned foreign after
/// planning also stops the install with nothing written.
///
/// Each file goes to a temporary sibling, is flushed, then replaces the target
/// through the crate's atomic publish helper. If a write fails, files this call
/// already wrote are put back (see [`InstallOutcome::RolledBack`]).
pub fn apply_install(unit_dir: &Path, plan: &InstallPlan) -> Result<InstallOutcome> {
    apply_install_with(unit_dir, plan, &mut write_unit_file)
}

/// [`apply_install`] with the per-file writer injected, so tests can fail the
/// second write.
/// Writes one unit file: the real atomic writer, or a test double.
type UnitWriter<'a> = dyn FnMut(&Path, &[u8]) -> std::result::Result<(), UnitWriteError> + 'a;

fn apply_install_with(
    unit_dir: &Path,
    plan: &InstallPlan,
    write: &mut UnitWriter<'_>,
) -> Result<InstallOutcome> {
    if plan.unit_dir != unit_dir {
        bail!(
            "the plan was made for {} but install was asked to write to {}",
            plan.unit_dir.display(),
            unit_dir.display()
        );
    }
    if plan.has_refusal() {
        let reasons: Vec<&str> = plan
            .files
            .iter()
            .filter_map(|file| match &file.action {
                InstallAction::Refuse(reason) => Some(reason.as_str()),
                _ => None,
            })
            .collect();
        bail!("install refused, nothing written: {}", reasons.join("; "));
    }

    // Inspect again: the plan is data and the disk may have moved on since.
    let mut unchanged = Vec::new();
    let mut pending: Vec<(&PlannedFile, PathBuf, Option<String>)> = Vec::new();
    let mut refusals = Vec::new();
    for file in &plan.files {
        check_unit_file_name(&file.file_name)?;
        if !is_generated(&file.contents) {
            bail!(
                "unit file {} does not begin with the Execute marker",
                file.file_name
            );
        }
        let path = unit_dir.join(&file.file_name);
        match inspect(&path)? {
            OnDisk::Absent => pending.push((file, path, None)),
            OnDisk::Generated(existing) if existing == file.contents => {
                unchanged.push(file.file_name.clone());
            }
            OnDisk::Generated(existing) => pending.push((file, path, Some(existing))),
            OnDisk::Foreign(reason) => {
                refusals.push(refusal(&file.file_name, reason, "overwrite"));
            }
        }
    }
    if !refusals.is_empty() {
        bail!("install refused, nothing written: {}", refusals.join("; "));
    }

    if !pending.is_empty() {
        fs::create_dir_all(unit_dir).with_context(|| format!("creating {}", unit_dir.display()))?;
    }
    // Indices into `pending` for every file this call has put on disk.
    let mut written: Vec<usize> = Vec::new();
    for (index, (file, path, _)) in pending.iter().enumerate() {
        match write(path, file.contents.as_bytes()) {
            Ok(()) => written.push(index),
            Err(error) => {
                if error.published {
                    // The rename happened; the target holds the new bytes.
                    written.push(index);
                }
                let mut restored = Vec::new();
                let mut removed = Vec::new();
                let mut rollback_errors = Vec::new();
                for done in written.iter().rev() {
                    let (done_file, done_path, previous) = &pending[*done];
                    let name = done_file.file_name.clone();
                    match previous {
                        Some(text) => match write_unit_file(done_path, text.as_bytes()) {
                            Ok(()) => restored.push(name),
                            Err(restore) => {
                                rollback_errors.push(format!("{name}: {}", restore.error))
                            }
                        },
                        None => match fs::remove_file(done_path) {
                            Ok(()) => removed.push(name),
                            Err(remove) => rollback_errors.push(format!("{name}: {remove}")),
                        },
                    }
                }
                return Ok(InstallOutcome::RolledBack {
                    failed_file: file.file_name.clone(),
                    error: error.error.to_string(),
                    restored,
                    removed,
                    rollback_errors,
                });
            }
        }
    }

    let changed = pending
        .iter()
        .map(|(file, _, _)| file.file_name.clone())
        .collect();
    let mut commands = vec![argv(&["systemctl", "--user", "daemon-reload"])];
    for file in &plan.files {
        if file.file_name.ends_with(".timer") || file.file_name.ends_with(".path") {
            commands.push(argv(&[
                "systemctl",
                "--user",
                "enable",
                "--now",
                file.file_name.as_str(),
            ]));
        }
    }
    Ok(InstallOutcome::Installed {
        changed,
        unchanged,
        commands,
    })
}

fn argv(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

/// A failed atomic write. `published` is true when the rename succeeded and
/// only the durability flush failed, so the target already holds the new bytes.
#[derive(Debug)]
struct UnitWriteError {
    published: bool,
    error: anyhow::Error,
}

static UNIT_WRITE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Write `contents` to `path` atomically: a new temporary sibling, mode 0644 on
/// Unix, flushed, then renamed over the target through the crate's publish
/// helper. The temporary is removed on any failure that did not publish.
fn write_unit_file(path: &Path, contents: &[u8]) -> std::result::Result<(), UnitWriteError> {
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or_else(|| UnitWriteError {
            published: false,
            error: anyhow!("unit path {} must name a file", path.display()),
        })?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let sequence = UNIT_WRITE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let temporary = parent.join(format!(
        ".{file_name}.{}.{sequence}.tmp",
        std::process::id()
    ));
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temporary)
    {
        Ok(file) => file,
        Err(error) => {
            return Err(UnitWriteError {
                published: false,
                error: anyhow::Error::new(error)
                    .context(format!("creating {}", temporary.display())),
            });
        }
    };
    let staged = (|| -> Result<()> {
        file.write_all(contents)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o644))?;
        }
        file.sync_all()?;
        Ok(())
    })();
    drop(file);
    if let Err(error) = staged {
        let _ = fs::remove_file(&temporary);
        return Err(UnitWriteError {
            published: false,
            error: error.context(format!("writing {}", temporary.display())),
        });
    }
    match crate::platform::replace_file(&temporary, path) {
        Ok(()) => Ok(()),
        Err(error) => {
            let published = error.published;
            if !published {
                let _ = fs::remove_file(&temporary);
            }
            Err(UnitWriteError {
                published,
                error: anyhow::Error::new(error).context(format!("publishing {}", path.display())),
            })
        }
    }
}

/// What `plan_uninstall` decided for one name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UninstallAction {
    /// Generated by Execute; will be removed.
    Remove,
    /// Not there; reported as already absent.
    Absent,
    /// Foreign, a link, or not a regular file. Nothing is removed.
    Refuse(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallEntry {
    pub file_name: String,
    pub action: UninstallAction,
}

/// What uninstalling named unit files would do. Pure data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallPlan {
    pub unit_dir: PathBuf,
    pub entries: Vec<UninstallEntry>,
}

impl UninstallPlan {
    /// True when at least one name is refused; [`apply_uninstall`] then removes nothing.
    pub fn has_refusal(&self) -> bool {
        self.entries
            .iter()
            .any(|entry| matches!(entry.action, UninstallAction::Refuse(_)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallOutcome {
    pub removed: Vec<String>,
    pub absent: Vec<String>,
}

/// Decide what removing each named file would do. Only files that carry the
/// Execute marker and are regular files are removable; links and directories
/// are refused. Writes nothing.
pub fn plan_uninstall(unit_dir: &Path, names: &[&str]) -> Result<UninstallPlan> {
    let mut seen = BTreeSet::new();
    let mut entries = Vec::with_capacity(names.len());
    for name in names {
        check_unit_file_name(name)?;
        if !seen.insert(*name) {
            bail!("unit file {name} is named twice");
        }
        let action = match inspect(&unit_dir.join(name))? {
            OnDisk::Absent => UninstallAction::Absent,
            OnDisk::Generated(_) => UninstallAction::Remove,
            OnDisk::Foreign(reason) => UninstallAction::Refuse(refusal(name, reason, "remove")),
        };
        entries.push(UninstallEntry {
            file_name: (*name).to_owned(),
            action,
        });
    }
    Ok(UninstallPlan {
        unit_dir: unit_dir.to_path_buf(),
        entries,
    })
}

/// Remove the planned files. Refuses to run if the plan holds a refusal, and
/// inspects every file again before removing any, so a file that turned
/// foreign after planning stops the uninstall with nothing removed.
pub fn apply_uninstall(unit_dir: &Path, plan: &UninstallPlan) -> Result<UninstallOutcome> {
    if plan.unit_dir != unit_dir {
        bail!(
            "the plan was made for {} but uninstall was asked to act on {}",
            plan.unit_dir.display(),
            unit_dir.display()
        );
    }
    if plan.has_refusal() {
        let reasons: Vec<&str> = plan
            .entries
            .iter()
            .filter_map(|entry| match &entry.action {
                UninstallAction::Refuse(reason) => Some(reason.as_str()),
                _ => None,
            })
            .collect();
        bail!("uninstall refused, nothing removed: {}", reasons.join("; "));
    }

    let mut absent = Vec::new();
    let mut doomed = Vec::new();
    let mut refusals = Vec::new();
    for entry in &plan.entries {
        check_unit_file_name(&entry.file_name)?;
        match inspect(&unit_dir.join(&entry.file_name))? {
            OnDisk::Absent => absent.push(entry.file_name.clone()),
            OnDisk::Generated(_) => doomed.push(entry.file_name.clone()),
            OnDisk::Foreign(reason) => {
                refusals.push(refusal(&entry.file_name, reason, "remove"));
            }
        }
    }
    if !refusals.is_empty() {
        bail!(
            "uninstall refused, nothing removed: {}",
            refusals.join("; ")
        );
    }

    let mut removed = Vec::new();
    for name in doomed {
        if let Err(error) = fs::remove_file(unit_dir.join(&name)) {
            bail!(
                "removing {name} failed after removing [{}]: {error}",
                removed.join(", ")
            );
        }
        removed.push(name);
    }
    Ok(UninstallOutcome { removed, absent })
}

/// The systemd user unit directory from environment values the caller read.
/// `XDG_CONFIG_HOME` wins when set; otherwise `HOME/.config`. Then
/// `systemd/user` is appended. An empty or relative value is refused, and so
/// is a missing one with no fallback.
pub fn user_unit_directory(xdg_config_home: Option<&str>, home: Option<&str>) -> Result<PathBuf> {
    let base = match xdg_config_home {
        Some(value) => absolute_base("XDG_CONFIG_HOME", value)?,
        None => {
            let home = home.ok_or_else(|| {
                anyhow!("neither XDG_CONFIG_HOME nor HOME is set; cannot find the systemd user unit directory")
            })?;
            absolute_base("HOME", home)?.join(".config")
        }
    };
    Ok(base.join("systemd").join("user"))
}

fn absolute_base(label: &str, value: &str) -> Result<PathBuf> {
    if value.is_empty() {
        bail!("{label} is set but empty");
    }
    if !value.starts_with('/') {
        bail!("{label} must be an absolute path");
    }
    Ok(PathBuf::from(value))
}

/// First line of an Omarchy hook shim. `omarchy-hook` executes the file, so the
/// shebang must come first and the marker follows on line two.
pub const HOOK_SHEBANG: &str = "#!/bin/sh";

/// True only when the first two lines are [`HOOK_SHEBANG`] and [`GENERATED_MARKER`].
fn is_generated_hook(contents: &str) -> bool {
    let mut lines = contents.lines();
    lines.next() == Some(HOOK_SHEBANG) && lines.next() == Some(GENERATED_MARKER)
}

/// The Omarchy `theme-set.d` hook directory under the Omarchy config root.
pub fn omarchy_theme_set_directory(omarchy_config: &Path) -> PathBuf {
    omarchy_config.join("hooks").join("theme-set.d")
}

/// A hook shim name: ASCII letters, digits, `-` and `_` only. No dot, so the
/// name cannot carry a suffix that a run-parts style runner would skip.
fn check_hook_file_name(file_name: &str) -> Result<()> {
    let valid = !file_name.is_empty()
        && file_name.len() <= MAX_UNIT_FILE_NAME
        && file_name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'));
    if !valid {
        bail!("hook file name {file_name:?} must use only ASCII letters, digits, '-' and '_'");
    }
    Ok(())
}

/// Single-quote one argument for a POSIX shell. An embedded quote is closed,
/// escaped and reopened. Control characters are refused.
fn sh_quote(value: &str, label: &str) -> Result<String> {
    if value
        .bytes()
        .any(|byte| byte == 0 || byte.is_ascii_control())
    {
        bail!("{label} cannot contain newlines, tabs, control characters or NUL");
    }
    Ok(format!("'{}'", value.replace('\'', "'\\''")))
}

/// What a theme-set shim runs: `execute run <program> --confirm <fingerprint>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HookRunSpec {
    /// Shim file name inside the hook directory, such as `execute-theme`.
    pub name: String,
    /// Absolute path of the `execute` binary.
    pub executable: String,
    /// Absolute path of the `.program` file.
    pub program: String,
    /// Fingerprint of the reviewed plan. `run --confirm` refuses on mismatch.
    pub fingerprint: String,
}

/// Render the shim for the theme-set hook. Its `exec` hands the program's exit
/// status to `omarchy-hook`, which prints a failure and runs the remaining hooks.
/// Pure: writes nothing.
pub fn render_theme_set_hook(spec: &HookRunSpec) -> Result<UnitFile> {
    check_hook_file_name(&spec.name)?;
    if !spec.executable.starts_with('/') {
        bail!("execute binary path must be absolute");
    }
    if !spec.program.starts_with('/') {
        bail!("program path must be absolute");
    }
    let fingerprint_ok = !spec.fingerprint.is_empty()
        && spec
            .fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b':' | b'-' | b'_'));
    if !fingerprint_ok {
        bail!("fingerprint must be non-empty and use only ASCII letters, digits, ':', '-' and '_'");
    }
    let executable = sh_quote(&spec.executable, "execute binary path")?;
    let program = sh_quote(&spec.program, "program path")?;
    let fingerprint = &spec.fingerprint;
    Ok(UnitFile {
        file_name: spec.name.clone(),
        contents: format!(
            "{HOOK_SHEBANG}\n{GENERATED_MARKER}\n\
# Runs one reviewed program on the Omarchy theme-set event. exec passes the\n\
# exit status back to omarchy-hook, which prints a failure and continues.\n\
exec {executable} run {program} --confirm {fingerprint}\n"
        ),
    })
}

/// Decide whether a hook shim is absent, unchanged, to be replaced, or refused.
/// Writes nothing. A file Execute did not write as a shim is refused.
pub fn plan_hook_install(hook_dir: &Path, hook: &UnitFile) -> Result<InstallPlan> {
    check_hook_file_name(&hook.file_name)?;
    if !is_generated_hook(&hook.contents) {
        bail!(
            "hook file {} does not begin with the shebang and Execute marker",
            hook.file_name
        );
    }
    let action = match inspect_with(&hook_dir.join(&hook.file_name), is_generated_hook)? {
        OnDisk::Absent => InstallAction::Create,
        OnDisk::Generated(existing) if existing == hook.contents => InstallAction::Unchanged,
        OnDisk::Generated(_) => InstallAction::Replace,
        OnDisk::Foreign(reason) => {
            InstallAction::Refuse(refusal(&hook.file_name, reason, "overwrite"))
        }
    };
    Ok(InstallPlan {
        unit_dir: hook_dir.to_path_buf(),
        files: vec![PlannedFile {
            file_name: hook.file_name.clone(),
            contents: hook.contents.clone(),
            action,
        }],
    })
}

/// Write a planned hook shim with mode 0755 on Unix. Refuses a plan with a
/// refusal, and inspects the disk again first. The outcome has no commands:
/// a hook runs on its event and needs no `systemctl`.
pub fn apply_hook_install(hook_dir: &Path, plan: &InstallPlan) -> Result<InstallOutcome> {
    if plan.unit_dir != hook_dir {
        bail!(
            "the plan was made for {} but install was asked to write to {}",
            plan.unit_dir.display(),
            hook_dir.display()
        );
    }
    if plan.has_refusal() {
        bail!("install refused, nothing written: {}", plan_refusals(plan));
    }
    let [file] = plan.files.as_slice() else {
        bail!("a hook install writes exactly one file");
    };
    check_hook_file_name(&file.file_name)?;
    if !is_generated_hook(&file.contents) {
        bail!(
            "hook file {} does not begin with the shebang and Execute marker",
            file.file_name
        );
    }
    let path = hook_dir.join(&file.file_name);
    let (changed, unchanged) = match inspect_with(&path, is_generated_hook)? {
        OnDisk::Generated(existing) if existing == file.contents => {
            (Vec::new(), vec![file.file_name.clone()])
        }
        OnDisk::Absent | OnDisk::Generated(_) => {
            fs::create_dir_all(hook_dir)
                .with_context(|| format!("creating {}", hook_dir.display()))?;
            write_unit_file(&path, file.contents.as_bytes()).map_err(|error| error.error)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&path, fs::Permissions::from_mode(0o755))
                    .with_context(|| format!("making {} executable", path.display()))?;
            }
            (vec![file.file_name.clone()], Vec::new())
        }
        OnDisk::Foreign(reason) => {
            bail!(
                "install refused, nothing written: {}",
                refusal(&file.file_name, reason, "overwrite")
            );
        }
    };
    Ok(InstallOutcome::Installed {
        changed,
        unchanged,
        commands: Vec::new(),
    })
}

/// Join the refusal messages of a plan for an error.
fn plan_refusals(plan: &InstallPlan) -> String {
    plan.files
        .iter()
        .filter_map(|file| match &file.action {
            InstallAction::Refuse(reason) => Some(reason.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("; ")
}

/// Decide what removing one hook shim would do. Writes nothing. Only a file
/// carrying the shim marker is removable.
pub fn plan_hook_uninstall(hook_dir: &Path, file_name: &str) -> Result<UninstallPlan> {
    check_hook_file_name(file_name)?;
    let action = match inspect_with(&hook_dir.join(file_name), is_generated_hook)? {
        OnDisk::Absent => UninstallAction::Absent,
        OnDisk::Generated(_) => UninstallAction::Remove,
        OnDisk::Foreign(reason) => UninstallAction::Refuse(refusal(file_name, reason, "remove")),
    };
    Ok(UninstallPlan {
        unit_dir: hook_dir.to_path_buf(),
        entries: vec![UninstallEntry {
            file_name: file_name.to_owned(),
            action,
        }],
    })
}

/// Remove the planned hook shim and nothing else. Inspects the disk again, so a
/// file that turned foreign after planning is refused and kept.
pub fn apply_hook_uninstall(hook_dir: &Path, plan: &UninstallPlan) -> Result<UninstallOutcome> {
    if plan.unit_dir != hook_dir {
        bail!(
            "the plan was made for {} but uninstall was asked to act on {}",
            plan.unit_dir.display(),
            hook_dir.display()
        );
    }
    if plan.has_refusal() {
        bail!("uninstall refused, nothing removed");
    }
    let [entry] = plan.entries.as_slice() else {
        bail!("a hook uninstall removes exactly one file");
    };
    check_hook_file_name(&entry.file_name)?;
    let path = hook_dir.join(&entry.file_name);
    match inspect_with(&path, is_generated_hook)? {
        OnDisk::Absent => Ok(UninstallOutcome {
            removed: Vec::new(),
            absent: vec![entry.file_name.clone()],
        }),
        OnDisk::Generated(_) => {
            fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
            Ok(UninstallOutcome {
                removed: vec![entry.file_name.clone()],
                absent: Vec::new(),
            })
        }
        OnDisk::Foreign(reason) => bail!(
            "uninstall refused, nothing removed: {}",
            refusal(&entry.file_name, reason, "remove")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    fn theme_hook() -> HookRunSpec {
        HookRunSpec {
            name: "execute-theme".into(),
            executable: "/usr/bin/execute".into(),
            program: "/home/cam/theme.program".into(),
            fingerprint: "sha256:abc123".into(),
        }
    }

    #[test]
    fn theme_set_hook_matches_golden_text() {
        let hook = render_theme_set_hook(&theme_hook()).unwrap();
        assert_eq!(hook.file_name, "execute-theme");
        assert_eq!(
            hook.contents,
            "#!/bin/sh\n\
# Generated by Execute. Do not edit by hand.\n\
# Runs one reviewed program on the Omarchy theme-set event. exec passes the\n\
# exit status back to omarchy-hook, which prints a failure and continues.\n\
exec '/usr/bin/execute' run '/home/cam/theme.program' --confirm sha256:abc123\n"
        );
    }

    #[test]
    fn theme_set_hook_quotes_hostile_paths_and_refuses_bad_input() {
        let mut spec = theme_hook();
        spec.program = "/home/cam/it's $HOME `x`.program".into();
        let hook = render_theme_set_hook(&spec).unwrap();
        assert!(hook
            .contents
            .contains("run '/home/cam/it'\\''s $HOME `x`.program' --confirm sha256:abc123\n"));

        let mut newline = theme_hook();
        newline.program = "/home/cam/a\nb".into();
        assert!(render_theme_set_hook(&newline).is_err());

        let mut escape = theme_hook();
        escape.name = "../escape".into();
        assert!(render_theme_set_hook(&escape).is_err());

        let mut dotted = theme_hook();
        dotted.name = "theme.sh".into();
        assert!(render_theme_set_hook(&dotted).is_err());

        let mut relative = theme_hook();
        relative.program = "theme.program".into();
        assert!(render_theme_set_hook(&relative).is_err());

        let mut spaced = theme_hook();
        spaced.fingerprint = "a b".into();
        assert!(render_theme_set_hook(&spaced).is_err());
    }

    #[test]
    fn theme_set_hook_installs_and_uninstalls_exactly_its_own_file() {
        let root = tempdir().unwrap();
        let dir = omarchy_theme_set_directory(&root.path().join("omarchy"));
        let hook = render_theme_set_hook(&theme_hook()).unwrap();

        let plan = plan_hook_install(&dir, &hook).unwrap();
        assert!(!plan.has_refusal());
        assert_eq!(plan.files[0].action, InstallAction::Create);
        assert!(apply_hook_install(&dir, &plan).unwrap().is_installed());
        assert_eq!(
            fs::read_to_string(dir.join("execute-theme")).unwrap(),
            hook.contents
        );

        fs::write(dir.join("other-hook"), "#!/bin/sh\necho mine\n").unwrap();
        let again = plan_hook_install(&dir, &hook).unwrap();
        assert_eq!(again.files[0].action, InstallAction::Unchanged);

        let uninstall = plan_hook_uninstall(&dir, "execute-theme").unwrap();
        assert!(!uninstall.has_refusal());
        let removed = apply_hook_uninstall(&dir, &uninstall).unwrap();
        assert_eq!(
            removed,
            UninstallOutcome {
                removed: vec!["execute-theme".into()],
                absent: vec![],
            }
        );
        assert!(!dir.join("execute-theme").exists());
        assert!(dir.join("other-hook").exists());

        let gone = plan_hook_uninstall(&dir, "execute-theme").unwrap();
        assert_eq!(gone.entries[0].action, UninstallAction::Absent);
    }

    #[test]
    fn theme_set_hook_refuses_a_foreign_file_and_rejects_unit_marker_contents() {
        let root = tempdir().unwrap();
        let dir = omarchy_theme_set_directory(root.path());
        fs::create_dir_all(&dir).unwrap();
        let unit_style = UnitFile {
            file_name: "execute-theme".into(),
            contents: format!("{GENERATED_MARKER}\n"),
        };
        assert!(plan_hook_install(&dir, &unit_style).is_err());

        fs::write(dir.join("execute-theme"), "#!/bin/sh\necho mine\n").unwrap();
        let hook = render_theme_set_hook(&theme_hook()).unwrap();
        let plan = plan_hook_install(&dir, &hook).unwrap();
        assert!(plan.has_refusal());
        assert!(apply_hook_install(&dir, &plan).is_err());
        assert_eq!(
            fs::read_to_string(dir.join("execute-theme")).unwrap(),
            "#!/bin/sh\necho mine\n"
        );

        let uninstall = plan_hook_uninstall(&dir, "execute-theme").unwrap();
        assert!(uninstall.has_refusal());
        assert!(apply_hook_uninstall(&dir, &uninstall).is_err());
        assert!(dir.join("execute-theme").exists());
    }

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

    /// Derive the hardening lines for a policy given as JSON, against `work`.
    fn hardening_for(policy: &serde_json::Value, work: &Path) -> String {
        let policy = crate::capabilities::CapabilityPolicy::from_json(&policy.to_string()).unwrap();
        derived_hardening(Some(&policy), work, &[]).unwrap()
    }

    /// Render a whole unit for a policy file on disk. `policy` receives the
    /// temporary write root.
    fn systemd_unit_under_policy(policy: impl FnOnce(&str) -> serde_json::Value) -> String {
        let directory = tempdir().unwrap();
        let workflow = directory.path().join("writer.program");
        fs::write(
            &workflow,
            "every 5 minutes\n  then encode \"body\" as json\n",
        )
        .unwrap();
        let working_directory = directory.path().join("work");
        fs::create_dir(&working_directory).unwrap();
        let data = directory.path().join("data");
        fs::create_dir(&data).unwrap();
        let policy_path = directory.path().join("policy.json");
        fs::write(
            &policy_path,
            policy(&data.display().to_string()).to_string(),
        )
        .unwrap();
        let executable = std::env::current_exe().unwrap();
        let inputs = BTreeMap::new();
        let arguments: Vec<String> = Vec::new();
        render_for_workflow(&WorkflowServiceRequest {
            format: ServiceFormat::Systemd,
            workflow_path: &workflow,
            executable: &executable,
            working_directory: &working_directory,
            manifest_directory: None,
            execution_mode: None,
            capability_policy: Some(&policy_path),
            data_flow_policy: None,
            child_environment_allowlist: None,
            answers_file: None,
            label: Some("com.example.writer"),
            inputs: &inputs,
            program_arguments: &arguments,
            persistent: false,
        })
        .unwrap_or_else(|error| panic!("render failed: {error:#}"))
    }

    #[test]
    fn derived_hardening_states_each_fact_the_policy_grants() {
        let directory = tempdir().unwrap();
        let data = directory.path().join("data");
        fs::create_dir(&data).unwrap();
        let work = directory.path().join("work");
        fs::create_dir(&work).unwrap();
        let rendered = hardening_for(
            &serde_json::json!({
                "version": 1,
                "deny_privileged": true,
                "applets": {
                    "writer": {
                        "capabilities": ["filesystem.write"],
                        "filesystem": { "write": [data.display().to_string()] }
                    }
                }
            }),
            &work,
        );
        assert!(rendered.contains("NoNewPrivileges=yes\n"));
        assert!(rendered.contains("PrivateNetwork=yes\n"));
        assert!(rendered.contains("ProtectSystem=strict\n"));
        assert!(rendered
            .lines()
            .any(|line| line.starts_with("ReadWritePaths=") && line.ends_with("data\"")));
        // Each directive is preceded by the comment that names its justification.
        assert!(rendered.contains("# derived: capability policy sets deny_privileged\n"));
    }

    #[test]
    fn derived_hardening_skips_facts_the_policy_does_not_state() {
        let directory = tempdir().unwrap();
        let work = directory.path().join("work");
        fs::create_dir(&work).unwrap();
        // A network grant and no deny_privileged: neither NoNewPrivileges nor
        // PrivateNetwork is stated. No write grant states ProtectSystem=strict.
        let networked = hardening_for(
            &serde_json::json!({
                "version": 1,
                "applets": {
                    "fetcher": {
                        "capabilities": ["network.connect"],
                        "network": true
                    }
                }
            }),
            &work,
        );
        assert!(!networked.contains("NoNewPrivileges"));
        assert!(!networked.contains("PrivateNetwork"));
        assert!(networked.contains("ProtectSystem=strict\n"));

        // A relative write grant resolves against a directory the workflow may
        // change, so no filesystem directive is derived.
        let relative = hardening_for(
            &serde_json::json!({
                "version": 1,
                "applets": {
                    "writer": {
                        "capabilities": ["filesystem.write"],
                        "filesystem": { "write": ["data"] }
                    }
                }
            }),
            &work,
        );
        assert!(!relative.contains("ReadWritePaths"));
        assert!(!relative.contains("ProtectSystem"));
        assert!(relative.contains("# not derived: a filesystem write grant is relative"));
    }

    #[test]
    fn derived_hardening_is_absent_without_a_policy() {
        let directory = tempdir().unwrap();
        let workflow = directory.path().join("writer.program");
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
        let unpolicied = render_for_workflow(&WorkflowServiceRequest {
            format: ServiceFormat::Systemd,
            workflow_path: &workflow,
            executable: &executable,
            working_directory: &working_directory,
            manifest_directory: None,
            execution_mode: None,
            capability_policy: None,
            data_flow_policy: None,
            child_environment_allowlist: None,
            answers_file: None,
            label: Some("com.example.writer"),
            inputs: &inputs,
            program_arguments: &arguments,
            persistent: false,
        })
        .unwrap();
        for directive in [
            "NoNewPrivileges",
            "ProtectSystem",
            "PrivateNetwork",
            "ReadWritePaths",
        ] {
            assert!(
                !unpolicied.contains(directive),
                "{directive} without a policy"
            );
        }
    }

    #[test]
    fn rendered_unit_places_policy_directives_in_service_section() {
        let rendered = systemd_unit_under_policy(|data| {
            serde_json::json!({
                "version": 1,
                "deny_privileged": true,
                "applets": {
                    "writer": {
                        "capabilities": ["filesystem.write"],
                        "filesystem": { "write": [data] }
                    }
                }
            })
        });
        assert!(rendered.contains("NoNewPrivileges=yes\n"));
        assert!(rendered.contains("ProtectSystem=strict\n"));
        assert!(rendered
            .lines()
            .any(|line| line.starts_with("ReadWritePaths=") && line.ends_with("data\"")));
        // The directives sit inside [Service], before [Install], and the
        // command line keeps its existing escaping.
        let install = rendered.find("\n[Install]").unwrap();
        assert!(rendered.find("NoNewPrivileges=yes").unwrap() < install);
        assert!(rendered.contains("ExecStart=\""));
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
            execution_mode: None,
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
            execution_mode: None,
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
                execution_mode: None,
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
            execution_mode: None,
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
            execution_mode: None,
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
            execution_mode: None,
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

    #[test]
    fn generated_service_forwards_execution_mode_and_refuses_untrusted_without_policy() {
        use crate::capabilities::ExecutionMode;

        let directory = tempdir().unwrap();
        let workflow = directory.path().join("scheduled.program");
        fs::write(
            &workflow,
            "every 5 minutes\n  then encode \"body\" as json\n",
        )
        .unwrap();
        let policy = directory.path().join("policy.json");
        fs::write(&policy, "{\"version\":1,\"applets\":{}}\n").unwrap();
        let executable = std::env::current_exe().unwrap();
        let inputs = BTreeMap::new();
        let arguments: Vec<String> = Vec::new();
        let mut request = WorkflowServiceRequest {
            format: ServiceFormat::Systemd,
            workflow_path: &workflow,
            executable: &executable,
            working_directory: directory.path(),
            manifest_directory: None,
            execution_mode: Some(ExecutionMode::Untrusted),
            capability_policy: Some(&policy),
            data_flow_policy: None,
            child_environment_allowlist: None,
            answers_file: None,
            label: Some("com.example.untrusted-service"),
            inputs: &inputs,
            program_arguments: &arguments,
            persistent: false,
        };
        let rendered = render_for_workflow(&request).unwrap();
        assert!(rendered.contains("\"--execution-mode\" \"untrusted\""));
        assert!(rendered.contains("\"--capability-policy\""));

        // Fail closed: an untrusted unit without a policy could never start.
        request.capability_policy = None;
        let error = render_for_workflow(&request).unwrap_err();
        assert!(
            format!("{error:#}").contains("untrusted execution requires a capability policy"),
            "{error:#}"
        );

        request.execution_mode = Some(ExecutionMode::Trusted);
        let rendered = render_for_workflow(&request).unwrap();
        assert!(rendered.contains("\"--execution-mode\" \"trusted\""));

        request.execution_mode = None;
        let rendered = render_for_workflow(&request).unwrap();
        assert!(!rendered.contains("--execution-mode"));
    }

    fn nightly_backup() -> RunUnitSpec {
        RunUnitSpec {
            name: "nightly-backup".into(),
            description: "Nightly backup".into(),
            executable: "/usr/bin/execute".into(),
            program: "/home/cam/backup.program".into(),
        }
    }

    #[test]
    fn timer_pair_matches_golden_text() {
        let pair = render_timer_pair(
            &nightly_backup(),
            &TimerTrigger::Calendar("*-*-* 03:00:00".into()),
        )
        .unwrap();
        assert_eq!(pair.trigger.file_name, "nightly-backup.timer");
        assert_eq!(
            pair.trigger.contents,
            concat!(
                "# Generated by Execute. Do not edit by hand.\n",
                "[Unit]\n",
                "Description=Nightly backup\n",
                "\n",
                "[Timer]\n",
                "OnCalendar=*-*-* 03:00:00\n",
                "Unit=nightly-backup.service\n",
                "\n",
                "[Install]\n",
                "WantedBy=timers.target\n",
            )
        );
        assert_eq!(pair.service.file_name, "nightly-backup.service");
        assert_eq!(
            pair.service.contents,
            concat!(
                "# Generated by Execute. Do not edit by hand.\n",
                "[Unit]\n",
                "Description=Nightly backup\n",
                "\n",
                "[Service]\n",
                "Type=oneshot\n",
                "ExecStart=\"/usr/bin/execute\" \"run\" \"/home/cam/backup.program\"\n",
                "StandardOutput=journal\n",
                "StandardError=journal\n",
            )
        );
    }

    #[test]
    fn monotonic_timer_renders_boot_and_interval_in_seconds_or_nanoseconds() {
        let pair = render_timer_pair(
            &nightly_backup(),
            &TimerTrigger::Monotonic {
                on_boot_sec: Duration::from_secs(300),
                on_unit_active_sec: Duration::from_millis(1500),
            },
        )
        .unwrap();
        assert!(pair.trigger.contents.contains(
            "[Timer]\nOnBootSec=300s\nOnUnitActiveSec=1500000000ns\nUnit=nightly-backup.service\n"
        ));
        let zero = TimerTrigger::Monotonic {
            on_boot_sec: Duration::ZERO,
            on_unit_active_sec: Duration::from_secs(60),
        };
        assert!(render_timer_pair(&nightly_backup(), &zero).is_err());
    }

    #[test]
    fn path_pair_matches_golden_text_and_shares_the_service() {
        let timer =
            render_timer_pair(&nightly_backup(), &TimerTrigger::Calendar("daily".into())).unwrap();
        let pair = render_path_pair(
            &nightly_backup(),
            &PathCondition::Changed("/home/cam/inbox".into()),
        )
        .unwrap();
        assert_eq!(pair.trigger.file_name, "nightly-backup.path");
        assert_eq!(
            pair.trigger.contents,
            concat!(
                "# Generated by Execute. Do not edit by hand.\n",
                "[Unit]\n",
                "Description=Nightly backup\n",
                "\n",
                "[Path]\n",
                "PathChanged=/home/cam/inbox\n",
                "Unit=nightly-backup.service\n",
                "\n",
                "[Install]\n",
                "WantedBy=paths.target\n",
            )
        );
        assert_eq!(pair.service, timer.service);
        let exists = render_path_pair(
            &nightly_backup(),
            &PathCondition::Exists("/run/ready".into()),
        )
        .unwrap();
        assert!(exists
            .trigger
            .contents
            .contains("\nPathExists=/run/ready\n"));
        let modified = render_path_pair(
            &nightly_backup(),
            &PathCondition::Modified("/run/ready".into()),
        )
        .unwrap();
        assert!(modified
            .trigger
            .contents
            .contains("\nPathModified=/run/ready\n"));
    }

    #[test]
    fn hostile_program_path_is_escaped_in_execstart_only() {
        let mut run = nightly_backup();
        run.program = "/home/cam/x$HOME%y\"z\\w".into();
        let pair = render_timer_pair(&run, &TimerTrigger::Calendar("daily".into())).unwrap();
        assert!(pair.service.contents.contains(
            "ExecStart=\"/usr/bin/execute\" \"run\" \"/home/cam/x$$HOME%%y\\\"z\\\\w\"\n"
        ));
        // Every line of the service is still a single directive.
        assert_eq!(pair.service.contents.matches("ExecStart=").count(), 1);
    }

    #[test]
    fn hostile_description_doubles_percent_and_refuses_quotes_and_newlines() {
        assert_eq!(description_value("100% done").unwrap(), "100%% done");
        assert_eq!(
            description_value("$HOME stays literal").unwrap(),
            "$HOME stays literal"
        );
        assert!(description_value("Backup\nExecStart=/bin/sh").is_err());
        assert!(description_value("say \"hi\"").is_err());
        assert!(description_value("tail\\").is_err());
        assert!(description_value("").is_err());
        let mut run = nightly_backup();
        run.description = "Backup %n\nExecStart=/bin/sh".into();
        assert!(render_timer_pair(&run, &TimerTrigger::Calendar("daily".into())).is_err());
    }

    #[test]
    fn hostile_watched_path_is_escaped_as_a_single_value() {
        let pair = render_path_pair(
            &nightly_backup(),
            &PathCondition::Changed("/srv/in%box$HOME".into()),
        )
        .unwrap();
        // systemd expands %-specifiers in path values but not $VARS.
        assert!(pair
            .trigger
            .contents
            .contains("\nPathChanged=/srv/in%%box$HOME\n"));
        for hostile in [
            "/srv/inbox\nExecStart=/bin/sh",
            "/srv/inbox\0",
            "/srv/in\"box",
            "/srv/in\\box",
            "relative/inbox",
        ] {
            assert!(
                render_path_pair(&nightly_backup(), &PathCondition::Changed(hostile.into()))
                    .is_err(),
                "{hostile:?} must be refused"
            );
        }
    }

    #[test]
    fn hostile_calendar_expression_is_refused_before_it_reaches_the_file() {
        for hostile in [
            "daily\nExecStart=/bin/sh",
            "daily\r\n[Install]",
            "daily\0",
            "daily\tx",
            "%n",
            "*-*-* 03:00\\",
            "daily \"x\"",
            "",
        ] {
            assert!(
                render_timer_pair(&nightly_backup(), &TimerTrigger::Calendar(hostile.into()))
                    .is_err(),
                "{hostile:?} must be refused"
            );
        }
        // Calendar grammar is left to systemd-analyze: a malformed but safe
        // value is written through.
        let pair = render_timer_pair(
            &nightly_backup(),
            &TimerTrigger::Calendar("not a real spec".into()),
        )
        .unwrap();
        assert!(pair
            .trigger
            .contents
            .contains("OnCalendar=not a real spec\n"));
    }

    #[test]
    fn program_and_executable_must_be_absolute_and_control_free() {
        let mut run = nightly_backup();
        run.program = "backup.program".into();
        assert!(render_timer_pair(&run, &TimerTrigger::Calendar("daily".into())).is_err());
        run = nightly_backup();
        run.executable = "execute".into();
        assert!(render_timer_pair(&run, &TimerTrigger::Calendar("daily".into())).is_err());
        run = nightly_backup();
        run.program = "/home/cam/a\nExecStart=/bin/sh".into();
        assert!(render_timer_pair(&run, &TimerTrigger::Calendar("daily".into())).is_err());
        assert!(render_path_pair(&run, &PathCondition::Exists("/run/x".into())).is_err());
        run = nightly_backup();
        run.program = "/home/cam/a\0b".into();
        assert!(render_timer_pair(&run, &TimerTrigger::Calendar("daily".into())).is_err());
    }

    #[test]
    fn unit_names_follow_the_systemd_character_set() {
        let longest = "a".repeat(247);
        for valid in ["nightly-backup", "a.b_c:d", "Unit9", longest.as_str()] {
            assert!(validate_unit_name(valid).is_ok(), "{valid:?}");
        }
        let too_long = "a".repeat(248);
        for invalid in [
            "",
            ".hidden",
            "../escape",
            "a/b",
            "a b",
            "a\"b",
            "a%b",
            "a$b",
            "a\\b",
            "a\nb",
            "backup.service",
            "backup.timer",
            "backup.path",
            too_long.as_str(),
        ] {
            assert!(validate_unit_name(invalid).is_err(), "{invalid:?}");
        }
        let mut run = nightly_backup();
        run.name = "../evil".into();
        assert!(render_timer_pair(&run, &TimerTrigger::Calendar("daily".into())).is_err());
        assert!(render_path_pair(&run, &PathCondition::Exists("/run/x".into())).is_err());
    }

    #[test]
    fn ownership_marker_identifies_generated_files_only() {
        let pair =
            render_timer_pair(&nightly_backup(), &TimerTrigger::Calendar("daily".into())).unwrap();
        assert!(is_generated(&pair.trigger.contents));
        assert!(is_generated(&pair.service.contents));
        let path =
            render_path_pair(&nightly_backup(), &PathCondition::Exists("/run/x".into())).unwrap();
        assert!(is_generated(&path.trigger.contents));
        assert!(is_generated(&path.service.contents));
        // Windows line endings still carry the marker on the first line.
        assert!(is_generated(&pair.trigger.contents.replace('\n', "\r\n")));

        // Foreign files: a hand-written unit, an empty file, a marker that is
        // not the first line, a marker with extra text, and the review-only
        // header that the rendering for review uses.
        assert!(!is_generated("[Unit]\nDescription=mine\n"));
        assert!(!is_generated(""));
        assert!(!is_generated(
            "[Unit]\n# Generated by Execute. Do not edit by hand.\n"
        ));
        assert!(!is_generated(
            "# Generated by Execute. Do not edit by hand. (copied)\n[Unit]\n"
        ));
        assert!(!is_generated("# Generated for review. Save as x.service\n"));
    }

    fn nightly_files() -> Vec<UnitFile> {
        let pair =
            render_timer_pair(&nightly_backup(), &TimerTrigger::Calendar("daily".into())).unwrap();
        vec![pair.trigger, pair.service]
    }

    fn unit_dir_in(root: &Path) -> PathBuf {
        root.join("systemd").join("user")
    }

    fn real_write(path: &Path, bytes: &[u8]) -> std::result::Result<(), UnitWriteError> {
        write_unit_file(path, bytes)
    }

    #[test]
    fn user_unit_directory_prefers_xdg_then_home() {
        assert_eq!(
            user_unit_directory(Some("/xdg"), Some("/home/cam")).unwrap(),
            PathBuf::from("/xdg/systemd/user")
        );
        assert_eq!(
            user_unit_directory(None, Some("/home/cam")).unwrap(),
            PathBuf::from("/home/cam/.config/systemd/user")
        );
        assert!(user_unit_directory(None, None).is_err());
    }

    #[test]
    fn user_unit_directory_refuses_empty_or_relative_values() {
        assert!(user_unit_directory(Some(""), Some("/home/cam")).is_err());
        assert!(user_unit_directory(Some("config"), Some("/home/cam")).is_err());
        assert!(user_unit_directory(None, Some("")).is_err());
        assert!(user_unit_directory(None, Some("home/cam")).is_err());
    }

    #[test]
    fn install_creates_then_reports_unchanged_on_the_second_run() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        let files = nightly_files();

        let plan = plan_install(&dir, &files).unwrap();
        assert!(!plan.has_refusal());
        assert!(plan
            .files
            .iter()
            .all(|file| file.action == InstallAction::Create));
        let outcome = apply_install(&dir, &plan).unwrap();
        assert_eq!(
            outcome,
            InstallOutcome::Installed {
                changed: vec![
                    "nightly-backup.timer".into(),
                    "nightly-backup.service".into()
                ],
                unchanged: vec![],
                commands: vec![
                    argv(&["systemctl", "--user", "daemon-reload"]),
                    argv(&[
                        "systemctl",
                        "--user",
                        "enable",
                        "--now",
                        "nightly-backup.timer"
                    ]),
                ],
            }
        );
        assert_eq!(
            fs::read_to_string(dir.join("nightly-backup.timer")).unwrap(),
            files[0].contents
        );

        let again = plan_install(&dir, &files).unwrap();
        assert!(again
            .files
            .iter()
            .all(|file| file.action == InstallAction::Unchanged));
        let second = apply_install(&dir, &again).unwrap();
        assert!(second.is_installed());
        assert_eq!(
            second,
            InstallOutcome::Installed {
                changed: vec![],
                unchanged: vec![
                    "nightly-backup.timer".into(),
                    "nightly-backup.service".into()
                ],
                commands: outcome_commands(&outcome),
            }
        );
    }

    fn outcome_commands(outcome: &InstallOutcome) -> Vec<Vec<String>> {
        match outcome {
            InstallOutcome::Installed { commands, .. } => commands.clone(),
            InstallOutcome::RolledBack { .. } => panic!("expected an installed outcome"),
        }
    }

    #[test]
    fn install_replaces_a_generated_file_whose_contents_changed() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        fs::create_dir_all(&dir).unwrap();
        let old = format!("{GENERATED_MARKER}\n[Unit]\nDescription=old\n");
        fs::write(dir.join("nightly-backup.service"), &old).unwrap();

        let plan = plan_install(&dir, &nightly_files()).unwrap();
        let actions: Vec<&InstallAction> = plan.files.iter().map(|file| &file.action).collect();
        assert_eq!(
            actions,
            vec![&InstallAction::Create, &InstallAction::Replace]
        );
        let outcome = apply_install(&dir, &plan).unwrap();
        assert!(outcome.is_installed());
        assert_ne!(
            fs::read_to_string(dir.join("nightly-backup.service")).unwrap(),
            old
        );
    }

    #[test]
    fn foreign_file_refuses_overwrite_and_nothing_is_written() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        fs::create_dir_all(&dir).unwrap();
        let foreign = "[Unit]\nDescription=mine\n";
        fs::write(dir.join("nightly-backup.service"), foreign).unwrap();

        let plan = plan_install(&dir, &nightly_files()).unwrap();
        assert!(plan.has_refusal());
        assert_eq!(plan.files[0].action, InstallAction::Create);
        assert_eq!(
            plan.files[1].action,
            InstallAction::Refuse(
                "nightly-backup.service was not generated by Execute; refusing to overwrite it"
                    .into()
            )
        );
        let error = apply_install(&dir, &plan).unwrap_err().to_string();
        assert!(error.contains("nothing written"), "{error}");
        assert!(!dir.join("nightly-backup.timer").exists());
        assert_eq!(
            fs::read_to_string(dir.join("nightly-backup.service")).unwrap(),
            foreign
        );
    }

    #[test]
    fn apply_rechecks_the_disk_when_a_file_turns_foreign_after_planning() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        let plan = plan_install(&dir, &nightly_files()).unwrap();
        assert!(!plan.has_refusal());
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("nightly-backup.service"), "hand written\n").unwrap();

        let error = apply_install(&dir, &plan).unwrap_err().to_string();
        assert!(error.contains("nothing written"), "{error}");
        assert!(!dir.join("nightly-backup.timer").exists());
    }

    #[test]
    fn install_rolls_back_when_the_second_write_fails() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        let plan = plan_install(&dir, &nightly_files()).unwrap();
        let mut calls = 0;
        let mut second_fails = |path: &Path, bytes: &[u8]| {
            calls += 1;
            if calls == 2 {
                return Err(UnitWriteError {
                    published: false,
                    error: anyhow!("disk full"),
                });
            }
            real_write(path, bytes)
        };

        let outcome = apply_install_with(&dir, &plan, &mut second_fails).unwrap();
        assert_eq!(
            outcome,
            InstallOutcome::RolledBack {
                failed_file: "nightly-backup.service".into(),
                error: "disk full".into(),
                restored: vec![],
                removed: vec!["nightly-backup.timer".into()],
                rollback_errors: vec![],
            }
        );
        assert!(!dir.join("nightly-backup.timer").exists());
        assert!(!dir.join("nightly-backup.service").exists());
    }

    #[test]
    fn rollback_restores_a_replaced_file_to_its_previous_contents() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        fs::create_dir_all(&dir).unwrap();
        let old = format!("{GENERATED_MARKER}\n[Unit]\nDescription=old\n");
        fs::write(dir.join("nightly-backup.timer"), &old).unwrap();
        let plan = plan_install(&dir, &nightly_files()).unwrap();
        let mut calls = 0;
        let mut second_fails = |path: &Path, bytes: &[u8]| {
            calls += 1;
            if calls == 2 {
                return Err(UnitWriteError {
                    published: false,
                    error: anyhow!("disk full"),
                });
            }
            real_write(path, bytes)
        };

        let outcome = apply_install_with(&dir, &plan, &mut second_fails).unwrap();
        assert_eq!(
            outcome,
            InstallOutcome::RolledBack {
                failed_file: "nightly-backup.service".into(),
                error: "disk full".into(),
                restored: vec!["nightly-backup.timer".into()],
                removed: vec![],
                rollback_errors: vec![],
            }
        );
        assert_eq!(
            fs::read_to_string(dir.join("nightly-backup.timer")).unwrap(),
            old
        );
    }

    #[test]
    fn rollback_removes_a_file_published_before_its_flush_failed() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        let plan = plan_install(&dir, &nightly_files()).unwrap();
        let mut published_then_fails =
            |path: &Path, bytes: &[u8]| -> std::result::Result<(), UnitWriteError> {
                real_write(path, bytes)?;
                Err(UnitWriteError {
                    published: true,
                    error: anyhow!("flush failed"),
                })
            };

        let outcome = apply_install_with(&dir, &plan, &mut published_then_fails).unwrap();
        assert_eq!(
            outcome,
            InstallOutcome::RolledBack {
                failed_file: "nightly-backup.timer".into(),
                error: "flush failed".into(),
                restored: vec![],
                removed: vec!["nightly-backup.timer".into()],
                rollback_errors: vec![],
            }
        );
        assert!(!dir.join("nightly-backup.timer").exists());
    }

    #[test]
    fn apply_refuses_a_plan_made_for_another_directory() {
        let root = tempdir().unwrap();
        let plan = plan_install(&unit_dir_in(root.path()), &nightly_files()).unwrap();
        assert!(apply_install(&root.path().join("elsewhere"), &plan).is_err());
        assert!(!root.path().join("elsewhere").exists());
    }

    #[test]
    fn plan_install_rejects_names_that_escape_or_are_not_units() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        let body = format!("{GENERATED_MARKER}\n[Unit]\n");
        for name in [
            "../evil.service",
            "sub/x.service",
            "x\\y.service",
            "x.conf",
            "-x.timer",
            "x.timer.timer",
            ".service",
            "x.service.timer",
        ] {
            let file = UnitFile {
                file_name: name.into(),
                contents: body.clone(),
            };
            assert!(plan_install(&dir, &[file]).is_err(), "accepted {name}");
        }
    }

    #[test]
    fn plan_install_rejects_unmarked_contents_and_duplicate_names() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        let unmarked = UnitFile {
            file_name: "x.service".into(),
            contents: "[Unit]\n".into(),
        };
        assert!(plan_install(&dir, &[unmarked]).is_err());
        let marked = |name: &str| UnitFile {
            file_name: name.into(),
            contents: format!("{GENERATED_MARKER}\n"),
        };
        assert!(plan_install(&dir, &[marked("x.service"), marked("x.service")]).is_err());
    }

    #[test]
    fn uninstall_removes_exactly_the_named_generated_files_then_reports_absent() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        let plan = plan_install(&dir, &nightly_files()).unwrap();
        assert!(apply_install(&dir, &plan).unwrap().is_installed());
        fs::write(
            dir.join("other.timer"),
            format!("{GENERATED_MARKER}\n[Timer]\n"),
        )
        .unwrap();

        let names = ["nightly-backup.timer", "nightly-backup.service"];
        let first = plan_uninstall(&dir, &names).unwrap();
        assert!(!first.has_refusal());
        let removed = apply_uninstall(&dir, &first).unwrap();
        assert_eq!(
            removed,
            UninstallOutcome {
                removed: names.iter().map(|name| (*name).to_owned()).collect(),
                absent: vec![],
            }
        );
        assert!(!dir.join("nightly-backup.timer").exists());
        assert!(!dir.join("nightly-backup.service").exists());
        assert!(dir.join("other.timer").exists());

        let second_plan = plan_uninstall(&dir, &names).unwrap();
        assert!(second_plan
            .entries
            .iter()
            .all(|entry| entry.action == UninstallAction::Absent));
        let second = apply_uninstall(&dir, &second_plan).unwrap();
        assert_eq!(second.removed, Vec::<String>::new());
        assert_eq!(second.absent, names.map(str::to_owned).to_vec());
    }

    #[test]
    fn uninstall_refuses_a_foreign_file_and_removes_nothing() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("mine.timer"),
            format!("{GENERATED_MARKER}\n[Timer]\n"),
        )
        .unwrap();
        fs::write(dir.join("mine.service"), "[Service]\n").unwrap();

        let plan = plan_uninstall(&dir, &["mine.timer", "mine.service"]).unwrap();
        assert!(plan.has_refusal());
        assert!(apply_uninstall(&dir, &plan).is_err());
        assert!(dir.join("mine.timer").exists());
        assert!(dir.join("mine.service").exists());
    }

    #[test]
    fn uninstall_never_removes_a_directory() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        fs::create_dir_all(dir.join("trap.service")).unwrap();
        let plan = plan_uninstall(&dir, &["trap.service"]).unwrap();
        assert_eq!(
            plan.entries[0].action,
            UninstallAction::Refuse(
                "trap.service is not a regular file; refusing to remove it".into()
            )
        );
        assert!(apply_uninstall(&dir, &plan).is_err());
        assert!(dir.join("trap.service").is_dir());
    }

    #[test]
    fn uninstall_plan_rejects_path_like_names() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        assert!(plan_uninstall(&dir, &["../x.timer"]).is_err());
        assert!(plan_uninstall(&dir, &["x.timer", "x.timer"]).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn symbolic_links_are_refused_by_install_and_uninstall_and_never_followed() {
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        fs::create_dir_all(&dir).unwrap();
        let elsewhere = root.path().join("elsewhere.service");
        fs::write(&elsewhere, format!("{GENERATED_MARKER}\n[Service]\n")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, dir.join("nightly-backup.service")).unwrap();

        let install = plan_install(&dir, &nightly_files()).unwrap();
        assert_eq!(
            install.files[1].action,
            InstallAction::Refuse(
                "nightly-backup.service is a symbolic link; refusing to overwrite it".into()
            )
        );
        assert!(apply_install(&dir, &install).is_err());
        assert!(!dir.join("nightly-backup.timer").exists());

        let uninstall = plan_uninstall(&dir, &["nightly-backup.service"]).unwrap();
        assert!(uninstall.has_refusal());
        assert!(apply_uninstall(&dir, &uninstall).is_err());
        assert!(fs::symlink_metadata(dir.join("nightly-backup.service"))
            .unwrap()
            .file_type()
            .is_symlink());
        assert!(elsewhere.exists());
    }

    #[cfg(unix)]
    #[test]
    fn installed_unit_files_are_mode_0644() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempdir().unwrap();
        let dir = unit_dir_in(root.path());
        let plan = plan_install(&dir, &nightly_files()).unwrap();
        assert!(apply_install(&dir, &plan).unwrap().is_installed());
        let mode = fs::metadata(dir.join("nightly-backup.timer"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o644);
    }

    #[test]
    fn undeclared_steps_fall_back_to_the_baseline_and_name_the_step() {
        let directory = tempdir().unwrap();
        let data = directory.path().join("data");
        fs::create_dir(&data).unwrap();
        let work = directory.path().join("work");
        fs::create_dir(&work).unwrap();
        let policy = crate::capabilities::CapabilityPolicy::from_json(
            &serde_json::json!({
                "version": 1,
                "deny_privileged": true,
                "applets": {
                    "writer": {
                        "capabilities": ["filesystem.write"],
                        "filesystem": { "write": [data.display().to_string()] }
                    }
                }
            })
            .to_string(),
        )
        .unwrap();
        let reasons = vec!["line 3: /opt/tool has no declared effects".to_owned()];
        let rendered = derived_hardening(Some(&policy), &work, &reasons).unwrap();

        assert!(
            rendered.contains("# baseline forced by: line 3: /opt/tool has no declared effects\n")
        );
        for directive in BASELINE_DIRECTIVES {
            assert!(
                rendered.lines().any(|line| line == *directive),
                "{directive}"
            );
        }
        assert!(!rendered.contains("NoNewPrivileges"));
        assert!(!rendered.contains("PrivateNetwork"));
        assert!(!rendered.contains("ProtectSystem"));
        assert!(!rendered.contains("ReadWritePaths"));
    }

    #[test]
    fn undeclared_reason_text_cannot_inject_a_directive() {
        let directory = tempdir().unwrap();
        let policy = crate::capabilities::CapabilityPolicy::from_json(
            &serde_json::json!({ "version": 1, "deny_privileged": true, "applets": {} })
                .to_string(),
        )
        .unwrap();
        let reasons = vec!["line 1: tool\nPrivateTmp=no".to_owned()];
        let rendered = derived_hardening(Some(&policy), directory.path(), &reasons).unwrap();

        assert!(!rendered.lines().any(|line| line == "PrivateTmp=no"));
        assert!(rendered.lines().any(|line| line == "PrivateTmp=yes"));
    }

    #[test]
    fn hardening_mode_none_emits_no_sandbox_lines() {
        assert_eq!(HardeningMode::parse("none").unwrap(), HardeningMode::Off);
        assert_eq!(
            HardeningMode::parse("baseline").unwrap(),
            HardeningMode::Baseline
        );
        assert!(HardeningMode::parse("strict").is_err());

        let directory = tempdir().unwrap();
        let workflow = directory.path().join("writer.program");
        fs::write(
            &workflow,
            "every 5 minutes\n  then encode \"body\" as json\n",
        )
        .unwrap();
        let working_directory = directory.path().join("work");
        fs::create_dir(&working_directory).unwrap();
        let policy_path = directory.path().join("policy.json");
        fs::write(
            &policy_path,
            serde_json::json!({ "version": 1, "deny_privileged": true, "applets": {} }).to_string(),
        )
        .unwrap();
        let executable = std::env::current_exe().unwrap();
        let inputs = BTreeMap::new();
        let arguments: Vec<String> = Vec::new();
        let request = WorkflowServiceRequest {
            format: ServiceFormat::Systemd,
            workflow_path: &workflow,
            executable: &executable,
            working_directory: &working_directory,
            manifest_directory: None,
            execution_mode: None,
            capability_policy: Some(&policy_path),
            data_flow_policy: None,
            child_environment_allowlist: None,
            answers_file: None,
            label: Some("com.example.writer"),
            inputs: &inputs,
            program_arguments: &arguments,
            persistent: false,
        };
        let registry = crate::manifest::Registry::default();
        let off =
            render_for_workflow_with_hardening(&request, &registry, HardeningMode::Off).unwrap();
        assert!(!off.contains("NoNewPrivileges"));
        assert!(!off.contains("PrivateTmp"));
        assert!(!off.contains("# derived"));

        let baseline =
            render_for_workflow_with_hardening(&request, &registry, HardeningMode::Baseline)
                .unwrap();
        assert!(baseline.contains("PrivateTmp=yes\n"));
        assert!(!baseline.contains("NoNewPrivileges"));
    }
}
