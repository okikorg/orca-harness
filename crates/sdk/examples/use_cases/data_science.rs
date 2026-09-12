//! Data science agent: an analyst over a table it is allowed to read.
//!
//! The agent answers a question about a sales extract by grouping and
//! summarizing it, then writes the finding in a sentence a human can act
//! on. The tools do real arithmetic over a real CSV in the workspace, so
//! the numbers in the transcript are computed rather than narrated - the
//! property that separates an analysis agent from one that guesses.
//!
//! The agent definition is the first thing in `main`: system prompt,
//! analysis tools, and the read-only tool preset that keeps it from
//! editing the data it is measuring. Everything below `main` is
//! scaffolding.
//!
//! Deterministic: the model is scripted and never calls a provider.

#[path = "../support/mod.rs"]
mod support;

use std::path::PathBuf;

use orca_harness_core::testing::{call, ScriptedModel};
use orca_harness_core::{FnTool, ModelResponse, ToolError};
use orca_harness_extensions::HarnessEvent;
use orca_harness_sdk::{Harness, RunRequest, ToolPreset};
use serde_json::json;
use support::TempWorkspace;

const ANALYST_PROMPT: &str = "\
You are a data analyst working over CSV extracts in the workspace.
- Use `group_by` to aggregate a metric across a category.
- Use `describe` for the distribution of a single numeric column.
- Never state a number you did not compute with a tool, and always say
  which column and how many rows it came from.";

const SALES_CSV: &str = "\
region,channel,revenue_usd
north,online,1840.50
north,retail,910.00
south,online,2210.75
south,retail,415.25
east,online,980.00
east,retail,1502.00
west,online,3120.40
west,retail,245.60
";

/// Groups a CSV by one column and aggregates another, for real. Reading
/// happens under the workspace root the agent was built with.
fn group_by(root: PathBuf) -> FnTool {
    FnTool::new(
        "group_by",
        "Sum a numeric column of a workspace CSV, grouped by a category column.",
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string" },
                "group_column": { "type": "string" },
                "metric_column": { "type": "string" }
            },
            "required": ["file", "group_column", "metric_column"]
        }),
        move |args, _ctx| {
            let root = root.clone();
            async move {
                let table = read_csv(&root, args["file"].as_str().unwrap_or_default())?;
                let group = column_index(&table.header, &args, "group_column")?;
                let metric = column_index(&table.header, &args, "metric_column")?;

                let mut totals: Vec<(String, f64)> = Vec::new();
                for row in &table.rows {
                    let key = row[group].clone();
                    let value: f64 = row[metric].parse().unwrap_or_default();
                    match totals.iter_mut().find(|(name, _)| *name == key) {
                        Some((_, sum)) => *sum += value,
                        None => totals.push((key, value)),
                    }
                }
                totals.sort_by(|a, b| b.1.total_cmp(&a.1));

                Ok(json!({
                    "rows_scanned": table.rows.len(),
                    "groups": totals
                        .iter()
                        .map(|(name, sum)| json!({ "group": name, "total": sum }))
                        .collect::<Vec<_>>()
                }))
            }
        },
    )
}

