use crate::{graph, template, GraphError, Kind, Stage, StageId, DEFAULT_STAGE_CAP};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RunState {
    Running,
    Done,
    Failed,
    Cancelled,
    Stalled,
}
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StageStatus {
    Pending,
    Running,
    Done,
    Failed,
    Cancelled,
    /// Abandoned because the run ended for another reason: a sibling failed or
    /// the graph stalled. Distinct from `Cancelled`, which means the run itself
    /// was stopped, so the status map never implies a cancel that never happened.
    Stopped,
}
#[derive(Clone, Debug, Serialize)]
pub struct RunOutcome {
    pub state: RunState,
    pub outputs: BTreeMap<StageId, String>,
    pub stages: BTreeMap<StageId, StageStatus>,
    pub degraded: bool,
    pub error: Option<String>,
}
#[derive(Clone, Debug)]
pub enum Advance {
    Spawn(Vec<Stage>),
    Done(RunOutcome),
    Stalled(String),
}
/// An emitted stage is in flight before the host sees it. Duplicate and late
/// completions are ignored. Map nodes are barriers over ordered child outputs.
pub struct Dag {
    graph: BTreeMap<StageId, Stage>,
    edges: BTreeMap<StageId, Vec<StageId>>,
    pending: BTreeMap<StageId, usize>,
    output: BTreeMap<StageId, String>,
    in_flight: BTreeSet<StageId>,
    status: BTreeMap<StageId, StageStatus>,
    maps: BTreeMap<StageId, Vec<StageId>>,
    children: BTreeMap<StageId, StageId>,
    retries: BTreeSet<StageId>,
    prompts: BTreeMap<StageId, String>,
    pub state: RunState,
    degraded: bool,
    cap: usize,
    started: bool,
}
impl Dag {
    pub fn validate(graph: &[serde_json::Value]) -> Result<Self, GraphError> {
        Self::with_cap(graph, DEFAULT_STAGE_CAP)
    }
    pub fn with_cap(values: &[serde_json::Value], cap: usize) -> Result<Self, GraphError> {
        let stages = values
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<Result<Vec<Stage>, _>>()
            .map_err(|e| GraphError(e.to_string()))?;
        let graph = graph::validate(stages, cap)?;
        let mut edges: BTreeMap<_, Vec<_>> = graph.keys().map(|id| (id.clone(), vec![])).collect();
        for stage in graph.values() {
            for dep in &stage.needs {
                edges.get_mut(dep).unwrap().push(stage.id.clone());
            }
        }
        Ok(Self {
            pending: graph
                .values()
                .map(|s| (s.id.clone(), s.needs.len()))
                .collect(),
            status: graph
                .keys()
                .map(|id| (id.clone(), StageStatus::Pending))
                .collect(),
            graph,
            edges,
            output: BTreeMap::new(),
            in_flight: BTreeSet::new(),
            maps: BTreeMap::new(),
            children: BTreeMap::new(),
            retries: BTreeSet::new(),
            prompts: BTreeMap::new(),
            state: RunState::Running,
            degraded: false,
            cap,
            started: false,
        })
    }
    pub fn stages(&self) -> impl Iterator<Item = &Stage> {
        self.graph.values()
    }
    pub fn statuses(&self) -> &BTreeMap<StageId, StageStatus> {
        &self.status
    }
    pub fn output(&self, id: &StageId) -> Option<&str> {
        self.output.get(id).map(String::as_str)
    }
    /// Visible worker that produced this mapped item. For a map of a map,
    /// align the item with the preceding map's child rather than its virtual barrier.
    pub fn map_source(&self, id: &StageId) -> Option<&StageId> {
        let map = self.children.get(id)?;
        let source = self.graph[map].over.as_ref()?;
        let index = self.maps[map].iter().position(|child| child == id)?;
        Some(
            self.maps
                .get(source)
                .and_then(|children| children.get(index))
                .unwrap_or(source),
        )
    }
    /// Canonical recursive key material; the host may hash it for storage but
    /// must compare this material on lookup, so hash collisions cannot replay.
    pub fn stage_key(&self, id: &StageId) -> String {
        fn visit(dag: &Dag, id: &StageId, included: &mut BTreeSet<StageId>) {
            if !included.insert(id.clone()) {
                return;
            }
            for dep in &dag.graph[id].needs {
                visit(dag, dep, included);
            }
        }
        let mut included = BTreeSet::new();
        visit(self, id, &mut included);
        serde_json::to_string(
            &included
                .iter()
                .map(|id| &self.graph[id])
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    pub fn start(&mut self) -> Advance {
        if self.started || self.state != RunState::Running {
            return Advance::Spawn(vec![]);
        }
        self.started = true;
        self.emit()
    }
    pub fn complete(&mut self, id: &StageId, answer: Result<String, String>) -> Advance {
        if self.state != RunState::Running || !self.in_flight.remove(id) {
            return Advance::Spawn(vec![]);
        }
        let answer = match answer {
            Ok(a) => a,
            Err(e) => {
                self.status.insert(id.clone(), StageStatus::Failed);
                return self.finish(RunState::Failed, Some(format!("{id}: {e}")));
            }
        };
        if let Some(schema) = self.graph[id].schema.as_deref() {
            if let Err(error) = parse_items(&answer, schema) {
                if self.retries.insert(id.clone()) {
                    let mut retry = self.graph[id].clone();
                    retry.prompt = self.prompts.get(id).cloned().unwrap_or(retry.prompt);
                    retry.prompt.push_str(&format!("\nYour previous answer did not match {schema}: {error}. Return only the corrected JSON array."));
                    self.in_flight.insert(id.clone());
                    return Advance::Spawn(vec![retry]);
                }
                self.status.insert(id.clone(), StageStatus::Failed);
                return self.finish(RunState::Failed, Some(format!("{id}: {error}")));
            }
        }
        self.settle(id, answer);
        self.emit()
    }
    pub fn cancel(&mut self) -> Advance {
        if self.state != RunState::Running {
            return Advance::Spawn(vec![]);
        }
        self.finish(RunState::Cancelled, Some("workflow cancelled".into()))
    }
    fn settle(&mut self, id: &StageId, answer: String) {
        self.output.insert(id.clone(), answer);
        self.status.insert(id.clone(), StageStatus::Done);
        self.pending.remove(id);
        for next in self.edges.get(id).into_iter().flatten() {
            if let Some(n) = self.pending.get_mut(next) {
                *n = n.saturating_sub(1);
            }
        }
    }
    fn emit(&mut self) -> Advance {
        let mut ready = Vec::new();
        loop {
            let ids: Vec<_> = self
                .pending
                .iter()
                .filter(|(id, n)| {
                    **n == 0 && !self.in_flight.contains(*id) && !self.maps.contains_key(*id)
                })
                .map(|(id, _)| id.clone())
                .collect();
            if ids.is_empty() {
                break;
            }
            for id in ids {
                let mut stage = self.graph[&id].clone();
                if stage.kind == Kind::Map {
                    let source = stage.over.as_ref().unwrap();
                    let schema = self.graph[source].schema.as_deref().unwrap_or("json[]");
                    let items = match parse_items(&self.output[source], schema) {
                        Ok(items) => items,
                        Err(e) => return self.finish(RunState::Failed, Some(e)),
                    };
                    if self.graph.len().saturating_add(items.len()) > self.cap {
                        return self.finish(
                            RunState::Failed,
                            Some(format!("map {id} exceeds stage cap {}", self.cap)),
                        );
                    }
                    self.degraded |= items.is_empty();
                    let mut children = Vec::new();
                    for (index, item) in items.iter().enumerate() {
                        let child_id = format!("{id}[{index}]");
                        let item = item
                            .as_str()
                            .map(str::to_owned)
                            .unwrap_or_else(|| item.to_string());
                        let prompt =
                            match template::render(&stage.prompt, &self.output, Some(&item)) {
                                Ok(p) => p,
                                Err(e) => {
                                    return self.finish(RunState::Failed, Some(e.to_string()))
                                }
                            };
                        let child = Stage {
                            id: child_id.clone(),
                            prompt,
                            needs: stage.needs.clone(),
                            kind: Kind::Agent,
                            over: None,
                            schema: stage.schema.clone(),
                            model: stage.model.clone(),
                        };
                        self.children.insert(child_id.clone(), id.clone());
                        self.graph.insert(child_id.clone(), child.clone());
                        self.status.insert(child_id.clone(), StageStatus::Running);
                        self.in_flight.insert(child_id.clone());
                        self.prompts.insert(child_id.clone(), child.prompt.clone());
                        children.push(child_id);
                        ready.push(child);
                    }
                    self.maps.insert(id.clone(), children);
                    self.status.insert(id.clone(), StageStatus::Running);
                } else {
                    stage.prompt = match template::render(&stage.prompt, &self.output, None) {
                        Ok(p) => p,
                        Err(e) => return self.finish(RunState::Failed, Some(e.to_string())),
                    };
                    self.prompts.insert(id.clone(), stage.prompt.clone());
                    self.in_flight.insert(id.clone());
                    self.status.insert(id, StageStatus::Running);
                    ready.push(stage);
                }
            }
            self.settle_maps();
        }
        self.settle_maps();
        // Settling a map may have unlocked normal downstream nodes.
        if self
            .pending
            .iter()
            .any(|(id, n)| *n == 0 && !self.in_flight.contains(id) && !self.maps.contains_key(id))
        {
            match self.emit() {
                Advance::Spawn(more) => ready.extend(more),
                other => return other,
            }
        }
        if self.pending.is_empty() && self.in_flight.is_empty() {
            return self.finish(RunState::Done, None);
        }
        if ready.is_empty() && self.in_flight.is_empty() {
            self.state = RunState::Stalled;
            return Advance::Stalled(format!(
                "unsatisfied stages: {}",
                self.pending.keys().cloned().collect::<Vec<_>>().join(", ")
            ));
        }
        Advance::Spawn(ready)
    }
    fn settle_maps(&mut self) {
        let done: Vec<_> = self
            .maps
            .iter()
            .filter(|(id, children)| {
                !self.output.contains_key(*id)
                    && children.iter().all(|child| self.output.contains_key(child))
            })
            .map(|(id, children)| {
                (
                    id.clone(),
                    serde_json::to_string(
                        &children
                            .iter()
                            .map(|child| &self.output[child])
                            .collect::<Vec<_>>(),
                    )
                    .unwrap(),
                )
            })
            .collect();
        for (id, output) in done {
            self.settle(&id, output);
        }
    }
    fn finish(&mut self, state: RunState, error: Option<String>) -> Advance {
        self.state = state.clone();
        let unfinished = if state == RunState::Cancelled {
            StageStatus::Cancelled
        } else {
            StageStatus::Stopped
        };
        for status in self.status.values_mut() {
            if matches!(status, StageStatus::Pending | StageStatus::Running) {
                *status = unfinished.clone();
            }
        }
        let outputs = self
            .output
            .iter()
            .filter(|(id, _)| {
                self.edges.get(*id).is_some_and(Vec::is_empty) && !self.children.contains_key(*id)
            })
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Advance::Done(RunOutcome {
            state,
            outputs,
            stages: self.status.clone(),
            degraded: self.degraded,
            error,
        })
    }
}
fn parse_items(answer: &str, schema: &str) -> Result<Vec<serde_json::Value>, String> {
    let values: Vec<serde_json::Value> = serde_json::from_str(answer).map_err(|e| e.to_string())?;
    if schema == "string[]" && values.iter().any(|v| !v.is_string()) {
        return Err("expected an array of strings".into());
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn lost_decrement_stalls_instead_of_waiting_forever() {
        let mut dag = Dag::validate(&[serde_json::json!({"id":"a","prompt":"a"})]).unwrap();
        dag.pending.insert("a".into(), 1);
        assert!(matches!(dag.start(), Advance::Stalled(_)));
    }
}
