use flow::{
    capabilities::{
        authorize_environment_variable, authorize_host_path, effective_environment,
        filter_environment, parse_child_environment_allowlist, policy_fingerprint,
        preflight_with_cache_directory, AppletGrant, CapabilityPolicy, ContractView,
        FilesystemGrant, HostAccess, HostGrant,
    },
    manifest::Registry,
};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::Path;

fn grant(capabilities: &[&str]) -> AppletGrant {
    AppletGrant {
        capabilities: capabilities
            .iter()
            .map(|value| (*value).to_owned())
            .collect(),
        binary_sha256: None,
        environment: BTreeSet::new(),
        filesystem: FilesystemGrant::default(),
        network: false,
        network_origins: BTreeSet::new(),
        sandbox_required: false,
    }
}

fn host_grant(read: Vec<std::path::PathBuf>, write: Vec<std::path::PathBuf>) -> HostGrant {
    let mut capabilities = BTreeSet::new();
    if !read.is_empty() {
        capabilities.insert("filesystem.read".into());
    }
    if !write.is_empty() {
        capabilities.insert("filesystem.write".into());
    }
    HostGrant {
        capabilities,
        filesystem: FilesystemGrant { read, write },
    }
}

fn contract<'a>(capabilities: &'a [String], effects: &'a [String]) -> ContractView<'a> {
    ContractView {
        version: 1,
        capabilities,
        effects,
    }
}

fn registry_with_applet(name: &str, binary: &Path) -> Registry {
    let source = serde_json::json!({
        "name": name,
        "binary": binary,
        "arguments": [],
        "contract": {"version": 1}
    });
    Registry::from_sources(&[source.to_string()]).unwrap()
}

#[test]
fn policy_is_strictly_versioned_and_unknown_fields_fail_closed() {
    let valid = r#"{"version":1,"applets":{"fixture.read":{"capabilities":["filesystem.read"]}}}"#;
    assert!(CapabilityPolicy::from_json(valid).is_ok());
    assert!(CapabilityPolicy::from_json(
        r#"{"version":1,"host":{"capabilities":["filesystem.write"],"filesystem":{"write":["/tmp/results"]}},"applets":{}}"#
    )
    .is_ok());
    assert!(CapabilityPolicy::from_json(
        r#"{"version":1,"host":{"filesystem":{"write":["/tmp/results"]}},"applets":{}}"#
    )
    .unwrap_err()
    .to_string()
    .contains("filesystem.write"));
    assert!(CapabilityPolicy::from_json(r#"{"version":2,"applets":{}}"#)
        .unwrap_err()
        .to_string()
        .contains("unsupported capability policy version"));
    assert!(CapabilityPolicy::from_json(
        r#"{"version":1,"applets":{},"allow_direct_commands":true}"#
    )
    .is_err());
    assert!(CapabilityPolicy::from_json(
        r#"{"version":1,"applets":{"fixture.read":{"capabilities":[],"unknown":true}}}"#
    )
    .is_err());
    assert!(CapabilityPolicy::from_json(r#"{"version":1,"version":1,"applets":{}}"#).is_err());
    let oversized = format!(r#"{{"version":1,"applets":{{}}}}{}"#, " ".repeat(64 * 1024));
    assert!(CapabilityPolicy::from_json(&oversized)
        .unwrap_err()
        .to_string()
        .contains("size limit"));
    assert!(CapabilityPolicy::from_json(
        r#"{"version":1,"applets":{"fixture.read":{"capabilities":[],"network":true}}}"#
    )
    .unwrap_err()
    .to_string()
    .contains("network.connect"));
    assert!(CapabilityPolicy::from_json(
        r#"{"version":1,"applets":{"fixture.read":{"capabilities":["network.connect"]}}}"#
    )
    .is_err());
    assert!(CapabilityPolicy::from_json(
        r#"{"version":1,"applets":{"fixture.read":{"capabilities":["network.connect"],"network":true,"network_origins":["https://example.test/path"]}}}"#
    )
    .is_err());
    assert!(CapabilityPolicy::from_json(
        r#"{"version":1,"applets":{"fixture.read":{"capabilities":["network.connect"],"network":true,"network_origins":["https://user@example.test"]}}}"#
    )
    .is_err());
}

#[test]
fn remote_endpoint_policy_matches_exact_canonical_origin() {
    use flow::capabilities::authorize_network_endpoint;

    let source = serde_json::json!({
        "name":"remote.lookup",
        "binary":"@remote-applet",
        "arguments":[],
        "contract":{
            "version":1,
            "backend":"remote",
            "remote":{"endpoint":"https://api.example.test/v1"},
            "capabilities":[]
        }
    });
    let registry = Registry::from_sources(&[source.to_string()]).unwrap();
    let mut remote = grant(&["network.connect"]);
    remote.network = true;
    remote
        .network_origins
        .insert("https://api.example.test".into());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("remote.lookup".into(), remote)]),
    }));
    authorize_network_endpoint(
        &registry,
        "remote.lookup",
        "https://api.example.test/v1/search?q=local",
    )
    .unwrap();
    assert!(authorize_network_endpoint(
        &registry,
        "remote.lookup",
        "https://other.example.test/v1/search"
    )
    .is_err());
    let program = flow::parser::parse("run remote.lookup\n").unwrap();
    let cwd = tempfile::tempdir().unwrap();
    flow::capabilities::preflight(&program, &registry, cwd.path()).unwrap();
    assert!(authorize_network_endpoint(
        &registry,
        "remote.lookup",
        "https://user:secret@api.example.test/v1/search"
    )
    .is_err());
}

