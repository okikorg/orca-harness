//! The typed terminal outcome of a workflow run.
//!
//! One document serves the store, the host API ([`WorkflowStatus::outcome`]),
//! and the parent: the run-level notification carries
//! `serde_json::to_value(&outcome)` under `workflow` and, as `answer`, its
//! string form. The JSON keys are the `harness-dag` outcome's plus the
//! runtime's bookkeeping in camelCase. For a run that finished `Done`,
//! `Cancelled`, or `Failed` through its graph, and for every per-stage
//! timing shape, the bytes are those of the untyped document the CLI
//! renders; a run that stalled (failed outside its graph) now also carries
//! empty `outputs` and `stages` and `degraded: false`, which the untyped
//! document omitted.
//!
//! [`WorkflowStatus::outcome`]: super::WorkflowStatus::outcome

use orca_harness_dag::{RunOutcome, RunState, StageId, StageStatus};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;

/// Where a finished run ended and what it produced.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowOutcome {
    pub state: RunState,
    /// Terminal stages' outputs (stages nothing depends on, map barriers
    /// included, their mapped items excluded).
    pub outputs: BTreeMap<StageId, String>,
    /// Every stage's final status; empty when the run stalled before its
    /// graph could report one.
    pub stages: BTreeMap<StageId, StageStatus>,
    pub degraded: bool,
    pub error: Option<String>,
    /// Wall-clock milliseconds from admission to the terminal notification.
    pub runtime_ms: u64,
    /// Most stage workers this run had admitted at once.
    pub peak_admitted: usize,
    /// Most stage workers this run had holding a running slot at once.
    pub peak_running: usize,
    pub timings: BTreeMap<StageId, StageTiming>,
}

impl WorkflowOutcome {
    /// The engine's outcome plus the runtime's bookkeeping; `timings`
    /// starts as the stages that ran and is completed by the runtime.
    pub(super) fn new(outcome: RunOutcome, runtime_ms: u64) -> Self {
        Self {
            state: outcome.state,
            outputs: outcome.outputs,
            stages: outcome.stages,
            degraded: outcome.degraded,
            error: outcome.error,
            runtime_ms,
            peak_admitted: 0,
            peak_running: 0,
            timings: BTreeMap::new(),
        }
    }
}

/// How one stage spent the run. Serialized as `{"runtimeMs", "cached"}`
/// for a stage that ran or was replayed and `{"runtimeMs": null,
/// "virtual"}` for one that never ran, so a reader can tell the two
/// apart even when a failed stage reports no runtime.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StageTiming {
    /// The stage ran, or its output was replayed from a prior run
    /// (`cached`, with a zero runtime). A stage that failed reports no
    /// runtime.
    Ran {
        runtime_ms: Option<u64>,
        cached: bool,
    },
    /// The stage never ran: the run ended first, or it is a map barrier
    /// (`virtual_stage`) that settles from its items.
    Skipped { virtual_stage: bool },
}

impl Serialize for StageTiming {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut timing = serializer.serialize_struct("StageTiming", 2)?;
        match self {
            Self::Ran { runtime_ms, cached } => {
                timing.serialize_field("runtimeMs", runtime_ms)?;
                timing.serialize_field("cached", cached)?;
            }
            Self::Skipped { virtual_stage } => {
                timing.serialize_field("runtimeMs", &None::<u64>)?;
                timing.serialize_field("virtual", virtual_stage)?;
            }
        }
        timing.end()
    }
}

impl<'de> Deserialize<'de> for StageTiming {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Raw {
            runtime_ms: Option<u64>,
            cached: Option<bool>,
            #[serde(rename = "virtual")]
            virtual_stage: Option<bool>,
        }
        let raw = Raw::deserialize(deserializer)?;
        match (raw.cached, raw.virtual_stage) {
            (Some(cached), None) => Ok(Self::Ran {
                runtime_ms: raw.runtime_ms,
                cached,
            }),
            (None, Some(virtual_stage)) => Ok(Self::Skipped { virtual_stage }),
            _ => Err(serde::de::Error::custom(
                "a stage timing carries exactly one of `cached` and `virtual`",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn timings_keep_the_delivered_shapes() {
        for (timing, expected) in [
            (
                StageTiming::Ran {
                    runtime_ms: Some(12),
                    cached: false,
                },
                json!({"runtimeMs": 12, "cached": false}),
            ),
            (
                StageTiming::Ran {
                    runtime_ms: None,
                    cached: false,
                },
                json!({"runtimeMs": null, "cached": false}),
            ),
            (
                StageTiming::Ran {
                    runtime_ms: Some(0),
                    cached: true,
                },
                json!({"runtimeMs": 0, "cached": true}),
            ),
            (
                StageTiming::Skipped {
                    virtual_stage: true,
                },
                json!({"runtimeMs": null, "virtual": true}),
            ),
        ] {
            let value = serde_json::to_value(&timing).unwrap();
            assert_eq!(value, expected);
            assert_eq!(
                serde_json::from_value::<StageTiming>(value).unwrap(),
                timing
            );
        }
        assert!(serde_json::from_value::<StageTiming>(json!({"runtimeMs": 1})).is_err());
    }

    #[test]
    fn outcome_keys_are_camel_case() {
        let outcome = WorkflowOutcome {
            state: RunState::Done,
            outputs: BTreeMap::from([("b".to_string(), "beta".to_string())]),
            stages: BTreeMap::from([("b".to_string(), StageStatus::Done)]),
            degraded: false,
            error: None,
            runtime_ms: 3,
            peak_admitted: 1,
            peak_running: 1,
            timings: BTreeMap::new(),
        };
        let value = serde_json::to_value(&outcome).unwrap();
        assert_eq!(
            value,
            json!({
                "state": "done", "outputs": {"b": "beta"}, "stages": {"b": "done"},
                "degraded": false, "error": null, "runtimeMs": 3,
                "peakAdmitted": 1, "peakRunning": 1, "timings": {}
            })
        );
        assert_eq!(
            serde_json::from_value::<WorkflowOutcome>(value).unwrap(),
            outcome
        );
    }
}
