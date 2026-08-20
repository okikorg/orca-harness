//! Kernel execution limits. Limits are mechanisms, not policy: the host
//! decides values, the kernel enforces them.

use tokio::time::Instant;

#[derive(Debug, Clone)]
pub struct Limits {
    /// Maximum number of model invocations in one run.
    pub max_steps: u32,
    /// Maximum number of tool calls executing at the same instant.
    pub max_parallel_tools: usize,
    /// Absolute wall-clock deadline for the whole run.
    pub deadline: Option<Instant>,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_steps: 32,
            max_parallel_tools: 8,
            deadline: None,
        }
    }
}