#[test]
fn grants_require_canonical_allowlist_and_exact_contract_capabilities() {
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("fixture");
    fs::write(&binary, b"reviewed fixture").unwrap();
    let policy = CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.read".into(), grant(&["filesystem.read"]))]),
    };
    let capabilities = vec!["filesystem.read".to_owned()];
    let effects = capabilities.clone();
    policy
        .authorize(
            "fixture.read",
            false,
            &binary,
            Some(contract(&capabilities, &effects)),
        )
        .unwrap();
    assert!(policy
        .authorize(
            "fixture.read",
            true,
            &binary,
            Some(contract(&capabilities, &effects)),
        )
        .unwrap_err()
        .to_string()
        .contains("direct quoted commands"));
    assert!(policy
        .authorize(
            "unlisted.applet",
            false,
            &binary,
            Some(contract(&capabilities, &effects)),
        )
        .unwrap_err()
        .to_string()
        .contains("no allowlist entry"));
    let forbidden = vec!["network.connect".to_owned()];
    assert!(policy
        .authorize(
            "fixture.read",
            false,
            &binary,
            Some(contract(&forbidden, &forbidden)),
        )
        .unwrap_err()
        .to_string()
        .contains("does not grant"));
}

#[test]
fn process_backends_require_a_separate_process_exec_grant() {
    use flow::{capabilities::preflight, parser};

    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("fixture");
    fs::write(&binary, b"reviewed fixture").unwrap();
    let registry = registry_with_applet("fixture.command", &binary);
    let program = parser::parse("run fixture.command\n").unwrap();
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.command".into(), grant(&[]))]),
    }));
    let error = preflight(&program, &registry, directory.path()).unwrap_err();
    assert!(error
        .to_string()
        .contains("backend requirement \"process.exec\""));

    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.command".into(), grant(&["process.exec"]))]),
    }));
    // This test checks declaration authorization, independently of OS support.
    registry.set_host_state(Some(flow::capabilities::ExecutionMode::Trusted));
    preflight(&program, &registry, directory.path()).unwrap();
}

#[test]
fn strict_policy_allows_shipped_pure_json_transformations() {
    let manifest = include_str!("../manifests/core.json_parse.json");
    let registry = Registry::from_sources(&[manifest]).unwrap();
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("json.parse".into(), grant(&["json.convert"]))]),
    }));
    let program = flow::parser::parse("run json.parse with text \"{}\"\n").unwrap();
    flow::capabilities::preflight(&program, &registry, Path::new(env!("CARGO_MANIFEST_DIR")))
        .unwrap();
}

#[test]
fn binary_pin_is_checked_and_policy_identity_changes_with_grants() {
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("fixture");
    fs::write(&binary, b"reviewed fixture").unwrap();
    let digest = flow::capabilities::sha256_file(&binary).unwrap();
    let mut pinned = grant(&[]);
    pinned.binary_sha256 = Some(digest);
    let policy = CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.read".into(), pinned)]),
    };
    let capabilities = Vec::new();
    policy
        .authorize(
            "fixture.read",
            false,
            &binary,
            Some(contract(&capabilities, &capabilities)),
        )
        .unwrap();
    fs::write(&binary, b"replaced fixture").unwrap();
    assert!(policy
        .authorize(
            "fixture.read",
            false,
            &binary,
            Some(contract(&capabilities, &capabilities)),
        )
        .unwrap_err()
        .to_string()
        .contains("binary hash mismatch"));

    let registry = registry_with_applet("fixture.read", &binary);
    registry.set_host_state(Some(policy));
    let first = policy_fingerprint(&registry, "fixture.read").unwrap();
    let mut changed = grant(&["filesystem.read"]);
    changed.environment.insert("PATH".into());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.read".into(), changed)]),
    }));
    let second = policy_fingerprint(&registry, "fixture.read").unwrap();
    assert_ne!(
        first, second,
        "worker pools must not reuse a different grant"
    );
}

#[test]
fn environment_is_cleared_to_the_applet_allowlist() {
    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("fixture");
    fs::write(&binary, b"fixture").unwrap();
    let mut configured = grant(&[]);
    configured.environment.insert("PATH".into());
    let registry = registry_with_applet("fixture.read", &binary);
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.read".into(), configured)]),
    }));
    let filtered = filter_environment(
        &registry,
        "fixture.read",
        vec![
            (OsString::from("PATH"), OsString::from("/usr/bin")),
            (
                OsString::from("AWS_SECRET_ACCESS_KEY"),
                OsString::from("hidden"),
            ),
        ],
    )
    .unwrap()
    .unwrap();
    assert_eq!(
        filtered,
        vec![(OsString::from("PATH"), OsString::from("/usr/bin"))]
    );
    authorize_environment_variable(&registry, "fixture.read", "PATH").unwrap();
    assert!(
        authorize_environment_variable(&registry, "fixture.read", "AWS_SECRET_ACCESS_KEY").is_err()
    );
}

#[test]
fn applet_environment_overlay_is_child_scoped_and_policy_checked() {
    let canary = "EXECUTE_TEST_ENV_OVERLAY";
    let before = std::env::var_os(canary);
    let source = serde_json::json!({
        "name":"fixture.env",
        "binary":"/usr/bin/env",
        "arguments":[],
        "contract":{
            "version":1,
            "environment":{
                "set":{(canary):"literal-value"},
                "unset":["PATH"]
            }
        }
    });
    let registry = Registry::from_sources(&[source.to_string()]).unwrap();
    let mut applet_grant = grant(&["process.exec"]);
    applet_grant.environment.insert(canary.into());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.env".into(), applet_grant)]),
    }));

    let environment = effective_environment(&registry, "fixture.env").unwrap();
    assert_eq!(
        environment
            .iter()
            .find(|(name, _)| name == std::ffi::OsStr::new(canary))
            .map(|(_, value)| value.as_os_str()),
        Some(std::ffi::OsStr::new("literal-value"))
    );
    assert!(!environment
        .iter()
        .any(|(name, _)| name == std::ffi::OsStr::new("PATH")));
    assert_eq!(std::env::var_os(canary), before, "host env must not change");
    // Exercise environment and argv construction in explicitly trusted mode.
    registry.set_host_state(Some(flow::capabilities::ExecutionMode::Trusted));
    let command = flow::capabilities::command_for(
        Path::new("unused-executable"),
        ["argument with spaces", "semi;colon"],
        &registry,
        "fixture.env",
        &flow::working_dir::WorkingDirectory::plain("."),
    )
    .unwrap();
    let expected_arguments = ["argument with spaces", "semi;colon"];
    assert_eq!(
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        expected_arguments
    );

    let mut denied = serde_json::json!(source);
    denied["name"] = serde_json::json!("fixture.denied");
    denied["contract"]["environment"]["set"] =
        serde_json::json!({"EXECUTE_TEST_UNGRANTED_ENV":"not-logged"});
    let denied_registry = Registry::from_sources(&[denied.to_string()]).unwrap();
    denied_registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.denied".into(), grant(&[]))]),
    }));
    let error = effective_environment(&denied_registry, "fixture.denied").unwrap_err();
    assert!(error
        .to_string()
        .contains("denies setting an environment variable"));
    assert!(!format!("{error:#}").contains("not-logged"));
}

