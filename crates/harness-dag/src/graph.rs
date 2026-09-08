use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GraphError(pub String);
impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for GraphError {}

pub(crate) fn validate(
    mut stages: Vec<Stage>,
    cap: usize,
) -> Result<BTreeMap<StageId, Stage>, GraphError> {
    let error = |s: String| GraphError(s);
    if stages.is_empty() || stages.len() > cap {
        return Err(error(format!("graph must contain 1..={cap} stages")));
    }
    let mut graph = BTreeMap::new();
    for stage in &mut stages {
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
        if graph.insert(stage.id.clone(), stage.clone()).is_some() {
            return Err(error(format!("duplicate stage `{}`", stage.id)));
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
    let mut ancestors: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    while ancestors.len() < graph.len() {
        let before = ancestors.len();
        for stage in graph.values() {
            if ancestors.contains_key(&stage.id)
                || !stage.needs.iter().all(|id| ancestors.contains_key(id))
            {
                continue;
            }
            let mut upstream = BTreeSet::new();
            for dep in &stage.needs {
                upstream.insert(dep.clone());
                upstream.extend(ancestors[dep].iter().cloned());
            }
            super::template::validate(&stage.prompt, &upstream, stage.kind == Kind::Map)?;
            ancestors.insert(stage.id.clone(), upstream);
        }
        if before == ancestors.len() {
            return Err(error(format!(
                "cycle involving: {}",
                graph
                    .keys()
                    .filter(|id| !ancestors.contains_key(*id))
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(graph)
}
