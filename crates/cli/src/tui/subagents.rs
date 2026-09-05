use super::state::SubagentSetting;
use orca_harness_tools::SubagentDepth;

pub(crate) const CUSTOM_VALUE: &str = "custom…";

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
    if let Some(field) = setting.numeric() {
        return field.label;
    }
    match setting {
        SubagentSetting::Route => "default route",
        SubagentSetting::LocalModel => "local model",
        SubagentSetting::FlashModel => "flash model",
        SubagentSetting::MidModel => "mid model",
        SubagentSetting::FrontierModel => "frontier model",
        _ => unreachable!(),
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
    setting
        .numeric()
        .expect("numeric setting")
        .current(settings)
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
    let field = setting.numeric().expect("numeric setting");
    let current = (field.read)(settings).unwrap_or_else(field.default);
    let mut numbers = field.presets.to_vec();
    numbers.push(current);
    if field.unlimited {
        numbers.push(0);
    }
    numbers.sort_unstable();
    numbers.dedup();
    let mut values: Vec<_> = numbers
        .into_iter()
        .map(|n| {
            if n == 0 && field.unlimited {
                "unlimited".into()
            } else {
                n.to_string()
            }
        })
        .collect();
    values.push(CUSTOM_VALUE.into());
    values
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
        subagent_current(settings, setting)
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
    if let Some(field) = setting.numeric() {
        if let Ok(value) = field.parse(value) {
            (field.write)(settings, value);
        }
    }
}

pub(crate) fn save_subagent_note(settings: &SubagentDepth, setting: SubagentSetting) -> String {
    let value = subagent_current(settings, setting);
    match crate::config::save_subagent_settings(settings) {
        Ok(_) => format!(
            "subagent {} set to {value} (saved; {})",
            subagent_setting_label(setting),
            setting.applies()
        ),
        Err(err) => format!(
            "subagent {} set to {value} for this session (save failed: {err})",
            subagent_setting_label(setting)
        ),
    }
}
