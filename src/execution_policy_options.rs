//! Shared policy options for program-running tools.
use anyhow::{bail, Context, Result};
use std::path::PathBuf;
#[cfg(feature = "tooling")]
use std::process::Command;

#[derive(Default)]
pub struct ExecutionPolicyOptions {
    pub capability: Option<PathBuf>,
    pub data_flow: Option<PathBuf>,
    pub mode: Option<flow::capabilities::ExecutionMode>,
}

impl ExecutionPolicyOptions {
    pub fn parse_option(&mut self, args: &[String], index: &mut usize) -> Result<bool> {
        let flag = args[*index].as_str();
        if flag == "--allow" {
            bail!("--allow is only supported by explain and cannot be used for execution");
        }
        if !matches!(
            flag,
            "--capability-policy" | "--data-flow-policy" | "--execution-mode"
        ) {
            return Ok(false);
        }
        *index += 1;
        let value = args
            .get(*index)
            .with_context(|| format!("{flag} requires a value"))?;
        if value.starts_with("--") {
            bail!("{flag} requires a value");
        }
        match flag {
            "--execution-mode" => {
                if self.mode.is_some() {
                    bail!("--execution-mode may be specified only once");
                }
                self.mode = Some(match value.as_str() {
                    "trusted" => flow::capabilities::ExecutionMode::Trusted,
                    "untrusted" => flow::capabilities::ExecutionMode::Untrusted,
                    _ => bail!("--execution-mode must be trusted or untrusted"),
                });
            }
            _ => {
                let slot = if flag == "--capability-policy" {
                    &mut self.capability
                } else {
                    &mut self.data_flow
                };
                if slot.is_some() {
                    bail!("{flag} may be specified only once");
                }
                *slot = Some(
                    std::fs::canonicalize(value)
                        .with_context(|| format!("resolving {flag} {value}"))?,
                );
            }
        }
        Ok(true)
    }
    pub fn load(
        &self,
    ) -> Result<(
        Option<flow::capabilities::CapabilityPolicy>,
        Option<flow::security::DataFlowPolicy>,
    )> {
        let capability = self
            .capability
            .as_deref()
            .map(crate::load_capability_policy)
            .transpose()?;
        if self.mode == Some(flow::capabilities::ExecutionMode::Untrusted) && capability.is_none() {
            bail!("untrusted execution requires a capability policy");
        }
        let data_flow = self
            .data_flow
            .as_deref()
            .map(crate::load_data_flow_policy)
            .transpose()?;
        Ok((capability, data_flow))
    }
    #[cfg(feature = "tooling")]
    pub fn forward(&self, command: &mut Command) {
        if let Some(path) = &self.capability {
            command.arg("--capability-policy").arg(path);
        }
        if let Some(path) = &self.data_flow {
            command.arg("--data-flow-policy").arg(path);
        }
        if let Some(mode) = self.mode {
            command.arg("--execution-mode").arg(mode.as_str());
        }
    }
}
