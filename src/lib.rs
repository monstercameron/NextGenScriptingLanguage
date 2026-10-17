#[cfg(not(target_arch = "wasm32"))]
pub mod agent;
#[cfg(not(target_arch = "wasm32"))]
pub mod applets;
pub mod ast;
#[cfg(not(target_arch = "wasm32"))]
pub mod capabilities;
pub mod contracts;
#[cfg(not(target_arch = "wasm32"))]
pub mod desktop;
#[cfg(not(target_arch = "wasm32"))]
pub mod diagnostics;
#[cfg(not(target_arch = "wasm32"))]
pub mod discovery;
#[cfg(not(target_arch = "wasm32"))]
pub mod graph;
pub mod inputs;
pub mod manifest;
#[cfg(not(target_arch = "wasm32"))]
pub mod models;
#[cfg(not(target_arch = "wasm32"))]
pub mod modules;
pub mod parser;
#[cfg(not(target_arch = "wasm32"))]
pub mod remote;
#[cfg_attr(target_arch = "wasm32", path = "browser.rs")]
pub mod runtime;
#[cfg(not(target_arch = "wasm32"))]
pub mod schedule;
#[cfg(not(target_arch = "wasm32"))]
pub mod security;
#[cfg(not(target_arch = "wasm32"))]
pub mod service_deployment;
#[cfg(not(target_arch = "wasm32"))]
pub mod stream;
pub mod suggest;
mod structured;
#[cfg(not(target_arch = "wasm32"))]
pub mod tasks;

#[cfg(not(target_arch = "wasm32"))]
pub mod ui;

#[cfg(not(target_arch = "wasm32"))]
pub mod live;

#[cfg(not(target_arch = "wasm32"))]
pub mod language_server;

#[cfg(not(target_arch = "wasm32"))]
mod platform;
