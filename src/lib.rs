pub mod ast;
pub mod inputs;
pub mod manifest;
#[cfg(not(target_arch = "wasm32"))]
pub mod models;
pub mod parser;
#[cfg_attr(target_arch = "wasm32", path = "browser.rs")]
pub mod runtime;
#[cfg(not(target_arch = "wasm32"))]
pub mod schedule;
#[cfg(not(target_arch = "wasm32"))]
pub mod stream;
mod structured;

#[cfg(not(target_arch = "wasm32"))]
pub mod ui;

#[cfg(not(target_arch = "wasm32"))]
pub mod live;

#[cfg(not(target_arch = "wasm32"))]
mod platform;
