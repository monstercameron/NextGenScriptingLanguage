use flow::{
    capabilities::{
        authorize_environment_variable, authorize_host_path, effective_environment,
        filter_environment, policy_fingerprint, preflight_with_cache_directory, AppletGrant,
        CapabilityPolicy, ContractView, FilesystemGrant,
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        applets: BTreeMap::from([("fixture.command".into(), grant(&[]))]),
    }));
    let error = preflight(&program, &registry, directory.path()).unwrap_err();
    assert!(error
        .to_string()
        .contains("backend requirement \"process.exec\""));

    registry.set_host_state(Some(CapabilityPolicy {
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        applets: BTreeMap::from([("fixture.command".into(), grant(&["process.exec"]))]),
    }));
    preflight(&program, &registry, directory.path()).unwrap();
}

#[test]
fn strict_policy_allows_shipped_pure_json_transformations() {
    let manifest = include_str!("../manifests/core.json_parse.json");
    let registry = Registry::from_sources(&[manifest]).unwrap();
    registry.set_host_state(Some(CapabilityPolicy {
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
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
    let command = flow::capabilities::command_for(
        Path::new("unused-executable"),
        ["argument with spaces", "semi;colon"],
        &registry,
        "fixture.env",
        &flow::working_dir::WorkingDirectory::plain("."),
    )
    .unwrap();
    assert_eq!(
        command
            .get_args()
            .map(|argument| argument.to_string_lossy().into_owned())
            .collect::<Vec<_>>(),
        ["argument with spaces", "semi;colon"]
    );

    let mut denied = serde_json::json!(source);
    denied["name"] = serde_json::json!("fixture.denied");
    denied["contract"]["environment"]["set"] =
        serde_json::json!({"EXECUTE_TEST_UNGRANTED_ENV":"not-logged"});
    let denied_registry = Registry::from_sources(&[denied.to_string()]).unwrap();
    denied_registry.set_host_state(Some(CapabilityPolicy {
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        applets: BTreeMap::from([("fixture.read".into(), configured)]),
    }));

    authorize_host_path(&registry, &allowed_path, false, directory.path()).unwrap();
    authorize_host_path(&registry, &denied_path, false, directory.path()).unwrap_err();
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        applets: BTreeMap::from([("fixture.read".into(), configured)]),
    }));
    let cwd = directory.path();
    authorize_host_path(&registry, &read_root.join("source.txt"), false, cwd).unwrap();
    authorize_host_path(&registry, &write_root.join("result.txt"), true, cwd).unwrap();
    assert!(authorize_host_path(&registry, &read_root, true, cwd).is_err());
    assert!(authorize_host_path(&registry, &write_root, false, cwd).is_err());
    assert!(authorize_host_path(
        &registry,
        &directory.path().join("input/../../outside.txt"),
        false,
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
        assert!(
            authorize_host_path(&registry, &read_root.join("escape/secret.txt"), false, cwd)
                .is_err()
        );
    }
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        applets: BTreeMap::from([
            ("allowed.reader".into(), reader),
            ("remote.lookup".into(), remote),
        ]),
    }));

    authorize_applet_host_path(
        &registry,
        "allowed.reader",
        &read_root.join("token"),
        false,
        directory.path(),
    )
    .unwrap();
    assert!(authorize_applet_host_path(
        &registry,
        "remote.lookup",
        &read_root.join("token"),
        false,
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
    let program = parser::parse("run fixture.cache and cache\n").unwrap();
    let cache_directory = directory.path().join("cache");
    let mut allowed = grant(&["filesystem.read", "filesystem.write", "process.exec"]);
    allowed.filesystem.read.push(directory.path().to_path_buf());
    allowed
        .filesystem
        .write
        .push(directory.path().to_path_buf());
    registry.set_host_state(Some(CapabilityPolicy {
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        applets: BTreeMap::from([("allowed.first".into(), grant(&["process.exec"]))]),
    }));
    let program = parser::parse("run allowed.first\nrun denied.later\n").unwrap();
    let options = runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: directory.path().into(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024,
        command_timeout: Some(Duration::from_secs(2)),
        total_timeout: None,
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        applets: BTreeMap::from([("allowed.first".into(), allowed)]),
    }));
    let program =
        parser::parse("run allowed.first\nthen save to file \"early.txt\"\nrun denied.later\n")
            .unwrap();
    let options = runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: directory.path().into(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024,
        command_timeout: Some(Duration::from_secs(2)),
        total_timeout: None,
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        applets: BTreeMap::from([
            ("allowed.first".into(), grant(&["process.exec"])),
            ("fs.read".into(), read_grant),
        ]),
    }));
    let program = parser::parse("run allowed.first\nrun fs.read with path \"secret\"\n").unwrap();
    let options = runtime::RunOptions {
        inputs: BTreeMap::new(),
        working_directory: directory.path().into(),
        cache_directory: None,
        trace: false,
        max_output_bytes: 1024,
        command_timeout: Some(Duration::from_secs(2)),
        total_timeout: None,
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
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        applets: BTreeMap::from([("fixture.in_process".into(), required)]),
    }));
    let program = parser::parse("run fixture.in_process\n").unwrap();
    let error = preflight(&program, &registry, directory.path()).unwrap_err();
    assert!(error
        .to_string()
        .contains("cannot isolate in-process applet"));
}
