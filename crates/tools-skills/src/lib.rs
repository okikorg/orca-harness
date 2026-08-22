//! # Orca Harness Skills
//!
//! A skill is a folder with a `SKILL.md` in it: frontmatter naming and
//! describing a procedure, and a body telling the agent how to carry it
//! out. Skills are discovered on disk rather than configured, and they
//! reach outside the workspace (a user's `~/.claude/skills` counts), so
//! they live here rather than in the core tool set — the same host
//! opt-in framing as the MCP client.
//!
//! [`discover`] scans a list of [`SkillRoot`]s in precedence order and
//! returns what loaded, what was shadowed by an earlier root, and what
//! failed to parse. [`SkillTool`] turns the loaded set into a single
//! `skill` tool whose description carries the catalog: one call loads a
//! skill's instructions, or one of the text resources beside it.
//!
//! ```no_run
//! use orca_harness_core::Agent;
//! use orca_harness_tools_skills::{discover, roots, SkillTool};
//!
//! # fn example(model: impl orca_harness_core::Model) {
//! let found = discover(&roots("/repo".as_ref(), None, None));
//! let agent = Agent::new(model).tool_arc(std::sync::Arc::new(SkillTool::new(found.skills)));
//! # let _ = agent; }
//! ```
//!
//! Nothing in a skill directory is ever executed by the loader. A body
//! may tell the agent to run a script; the agent then runs it with the
//! ordinary `shell` tool, under the ordinary approval gate.

mod install;
mod skill;
mod tool;

pub use install::{
    checkout, find_candidates, install, parse_request, scaffold, uninstall, Candidate, Checkout,
    Installed, Origin, Request,
};
pub use skill::{
    discover, parse_frontmatter, roots, Discovered, Frontmatter, Shadowed, Skill, SkillFailure,
    SkillRoot, HOME_ROOTS, WORKSPACE_ROOTS,
};
pub use tool::SkillTool;
