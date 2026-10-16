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
    Launchd,
}

impl ServiceFormat {
    pub fn parse(value: &str) -> Result<Self> {
        match value {
            "systemd" => Ok(Self::Systemd),
            "launchd" => Ok(Self::Launchd),
            _ => bail!("service format must be `systemd` or `launchd`"),
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
    pub label: Option<&'a str>,
    pub inputs: &'a BTreeMap<String, String>,
    pub program_arguments: &'a [String],
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

    validate_explicit_environment_inputs(&program.inputs, request.inputs)?;
    let supplied = crate::inputs::parse_cli_arguments(
        &program.inputs,
        request.inputs,
        request.program_arguments,
    )?;
    validate_service_inputs(&program.inputs, &supplied, request.program_arguments.len())?;

    let mut argv = vec![utf8_path(&executable, "execute binary")?, "run".into()];
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
    for (name, value) in request.inputs {
        argv.push("--input".into());
        argv.push(format!("{name}={value}"));
    }
    argv.push(utf8_path(&workflow, "workflow")?);
    if !request.program_arguments.is_empty() {
        argv.push("--".into());
        argv.extend(request.program_arguments.iter().cloned());
    }
    validate_arguments(&argv)?;

    match request.format {
        ServiceFormat::Systemd => render_systemd(&label, &working_directory, &argv),
        ServiceFormat::Launchd => render_launchd(&label, &working_directory, &argv, launchd_home),
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
        let rendered = render_for_workflow(&WorkflowServiceRequest {
            format: ServiceFormat::Systemd,
            workflow_path: &workflow,
            executable: &executable,
            working_directory: &working_directory,
            manifest_directory: None,
            capability_policy: None,
            data_flow_policy: None,
            label: Some("com.example.daily-task"),
            inputs: &inputs,
            program_arguments: &arguments,
        })
        .unwrap();
        assert!(rendered.contains("KillMode=control-group"));
        assert!(rendered.contains("Restart=on-failure"));
        assert!(rendered.contains("\"two words\""));
        assert!(rendered.contains("Generated for review"));
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
            label: Some("com.example.catalog-service"),
            inputs: &inputs,
            program_arguments: &arguments,
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
                label: Some("com.example.scheduled"),
                inputs: &inputs,
                program_arguments: &arguments,
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
            label: None,
            inputs: &inputs,
            program_arguments: &arguments,
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
    }
}
