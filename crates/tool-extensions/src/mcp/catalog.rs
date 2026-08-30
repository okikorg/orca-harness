//! One lazy, model-facing catalog over every connected MCP server.

use std::cmp::Reverse;
use std::collections::HashSet;
use std::sync::{Arc, RwLock};

use async_trait::async_trait;
use serde_json::{json, Map, Value};

use orca_harness_core::{Concurrency, Tool, ToolContext, ToolError, ToolSchema};

use super::client::{request_with_context, McpClient, McpConnection};
use super::tool::McpTool;

const SEARCH_DEFAULT_LIMIT: u64 = 10;

#[derive(Clone, Default)]
pub struct McpCatalog {
    servers: Arc<RwLock<Vec<Server>>>,
}

struct Server {
    name: String,
    client: Arc<McpClient>,
    tools: Vec<Arc<McpTool>>,
}

impl McpCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace one server while preserving the configured server order.
    pub fn insert(&self, name: String, connection: McpConnection) -> Result<(), super::McpError> {
        let incoming: HashSet<String> = connection
            .tools()
            .iter()
            .map(|tool| tool.schema().name)
            .collect();
        let (client, tools) = connection.into_parts();
        let mut servers = self.servers.write().expect("mcp catalog lock");
        if let Some(collision) = servers
            .iter()
            .filter(|server| server.name != name)
            .flat_map(|server| &server.tools)
            .map(|tool| tool.schema().name)
            .find(|tool_name| incoming.contains(tool_name))
        {
            return Err(super::McpError::Protocol(format!(
                "duplicate MCP tool name: {collision}"
            )));
        }
        if let Some(slot) = servers.iter_mut().find(|server| server.name == name) {
            *slot = Server {
                name,
                client,
                tools,
            };
        } else {
            servers.push(Server {
                name,
                client,
                tools,
            });
        }
        Ok(())
    }

    pub fn remove(&self, name: &str) {
        self.servers
            .write()
            .expect("mcp catalog lock")
            .retain(|server| server.name != name);
    }

    /// Match catalog iteration to the caller's complete desired-server order.
    /// Unlisted servers remain at the end in their existing relative order.
    pub fn reorder(&self, desired: &[String]) {
        self.servers
            .write()
            .expect("mcp catalog lock")
            .sort_by_key(|server| {
                desired
                    .iter()
                    .position(|name| name == &server.name)
                    .unwrap_or(usize::MAX)
            });
    }

    pub fn healthy(&self, name: &str) -> bool {
        self.servers
            .read()
            .expect("mcp catalog lock")
            .iter()
            .find(|server| server.name == name)
            .is_some_and(|server| server.client.healthy())
    }

    pub fn server_tools(&self, name: &str) -> Vec<Arc<dyn Tool>> {
        self.servers
            .read()
            .expect("mcp catalog lock")
            .iter()
            .find(|server| server.name == name)
            .map(|server| {
                server
                    .tools
                    .iter()
                    .cloned()
                    .map(|tool| tool as Arc<dyn Tool>)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The three stable interfaces are registered once; remote tools stay
    /// registered but hidden until `mcp_select_tool` selects them.
    pub fn interface_tools(&self) -> Vec<Arc<dyn Tool>> {
        vec![
            Arc::new(SearchTools(self.clone())),
            Arc::new(SelectTool(self.clone())),
            Arc::new(Features(self.clone())),
        ]
    }

    /// Non-MCP schemas and the three interfaces are always visible. Remote
    /// schemas known to this catalog appear only after exact selection.
    pub fn schema_visible(&self, name: &str) -> bool {
        self.servers
            .read()
            .expect("mcp catalog lock")
            .iter()
            .flat_map(|server| &server.tools)
            .find(|tool| tool.schema().name == name)
            .is_none_or(|tool| tool.is_selected())
    }

    fn client(&self, name: &str) -> Result<Arc<McpClient>, ToolError> {
        self.servers
            .read()
            .expect("mcp catalog lock")
            .iter()
            .find(|server| server.name == name)
            .map(|server| server.client.clone())
            .ok_or_else(|| ToolError::msg(format!("unknown MCP server: {name}")))
    }
}

struct SearchTools(McpCatalog);

#[async_trait]
impl Tool for SearchTools {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_search_tools".into(),
            description: "Search tool metadata across all configured MCP servers. Precise all-term matches rank first; when none exist, relevant partial matches are returned so natural-language qualifiers do not hide available tools. Returns exact names for mcp_select_tool without loading input schemas.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "minLength": 1, "description": "Required case-insensitive capability terms matched against tool name, description, server, and input schema. Describe the operation or data needed." },
                    "server": { "type": "string", "description": "Optional exact server name filter." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "default": SEARCH_DEFAULT_LIMIT }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        require_object(&input)?;
        reject_unknown(&input, &["query", "server", "limit"])?;
        let query = required_str(&input, "query")?;
        let terms = tokens(query);
        if terms.is_empty() {
            return Err(ToolError::msg("missing or invalid query"));
        }
        let filter = optional_str(&input, "server")?;
        if let Some(filter) = filter {
            let known = self
                .0
                .servers
                .read()
                .expect("mcp catalog lock")
                .iter()
                .any(|server| server.name == filter);
            if !known {
                return Err(ToolError::msg(format!("unknown MCP server: {filter}")));
            }
        }
        let limit = optional_u64(&input, "limit", 1, 100)?.unwrap_or(SEARCH_DEFAULT_LIMIT) as usize;
        let servers = self.0.servers.read().expect("mcp catalog lock");
        let mut matches = Vec::new();
        let mut fallback_matches = Vec::new();
        let mut order = 0;
        for server in servers
            .iter()
            .filter(|server| filter.is_none_or(|f| f == server.name))
        {
            for tool in &server.tools {
                let schema = tool.schema();
                let fields = SearchFields {
                    name: tokens(tool.remote_name()),
                    description: tokens(&schema.description),
                    server: tokens(&server.name),
                    parameters: tokens(&schema.parameters.to_string()),
                };
                let ranked = |score| {
                    (
                        Reverse(score),
                        fields.name.len(),
                        order,
                        json!({
                            "name": schema.name,
                            "server": server.name,
                            "remote_name": tool.remote_name(),
                            "description": schema.description,
                            "selected": tool.is_selected(),
                        }),
                    )
                };
                if let Some(score) = relevance(&terms, &fields) {
                    matches.push(ranked(score));
                } else if let Some(score) = partial_relevance(&terms, &fields) {
                    fallback_matches.push(ranked(score));
                }
                order += 1;
            }
        }
        if matches.is_empty() {
            matches = fallback_matches;
        }
        matches.sort_by_key(|(score, name_len, order, _)| (*score, *name_len, *order));
        Ok(json!({
            "tools": matches
                .into_iter()
                .take(limit)
                .map(|(_, _, _, value)| value)
                .collect::<Vec<_>>()
        }))
    }
}

struct SearchFields {
    name: Vec<String>,
    description: Vec<String>,
    server: Vec<String>,
    parameters: Vec<String>,
}

fn tokens(value: &str) -> Vec<String> {
    value
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn singular(token: &str) -> String {
    if token.len() > 4 && token.ends_with("ies") {
        return format!("{}y", &token[..token.len() - 3]);
    }
    for suffix in ["ches", "shes", "sses", "xes", "zes"] {
        if token.len() > suffix.len() && token.ends_with(suffix) {
            return token[..token.len() - 2].to_owned();
        }
    }
    if token.len() > 3
        && token.ends_with('s')
        && !token.ends_with("ss")
        && !token.ends_with("us")
        && !token.ends_with("is")
    {
        return token[..token.len() - 1].to_owned();
    }
    token.to_owned()
}

fn term_match(term: &str, field: &[String], normalize_plural: bool) -> u64 {
    if field.iter().any(|token| token == term) {
        3
    } else if field.iter().any(|token| token.contains(term)) {
        2
    } else if normalize_plural && field.iter().any(|token| singular(token) == singular(term)) {
        1
    } else {
        0
    }
}

fn relevance(terms: &[String], fields: &SearchFields) -> Option<u64> {
    let (matched, mut score) = match_score(terms, fields);
    if matched != terms.len() {
        return None;
    }

    let normalized_terms: Vec<String> = terms.iter().map(|term| singular(term)).collect();
    let normalized_name: Vec<String> = fields.name.iter().map(|term| singular(term)).collect();
    if normalized_terms == normalized_name {
        score += 100;
    } else if normalized_terms
        .iter()
        .all(|term| normalized_name.contains(term))
    {
        score += 20;
    }
    Some(score)
}

/// Natural-language searches often add words absent from compact MCP schemas.
/// Use this only when no tool matched every term, preserving precise results
/// while keeping one unmatched adjective from hiding the whole catalog.
fn partial_relevance(terms: &[String], fields: &SearchFields) -> Option<u64> {
    let (matched, mut score) = match_score(terms, fields);
    if matched == 0 {
        return None;
    }
    score += (matched as u64 * 100) / terms.len() as u64;

    let normalized_terms: Vec<String> = terms.iter().map(|term| singular(term)).collect();
    let normalized_name: Vec<String> = fields.name.iter().map(|term| singular(term)).collect();
    if normalized_name
        .iter()
        .all(|term| normalized_terms.contains(term))
    {
        score += 20;
    }
    Some(score)
}

fn match_score(terms: &[String], fields: &SearchFields) -> (usize, u64) {
    terms.iter().fold((0, 0), |(matched, score), term| {
        let best = [
            term_match(term, &fields.name, true) * 8,
            term_match(term, &fields.description, false) * 4,
            term_match(term, &fields.server, false) * 2,
            term_match(term, &fields.parameters, false),
        ]
        .into_iter()
        .max()
        .unwrap_or_default();
        (matched + usize::from(best > 0), score + best)
    })
}

struct SelectTool(McpCatalog);

#[async_trait]
impl Tool for SelectTool {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_select_tool".into(),
            description: "Load one exact MCP tool discovered by mcp_search_tools. Its full executable schema becomes available on the next model turn.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Exact model-facing name returned by mcp_search_tools." }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
        }
    }

    async fn call(&self, input: Value, _ctx: &ToolContext) -> Result<Value, ToolError> {
        require_object(&input)?;
        reject_unknown(&input, &["name"])?;
        let name = required_str(&input, "name")?;
        let servers = self.0.servers.read().expect("mcp catalog lock");
        let tool = servers
            .iter()
            .flat_map(|server| &server.tools)
            .find(|tool| tool.schema().name == name)
            .ok_or_else(|| {
                ToolError::msg(format!(
                    "unknown MCP tool: {name}; search first with mcp_search_tools"
                ))
            })?;
        let already_selected = tool.select();
        let schema = tool.schema();
        Ok(json!({
            "name": schema.name,
            "description": schema.description,
            "input_schema": schema.parameters,
            "already_selected": already_selected,
            "available": "next_model_turn"
        }))
    }
}

