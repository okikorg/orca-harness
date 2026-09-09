//! Dynamic expansion and ordered map barriers.
use super::*;

impl Dag {
    pub(super) fn expand_map(
        &mut self,
        stage: &Stage,
        ready: &mut Vec<Stage>,
    ) -> Result<bool, String> {
        let id = &stage.id;
        let source = stage.over.as_ref().unwrap();
        let schema = self.graph[source].schema.as_deref().unwrap_or("json[]");
        let items = parse_items(&self.output[source], schema)?;
        if self.graph.len().saturating_add(items.len()) > self.cap {
            return Err(format!("map {id} exceeds stage cap {}", self.cap));
        }
        self.degraded |= items.is_empty();
        let mut children = Vec::with_capacity(items.len());
        for (index, item) in items.iter().enumerate() {
            let child_id = format!("{id}[{index}]");
            let item = item
                .as_str()
                .map(std::borrow::Cow::Borrowed)
                .unwrap_or_else(|| std::borrow::Cow::Owned(item.to_string()));
            let prompt = template::render(&stage.prompt, &self.output, Some(&item))
                .map_err(|e| e.to_string())?;
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
            if child.schema.is_some() {
                self.prompts.insert(child_id.clone(), child.prompt.clone());
            }
            children.push(child_id);
            ready.push(child);
        }
        let remaining = children.len();
        self.maps.insert(id.clone(), children);
        self.status.insert(id.clone(), StageStatus::Running);
        if remaining != 0 {
            self.map_remaining.insert(id.clone(), remaining);
        }
        Ok(remaining == 0)
    }

    pub(super) fn settle_parent(&mut self, child: &StageId) {
        let Some(parent) = self.children.get(child) else {
            return;
        };
        let remaining = self.map_remaining.get_mut(parent).unwrap();
        *remaining -= 1;
        if *remaining != 0 {
            return;
        }
        let parent = parent.clone();
        self.map_remaining.remove(&parent);
        let output = serde_json::to_string(
            &self.maps[&parent]
                .iter()
                .map(|child| &self.output[child])
                .collect::<Vec<_>>(),
        )
        .unwrap();
        self.settle(&parent, output);
    }
}

pub(super) fn parse_items(answer: &str, schema: &str) -> Result<Vec<serde_json::Value>, String> {
    let values: Vec<serde_json::Value> = serde_json::from_str(answer).map_err(|e| e.to_string())?;
    if schema == "string[]" && values.iter().any(|v| !v.is_string()) {
        return Err("expected an array of strings".into());
    }
    Ok(values)
}
