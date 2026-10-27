//! `execute repl [--manifest-dir DIR]`: read statements from standard input and run
//! them against a session. The session logic lives in the library crate.

use anyhow::{bail, Context, Result};
use flow::repl::{self, Session};
use std::env;
use std::io::{self, IsTerminal};
use std::path::PathBuf;

pub fn repl(arguments: &[String]) -> Result<()> {
    let mut policy = crate::execution_policy_options::ExecutionPolicyOptions::default();
    let mut manifest_dir: Option<PathBuf> = None;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].as_str() {
            "--manifest-dir" => {
                index += 1;
                manifest_dir = Some(PathBuf::from(
                    arguments
                        .get(index)
                        .context("--manifest-dir requires a directory")?,
                ));
            }
            _ if policy.parse_option(arguments, &mut index)? => {}
            other => bail!("unknown repl option or argument {other:?}"),
        }
        index += 1;
    }
    let (capability, data_flow) = policy.load()?;
    let registry = crate::load_workflow_registry(
        manifest_dir,
        None,
        flow::manifest::ManifestCheck::Lazy,
        capability.as_ref(),
        data_flow.as_ref(),
    )?;
    if policy.mode.is_some() {
        registry.set_host_state(policy.mode);
    }
    let working_directory = env::current_dir().context("could not determine current directory")?;
    let mut session = Session::new(&registry, working_directory);
    let interactive = io::stdin().is_terminal();
    repl::run(
        &mut session,
        io::stdin().lock(),
        io::stdout().lock(),
        interactive,
    )?;
    Ok(())
}
