//! Tool adapter implementations.
//!
//! Each submodule implements a single `run` function that takes the typed
//! config for that adapter, an argument map, and an optional timeout, and
//! returns a [`ToolOutput`] or [`ToolError`].

pub mod http;
pub mod mcp;
mod mcp_http;
mod process;
pub mod subprocess;
