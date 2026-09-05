//! Host-independent subagent policy; hosts retain lifecycle and approval ordering.

use std::sync::Arc;

use orca_harness_core::{Extension, Model};
use orca_harness_extensions::Truncation;
use orca_harness_tool_extensions::mcp::{McpCatalog, McpModel};
use orca_harness_tools::{
    MutationPreflight, SubagentDepth, SubagentModel, SubagentTool, Workspace,
};

/// The inherited model is already wrapped by its host. Only routed choices need
/// the shared MCP catalog, so inherited calls never acquire a second wrapper.
pub(crate) fn tool(
    inherited: Arc<dyn Model>,
    workspace: &Workspace,
    identity: (&str, &str),
    settings: &SubagentDepth,
    choices: Vec<SubagentModel<Arc<dyn Model>>>,
    catalog: McpCatalog,
) -> SubagentTool<Arc<dyn Model>> {
    // The catalog goes on before the handle is shared: `max_depth`
    // publishes whatever models the builder holds at that moment, and an
    // empty list would drop the user's `/subagents` route and preferred
    // models on every rebuild (/clear, /model, /provider, reloads).
    let tool = SubagentTool::new(inherited, workspace)
        .inherited_identity(identity.0, identity.1)
        .models(choices.into_iter().map(|choice| SubagentModel {
            model: Arc::new(McpModel::new(choice.model, catalog.clone())) as Arc<dyn Model>,
            ..choice
        }))
        .max_depth(settings.clone());
    crate::subagent_settings::configure_tool_retry(tool, settings)
}

/// Mutation validation precedes host hooks, and truncation follows them. The
/// host supplies hooks in its established order (including approval and plugins).
pub(crate) fn extensions(
    settings: &SubagentDepth,
    host_hooks: impl IntoIterator<Item = Arc<dyn Extension>>,
) -> Vec<Arc<dyn Extension>> {
    let mut extensions = vec![Arc::new(MutationPreflight) as Arc<dyn Extension>];
    extensions.extend(host_hooks);
    if settings.output_chars() != 0 {
        extensions.push(Arc::new(Truncation::new(settings.output_chars() as usize)));
    }
    extensions
}

#[cfg(test)]
mod tests {
    use super::*;
    use orca_harness_core::testing::ScriptedModel;

    fn choices() -> Vec<SubagentModel<Arc<dyn Model>>> {
        ["local/a", "local/b", "flash/a", "flash/b"]
            .into_iter()
            .map(|id| {
                SubagentModel::new(
                    id,
                    id,
                    Arc::new(ScriptedModel::new(vec![])) as Arc<dyn Model>,
                )
            })
            .collect()
    }

    /// /clear, /model, /provider and every reload rebuild the agent, and
    /// with it this tool. The user's `/subagents` route and preferred
    /// models live in the shared handle and must come through unchanged.
    #[test]
    fn rebuilding_the_tool_keeps_route_and_preferred_models() {
        let settings = SubagentDepth::new(1);
        let ws = Workspace::new(std::env::temp_dir());
        let inherited: Arc<dyn Model> = Arc::new(ScriptedModel::new(vec![]));
        let build = || {
            tool(
                inherited.clone(),
                &ws,
                ("local", "m"),
                &settings,
                choices(),
                McpCatalog::new(),
            )
        };
        let _first = build();
        assert!(settings.set_preferred_model("flash", "flash/b".into()));
        assert!(settings.set_preferred_model("local", "local/b".into()));
        assert!(settings.set_model_route(Some("flash".into())));

        let _rebuilt = build();
        assert_eq!(settings.model_route().as_deref(), Some("flash"));
        assert_eq!(
            settings.preferred_model("flash").as_deref(),
            Some("flash/b")
        );
        assert_eq!(
            settings.preferred_model("local").as_deref(),
            Some("local/b")
        );
    }

    #[test]
    fn host_hooks_keep_order_and_output_policy_reads_live_settings() {
        let settings = SubagentDepth::default();
        let hooks = || {
            vec![
                Arc::new(Truncation::new(17)) as Arc<dyn Extension>,
                Arc::new(MutationPreflight) as Arc<dyn Extension>,
            ]
        };
        settings.set_output_chars(0);
        let names = |extensions: Vec<Arc<dyn Extension>>| {
            extensions
                .iter()
                .map(|ext| ext.name().to_owned())
                .collect::<Vec<_>>()
        };
        let disabled = names(extensions(&settings, hooks()));
        assert_eq!(
            disabled,
            [
                MutationPreflight.name(),
                "truncation",
                MutationPreflight.name()
            ]
        );
        settings.set_output_chars(1024);
        let enabled = names(extensions(&settings, hooks()));
        assert_eq!(&enabled[..disabled.len()], disabled);
        assert_eq!(enabled.last().unwrap(), "truncation");
        assert_eq!(enabled.len(), disabled.len() + 1);
    }
}