#[test]
fn decisions_record_approvals_and_denials_without_paths_or_environment_names() {
    use flow::security::{policy_decisions, PolicyDecisionOutcome};

    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("fixture");
    fs::write(&binary, b"fixture").unwrap();
    let allowed_root = directory.path().join("allowed");
    fs::create_dir(&allowed_root).unwrap();
    let allowed_path = allowed_root.join("inside.txt");
    fs::write(&allowed_path, b"public fixture").unwrap();
    let denied_path = directory.path().join("sensitive-path-canary");
    fs::write(&denied_path, b"private").unwrap();
    let registry = registry_with_applet("fixture.read", &binary);
    let mut configured = grant(&["filesystem.read"]);
    configured.filesystem.read.push(allowed_root);
    registry.set_host_state(Some(CapabilityPolicy {
        host: Some(host_grant(
            vec![directory.path().join("allowed")],
            Vec::new(),
        )),
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.read".into(), configured)]),
    }));

    authorize_host_path(&registry, &allowed_path, HostAccess::Read, directory.path()).unwrap();
    authorize_host_path(&registry, &denied_path, HostAccess::Read, directory.path()).unwrap_err();
    authorize_environment_variable(&registry, "fixture.read", "API_SECRET_CANARY").unwrap_err();

    let decisions = policy_decisions(&registry);
    assert_eq!(decisions.len(), 3);
    assert!(decisions
        .iter()
        .any(|decision| decision.outcome == PolicyDecisionOutcome::Approved));
    assert_eq!(
        decisions
            .iter()
            .filter(|decision| decision.outcome == PolicyDecisionOutcome::Denied)
            .count(),
        2
    );
    let serialized = serde_json::to_string(&decisions).unwrap();
    assert!(!serialized.contains("sensitive-path-canary"));
    assert!(!serialized.contains("API_SECRET_CANARY"));
}

#[test]
fn host_path_grants_are_canonical_and_directional() {
    let directory = tempfile::tempdir().unwrap();
    let read_root = directory.path().join("input");
    let write_root = directory.path().join("output");
    fs::create_dir(&read_root).unwrap();
    fs::create_dir(&write_root).unwrap();
    let mut configured = grant(&["filesystem.read", "filesystem.write"]);
    configured.filesystem.read.push(read_root.clone());
    configured.filesystem.write.push(write_root.clone());
    let registry = registry_with_applet("fixture.read", directory.path());
    registry.set_host_state(Some(CapabilityPolicy {
        host: Some(host_grant(
            vec![read_root.clone()],
            vec![write_root.clone()],
        )),
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.read".into(), configured)]),
    }));
    let cwd = directory.path();
    authorize_host_path(
        &registry,
        &read_root.join("source.txt"),
        HostAccess::Read,
        cwd,
    )
    .unwrap();
    authorize_host_path(
        &registry,
        &write_root.join("result.txt"),
        HostAccess::Write,
        cwd,
    )
    .unwrap();
    assert!(authorize_host_path(&registry, &read_root, HostAccess::Write, cwd).is_err());
    assert!(authorize_host_path(&registry, &write_root, HostAccess::Read, cwd).is_err());
    assert!(authorize_host_path(
        &registry,
        &directory.path().join("input/../../outside.txt"),
        HostAccess::Read,
        cwd
    )
    .is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = directory.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"hidden").unwrap();
        symlink(&outside, read_root.join("escape")).unwrap();
        assert!(authorize_host_path(
            &registry,
            &read_root.join("escape/secret.txt"),
            HostAccess::Read,
            cwd
        )
        .is_err());
    }
}

#[test]
fn host_save_cannot_borrow_an_unrelated_applets_write_grant() {
    let directory = tempfile::tempdir().unwrap();
    let applet_root = directory.path().join("applet-output");
    fs::create_dir(&applet_root).unwrap();
    let save_path = applet_root.join("result.txt");
    let registry = registry_with_applet("fixture.writer", directory.path());
    let mut applet = grant(&["filesystem.write"]);
    applet.filesystem.write.push(applet_root);
    registry.set_host_state(Some(CapabilityPolicy {
        version: 1,
        applets: BTreeMap::from([("fixture.writer".into(), applet)]),
        host: None,
        host_environment: Default::default(),
        deny_privileged: false,
        allow_wrappers: Default::default(),
    }));

    assert!(
        authorize_host_path(&registry, &save_path, HostAccess::Write, directory.path()).is_err()
    );

    let mut policy = registry
        .host_state::<Option<CapabilityPolicy>>()
        .as_ref()
        .as_ref()
        .unwrap()
        .clone();
    policy.host = Some(host_grant(
        Vec::new(),
        vec![directory.path().join("applet-output")],
    ));
    registry.set_host_state(Some(policy));
    authorize_host_path(&registry, &save_path, HostAccess::Write, directory.path()).unwrap();
}

#[test]
fn save_preflight_requires_a_host_write_grant() {
    let directory = tempfile::tempdir().unwrap();
    let binary = std::env::current_exe().unwrap();
    let registry = registry_with_applet("fixture.writer", &binary);
    let mut applet = grant(&["process.exec", "filesystem.write"]);
    applet.filesystem.write.push(directory.path().to_path_buf());
    registry.set_host_state(Some(CapabilityPolicy {
        version: 1,
        applets: BTreeMap::from([("fixture.writer".into(), applet)]),
        host: None,
        host_environment: Default::default(),
        deny_privileged: false,
        allow_wrappers: Default::default(),
    }));
    let program =
        flow::parser::parse("run fixture.writer\nthen save to file \"result.txt\"\n").unwrap();

    assert!(flow::capabilities::preflight_plan(&program, &registry, directory.path()).is_err());

    let mut policy = registry
        .host_state::<Option<CapabilityPolicy>>()
        .as_ref()
        .as_ref()
        .unwrap()
        .clone();
    policy.host = Some(host_grant(Vec::new(), vec![directory.path().to_path_buf()]));
    registry.set_host_state(Some(policy));
    flow::capabilities::preflight_plan(&program, &registry, directory.path()).unwrap();
}

