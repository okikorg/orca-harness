use super::state::SubagentSetting;
use orca_harness_tools::SubagentDepth;

fn tier(setting: SubagentSetting) -> Option<&'static str> {
    match setting {
        SubagentSetting::LocalModel => Some("local"),
        SubagentSetting::FlashModel => Some("flash"),
        SubagentSetting::MidModel => Some("mid"),
        SubagentSetting::FrontierModel => Some("frontier"),
        _ => None,
    }
}

pub(crate) fn subagent_setting_label(setting: SubagentSetting) -> &'static str {
    match setting {
        SubagentSetting::Route => "default route",
        SubagentSetting::LocalModel => "local model",
        SubagentSetting::FlashModel => "flash model",
        SubagentSetting::MidModel => "mid model",
        SubagentSetting::FrontierModel => "frontier model",
        SubagentSetting::Depth => "nesting depth",
        SubagentSetting::Steps => "model steps",
        SubagentSetting::Timeout => "wall clock",
        SubagentSetting::Output => "tool output",
        SubagentSetting::ToolAttempts => "tool attempts",
        SubagentSetting::Backoff => "retry backoff",
    }
}

pub(crate) fn subagent_current(settings: &SubagentDepth, setting: SubagentSetting) -> String {
    if setting == SubagentSetting::Route {
        return settings.model_route().unwrap_or_else(|| "inherit".into());
    }
    if let Some(tier) = tier(setting) {
        return settings
            .preferred_model(tier)
            .unwrap_or_else(|| "unavailable".into());
    }
    match setting {
        SubagentSetting::Depth => settings.get().to_string(),
        SubagentSetting::Steps => settings.max_steps().to_string(),
        SubagentSetting::Timeout => format!("{}s", settings.timeout_secs()),
        SubagentSetting::Output => format!("{} chars", settings.output_chars()),
        SubagentSetting::ToolAttempts => settings.tool_attempts().to_string(),
        SubagentSetting::Backoff => format!("{}ms", settings.retry_backoff_ms()),
        _ => unreachable!(),
    }
}

pub(crate) fn subagent_values(settings: &SubagentDepth, setting: SubagentSetting) -> Vec<String> {
    if setting == SubagentSetting::Route {
        let mut routes = vec![
            "inherit".to_string(),
            orca_harness_tools::AUTO_SUBAGENT_ROUTE.to_string(),
            orca_harness_tools::PREFERENCE_SUBAGENT_ROUTE.to_string(),
        ];
        routes.extend(
            ["local", "flash", "mid", "frontier"]
                .into_iter()
                .filter(|tier| !settings.models_for_tier(tier).is_empty())
                .map(str::to_string),
        );
        return routes;
    }
    if let Some(tier) = tier(setting) {
        return settings.models_for_tier(tier);
    }
    let numbers: &[u32] = match setting {
        SubagentSetting::Depth => &[1, 2, 3, 4, 5],
        SubagentSetting::Steps => &[6, 12, 18, 24, 36, 48, 72, 96],
        SubagentSetting::Timeout => &[30, 60, 180, 300, 600, 1_800],
        SubagentSetting::Output => &[1_000, 4_000, 8_000, 16_000, 32_000, 64_000],
        SubagentSetting::ToolAttempts => &[1, 2, 3, 5, 10],
        SubagentSetting::Backoff => &[0, 100, 250, 500, 1_000, 2_000],
        _ => unreachable!(),
    };
    numbers.iter().map(u32::to_string).collect()
}

pub(crate) fn subagent_route_description(route: &str) -> &'static str {
    match route {
        "inherit" => "always use the parent model",
        orca_harness_tools::AUTO_SUBAGENT_ROUTE => "model chooses per subagent",
        orca_harness_tools::PREFERENCE_SUBAGENT_ROUTE => {
            "model chooses only from your preferred models"
        }
        "local" => "always use the preferred local model",
        "flash" => "always use the preferred fast cloud model",
        "mid" => "always use the preferred balanced cloud model",
        "frontier" => "always use the preferred strongest cloud model",
        _ => "approved worker route",
    }
}

pub(crate) fn subagent_selected(
    settings: &SubagentDepth,
    setting: SubagentSetting,
    values: &[String],
) -> usize {
    let current = if setting == SubagentSetting::Route {
        settings.model_route().unwrap_or_else(|| "inherit".into())
    } else if let Some(tier) = tier(setting) {
        settings.preferred_model(tier).unwrap_or_default()
    } else {
        match setting {
            SubagentSetting::Depth => settings.get().to_string(),
            SubagentSetting::Steps => settings.max_steps().to_string(),
            SubagentSetting::Timeout => settings.timeout_secs().to_string(),
            SubagentSetting::Output => settings.output_chars().to_string(),
            SubagentSetting::ToolAttempts => settings.tool_attempts().to_string(),
            SubagentSetting::Backoff => settings.retry_backoff_ms().to_string(),
            _ => unreachable!(),
        }
    };
    values
        .iter()
        .position(|value| value == &current)
        .unwrap_or(0)
}

pub(crate) fn apply_subagent_value(
    settings: &SubagentDepth,
    setting: SubagentSetting,
    value: &str,
) {
    if setting == SubagentSetting::Route {
        settings.set_model_route((value != "inherit").then(|| value.to_string()));
        return;
    }
    if let Some(tier) = tier(setting) {
        settings.set_preferred_model(tier, value.to_string());
        return;
    }
    let Ok(value) = value.parse::<u32>() else {
        return;
    };
    match setting {
        SubagentSetting::Depth => {
            settings.set(value);
        }
        SubagentSetting::Steps => {
            settings.set_max_steps(value);
        }
        SubagentSetting::Timeout => {
            settings.set_timeout_secs(value);
        }
        SubagentSetting::Output => {
            settings.set_output_chars(value);
        }
        SubagentSetting::ToolAttempts => {
            settings.set_tool_attempts(value);
        }
        SubagentSetting::Backoff => {
            settings.set_retry_backoff_ms(value);
        }
        _ => unreachable!(),
    }
}
