//! Application layer: venue actors (Kameo), the comparison engine shared by the HTTP API
//! and the MCP server, persistence and metrics.
//!
//! Financial logic lives in `oa_core`; this crate only orchestrates it.

// The OpenAPI document is one large `serde_json::json!` literal.
#![recursion_limit = "256"]

#[cfg(feature = "live")]
compile_error!(
    "LIVE execution is not implemented. Enabling it requires a dedicated, reviewed build and the \
     owner's explicit approval (platforms, accounts, permissions, caps, signing)."
);

#[cfg(all(feature = "testnet", feature = "mainnet-nc"))]
compile_error!(
    "`testnet` and `mainnet-nc` are separate builds (separate databases and env files): \
     build one or the other"
);

pub mod api;
pub mod binaries;
pub mod config;
pub mod engine;
pub mod execution;
pub mod fixtures;
pub mod geo;
pub mod integrators;
pub mod live;
pub mod mcp;
pub mod metrics;
pub mod openapi;
pub mod push;
pub mod store;
pub mod strategy;
pub mod venue;
pub mod webhooks;

#[cfg(test)]
mod e2e_tests;