#[test]
fn host_read_and_use_folder_require_explicit_nested_host_grants() {
    use flow::{capabilities::ExecutionMode, parser, runtime};

    let directory = tempfile::tempdir().unwrap();
    let nested = directory.path().join("applet-area/nested");
    let sibling = directory.path().join("applet-area/sibling");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&sibling).unwrap();
    fs::write(nested.join("input.txt"), b"nested data").unwrap();
    fs::write(sibling.join("input.txt"), b"sibling data").unwrap();

    let binary = std::env::current_exe().unwrap();
    let registry = registry_with_applet("fixture.reader", &binary);
    let mut applet = grant(&["filesystem.read"]);
    // This broader applet grant must not authorize host-language actions.
    applet
        .filesystem
        .read
        .push(directory.path().join("applet-area"));
    let mut policy = CapabilityPolicy {
        version: 1,
        applets: BTreeMap::from([("fixture.reader".into(), applet)]),
        host: None,
        host_environment: Default::default(),
        deny_privileged: false,
        allow_wrappers: Default::default(),
    };
    registry.set_host_state(Some(ExecutionMode::Trusted));
    registry.set_host_state(Some(policy.clone()));
    let options = runtime::RunOptions::new(directory.path().to_path_buf());

    let direct_read = parser::parse("read \"applet-area/nested/input.txt\"\n").unwrap();
    assert!(runtime::run(&direct_read, &registry, &options).is_err());

    // A nested host grant permits that subtree while keeping its sibling denied.
    policy.host = Some(host_grant(vec![nested.clone()], Vec::new()));
    registry.set_host_state(Some(policy.clone()));
    let result = runtime::run(&direct_read, &registry, &options).unwrap();
    assert_eq!(result.output, b"nested data");

    let sibling_read = parser::parse("read \"applet-area/sibling/input.txt\"\n").unwrap();
    assert!(runtime::run(&sibling_read, &registry, &options).is_err());

    // UseFolder is itself a host read action and remains blocked without a
    // host grant, even though the applet has a broader filesystem read grant.
    policy.host = None;
    registry.set_host_state(Some(policy.clone()));
    let in_folder =
        parser::parse("use folder \"applet-area/nested\"\nread \"input.txt\"\n").unwrap();
    assert!(runtime::run(&in_folder, &registry, &options).is_err());

    policy.host = Some(host_grant(vec![nested], Vec::new()));
    registry.set_host_state(Some(policy));
    let result = runtime::run(&in_folder, &registry, &options).unwrap();
    assert_eq!(result.output, b"nested data");
}

#[test]
fn applet_file_reads_cannot_borrow_another_applets_path_grant() {
    use flow::capabilities::authorize_applet_host_path;

    let directory = tempfile::tempdir().unwrap();
    let binary = directory.path().join("fixture");
    fs::write(&binary, b"fixture").unwrap();
    let read_root = directory.path().join("credential");
    fs::create_dir(&read_root).unwrap();
    let registry = Registry::from_sources(&[
        serde_json::json!({
            "name":"allowed.reader", "binary":binary,
            "arguments":[], "contract":{"version":1,"capabilities":["filesystem.read"]}
        })
        .to_string(),
        serde_json::json!({
            "name":"remote.lookup", "binary":"@remote-applet",
            "arguments":[],
            "contract":{"version":1,"backend":"remote","remote":{"endpoint":"https://api.example.test"}}
        })
        .to_string(),
    ])
    .unwrap();
    let mut reader = grant(&["filesystem.read"]);
    reader.filesystem.read.push(read_root.clone());
    let mut remote = grant(&["network.connect"]);
    remote.network = true;
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([
            ("allowed.reader".into(), reader),
            ("remote.lookup".into(), remote),
        ]),
    }));

    authorize_applet_host_path(
        &registry,
        "allowed.reader",
        &read_root.join("token"),
        HostAccess::Read,
        directory.path(),
    )
    .unwrap();
    assert!(authorize_applet_host_path(
        &registry,
        "remote.lookup",
        &read_root.join("token"),
        HostAccess::Read,
        directory.path(),
    )
    .is_err());
}

#[test]
fn cache_directory_requires_applet_scoped_read_and_write_grants() {
    use flow::parser;

    let directory = tempfile::tempdir().unwrap();
    let binary = Path::new("/usr/bin/true");
    let registry = registry_with_applet("fixture.cache", binary);
    // This fixture checks host cache authorization without launching a child.
    registry.set_host_state(Some(flow::capabilities::ExecutionMode::Trusted));
    let program = parser::parse("run fixture.cache and cache\n").unwrap();
    let cache_directory = directory.path().join("cache");
    let mut allowed = grant(&["filesystem.read", "filesystem.write", "process.exec"]);
    allowed.filesystem.read.push(directory.path().to_path_buf());
    allowed
        .filesystem
        .write
        .push(directory.path().to_path_buf());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.cache".into(), allowed.clone())]),
    }));
    preflight_with_cache_directory(
        &program,
        &registry,
        directory.path(),
        Some(&cache_directory),
    )
    .unwrap();

    allowed.filesystem.write.clear();
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.cache".into(), allowed)]),
    }));
    assert!(preflight_with_cache_directory(
        &program,
        &registry,
        directory.path(),
        Some(&cache_directory),
    )
    .unwrap_err()
    .to_string()
    .contains("applet filesystem access"));
}

