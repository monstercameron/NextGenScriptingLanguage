mod app;
pub mod syntax;
pub use app::{run, snapshot};

pub(crate) use app::live::{
    initial_state as initial_live_state, normalize_state as normalize_live_state, run as run_live,
    validate_state as validate_live_state,
};
