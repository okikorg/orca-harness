//! Model provider adapters for Orca Harness.
//!
//! Modules are organized by provider while protocol reuse stays internal:
//! OpenRouter delegates generation to the OpenAI-compatible adapter, and
//! Codex owns its distinct Responses protocol and credential lifecycle.

pub mod catalog;
pub mod openai;
pub mod openai_codex;
pub mod openrouter;

pub use catalog::{ModelInfo, Pricing};
pub use openai::OpenAiModel;
pub use openai_codex::OpenAiCodexModel;
pub use openrouter::OpenRouterModel;
