use orca_harness_tools::SubagentDepth;

pub(crate) const SUBAGENT_ROWS: usize = SubagentSetting::ALL.len();

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubagentSetting {
    Route,
    LocalModel,
    FlashModel,
    MidModel,
    FrontierModel,
    Depth,
    Background,
    Steps,
    Timeout,
    Output,
    ToolAttempts,
    Backoff,
    ParallelTools,
    ModelConcurrency,
    ModelAttempts,
    ModelBackoff,
    ModelMaxBackoff,
}

impl SubagentSetting {
    pub(crate) const ALL: &[Self] = &[
        Self::Route,
        Self::LocalModel,
        Self::FlashModel,
        Self::MidModel,
        Self::FrontierModel,
        Self::Depth,
        Self::Background,
        Self::Steps,
        Self::Timeout,
        Self::Output,
        Self::ToolAttempts,
        Self::Backoff,
        Self::ParallelTools,
        Self::ModelConcurrency,
        Self::ModelAttempts,
        Self::ModelBackoff,
        Self::ModelMaxBackoff,
    ];
}

/// Shared by persistence, preset selection, and custom numeric entry.
pub(crate) struct NumericSetting {
    pub setting: SubagentSetting,
    pub key: &'static str,
    pub label: &'static str,
    pub presets: &'static [u32],
    pub unit: &'static str,
    pub unlimited: bool,
    pub read: fn(&SubagentDepth) -> Option<u32>,
    pub write: fn(&SubagentDepth, u32) -> u32,
    pub default: fn() -> u32,
}

impl NumericSetting {
    pub fn parse(&self, input: &str) -> Result<u32, &'static str> {
        let value = if self.unlimited && input.trim().eq_ignore_ascii_case("unlimited") {
            0
        } else {
            input
                .trim()
                .parse::<u32>()
                .map_err(|_| "Enter a whole number")?
        };
        if value == 0
            && !self.unlimited
            && !matches!(
                self.setting,
                SubagentSetting::Backoff | SubagentSetting::ModelBackoff
            )
        {
            return Err("Enter a positive whole number");
        }
        Ok(value)
    }

    pub fn current(&self, settings: &SubagentDepth) -> String {
        let value = (self.read)(settings).unwrap_or_else(self.default);
        if value == 0 && self.unlimited {
            "unlimited".into()
        } else {
            value.to_string()
        }
    }
}

impl SubagentSetting {
    pub fn numeric(self) -> Option<&'static NumericSetting> {
        NUMERIC.iter().find(|field| field.setting == self)
    }

    pub fn applies(self) -> &'static str {
        match self {
            Self::ModelConcurrency => "shared with parent; queued requests update immediately",
            Self::ModelAttempts | Self::ModelBackoff | Self::ModelMaxBackoff => {
                "shared with parent; next retry decision"
            }
            _ => "next spawn",
        }
    }
}

// Suggested choices only; custom entries are validated independently.
const CONCURRENCY: &[u32] = &[1, 2, 4, 8, 16, 32, 64];
const ATTEMPTS: &[u32] = &[1, 2, 3, 5, 10];
const DELAYS: &[u32] = &[0, 100, 250, 500, 1_000, 2_000];

pub(crate) const NUMERIC: &[NumericSetting] = &[
    NumericSetting {
        setting: SubagentSetting::Depth,
        presets: &[1, 2, 3, 4, 5],
        key: "depth",
        label: "nesting depth",
        unit: "levels",
        unlimited: false,
        read: |s| Some(s.get()),
        write: SubagentDepth::set,
        default: || 0,
    },
    NumericSetting {
        setting: SubagentSetting::Background,
        presets: CONCURRENCY,
        key: "background_limit",
        label: "background agents",
        unit: "at once",
        unlimited: true,
        read: |s| Some(s.background_limit()),
        write: SubagentDepth::set_background_limit,
        default: || 0,
    },
    NumericSetting {
        setting: SubagentSetting::Steps,
        presets: &[6, 12, 18, 24, 36, 48, 72, 96],
        key: "max_steps",
        label: "model steps",
        unit: "steps per worker",
        unlimited: false,
        read: |s| Some(s.max_steps()),
        write: SubagentDepth::set_max_steps,
        default: || 0,
    },
    NumericSetting {
        setting: SubagentSetting::Timeout,
        presets: &[30, 60, 180, 300, 600, 1_800],
        key: "timeout_secs",
        label: "wall clock",
        unit: "seconds per worker",
        unlimited: true,
        read: |s| Some(s.timeout_secs()),
        write: SubagentDepth::set_timeout_secs,
        default: || 0,
    },
    NumericSetting {
        setting: SubagentSetting::Output,
        presets: &[1_000, 4_000, 8_000, 16_000, 32_000, 64_000],
        key: "output_chars",
        label: "tool output",
        unit: "characters per result",
        unlimited: true,
        read: |s| Some(s.output_chars()),
        write: SubagentDepth::set_output_chars,
        default: || 0,
    },
    NumericSetting {
        setting: SubagentSetting::ToolAttempts,
        presets: ATTEMPTS,
        key: "tool_attempts",
        label: "tool attempts",
        unit: "total attempts",
        unlimited: false,
        read: |s| Some(s.tool_attempts()),
        write: SubagentDepth::set_tool_attempts,
        default: || 0,
    },
    NumericSetting {
        setting: SubagentSetting::Backoff,
        presets: DELAYS,
        key: "retry_backoff_ms",
        label: "tool retry delay",
        unit: "milliseconds",
        unlimited: false,
        read: |s| Some(s.retry_backoff_ms()),
        write: SubagentDepth::set_retry_backoff_ms,
        default: || 0,
    },
    NumericSetting {
        setting: SubagentSetting::ParallelTools,
        presets: CONCURRENCY,
        key: "parallel_tools",
        label: "parallel tools",
        unit: "per worker",
        unlimited: true,
        read: |s| s.parallel_tools(),
        write: SubagentDepth::set_parallel_tools,
        default: || 0,
    },
    NumericSetting {
        setting: SubagentSetting::ModelConcurrency,
        presets: CONCURRENCY,
        key: "model_concurrency",
        label: "provider streams",
        unit: "per provider/account",
        unlimited: true,
        read: |s| Some(s.model_concurrency()),
        write: SubagentDepth::set_model_concurrency,
        default: || 0,
    },
    NumericSetting {
        setting: SubagentSetting::ModelAttempts,
        presets: ATTEMPTS,
        key: "model_attempts",
        label: "model attempts",
        unit: "total attempts per request",
        unlimited: true,
        read: |s| Some(s.model_attempts()),
        write: SubagentDepth::set_model_attempts,
        default: || 0,
    },
    NumericSetting {
        setting: SubagentSetting::ModelBackoff,
        presets: DELAYS,
        key: "model_backoff_ms",
        label: "model retry delay",
        unit: "milliseconds",
        unlimited: false,
        read: |s| s.model_backoff_ms(),
        write: SubagentDepth::set_model_backoff_ms,
        default: || {
            orca_harness_extensions::ModelRetryConfig::default()
                .backoff
                .as_millis() as u32
        },
    },
    NumericSetting {
        setting: SubagentSetting::ModelMaxBackoff,
        presets: DELAYS,
        key: "model_max_backoff_ms",
        label: "max retry delay",
        unit: "milliseconds; Retry-After takes precedence",
        unlimited: true,
        read: |s| Some(s.model_max_backoff_ms()),
        write: SubagentDepth::set_model_max_backoff_ms,
        default: || 0,
    },
];

