mod app;
pub mod syntax;
#[cfg(feature = "tui")]
pub use app::run;
pub use app::snapshot;

/// Without the `tui` feature, interactive screens cannot run; headless snapshots still can.
#[cfg(not(feature = "tui"))]
pub fn run(
    _screen: &syntax::Screen,
    _registry: &crate::manifest::Registry,
    _options: &crate::runtime::RunOptions,
    _max_runs: Option<usize>,
) -> anyhow::Result<()> {
    anyhow::bail!("interactive screens require the tui feature; use --headless for a JSON snapshot")
}

#[cfg(feature = "tui")]
pub(crate) use app::live::run as run_live;
#[cfg(feature = "live")]
pub(crate) use app::live::{
    initial_state as initial_live_state, normalize_state as normalize_live_state,
    validate_state as validate_live_state,
};

/// Without the `tui` feature, live screens cannot run interactively.
#[cfg(all(feature = "live", not(feature = "tui")))]
pub(crate) fn run_live(
    _shared: std::sync::Arc<crate::live::state::Shared>,
    _options: &crate::runtime::RunOptions,
) -> anyhow::Result<()> {
    anyhow::bail!("interactive screens require the tui feature")
}
