//! Startup flags resolve to one mode, the safest one asked for.

use std::path::PathBuf;

use super::Mode;
use crate::msg::Provider;
use crate::Config;

/// Ambiguous startup combinations resolve toward the safer mode.
#[test]
fn startup_flags_pick_the_safest_requested_mode() {
    let base = Config {
        provider: Provider::Local,
        model: "m".into(),
        base_url: "u".into(),
        api_key: None,
        firecrawl_key: None,
        openrouter: false,
        list_models: false,
        list_providers: false,
        workspace: PathBuf::from("."),
        add_dirs: Vec::new(),
        prompt: None,
        prompt_source: None,
        mcp_config: Vec::new(),
        approvals: Default::default(),
        read_only: false,
        images: Vec::new(),
        json: false,
        auto_approve: false,
        max_steps: 4,
        max_output_tokens: None,
        reasoning_effort: None,
        prompt_cache: false,
        tools: None,
        bare: false,
        subagent_depth: 1,
        continue_latest: false,
        resume_id: None,
        no_session: true,
        theme: "default".into(),
        plan: false,
        orchestrate: false,
        normal: false,
        auto: false,
        yolo: false,
    };
    assert_eq!(base.mode(), Mode::Normal);
    assert_eq!(
        Config {
            plan: true,
            ..base.clone()
        }
        .mode(),
        Mode::Plan
    );
    assert_eq!(
        Config {
            orchestrate: true,
            ..base.clone()
        }
        .mode(),
        Mode::Orchestrate
    );
    assert_eq!(
        Config {
            normal: true,
            ..base.clone()
        }
        .mode(),
        Mode::Normal
    );
    assert_eq!(
        Config {
            auto: true,
            ..base.clone()
        }
        .mode(),
        Mode::Auto
    );
    assert_eq!(
        Config {
            yolo: true,
            ..base.clone()
        }
        .mode(),
        Mode::Yolo
    );
    // Orchestrate outranks auto/yolo, normal outranks auto/yolo, and
    // plan outranks all four.
    assert_eq!(
        Config {
            normal: true,
            auto: true,
            yolo: true,
            ..base.clone()
        }
        .mode(),
        Mode::Normal
    );
    assert_eq!(
        Config {
            orchestrate: true,
            auto: true,
            yolo: true,
            ..base.clone()
        }
        .mode(),
        Mode::Orchestrate
    );
    // Orchestrate also outranks normal: it is normal's stance with the
    // top-level agent's mutations removed.
    assert_eq!(
        Config {
            normal: true,
            orchestrate: true,
            ..base.clone()
        }
        .mode(),
        Mode::Orchestrate
    );
    assert_eq!(
        Config {
            auto: true,
            yolo: true,
            ..base.clone()
        }
        .mode(),
        Mode::Auto
    );
    assert_eq!(
        Config {
            plan: true,
            normal: true,
            orchestrate: true,
            auto: true,
            yolo: true,
            ..base
        }
        .mode(),
        Mode::Plan
    );
}