#[cfg(unix)]
#[test]
fn whole_program_preflight_denies_later_applet_before_prior_spawn() {
    use flow::{parser, runtime};
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, time::Duration};

    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("first-applet-ran");
    let executable = directory.path().join("owned-fixture.sh");
    fs::write(&executable, format!("#!/bin/sh\nprintf x > {:?}\n", marker)).unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();

    let sources = [
        serde_json::json!({
            "name":"allowed.first", "binary":executable,
            "arguments":[], "contract":{"version":1}
        })
        .to_string(),
        serde_json::json!({
            "name":"denied.later", "binary":"/usr/bin/true",
            "arguments":[], "contract":{"version":1}
        })
        .to_string(),
    ];
    let registry = Registry::from_sources(&sources).unwrap();
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("allowed.first".into(), grant(&["process.exec"]))]),
    }));
    let program = parser::parse("run allowed.first\nrun denied.later\n").unwrap();
    let options = runtime::RunOptions {
        max_output_bytes: 1024,
        command_timeout: Some(Duration::from_secs(2)),
        ..runtime::RunOptions::new(directory.path().into())
    };
    let error = runtime::run(&program, &registry, &options)
        .err()
        .expect("policy preflight should reject the later applet");
    assert!(error.to_string().contains("no allowlist entry"));
    assert!(
        !marker.exists(),
        "full preflight must precede earlier applet spawn"
    );
}

#[cfg(unix)]
#[test]
fn policy_preflight_rejects_later_unlisted_applet_before_prior_save() {
    use flow::{parser, runtime};
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, time::Duration};

    let directory = tempfile::tempdir().unwrap();
    let executable = directory.path().join("owned-fixture.sh");
    fs::write(&executable, b"#!/bin/sh\nprintf output\n").unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();
    let sources = [
        serde_json::json!({
            "name":"allowed.first", "binary":executable,
            "arguments":[], "contract":{"version":1,"capabilities":["filesystem.write"]}
        })
        .to_string(),
        serde_json::json!({
            "name":"denied.later", "binary":"/usr/bin/true",
            "arguments":[], "contract":{"version":1}
        })
        .to_string(),
    ];
    let registry = Registry::from_sources(&sources).unwrap();
    let mut allowed = grant(&["filesystem.write", "process.exec"]);
    allowed
        .filesystem
        .write
        .push(directory.path().to_path_buf());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("allowed.first".into(), allowed)]),
    }));
    let program =
        parser::parse("run allowed.first\nthen save to file \"early.txt\"\nrun denied.later\n")
            .unwrap();
    let options = runtime::RunOptions {
        max_output_bytes: 1024,
        command_timeout: Some(Duration::from_secs(2)),
        ..runtime::RunOptions::new(directory.path().into())
    };
    let error = runtime::run(&program, &registry, &options)
        .err()
        .expect("policy preflight should reject the later applet");
    assert!(error.to_string().contains("no allowlist entry"));
    assert!(!directory.path().join("early.txt").exists());
}

#[cfg(unix)]
#[test]
fn strict_policy_rejects_effectful_in_process_alias_before_prior_spawn() {
    use flow::{parser, runtime};
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, time::Duration};

    let directory = tempfile::tempdir().unwrap();
    let marker = directory.path().join("prior-applet-ran");
    let executable = directory.path().join("owned-fixture.sh");
    fs::write(&executable, format!("#!/bin/sh\nprintf x > {:?}\n", marker)).unwrap();
    let mut permissions = fs::metadata(&executable).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&executable, permissions).unwrap();
    let allowed = serde_json::json!({
        "name":"allowed.first",
        "binary":executable,
        "arguments":[],
        "contract":{"version":1}
    });
    // This custom manifest asks the host's in-process dispatcher to execute
    // its filesystem-reading branch while declaring an empty effect list.
    let spoofed = serde_json::json!({
        "name":"fs.read",
        "binary":"@execute-applet",
        "parameters":{"path":{"type":"path","required":true}},
        "arguments":[
            {"literal":"fs.read"},
            {"literal":"--args"},
            {"json_arguments":true}
        ],
        "contract":{
            "version":1,
            "backend":"in_process",
            "capabilities":["filesystem.read"],
            "effects":[]
        }
    });
    let registry = Registry::from_sources(&[allowed.to_string(), spoofed.to_string()]).unwrap();
    let mut read_grant = grant(&["filesystem.read"]);
    read_grant
        .filesystem
        .read
        .push(directory.path().to_path_buf());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([
            ("allowed.first".into(), grant(&["process.exec"])),
            ("fs.read".into(), read_grant),
        ]),
    }));
    let program = parser::parse("run allowed.first\nrun fs.read with path \"secret\"\n").unwrap();
    let options = runtime::RunOptions {
        max_output_bytes: 1024,
        command_timeout: Some(Duration::from_secs(2)),
        ..runtime::RunOptions::new(directory.path().into())
    };
    let error = runtime::run(&program, &registry, &options)
        .err()
        .expect("policy preflight should reject the in-process applet");
    assert!(error
        .to_string()
        .contains("effectful or unreviewed in-process"));
    assert!(
        !marker.exists(),
        "strict preflight must precede earlier process effects"
    );
}

#[test]
fn required_sandbox_rejects_in_process_backend_during_preflight() {
    use flow::{capabilities::preflight, parser};

    let directory = tempfile::tempdir().unwrap();
    let source = serde_json::json!({
        "name":"fixture.in_process",
        "binary":"@execute-applet",
        "arguments":[],
        "contract":{"version":1,"backend":"in_process"}
    });
    let registry = Registry::from_sources(&[source.to_string()]).unwrap();
    let mut required = grant(&[]);
    required.sandbox_required = true;
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.in_process".into(), required)]),
    }));
    let program = parser::parse("run fixture.in_process\n").unwrap();
    let error = preflight(&program, &registry, directory.path()).unwrap_err();
    assert!(error
        .to_string()
        .contains("cannot isolate in-process applet"));
}

#[test]
fn environment_entry_inputs_are_denied_by_the_shared_policy_preflight() {
    let registry = Registry::default();
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        applets: BTreeMap::new(),
        host_environment: BTreeSet::new(),
        deny_privileged: false,
        allow_wrappers: BTreeSet::new(),
    }));
    let declaration = flow::ast::InputDeclaration {
        name: "secret".into(),
        kind: flow::ast::InputType::Text,
        default: None,
        source: Some(flow::ast::InputSource::Environment(
            "LANE_X2_UNDECLARED".into(),
        )),
        public: false,
        line: 1,
        description: None,
        choices: None,
    };
    let error =
        flow::capabilities::authorize_environment_inputs(&[declaration], &registry).unwrap_err();
    assert!(error
        .to_string()
        .contains("denies reading environment variable"));
}