struct Features(McpCatalog);

#[async_trait]
impl Tool for Features {
    fn schema(&self) -> ToolSchema {
        ToolSchema {
            name: "mcp_features".into(),
            description: "Use MCP resources, resource templates, prompts, and argument completion on an exact configured server.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["resource_list", "resource_templates", "resource_read", "prompt_list", "prompt_get", "prompt_complete", "resource_complete"] },
                    "server": { "type": "string" },
                    "cursor": { "type": "string" },
                    "uri": { "type": "string" },
                    "name": { "type": "string" },
                    "arguments": { "type": "object", "additionalProperties": { "type": "string" } },
                    "argument": {
                        "type": "object",
                        "properties": { "name": { "type": "string" }, "value": { "type": "string" } },
                        "required": ["name", "value"],
                        "additionalProperties": false
                    },
                    "context": { "type": "object" }
                },
                "required": ["action", "server"],
                "additionalProperties": false
            }),
        }
    }

    fn concurrency(&self, input: &Value) -> Concurrency {
        Concurrency::Keyed(format!("mcp:{}", input["server"].as_str().unwrap_or("")))
    }

    async fn call(&self, input: Value, ctx: &ToolContext) -> Result<Value, ToolError> {
        require_object(&input)?;
        reject_unknown(
            &input,
            &[
                "action",
                "server",
                "cursor",
                "uri",
                "name",
                "arguments",
                "argument",
                "context",
            ],
        )?;
        let action = required_str(&input, "action")?;
        validate_feature_fields(action, &input)?;
        let server = required_str(&input, "server")?;
        let client = self.0.client(server)?;
        require_capability(&client, action)?;
        let (method, params) = feature_request(action, &input)?;
        request_with_context(&client, method, params, ctx).await
    }
}

