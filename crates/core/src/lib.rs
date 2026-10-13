//! Pure domain model and decision engines.
//!
//! This crate must stay free of network I/O, HTTP types, database code and
//! actor frameworks. Everything here is deterministic and unit-testable.

pub mod approval;
pub mod book;
pub mod comparability;
pub mod cost;
pub mod domain;
pub mod fees;
pub mod risk;
pub mod routing;
pub mod strategy;

pub use rust_decimal::Decimal;
