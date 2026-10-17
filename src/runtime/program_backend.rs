//! Execution adapter for validated finite `.program` applet modules.
//!
//! The runtime dispatch layer supplies a preloaded module so this adapter does
//! no path lookup after the caller has begun producing effects. Calls made by
//! the module use the caller's same registry, policy, cancellation signal and
//! runtime session.

use super::{
    configured_inputs, run_finite_inner, FiniteRun, GroupOutput, Input, Manifest, Program,
    Registry, RunOptions, Signals,
};
use crate::ast::{Invocation, Value};
use crate::modules::LoadedProgramSource;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;

/// Execute one prevalidated module export as a private child scope.
#[allow(clippy::too_many_arguments)]
pub(super) fn execute(
    loaded: &LoadedProgramSource,
    call: &Invocation,
    manifest: &Manifest,
    input: Input,
    bindings: &BTreeMap<String, Value>,
    cwd: &Path,
    options: &RunOptions,
    signals: &Signals,
    registry: &Registry,
    launched: &mut usize,
    line: usize,
) -> Result<GroupOutput> {
    if loaded.is_program_service() {
        bail!(
            "persistent .program services can run only as graph service nodes, not as finite applet calls"
        );
    }
    if !crate::modules::supports_program_module_finite_interface(loaded.reference.interface_version)
    {
        bail!(
            "finite applet calls do not support program module interface version {}",
            loaded.reference.interface_version
        );
    }
    let definition = loaded
        .finite_definition()
        .ok_or_else(|| anyhow!("finite program module entry is missing"))?;
    let contract = manifest
        .execution_contract()
        .ok_or_else(|| anyhow!("program module applet requires a versioned execution contract"))?;
    loaded.validate_applet_manifest(manifest)?;
    let resolved_inputs = call
        .parameters
        .iter()
        .map(|(name, value)| {
            Ok((
                name.clone(),
                super::resolve(value, bindings)
                    .with_context(|| format!("resolving program module input {name:?}"))?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>>>()?;
    let child_program = Program {
        definitions: loaded.program.definitions.clone(),
        inputs: definition.inputs.clone(),
        steps: definition.steps.clone(),
        trigger: None,
    };
    let child_options = RunOptions {
        inputs: resolved_inputs,
        working_directory: cwd.to_path_buf(),
        cache_directory: options.cache_directory.clone(),
        trace: options.trace,
        max_output_bytes: options.max_output_bytes,
        command_timeout: options.command_timeout,
        total_timeout: options.total_timeout,
    };
    let configured =
        configured_inputs(&child_program, registry, &child_options).with_context(|| {
            format!(
                "line {line}: preparing program module {:?}",
                manifest.name_for(call)
            )
        })?;
    let process_count_before = signals.process_launch_count();
    let child = run_finite_inner(
        FiniteRun {
            program: &child_program,
            registry,
            options: &configured,
            signals,
        },
        input,
        None,
        None,
    );
    let child = match child {
        Ok(child) => child,
        Err(error) => {
            // Failed finite runs do not return their RunResult, but successful
            // process spawns remain part of the caller's process accounting.
            // The counter is scoped to this synchronous module run; task
            // children use independent counters and are aggregated at join.
            let spawned = signals
                .process_launch_count()
                .saturating_sub(process_count_before);
            *launched = launched
                .checked_add(spawned)
                .ok_or_else(|| anyhow!("launched process count overflow"))?;
            return Err(error).with_context(|| {
                format!(
                    "line {line}: executing program module {:?} entry {:?}",
                    manifest.name_for(call),
                    loaded.reference.entry
                )
            });
        }
    };
    *launched = launched
        .checked_add(child.launched_processes)
        .ok_or_else(|| anyhow!("launched process count overflow"))?;
    if child.output.len() > options.max_output_bytes {
        bail!(
            "program module output exceeds maximum of {} bytes",
            options.max_output_bytes
        );
    }
    // Decode here so malformed module output fails at the module boundary. The
    // ordinary applet result path still uses the declared status/outcome rules.
    let status = manifest
        .success_status()
        .ok_or_else(|| anyhow!("program module applet must declare a success outcome"))?;
    contract
        .decode_output(&child.output, status)
        .with_context(|| format!("line {line}: validating program module output"))?;
    Ok(GroupOutput {
        bytes: child.output,
        typed_json: None,
        status: Some(status),
        diagnostic: Vec::new(),
        stdout_captured: true,
        stages: vec![super::execution_result::StageResult::exited(
            line,
            manifest.name_for(call),
            status,
            manifest.outcome_for(status),
            None,
        )],
    })
}