fn validate_feature_fields(action: &str, input: &Value) -> Result<(), ToolError> {
    let specific = match action {
        "resource_list" | "resource_templates" | "prompt_list" => &["cursor"][..],
        "resource_read" => &["uri"][..],
        "prompt_get" => &["name", "arguments"][..],
        "prompt_complete" => &["name", "argument", "context"][..],
        "resource_complete" => &["uri", "argument", "context"][..],
        _ => {
            return Err(ToolError::msg(format!(
                "unknown MCP feature action: {action}"
            )))
        }
    };
    let mut allowed = vec!["action", "server"];
    allowed.extend_from_slice(specific);
    reject_unknown(input, &allowed)
}

fn supports_action(capabilities: &super::client::ServerCapabilities, action: &str) -> bool {
    match action {
        "resource_list" | "resource_templates" | "resource_read" => capabilities.resources,
        "prompt_list" | "prompt_get" => capabilities.prompts,
        "prompt_complete" => capabilities.prompts && capabilities.completions,
        "resource_complete" => capabilities.resources && capabilities.completions,
        _ => true,
    }
}

fn require_capability(client: &McpClient, action: &str) -> Result<(), ToolError> {
    let supported = supports_action(client.capabilities(), action);
    supported.then_some(()).ok_or_else(|| {
        ToolError::msg(format!(
            "MCP server {} does not advertise support for {action}",
            client.server()
        ))
    })
}

