//! Tests for the Dispatcher's concurrent execution semantics, driven
//! end-to-end through the Agent with a fake (scripted) LLM.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::timeout;

use orca_harness_core::testing::{call, ConcurrencyProbe, ScriptedModel};
use orca_harness_core::{
    Agent, CancellationToken, Concurrency, FnTool, HarnessError, Limits, Message,
};

const RUN_TIMEOUT: Duration = Duration::from_secs(10);

include!("concurrency/core.rs");
include!("concurrency/cancellation.rs");
include!("concurrency/hooks_and_scale.rs");
