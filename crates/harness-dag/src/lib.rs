//! Deterministic workflow scheduling. No IO, executors, or harness types.
mod engine;
mod graph;
mod template;
pub use engine::{Advance, Dag, RunOutcome, RunState, StageStatus};
pub use graph::{GraphError, Kind, Stage, StageId, DEFAULT_STAGE_CAP};
pub type RunId = u64;
