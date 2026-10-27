//! The parsed command line: one value per option, filled by the parser.

use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug)]
pub(crate) struct Args {
    pub(crate) inputs: BTreeMap<String, String>,
    /// `--answers FILE`: a JSON object of answers for declared prompts and inputs.
    pub(crate) answers: Option<PathBuf>,
    pub(crate) program_arguments: Vec<String>,
    /// True when program arguments followed an explicit `--` separator, so a
    /// leading `--help` is data for the program rather than a help request.
    pub(crate) program_arguments_literal: bool,
    pub(crate) json: bool,
    /// `check --explain CODE`: print the published diagnostic entry.
    pub(crate) diagnostic_explain: Option<String>,
    pub(crate) explain_version: Option<u8>,
    /// Categories approved with `explain --json --allow`. Empty means no gate.
    pub(crate) allow: Vec<String>,
    /// `run --what-if`: print the plan and its fingerprint, and run nothing.
    pub(crate) what_if: bool,
    /// `run --confirm FINGERPRINT`: run only if the plan still has this fingerprint.
    pub(crate) confirm: Option<String>,
    /// `explain --json --policy-header`: merge the program's `# execute-policy:`
    /// header into the gate's approved categories.
    pub(crate) policy_header: bool,
    /// `run --single-instance LOCK`: refuse to run while another run holds LOCK.
    pub(crate) single_instance: Option<PathBuf>,
    pub(crate) github: bool,
    pub(crate) command: &'static str,
    pub(crate) file: PathBuf,
    pub(crate) manifest_dir: Option<PathBuf>,
    pub(crate) manifest_catalog: Option<PathBuf>,
    pub(crate) capability_policy: Option<PathBuf>,
    pub(crate) execution_mode: Option<flow::capabilities::ExecutionMode>,
    pub(crate) data_flow_policy: Option<PathBuf>,
    pub(crate) cache_dir: Option<PathBuf>,
    pub(crate) checkpoint_dir: Option<PathBuf>,
    pub(crate) retry_uncertain: bool,
    pub(crate) once: bool,
    pub(crate) trace: bool,
    pub(crate) headless: bool,
    pub(crate) max_output_bytes: usize,
    pub(crate) max_loop_iterations: usize,
    pub(crate) max_runs: Option<usize>,
    pub(crate) missed_ticks: Option<String>,
    pub(crate) max_catch_up: Option<usize>,
    pub(crate) calendar_dedup: Option<String>,
    pub(crate) command_timeout: Option<std::time::Duration>,
    pub(crate) drain_timeout: Option<std::time::Duration>,
    pub(crate) total_timeout: Option<std::time::Duration>,
    pub(crate) max_record_bytes: usize,
    pub(crate) max_record_bytes_set: bool,
    pub(crate) record_dir: Option<PathBuf>,
    /// `run --control`: host the finite program under a live control endpoint.
    pub(crate) control: bool,
    /// `run --control --break-at LINE`: pause before these source lines from the start.
    pub(crate) break_at: Vec<usize>,
}