// ---------------------------------------------------------------------------
// Model loopback opt-in (#448): a program cannot grant
// `EXECUTE_MODEL_ALLOW_LOOPBACK` to itself under a capability policy unless
// the applet's `environment` grant lists it, and a filtered helper
// environment drops the opt-in otherwise. These tests never mutate the
// process environment: the self-grant uses a manifest overlay literal and
// the propagation case filters an explicit snapshot, so parallel tests in
// this binary cannot race on shared process state. `use environment`
// (manifest overlay) and `and environment` (command overlay) share
// `authorize_environment_override`, so the manifest path covers both.
// ---------------------------------------------------------------------------

/// A policy-mode program self-grant is denied without an `environment` grant
/// and allowed with one; the host process environment is never modified.
#[test]
fn model_loopback_self_grant_requires_policy_environment_grant() {
    const OPT_IN: &str = "EXECUTE_MODEL_ALLOW_LOOPBACK";
    const CANARY: &str = "self-grant-canary-448";

    let source = serde_json::json!({
        "name": "fixture.model",
        "binary": "/usr/bin/env",
        "arguments": [],
        "contract": {
            "version": 1,
            "environment": {
                "set": {OPT_IN: CANARY},
                "unset": []
            }
        }
    })
    .to_string();
    let before = std::env::var_os(OPT_IN);

    // Without the `environment` grant, the overlay self-grant is denied and
    // the value is redacted from the diagnostic.
    let denied_registry = Registry::from_sources(std::slice::from_ref(&source)).unwrap();
    denied_registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.model".into(), grant(&["process.exec"]))]),
    }));
    let error = effective_environment(&denied_registry, "fixture.model").unwrap_err();
    assert!(
        error
            .to_string()
            .contains("denies setting an environment variable"),
        "{error:?}"
    );
    assert!(
        !format!("{error:#}").contains(CANARY),
        "denial must not log the value"
    );

    // With the grant, the same overlay literal is delivered to the child
    // only; the host process environment is unchanged (read-only check).
    let mut allowed_grant = grant(&["process.exec"]);
    allowed_grant.environment.insert(OPT_IN.into());
    let allowed_registry = Registry::from_sources(&[source]).unwrap();
    allowed_registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.model".into(), allowed_grant)]),
    }));
    let environment = effective_environment(&allowed_registry, "fixture.model").unwrap();
    assert_eq!(
        environment
            .iter()
            .find(|(name, _)| name == std::ffi::OsStr::new(OPT_IN))
            .map(|(_, value)| value.as_os_str()),
        Some(std::ffi::OsStr::new(CANARY))
    );
    assert_eq!(std::env::var_os(OPT_IN), before, "host env must not change");
}

/// A filtered helper environment only forwards the loopback opt-in when it
/// is explicitly granted, by policy or by host allowlist selection.
#[test]
fn model_loopback_grant_is_stripped_by_filtered_helper_environments() {
    const OPT_IN: &str = "EXECUTE_MODEL_ALLOW_LOOPBACK";

    fn snapshot() -> Vec<(OsString, OsString)> {
        vec![
            (OsString::from(OPT_IN), OsString::from("1")),
            (OsString::from("PATH"), OsString::from("/usr/bin")),
        ]
    }

    fn registry_with(environment: BTreeSet<String>) -> Registry {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("fixture");
        std::fs::write(&binary, b"fixture").unwrap();
        // Only the grant contents matter to `filter_environment`; the
        // manifest binary path is never stat-ed on this path.
        let registry = registry_with_applet("fixture.model", &binary);
        let mut configured = grant(&[]);
        configured.environment = environment;
        registry.set_host_state(Some(CapabilityPolicy {
            host: None,
            version: 1,
            deny_privileged: false,
            allow_wrappers: Default::default(),
            host_environment: Default::default(),
            applets: BTreeMap::from([("fixture.model".into(), configured)]),
        }));
        registry
    }

    // Policy layer: without the grant the ambient opt-in is stripped even
    // though it is present in the captured snapshot (fail-closed).
    let stripped = registry_with(BTreeSet::from(["PATH".to_owned()]));
    let filtered = filter_environment(&stripped, "fixture.model", snapshot())
        .unwrap()
        .unwrap();
    assert_eq!(
        filtered,
        vec![(OsString::from("PATH"), OsString::from("/usr/bin"))]
    );
    assert!(
        authorize_environment_variable(&stripped, "fixture.model", OPT_IN).is_err(),
        "policy must deny reading the ungranted opt-in"
    );

    // With the grant the same snapshot forwards the opt-in.
    let forwarded = registry_with(BTreeSet::from(["PATH".to_owned(), OPT_IN.to_owned()]));
    let filtered = filter_environment(&forwarded, "fixture.model", snapshot())
        .unwrap()
        .unwrap();
    assert!(filtered.contains(&(OsString::from(OPT_IN), OsString::from("1"))));
    authorize_environment_variable(&forwarded, "fixture.model", OPT_IN).unwrap();

    // Host layer: `--clear-env` selects the empty allowlist (grant stripped),
    // while `--env-allow` must name the variable explicitly to forward it.
    assert!(parse_child_environment_allowlist("").unwrap().is_empty());
    assert!(!parse_child_environment_allowlist("PATH")
        .unwrap()
        .contains(&OPT_IN.to_owned()));
    assert!(
        parse_child_environment_allowlist("PATH,EXECUTE_MODEL_ALLOW_LOOPBACK")
            .unwrap()
            .contains(&OPT_IN.to_owned())
    );
}

