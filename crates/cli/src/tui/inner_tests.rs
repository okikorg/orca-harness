mod tests {
    const TEST_TERMINAL_WIDTH: usize = 80;

    include!("tests/paste_and_images.rs");
    include!("tests/interaction_and_copy.rs");
    include!("tests/inspector.rs");
    include!("tests/core_2.rs");
    include!("tests/core_3.rs");
    include!("tests/core_4.rs");
    include!("tests/core_5.rs");
    include!("tests/core_6.rs");
    include!("tests/agent_browser.rs");
    include!("tests/workflow_panel.rs");
    include!("tests/background_subagents.rs");
    include!("tests/agent_browser_cache.rs");
    include!("tests/subagent_history.rs");
    include!("tests/render_frames.rs");
}

include!("tests/theme_subagent_mode.rs");
include!("tests/subagent_settings.rs");
include!("tests/subagent_routes.rs");
include!("tests/extensions.rs");
include!("tests/mcp_stats_nested.rs");
include!("tests/plugins.rs");
include!("tests/skills.rs");
include!("tests/refine.rs");
