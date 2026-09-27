//! Sideport engine: everything between the UI and the protocol crates.
//!
//! Front-ends create one [`Engine`], call its methods, and consume [`JobHandle`]s. See
//! `docs/ARCHITECTURE.md` for the runtime model.

mod demo;
mod engine;
pub mod error;
pub mod job;
mod pipeline;
mod secrets;
mod settings;
mod store;
pub mod types;

pub use engine::{Engine, EngineConfig, RefreshEvent};
pub use error::{EngineError, Result};
pub use job::{Fact, JobContext, JobEvent, JobHandle, LogLevel, Prompt, PromptKind, PromptReply, Stage, TeamChoice};
pub use types::*;
