// The wasm build compiles a subset of the native modules, so helpers that only
// the excluded modules call are dead there. Native builds keep the lint.
#![cfg_attr(target_arch = "wasm32", allow(dead_code))]

#[cfg(not(target_arch = "wasm32"))]
pub mod agent;
#[cfg(not(target_arch = "wasm32"))]
pub mod applets;
pub mod ast;
pub mod bundled_manifests;
mod bytes;
#[cfg(not(target_arch = "wasm32"))]
pub mod capabilities;
#[cfg(feature = "tooling")]
pub mod compile_sh;
pub mod contracts;
#[cfg(all(feature = "desktop", not(target_arch = "wasm32")))]
pub mod desktop;
#[cfg(not(target_arch = "wasm32"))]
pub mod desktop_entry;
#[cfg(not(target_arch = "wasm32"))]
pub mod diagnostics;
#[cfg(not(target_arch = "wasm32"))]
pub mod doctor;
pub mod format;
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "tooling")]
pub mod discovery;
#[cfg(feature = "failpoints")]
pub(crate) mod failpoint;
pub mod github_actions;
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "graph")]
pub mod graph;
pub mod home;
#[cfg(not(target_arch = "wasm32"))]
pub mod i18n;
pub mod inputs;
#[cfg(not(target_arch = "wasm32"))]
pub mod lua_config;
pub mod manifest;
pub(crate) mod model_schema;
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "model")]
pub mod models;
#[cfg(not(target_arch = "wasm32"))]
pub mod modules;
#[cfg(not(target_arch = "wasm32"))]
pub mod named_state;
#[cfg(not(target_arch = "wasm32"))]
pub mod packages;
pub mod parser;
pub mod privilege;
#[cfg(not(target_arch = "wasm32"))]
pub mod process_table;
pub mod program_files;
#[cfg(not(target_arch = "wasm32"))]
pub mod prompt;
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "records")]
pub mod record;
#[cfg(all(feature = "remote", not(target_arch = "wasm32")))]
pub mod remote;
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "records")]
pub mod repair;
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "records")]
pub mod repl;
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "records")]
pub mod replay;
#[cfg(not(target_arch = "wasm32"))]
pub mod redact;
#[cfg(not(target_arch = "wasm32"))]
pub mod restart_required;
#[cfg_attr(target_arch = "wasm32", path = "browser.rs")]
pub mod runtime;
#[cfg(not(target_arch = "wasm32"))]
pub mod schedule;
#[cfg(not(target_arch = "wasm32"))]
pub mod security;
#[cfg(not(target_arch = "wasm32"))]
#[cfg(feature = "tooling")]
pub mod service_deployment;
pub mod ssrf;
#[cfg(not(target_arch = "wasm32"))]
pub mod stream;
mod structured;
pub mod suggest;
#[cfg(not(target_arch = "wasm32"))]
pub mod tasks;
#[cfg(not(target_arch = "wasm32"))]
pub mod sysfacts;
pub mod terminal_text;
pub mod working_dir;

#[cfg(all(feature = "screens", not(target_arch = "wasm32")))]
pub mod ui;

#[cfg(not(target_arch = "wasm32"))]
pub mod ipc;
#[cfg(all(feature = "live", not(target_arch = "wasm32")))]
pub mod live;

#[cfg(all(feature = "fuzzing", not(target_arch = "wasm32")))]
pub mod fuzz_entry;

#[cfg(all(feature = "lsp", not(target_arch = "wasm32")))]
pub mod language_server;

#[cfg(not(target_arch = "wasm32"))]
pub(crate) mod fs_io;
#[cfg(not(target_arch = "wasm32"))]
mod platform;