#[test]
fn model_file_params_require_the_applets_own_read_grant() {
    use flow::ast::Value;
    use flow::capabilities::authorize_model_file_params;

    let directory = tempfile::tempdir().unwrap();
    let allowed = directory.path().join("models");
    fs::create_dir(&allowed).unwrap();
    let registry = Registry::from_sources(&[
        serde_json::json!({
            "name": "llm.ask", "binary": "@execute-model",
            "arguments": [], "contract": {"version": 1}
        })
        .to_string(),
        serde_json::json!({
            "name": "plain.tool", "binary": "/bin/true",
            "arguments": [], "contract": {"version": 1}
        })
        .to_string(),
    ])
    .unwrap();
    let mut model = grant(&["filesystem.read"]);
    model.filesystem.read.push(allowed.clone());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("llm.ask".into(), model)]),
    }));

    let manifest = registry.resolve("llm.ask").unwrap();
    let empty: BTreeMap<String, Value> = BTreeMap::new();
    let param = |name: &str, value: &str| {
        BTreeMap::from([(name.to_owned(), Value::String(value.to_owned()))])
    };
    let granted = |name: &str| allowed.join(name).to_string_lossy().into_owned();

    // A granted credential file is authorized; anything outside fails closed.
    authorize_model_file_params(
        &registry,
        manifest,
        &param("env_file", &granted("keys.env")),
        &empty,
        directory.path(),
        false,
    )
    .unwrap();
    assert!(authorize_model_file_params(
        &registry,
        manifest,
        &param(
            "env_file",
            directory.path().join("other.env").to_str().unwrap()
        ),
        &empty,
        directory.path(),
        false,
    )
    .is_err());

    // An empty path is the helper's own configuration error, not a grant.
    authorize_model_file_params(
        &registry,
        manifest,
        &param("env_file", ""),
        &empty,
        directory.path(),
        false,
    )
    .unwrap();

    // A named fixture means the helper never reads `env_file`: an ungranted
    // credential path alongside a granted fixture is allowed, while an
    // ungranted fixture is denied.
    let mut both = param("fixture", &granted("fixture.json"));
    both.insert(
        "env_file".to_owned(),
        Value::String(
            directory
                .path()
                .join("other.env")
                .to_string_lossy()
                .into_owned(),
        ),
    );
    authorize_model_file_params(&registry, manifest, &both, &empty, directory.path(), false)
        .unwrap();
    assert!(authorize_model_file_params(
        &registry,
        manifest,
        &param(
            "fixture",
            directory.path().join("other.json").to_str().unwrap()
        ),
        &empty,
        directory.path(),
        false,
    )
    .is_err());

    // References resolve through bindings; unbound names are skipped for the
    // later argument resolution to reject, and non-text values fail closed.
    let reference = BTreeMap::from([("env_file".to_owned(), Value::Reference("keys".into()))]);
    let bound = BTreeMap::from([("keys".to_owned(), Value::String(granted("bound.env")))]);
    authorize_model_file_params(
        &registry,
        manifest,
        &reference,
        &bound,
        directory.path(),
        false,
    )
    .unwrap();
    authorize_model_file_params(
        &registry,
        manifest,
        &reference,
        &empty,
        directory.path(),
        false,
    )
    .unwrap();
    let mistyped = BTreeMap::from([("keys".to_owned(), Value::Bool(true))]);
    assert!(authorize_model_file_params(
        &registry,
        manifest,
        &reference,
        &mistyped,
        directory.path(),
        false
    )
    .is_err());

    // Relative literals defer only when the working directory is not yet
    // known; the spawn path re-checks them against the effective directory.
    let relative = param("env_file", "models/relative.env");
    authorize_model_file_params(
        &registry,
        manifest,
        &relative,
        &empty,
        directory.path(),
        true,
    )
    .unwrap();
    authorize_model_file_params(
        &registry,
        manifest,
        &relative,
        &empty,
        directory.path(),
        false,
    )
    .unwrap();
    let outside = param("env_file", "other.env");
    authorize_model_file_params(
        &registry,
        manifest,
        &outside,
        &empty,
        directory.path(),
        true,
    )
    .unwrap();
    assert!(authorize_model_file_params(
        &registry,
        manifest,
        &outside,
        &empty,
        directory.path(),
        false
    )
    .is_err());

    // Applets that do not use the model helper are unaffected, even with no
    // grant and no policy configured.
    let plain = registry.resolve("plain.tool").unwrap();
    authorize_model_file_params(
        &registry,
        plain,
        &param("env_file", "/elsewhere.env"),
        &empty,
        directory.path(),
        false,
    )
    .unwrap();
    let unconfigured = Registry::from_sources(&[serde_json::json!({
        "name": "llm.ask", "binary": "@execute-model",
        "arguments": [], "contract": {"version": 1}
    })
    .to_string()])
    .unwrap();
    authorize_model_file_params(
        &unconfigured,
        unconfigured.resolve("llm.ask").unwrap(),
        &param("env_file", "/elsewhere.env"),
        &empty,
        directory.path(),
        false,
    )
    .unwrap();
}

