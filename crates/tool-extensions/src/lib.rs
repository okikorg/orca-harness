//! Optional tool integrations for Orca Harness.
//!
//! These capabilities are kept outside the baseline host tool set because
//! they opt into external processes, instruction roots, or network egress.
//! Packaging them together avoids one crate per integration while preserving
//! explicit module and runtime activation boundaries.

pub mod agent_plugins;
pub mod mcp;
pub mod plugin_hooks;
pub mod skills;
pub mod web;
