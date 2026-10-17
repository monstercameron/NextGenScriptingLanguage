//! Machine interfaces for tools and agents. These modules add output formats
//! and CLI modes; they do not change the `.program` grammar or runtime semantics.

#[cfg(not(target_arch = "wasm32"))]
pub mod attach;
#[cfg(not(target_arch = "wasm32"))]
pub mod checkpoint;
#[cfg(not(target_arch = "wasm32"))]
pub mod errors;
#[cfg(not(target_arch = "wasm32"))]
pub mod explain;
#[cfg(not(target_arch = "wasm32"))]
pub mod gate;
#[cfg(not(target_arch = "wasm32"))]
pub mod help;
#[cfg(not(target_arch = "wasm32"))]
pub mod instance;
#[cfg(not(target_arch = "wasm32"))]
pub mod policy;
#[cfg(not(target_arch = "wasm32"))]
pub mod what_if;
