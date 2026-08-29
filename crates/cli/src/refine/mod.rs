//! /refine — review the session trajectory and propose exactly one new
//! Agent Skill, evidence-cited, validated, and applied only after the
//! user accepts it. Skills-only by design: no supplemental prompts or
//! memories, and the base system prompt is never touched.

mod apply;
mod proposal;
mod proposer;
mod trajectory;
mod validate;

pub use apply::{apply, undo, Applied};
pub use proposer::{run_proposer, RefineOutcome, Refined};
// Only tests construct proposals and checks by hand.
#[cfg(test)]
pub use proposal::{ScriptFile, SkillProposal};
#[cfg(test)]
pub use validate::Check;