/// Describes one numeric column: count, min, max, mean, median.
fn describe(root: PathBuf) -> FnTool {
    FnTool::new(
        "describe",
        "Summarize the distribution of a numeric column in a workspace CSV.",
        json!({
            "type": "object",
            "properties": {
                "file": { "type": "string" },
                "column": { "type": "string" }
            },
            "required": ["file", "column"]
        }),
        move |args, _ctx| {
            let root = root.clone();
            async move {
                let table = read_csv(&root, args["file"].as_str().unwrap_or_default())?;
                let index = column_index(&table.header, &args, "column")?;

                let mut values: Vec<f64> = table
                    .rows
                    .iter()
                    .filter_map(|row| row[index].parse::<f64>().ok())
                    .collect();
                if values.is_empty() {
                    return Err(ToolError::msg("column holds no numeric values"));
                }
                values.sort_by(f64::total_cmp);

                let count = values.len();
                let sum: f64 = values.iter().sum();
                let median = if count.is_multiple_of(2) {
                    (values[count / 2 - 1] + values[count / 2]) / 2.0
                } else {
                    values[count / 2]
                };

                Ok(json!({
                    "count": count,
                    "min": values[0],
                    "max": values[count - 1],
                    "mean": sum / count as f64,
                    "median": median
                }))
            }
        },
    )
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempWorkspace::new("data-science-agent");
    std::fs::write(workspace.path().join("sales.csv"), SALES_CSV)?;
    let harness = Harness::builder().workspace(workspace.path()).build()?;
    let root = harness.workspace().root().to_path_buf();

    // The agent definition: what it is, what it can do, what it may not do.
    // `ToolPreset::ReadOnly` adds file inspection without giving an agent
    // that measures data any way to change it.
    let agent = harness
        .agent(analysis_script())
        .name("analyst")
        .system_prompt(ANALYST_PROMPT)
        .tools(ToolPreset::ReadOnly)
        .tool(group_by(root.clone()))
        .tool(describe(root))
        .events(true)
        .build()?;

    let request = RunRequest::new(
        "Using sales.csv, which region brings in the most revenue, and how spread out \
         are the individual sales?",
    )
    .on_event(|event| {
        if let HarnessEvent::ToolResult {
            tool_name, output, ..
        } = event
        {
            println!("  [computed] {tool_name} -> {output}");
        }
    });

    let result = agent.run(request).await?;
    println!("\n{}", result.text);
    assert!(
        result.text.contains("west"),
        "expected the top region in the finding"
    );

    Ok(())
}

/// A parsed CSV: the header row and the data rows, split on commas.
struct Table {
    header: Vec<String>,
    rows: Vec<Vec<String>>,
}

fn read_csv(root: &PathBuf, file: &str) -> Result<Table, ToolError> {
    let path = root.join(file);
    if !path.starts_with(root) {
        return Err(ToolError::msg(format!("{file} is outside the workspace")));
    }
    let text =
        std::fs::read_to_string(&path).map_err(|err| ToolError::msg(format!("{file}: {err}")))?;
    let mut lines = text.lines().filter(|line| !line.trim().is_empty());
    let header = lines
        .next()
        .ok_or_else(|| ToolError::msg(format!("{file} is empty")))?
        .split(',')
        .map(|cell| cell.trim().to_string())
        .collect();
    let rows = lines
        .map(|line| {
            line.split(',')
                .map(|cell| cell.trim().to_string())
                .collect()
        })
        .collect();
    Ok(Table { header, rows })
}

fn column_index(
    header: &[String],
    args: &serde_json::Value,
    field: &str,
) -> Result<usize, ToolError> {
    let name = args[field].as_str().unwrap_or_default();
    header
        .iter()
        .position(|column| column == name)
        .ok_or_else(|| ToolError::msg(format!("no column named {name}")))
}

/// Aggregate, then describe, then report - the shape an analyst answer
/// takes when every number in it has to come from a tool.
fn analysis_script() -> ScriptedModel {
    ScriptedModel::new(vec![
        ModelResponse::ToolCalls {
            content: Some("Totaling revenue by region.".into()),
            calls: vec![call(
                "d1",
                "group_by",
                json!({
                    "file": "sales.csv",
                    "group_column": "region",
                    "metric_column": "revenue_usd"
                }),
            )],
            usage: None,
        },
        ModelResponse::ToolCalls {
            content: Some("Now the spread of individual sales.".into()),
            calls: vec![call(
                "d2",
                "describe",
                json!({ "file": "sales.csv", "column": "revenue_usd" }),
            )],
            usage: None,
        },
        ModelResponse::final_text(
            "Across 8 rows of revenue_usd in sales.csv, west leads at $3,366.00, ahead of \
             north ($2,750.50), south ($2,626.00) and east ($2,482.00). Individual sales \
             range from $245.60 to $3,120.40 with a median of $1,241.00 - the spread is \
             wide enough that west's lead rests largely on one online sale.",
        ),
    ])
}