#[test]
fn model_path_params_are_generic_not_name_locked() {
    use flow::ast::Value;
    use flow::capabilities::authorize_model_file_params;

    let directory = tempfile::tempdir().unwrap();
    let allowed = directory.path().join("models");
    fs::create_dir(&allowed).unwrap();
    let registry = Registry::from_sources(&[
        serde_json::json!({
            "name": "llm.ask", "binary": "@execute-model",
            "parameters": {
                "fixture": {"type": "path"},
                "env_file": {"type": "path"}
            },
            "arguments": [], "contract": {"version": 1}
        })
        .to_string(),
        serde_json::json!({
            "name": "renamed.helper", "binary": "@execute-model-v2",
            "parameters": {"data_file": {"type": "path"}},
            "arguments": [], "contract": {"version": 1}
        })
        .to_string(),
        serde_json::json!({
            "name": "legacy.stringy", "binary": "@execute-model",
            "parameters": {"fixture": {"type": "string"}},
            "arguments": [], "contract": {"version": 1}
        })
        .to_string(),
        serde_json::json!({
            "name": "plain.stringy", "binary": "/bin/true",
            "parameters": {"data": {"type": "string"}},
            "arguments": [], "contract": {"version": 1}
        })
        .to_string(),
    ])
    .unwrap();
    let mut model = grant(&["filesystem.read"]);
    model.filesystem.read.push(allowed.clone());
    let mut renamed = grant(&["filesystem.read"]);
    renamed.filesystem.read.push(allowed.clone());
    let mut legacy = grant(&["filesystem.read"]);
    legacy.filesystem.read.push(allowed.clone());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([
            ("llm.ask".into(), model),
            ("renamed.helper".into(), renamed),
            ("legacy.stringy".into(), legacy),
        ]),
    }));

    let empty: BTreeMap<String, Value> = BTreeMap::new();
    let granted = allowed.join("ok.json").to_string_lossy().into_owned();
    let outside = directory
        .path()
        .join("evil.json")
        .to_string_lossy()
        .into_owned();

    // Bundled path-typed params are covered without relying on the binary name.
    let manifest = registry.resolve("llm.ask").unwrap();
    authorize_model_file_params(
        &registry,
        manifest,
        &BTreeMap::from([("fixture".to_owned(), Value::String(granted.clone()))]),
        &empty,
        directory.path(),
        false,
    )
    .unwrap();
    assert!(authorize_model_file_params(
        &registry,
        manifest,
        &BTreeMap::from([("fixture".to_owned(), Value::String(outside.clone()))]),
        &empty,
        directory.path(),
        false,
    )
    .is_err());

    // A renamed helper binary with a path-typed file param stays covered.
    let renamed_manifest = registry.resolve("renamed.helper").unwrap();
    authorize_model_file_params(
        &registry,
        renamed_manifest,
        &BTreeMap::from([("data_file".to_owned(), Value::String(granted.clone()))]),
        &empty,
        directory.path(),
        false,
    )
    .unwrap();
    assert!(authorize_model_file_params(
        &registry,
        renamed_manifest,
        &BTreeMap::from([("data_file".to_owned(), Value::String(outside.clone()))]),
        &empty,
        directory.path(),
        false,
    )
    .is_err());

    // Legacy string-typed fixture on `@execute-model` is still denied outside
    // the grant via the name fallback.
    let legacy_manifest = registry.resolve("legacy.stringy").unwrap();
    assert!(authorize_model_file_params(
        &registry,
        legacy_manifest,
        &BTreeMap::from([("fixture".to_owned(), Value::String(outside.clone()))]),
        &empty,
        directory.path(),
        false,
    )
    .is_err());

    // A string-typed file param on another binary escapes: it is not a path
    // param and has no legacy name. File inputs must be declared as `path`.
    let plain = registry.resolve("plain.stringy").unwrap();
    authorize_model_file_params(
        &registry,
        plain,
        &BTreeMap::from([("data".to_owned(), Value::String(outside))]),
        &empty,
        directory.path(),
        false,
    )
    .unwrap();
}

#[test]
fn model_preflight_refuses_fixture_outside_grant_and_final_link_fails_closed() {
    use flow::ast::Value;
    use flow::capabilities::{authorize_model_file_params, preflight};
    use flow::parser;

    let directory = tempfile::tempdir().unwrap();
    let allowed = directory.path().join("models");
    fs::create_dir(&allowed).unwrap();
    let fixture = allowed.join("good.json");
    fs::write(&fixture, "{}").unwrap();

    let registry = Registry::from_sources(&[serde_json::json!({
        "name": "llm.ask", "binary": "@execute-model",
        "parameters": {
            "model": {"type": "string"},
            "instruction": {"type": "string"},
            "fixture": {"type": "path"},
            "env_file": {"type": "path"}
        },
        "arguments": [], "contract": {"version": 1}
    })
    .to_string()])
    .unwrap();
    let mut model = grant(&["filesystem.read", "process.exec"]);
    model.filesystem.read.push(allowed.clone());
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("llm.ask".into(), model)]),
    }));

    // End-to-end preflight refusal for a fixture outside the grant.
    let outside = directory
        .path()
        .join("outside.json")
        .to_string_lossy()
        .into_owned();
    let program = parser::parse(&format!(
        "run llm.ask with model \"m\" and instruction \"i\" and fixture \"{outside}\"\n"
    ))
    .unwrap();
    let error = preflight(&program, &registry, directory.path())
        .expect_err("preflight should refuse the outside fixture");
    assert!(
        error
            .to_string()
            .contains("denies applet filesystem access"),
        "unexpected error: {error:#}"
    );

    // A granted fixture passes preflight.
    let inside = fixture.to_string_lossy().into_owned();
    let program = parser::parse(&format!(
        "run llm.ask with model \"m\" and instruction \"i\" and fixture \"{inside}\"\n"
    ))
    .unwrap();
    preflight(&program, &registry, directory.path()).unwrap();

    // Dynamic binding: a Reference resolves against bindings at spawn time.
    // The spawn path re-checks with defer_relative=false, so an outside
    // target is denied even when preflight deferred the relative literal.
    let manifest = registry.resolve("llm.ask").unwrap();
    let empty: BTreeMap<String, Value> = BTreeMap::new();
    let reference = BTreeMap::from([("fixture".to_owned(), Value::Reference("next".into()))]);
    let bound_outside = BTreeMap::from([(
        "next".to_owned(),
        Value::String(
            directory
                .path()
                .join("late.json")
                .to_string_lossy()
                .into_owned(),
        ),
    )]);
    assert!(authorize_model_file_params(
        &registry,
        manifest,
        &reference,
        &bound_outside,
        directory.path(),
        false,
    )
    .is_err());
    // Unbound is skipped here and rejected later by argument resolution.
    authorize_model_file_params(
        &registry,
        manifest,
        &reference,
        &empty,
        directory.path(),
        false,
    )
    .unwrap();

    // Final-component symlink: the parent canonicalizes to the granted
    // target and allows it, while the child open pins the final component
    // and refuses the link. Ancestor swaps after the check are the
    // documented unfixable window: this test pins only the final link.
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let link = directory.path().join("link.json");
        symlink(&fixture, &link).unwrap();
        authorize_model_file_params(
            &registry,
            manifest,
            &BTreeMap::from([(
                "fixture".to_owned(),
                Value::String(link.to_string_lossy().into_owned()),
            )]),
            &empty,
            directory.path(),
            false,
        )
        .unwrap();
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink(),
            "test setup did not create a final-component link"
        );
    }
}