fn feature_request<'a>(action: &'a str, input: &Value) -> Result<(&'a str, Value), ToolError> {
    let cursor_params = || match optional_str(input, "cursor")? {
        Some(cursor) => Ok(json!({ "cursor": cursor })),
        None => Ok(json!({})),
    };
    Ok(match action {
        "resource_list" => ("resources/list", cursor_params()?),
        "resource_templates" => ("resources/templates/list", cursor_params()?),
        "resource_read" => (
            "resources/read",
            json!({ "uri": required_str(input, "uri")? }),
        ),
        "prompt_list" => ("prompts/list", cursor_params()?),
        "prompt_get" => {
            let mut params = Map::new();
            params.insert("name".into(), json!(required_str(input, "name")?));
            if let Some(arguments) = input.get("arguments") {
                let arguments = arguments
                    .as_object()
                    .filter(|values| values.values().all(Value::is_string))
                    .ok_or_else(|| ToolError::msg("missing or invalid arguments"))?;
                params.insert("arguments".into(), Value::Object(arguments.clone()));
            }
            ("prompts/get", Value::Object(params))
        }
        "prompt_complete" | "resource_complete" => {
            let reference = if action == "prompt_complete" {
                json!({ "type": "ref/prompt", "name": required_str(input, "name")? })
            } else {
                json!({ "type": "ref/resource", "uri": required_str(input, "uri")? })
            };
            let mut params = Map::new();
            params.insert("ref".into(), reference);
            let argument = input
                .get("argument")
                .and_then(Value::as_object)
                .ok_or_else(|| ToolError::msg("missing or invalid argument"))?;
            if argument.keys().any(|key| key != "name" && key != "value")
                || argument
                    .get("name")
                    .and_then(Value::as_str)
                    .is_none_or(|name| name.trim().is_empty())
                || argument.get("value").and_then(Value::as_str).is_none()
            {
                return Err(ToolError::msg("missing or invalid argument"));
            }
            params.insert("argument".into(), Value::Object(argument.clone()));
            if let Some(context) = input.get("context") {
                if !context.is_object() {
                    return Err(ToolError::msg("missing or invalid context"));
                }
                params.insert("context".into(), context.clone());
            }
            ("completion/complete", Value::Object(params))
        }
        _ => {
            return Err(ToolError::msg(format!(
                "unknown MCP feature action: {action}"
            )))
        }
    })
}

fn require_object(input: &Value) -> Result<&Map<String, Value>, ToolError> {
    input
        .as_object()
        .ok_or_else(|| ToolError::msg("input must be an object"))
}

fn reject_unknown(input: &Value, allowed: &[&str]) -> Result<(), ToolError> {
    let object = require_object(input)?;
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(ToolError::msg(format!("unknown input field: {key}")));
    }
    Ok(())
}

fn optional_str<'a>(input: &'a Value, key: &str) -> Result<Option<&'a str>, ToolError> {
    match input.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_str()
            .filter(|value| !value.trim().is_empty())
            .map(Some)
            .ok_or_else(|| ToolError::msg(format!("missing or invalid {key}"))),
    }
}

fn optional_u64(
    input: &Value,
    key: &str,
    minimum: u64,
    maximum: u64,
) -> Result<Option<u64>, ToolError> {
    match input.get(key) {
        None => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|value| (minimum..=maximum).contains(value))
            .map(Some)
            .ok_or_else(|| ToolError::msg(format!("missing or invalid {key}"))),
    }
}

fn required_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, ToolError> {
    optional_str(input, key)?.ok_or_else(|| ToolError::msg(format!("missing or invalid {key}")))
}

#[cfg(test)]
#[path = "catalog/tests.rs"]
mod tests;
