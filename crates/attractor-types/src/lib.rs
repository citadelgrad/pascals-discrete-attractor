//! Shared types, errors, context, and outcome for the Attractor pipeline engine.
//!
//! This crate provides the foundational types used across all other Attractor crates:
//! - `AttractorError` — unified error taxonomy
//! - `Context` — thread-safe key-value store for pipeline state
//! - `Outcome` — result of executing a node handler
//! - `Checkpoint` — serializable snapshot for crash recovery

pub mod context;
pub mod error;
pub mod types;

pub use context::Context;
pub use error::{AttractorError, Result};
pub use types::{Checkpoint, FidelityMode, Outcome, StageStatus};
