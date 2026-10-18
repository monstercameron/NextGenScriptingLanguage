//! Command-line parsing: the parsed `Args`, and the option table that fills it.

mod args;
mod parse;

pub(crate) use args::Args;
#[cfg(feature = "tooling")]
pub(crate) use parse::workflow_path;
pub(crate) use parse::{parse_args, take_option_value};