pub(crate) fn model_retry_config(
    settings: &SubagentDepth,
) -> orca_harness_extensions::ModelRetryConfig {
    use std::time::Duration;
    let defaults = orca_harness_extensions::ModelRetryConfig::default();
    orca_harness_extensions::ModelRetryConfig {
        max_attempts: match settings.model_attempts() {
            0 => None,
            n => Some(n),
        },
        backoff: settings
            .model_backoff_ms()
            .map_or(defaults.backoff, |n| Duration::from_millis(u64::from(n))),
        max_backoff: match settings.model_max_backoff_ms() {
            0 => None,
            n => Some(Duration::from_millis(u64::from(n))),
        },
    }
}

/// Both hosts attach the same tool retry policy without reloading live settings.
pub(crate) fn configure_tool_retry<M: orca_harness_core::Model + Clone + 'static>(
    tool: orca_harness_tools::SubagentTool<M>,
    settings: &SubagentDepth,
) -> orca_harness_tools::SubagentTool<M> {
    if crate::extensions::enabled("retry") {
        settings.ensure_retry_defaults(
            crate::extensions::TOOL_RETRY_ATTEMPTS,
            crate::extensions::TOOL_RETRY_BACKOFF_MS,
        );
        tool.retry_ok_when(crate::extensions::data_failure)
    } else {
        tool
    }
}

/// Load once per endpoint session; an explicit environment limit wins over storage.
pub(crate) fn configured(depth: u32, model_concurrency: Option<u32>) -> SubagentDepth {
    let settings = SubagentDepth::new(depth);
    settings.set_available_models(
        crate::config::stored_subagent_models()
            .into_iter()
            .map(|(tier, selection)| format!("{tier}/{}/{}", selection.provider, selection.model))
            .collect(),
    );
    crate::config::load_subagent_settings(&settings);
    if let Some(limit) = model_concurrency {
        settings.set_model_concurrency(limit);
    }
    settings
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_concurrency_overrides_storage_and_clones_stay_live() {
        let saved = SubagentDepth::new(2);
        saved.set_model_concurrency(3);
        crate::config::save_subagent_settings(&saved).unwrap();
        let settings = configured(1, Some(7));
        assert_eq!(settings.get(), 2);
        assert_eq!(settings.model_concurrency(), 7);
        let host = settings.clone();
        settings.set_model_concurrency(11);
        assert_eq!(host.model_concurrency(), 11);
        assert_eq!(configured(1, None).model_concurrency(), 3);
        assert_eq!(configured(1, Some(0)).model_concurrency(), 0);
    }

    #[test]
    fn retry_setup_preserves_live_custom_values_on_rebuild() {
        use orca_harness_core::testing::ScriptedModel;
        use orca_harness_tools::{SubagentTool, Workspace};
        crate::config::save_extension("retry", true).unwrap();
        let settings = SubagentDepth::default();
        let ws = Workspace::new(std::env::temp_dir());
        let build = || {
            SubagentTool::new(std::sync::Arc::new(ScriptedModel::new(vec![])), &ws)
                .max_depth(settings.clone())
        };
        let _tool = configure_tool_retry(build(), &settings);
        assert_eq!(
            settings.tool_attempts(),
            crate::extensions::TOOL_RETRY_ATTEMPTS
        );
        settings.set_tool_attempts(8);
        settings.set_retry_backoff_ms(0);
        let _rebuilt = configure_tool_retry(build(), &settings);
        assert_eq!(settings.tool_attempts(), 8);
        assert_eq!(settings.retry_backoff_ms(), 0);
    }
}
