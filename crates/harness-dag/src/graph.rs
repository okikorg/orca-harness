use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
pub type StageId = String;
pub const DEFAULT_STAGE_CAP: usize = 256;
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Agent,
    Map,
}
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    pub id: StageId,
    pub prompt: String,
    #[serde(default)]
    pub needs: Vec<StageId>,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default)]
    pub over: Option<StageId>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
}
impl Stage {
    /// An ordinary stage with no dependencies; set the other fields directly.
    pub fn new(id: impl Into<StageId>, prompt: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            prompt: prompt.into(),
            needs: Vec::new(),
            kind: Kind::Agent,
            over: None,
            schema: None,
            model: None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphError(pub String);
impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for GraphError {}

pub(crate) fn validate(
    stages: Vec<Stage>,
    cap: usize,
) -> Result<BTreeMap<StageId, Stage>, GraphError> {
    let error = |s: String| GraphError(s);
    if stages.is_empty() || stages.len() > cap {
        return Err(error(format!("graph must contain 1..={cap} stages")));
    }
    let mut graph = BTreeMap::new();
    for mut stage in stages {
        if stage.id.is_empty()
            || !stage
                .id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        {
            return Err(error(format!(
                "invalid stage id `{}`; use letters, numbers, _ or -",
                stage.id
            )));
        }
        if stage.prompt.trim().is_empty() {
            return Err(error(format!("{}: empty prompt", stage.id)));
        }
        if let Some(schema) = &stage.schema {
            if !matches!(schema.as_str(), "string[]" | "json[]") {
                return Err(error(format!(
                    "{}: unsupported schema `{schema}`",
                    stage.id
                )));
            }
        }
        match (&stage.kind, &stage.over) {
            (Kind::Map, Some(over)) => {
                if !stage.needs.contains(over) {
                    stage.needs.push(over.clone());
                }
            }
            (Kind::Map, None) => return Err(error(format!("{}: map requires over", stage.id))),
            (Kind::Agent, Some(_)) => {
                return Err(error(format!("{}: over requires map", stage.id)))
            }
            _ => {}
        }
        stage.needs.sort();
        stage.needs.dedup();
        match graph.entry(stage.id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(stage);
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                return Err(error(format!("duplicate stage `{}`", entry.key())));
            }
        }
    }
    for stage in graph.values() {
        for dep in &stage.needs {
            if !graph.contains_key(dep) {
                return Err(error(format!("{}: unknown dependency `{dep}`", stage.id)));
            }
        }
        if let Some(over) = &stage.over {
            if graph[over].schema.is_none() && graph[over].kind != Kind::Map {
                return Err(error(format!("{over}: map source requires schema")));
            }
        }
    }
    // Compact ancestor bits avoid copying stage IDs for every transitive edge.
    // Keep the validation waves in their original order to preserve diagnostics.
    let indices: BTreeMap<_, _> = graph
        .keys()
        .enumerate()
        .map(|(i, id)| (id.as_str(), i))
        .collect();
    let mut ancestors: Vec<Option<Vec<u64>>> = vec![None; graph.len()];
    let mut validated = 0;
    while validated < graph.len() {
        let before = validated;
        for (index, stage) in graph.values().enumerate() {
            if ancestors[index].is_some()
                || !stage
                    .needs
                    .iter()
                    .all(|id| ancestors[indices[id.as_str()]].is_some())
            {
                continue;
            }
            let mut upstream = vec![0u64; graph.len().div_ceil(64)];
            for dep in &stage.needs {
                let dep = indices[dep.as_str()];
                upstream[dep / 64] |= 1 << (dep % 64);
                for (word, inherited) in upstream.iter_mut().zip(ancestors[dep].as_ref().unwrap()) {
                    *word |= inherited;
                }
            }
            super::template::validate(
                &stage.prompt,
                |id| {
                    indices
                        .get(id)
                        .is_some_and(|&i| upstream[i / 64] & (1 << (i % 64)) != 0)
                },
                stage.kind == Kind::Map,
            )?;
            ancestors[index] = Some(upstream);
            validated += 1;
        }
        if before == validated {
            return Err(error(format!(
                "cycle involving: {}",
                graph
                    .keys()
                    .enumerate()
                    .filter(|(i, _)| ancestors[*i].is_none())
                    .map(|(_, id)| id.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(graph)
}
