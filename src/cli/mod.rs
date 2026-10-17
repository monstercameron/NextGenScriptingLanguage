//! Command-line parsing: the parsed `Args`, and the option table that fills it.

mod args;
mod parse;

pub(crate) use args::Args;
pub(crate) use parse::{parse_args, take_option_value, workflow_path};
