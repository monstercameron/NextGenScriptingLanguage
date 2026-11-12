//! Issue #270: repeated `command_for` setup must keep the same argv and child
//! environment, including policy filtering and declared secret variables.

use flow::{
    capabilities::{
        command_for, effective_environment, AppletGrant, CapabilityPolicy, ExecutionMode,
        FilesystemGrant,
    },
    manifest::Registry,
    security::DataFlowPolicy,
    working_dir::WorkingDirectory,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::path::Path;

fn sorted_pairs(mut pairs: Vec<(OsString, OsString)>) -> Vec<(OsString, OsString)> {
    pairs.sort();
    pairs
}

#[test]
fn repeated_policy_filtered_setup_keeps_argv_and_secret_environment() {
    let registry = Registry::from_sources(&[serde_json::json!({
        "name": "fixture.secret",
        "binary": "fixture",
        "arguments": [],
        "contract": {
            "version": 1,
            "capabilities": ["process.exec"],
            "environment": {
                "set": {
                    "API_TOKEN": "sentinel-token-value",
                    "PUBLIC_FLAG": "visible"
                }
            }
        }
    })
    .to_string()])
    .unwrap();
    registry.set_host_state(
        DataFlowPolicy::from_json(
            br#"{"version":1,"secrets":{"environment":{"fixture.secret":["API_TOKEN"]}}}"#,
        )
        .unwrap(),
    );
    let grant = AppletGrant {
        capabilities: ["process.exec".to_owned()].into_iter().collect(),
        binary_sha256: None,
        environment: ["API_TOKEN", "PUBLIC_FLAG"]
            .into_iter()
            .map(str::to_owned)
            .collect(),
        filesystem: FilesystemGrant::default(),
        network: false,
        network_origins: Default::default(),
        sandbox_required: false,
        resource_limits: None,
    };
    registry.set_host_state(Some(CapabilityPolicy {
        host: None,
        version: 1,
        deny_privileged: false,
        allow_wrappers: Default::default(),
        host_environment: Default::default(),
        applets: BTreeMap::from([("fixture.secret".into(), grant)]),
    }));
    registry.set_host_state(Some(ExecutionMode::Trusted));

    // The ambient environment is filtered out by the grant, so the resolved
    // snapshot holds exactly the granted manifest variables, the declared
    // secret included.
    let expected = sorted_pairs(effective_environment(&registry, "fixture.secret").unwrap());
    let names: Vec<String> = expected
        .iter()
        .map(|(name, _)| name.to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["API_TOKEN", "PUBLIC_FLAG"]);

    let cwd = WorkingDirectory::plain(".");
    for _ in 0..3 {
        let command = command_for(
            Path::new("fixture"),
            ["first", "", "two words"],
            &registry,
            "fixture.secret",
            &cwd,
        )
        .unwrap();
        assert_eq!(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["first", "", "two words"]
        );
        let actual = sorted_pairs(
            command
                .get_envs()
                .map(|(name, value)| {
                    (
                        name.to_os_string(),
                        value
                            .expect("filtered launch sets every variable")
                            .to_os_string(),
                    )
                })
                .collect(),
        );
        assert_eq!(actual, expected);
    }
}

#[test]
fn repeated_inherited_setup_carries_no_environment_changes() {
    let registry = Registry::from_sources(&[serde_json::json!({
        "name": "fixture.plain",
        "binary": "fixture",
        "arguments": [],
        "contract": {"version": 1, "capabilities": ["process.exec"]}
    })
    .to_string()])
    .unwrap();
    let cwd = WorkingDirectory::plain(".");
    for _ in 0..3 {
        let command = command_for(
            Path::new("fixture"),
            ["only"],
            &registry,
            "fixture.plain",
            &cwd,
        )
        .unwrap();
        assert_eq!(command.get_envs().count(), 0);
        assert_eq!(
            command
                .get_args()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect::<Vec<_>>(),
            ["only"]
        );
    }
}
