//! Execution adapter for validated finite `.program` applet modules.
//!
//! The runtime dispatch layer supplies a preloaded module so this adapter does
//! no path lookup after the caller has begun producing effects. Calls made by
//! the module use the caller's same registry, policy, cancellation signal and
//! runtime session.

use super::{
    configured_inputs, run_finite_inner, CallSite, FiniteRun, GroupOutput, Input, LaunchEnv,
    Program, RunOptions,
};
use crate::ast::{OutputDeclaration, OutputType};
use crate::modules::LoadedProgramSource;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::BTreeMap;

/// Execute one prevalidated module export as a private child scope.
pub(super) fn execute(
    loaded: &LoadedProgramSource,
    site: CallSite<'_>,
    input: Input,
    env: LaunchEnv<'_>,
    launched: &mut usize,
) -> Result<GroupOutput> {
    let CallSite {
        call,
        manifest,
        line,
    } = site;
    let LaunchEnv {
        bindings,
        cwd,
        options,
        signals,
        registry,
    } = env;
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
        output: None,
        trigger: None,
    };
    let child_options = RunOptions {
        inputs: resolved_inputs,
        cache_directory: options.cache_directory.clone(),
        trace: options.trace,
        max_output_bytes: options.max_output_bytes,
        max_loop_iterations: options.max_loop_iterations,
        loop_budget_scope: options.loop_budget_scope,
        command_timeout: options.command_timeout,
        total_timeout: options.total_timeout,
        ..RunOptions::new(cwd.to_path_buf())
    };
    let configured =
        configured_inputs(&child_program, registry, &child_options).with_context(|| {
            format!(
                "line {line}: preparing program module {:?}",
                manifest.name_for(call)
            )
        })?;
    let process_count_before = signals.process_launch_count();
    let module_signals = signals.module_capture_scope();
    let child_signals = module_signals.as_ref().unwrap_or(signals);
    let child = run_finite_inner(
        FiniteRun {
            program: &child_program,
            registry,
            options: &configured,
            signals: child_signals,
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
    check_declared_output(loaded.program.output.as_ref(), &child.output)
        .with_context(|| format!("line {line}: validating program module output"))?;
    // Decode here so malformed module output fails at the module boundary. The
    // ordinary applet result path still uses the declared status/outcome rules.
    let status = manifest
        .success_status()
        .ok_or_else(|| anyhow!("program module applet must declare a success outcome"))?;
    #[cfg(not(target_arch = "wasm32"))]
    let typed_json = if manifest.has_json_encoded_output_schema() {
        let value = crate::contracts::parse_json_strict(&child.output)
            .with_context(|| format!("line {line}: validating program module output"))?;
        contract
            .decode_json_output_value(value.clone(), &child.output, status)
            .with_context(|| format!("line {line}: validating program module output"))?;
        Some(value)
    } else {
        contract
            .decode_output(&child.output, status)
            .with_context(|| format!("line {line}: validating program module output"))?;
        None
    };
    #[cfg(target_arch = "wasm32")]
    let typed_json = {
        contract
            .decode_output(&child.output, status)
            .with_context(|| format!("line {line}: validating program module output"))?;
        None
    };
    Ok(GroupOutput {
        bytes: child.output,
        typed_json,
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

/// Check a program's final value against its `produce` declaration. A program
/// without a declaration is accepted unchanged.
fn check_declared_output(declared: Option<&OutputDeclaration>, output: &[u8]) -> Result<()> {
    let Some(declaration) = declared else {
        return Ok(());
    };
    if output_matches_type(declaration.kind, output) {
        return Ok(());
    }
    bail!(
        "output_type_mismatch: program declares produce {} but its final value is {}",
        declaration.kind.label(),
        actual_output_label(output)
    )
}

fn output_matches_type(kind: OutputType, output: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(output) else {
        return false;
    };
    let trimmed = text.trim();
    match kind {
        OutputType::Text => true,
        OutputType::Integer => trimmed.parse::<i64>().is_ok(),
        OutputType::Boolean => matches!(trimmed, "true" | "false"),
        OutputType::TextList => serde_json::from_str::<Vec<String>>(trimmed).is_ok(),
        OutputType::Json => serde_json::from_str::<serde_json::Value>(trimmed).is_ok(),
        OutputType::None => trimmed.is_empty(),
    }
}

/// The closest declarable type for a final value, used in mismatch messages.
fn actual_output_label(output: &[u8]) -> &'static str {
    let Ok(text) = std::str::from_utf8(output) else {
        return "bytes";
    };
    let trimmed = text.trim();
    if trimmed.is_empty() {
        "none"
    } else if trimmed.parse::<i64>().is_ok() {
        OutputType::Integer.label()
    } else if matches!(trimmed, "true" | "false") {
        OutputType::Boolean.label()
    } else {
        OutputType::Text.label()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declared(kind: OutputType) -> OutputDeclaration {
        OutputDeclaration {
            kind,
            description: None,
            line: 1,
        }
    }

    #[test]
    fn declared_integer_rejects_text_and_names_both_types() {
        let error = check_declared_output(Some(&declared(OutputType::Integer)), b"hello\n")
            .expect_err("text is not a declared integer");
        let message = format!("{error:#}");
        assert!(message.contains("output_type_mismatch"), "{message}");
        assert!(message.contains("produce integer"), "{message}");
        assert!(message.contains("final value is text"), "{message}");
    }

    #[test]
    fn declared_types_accept_matching_final_values() {
        assert!(check_declared_output(Some(&declared(OutputType::Integer)), b"42\n").is_ok());
        assert!(check_declared_output(Some(&declared(OutputType::Boolean)), b"true\n").is_ok());
        assert!(check_declared_output(Some(&declared(OutputType::Text)), b"any text").is_ok());
        assert!(check_declared_output(
            Some(&declared(OutputType::TextList)),
            b"[\"ok\", \"degraded\"]"
        )
        .is_ok());
        assert!(check_declared_output(Some(&declared(OutputType::Json)), b"{\"a\":1}").is_ok());
        assert!(check_declared_output(Some(&declared(OutputType::None)), b"\n").is_ok());
    }

    #[test]
    fn undeclared_output_is_accepted_and_invalid_utf8_is_refused() {
        assert!(check_declared_output(None, b"hello").is_ok());
        let error = check_declared_output(Some(&declared(OutputType::Text)), &[0xff, 0xfe])
            .expect_err("invalid UTF-8 is not text");
        assert!(format!("{error:#}").contains("final value is bytes"));
    }
}
