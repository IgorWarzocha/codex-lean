//! Curated user settings shared by CLI recovery commands and TUI pickers.
//! These are config edits, not live thread overrides or checkpoint migrations.

use serde_json::Value;
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CliSetting {
    Context,
    Retention,
    IdleRollover,
    Runtime,
    QuestionsOutsidePlan,
    MultiAgent,
    WaitAgent,
    Screenless,
    Acknowledgements,
    AutoResume,
    VoiceRefresh,
}

impl CliSetting {
    pub const ALL: [Self; 11] = [
        Self::Context,
        Self::Retention,
        Self::IdleRollover,
        Self::Runtime,
        Self::QuestionsOutsidePlan,
        Self::MultiAgent,
        Self::WaitAgent,
        Self::Screenless,
        Self::Acknowledgements,
        Self::AutoResume,
        Self::VoiceRefresh,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Context => "context",
            Self::Retention => "compaction-retention",
            Self::IdleRollover => "notes-idle-rollover",
            Self::Runtime => "code-mode",
            Self::QuestionsOutsidePlan => "questions-outside-plan",
            Self::MultiAgent => "multi-agent",
            Self::WaitAgent => "subagent-wait",
            Self::Screenless => "voice-screenless",
            Self::Acknowledgements => "voice-acknowledgements",
            Self::AutoResume => "voice-auto-resume",
            Self::VoiceRefresh => "voice-context-refresh",
        }
    }

    pub fn title(self) -> &'static str {
        match self {
            Self::Context => "Context management",
            Self::Retention => "Compaction retention",
            Self::IdleRollover => "Notes idle rollover",
            Self::Runtime => "Code Mode runtime",
            Self::QuestionsOutsidePlan => "Questions outside Plan mode",
            Self::MultiAgent => "Multi-agent tools",
            Self::WaitAgent => "Subagent wait tool",
            Self::Screenless => "Speak public progress and typed results",
            Self::Acknowledgements => "Delegation acknowledgements",
            Self::AutoResume => "Recover a closed voice call",
            Self::VoiceRefresh => "Refresh voice after context changes",
        }
    }

    pub fn description(self) -> &'static str {
        match self {
            Self::Context => {
                "New threads only. Notes needs Codex backend authentication and usable remote history storage. Compaction uses summaries. Existing history and checkpoints are unchanged."
            }
            Self::Retention => {
                "New threads only. User-message token retention for Compaction, not a total context limit."
            }
            Self::IdleRollover => {
                "New threads only. Notes rolls over before the next user turn after 25 idle minutes when fresh saved notes are available. Off disables idle rollover, not context management."
            }
            Self::Runtime => {
                "New threads only. Notebook needs a single local environment, full-access permissions and Deno. Deno uses an explicit path, PATH or a verified managed download. No permissions are changed. V8 supports sandboxed Code Mode. Off disables configured Code Mode, but a model that requires Code Mode can still select it."
            }
            Self::QuestionsOutsidePlan => {
                "New threads only. Off keeps the question tool in Plan mode. On also allows waiting and asynchronous questions outside Plan mode, where supported."
            }
            Self::MultiAgent => {
                "New threads only. Off disables both tool generations. Legacy uses original subagent tools. V2 uses task-oriented tools. Board availability also depends on session storage."
            }
            Self::WaitAgent => {
                "New threads only. Expose V2 wait_agent for explicitly waiting on agents. Off keeps automatic child-result delivery. Requires multi-agent V2; does not enable subagents by itself."
            }
            Self::Screenless => {
                "Next client launch and new thread. On speaks public progress and results from typed and spoken tasks. Off keeps spoken-task results without speaking typed work. Private tool output is never made public."
            }
            Self::Acknowledgements => {
                "Next client launch and new thread. Speak brief fillers while delegated work runs. Default leaves the choice to the server. Explicit per-call requests still take precedence."
            }
            Self::AutoResume => {
                "Next client launch and new thread. Replace an established voice call once after media transport closure, not after device or helper errors."
            }
            Self::VoiceRefresh => {
                "Next client launch and new thread. Replace voice serially after the host context changes. Does not reset host history."
            }
        }
    }

    pub fn choices(self) -> &'static [&'static str] {
        match self {
            Self::Context => &["notes", "compaction"],
            Self::Retention => &["16000", "32000", "64000"],
            Self::IdleRollover => &["off", "25"],
            Self::Runtime => &["off", "v8", "notebook"],
            Self::MultiAgent => &["off", "legacy", "v2"],
            Self::Acknowledgements => &["default", "on", "off"],
            _ => &["on", "off"],
        }
    }

    pub fn parse(key: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|setting| setting.key() == key)
    }

    /// Sparse leaf edits preserve structured-feature options, including bool-to-table migration.
    pub fn edits(self, choice: &str) -> Result<Vec<(&'static str, Value)>, String> {
        if !self.choices().contains(&choice) {
            return Err(format!(
                "Invalid choice `{choice}` for {}. Choose: {}",
                self.key(),
                self.choices().join(", ")
            ));
        }
        Ok(match self {
            Self::Context => vec![("context_strategy", json!(choice))],
            Self::Retention => vec![(
                "compaction_retention_tokens",
                json!(choice.parse::<u64>().map_err(|error| error.to_string())?),
            )],
            Self::IdleRollover => vec![(
                "context_idle_rollover_minutes",
                if choice == "off" {
                    Value::Null
                } else {
                    json!(choice.parse::<u64>().map_err(|error| error.to_string())?)
                },
            )],
            Self::Runtime => vec![
                ("features.code_mode.enabled", json!(choice != "off")),
                ("features.code_mode_only", json!(false)),
                (
                    "features.code_mode.runtime",
                    json!(if choice == "notebook" {
                        "notebook"
                    } else {
                        "v8"
                    }),
                ),
            ],
            Self::MultiAgent => vec![
                ("features.multi_agent", json!(choice != "off")),
                ("features.multi_agent_v2.enabled", json!(choice == "v2")),
            ],
            Self::QuestionsOutsidePlan => vec![(
                "features.default_mode_request_user_input",
                json!(choice == "on"),
            )],
            Self::WaitAgent => vec![(
                "features.multi_agent_v2.wait_agent_enabled",
                json!(choice == "on"),
            )],
            Self::Screenless => vec![("realtime.screenless", json!(choice == "on"))],
            Self::Acknowledgements => vec![(
                "realtime.delegation_ack_filler",
                if choice == "default" {
                    Value::Null
                } else {
                    json!(choice == "on")
                },
            )],
            Self::AutoResume => vec![("realtime.auto_resume", json!(choice == "on"))],
            Self::VoiceRefresh => {
                vec![("realtime.refresh_on_context_change", json!(choice == "on"))]
            }
        })
    }

    /// Configured values with documented defaults, never a claim about the running thread.
    pub fn configured_choice(self, config: &Value) -> String {
        let enabled = |key: &str, default: bool| {
            let value = config.pointer(&format!("/features/{key}"));
            value
                .and_then(Value::as_bool)
                .or_else(|| {
                    value
                        .and_then(|v| v.get("enabled"))
                        .and_then(Value::as_bool)
                })
                .unwrap_or(default)
        };
        let scalar = |pointer: &str, default: &str| {
            config
                .pointer(pointer)
                .filter(|v| !v.is_null())
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .unwrap_or_else(|| v.to_string())
                })
                .unwrap_or_else(|| default.to_string())
        };
        match self {
            Self::Context => scalar("/context_strategy", "notes"),
            Self::Retention => scalar("/compaction_retention_tokens", "64000"),
            Self::IdleRollover => scalar("/context_idle_rollover_minutes", "off"),
            Self::Runtime => {
                if !enabled("code_mode", true) && !enabled("code_mode_only", false) {
                    "off".into()
                } else {
                    scalar("/features/code_mode/runtime", "notebook")
                }
            }
            Self::MultiAgent => {
                if !enabled("multi_agent", true) {
                    "off".into()
                } else if enabled("multi_agent_v2", true) {
                    "v2".into()
                } else {
                    "legacy".into()
                }
            }
            Self::QuestionsOutsidePlan => if enabled("default_mode_request_user_input", false) {
                "on"
            } else {
                "off"
            }
            .into(),
            Self::Acknowledgements => match config
                .pointer("/realtime/delegation_ack_filler")
                .and_then(Value::as_bool)
            {
                Some(true) => "on",
                Some(false) => "off",
                None => "default",
            }
            .into(),
            Self::WaitAgent => if config
                .pointer("/features/multi_agent_v2/wait_agent_enabled")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                "on"
            } else {
                "off"
            }
            .into(),
            Self::Screenless | Self::AutoResume | Self::VoiceRefresh => {
                let pointer = match self {
                    Self::Screenless => "/realtime/screenless",
                    Self::AutoResume => "/realtime/auto_resume",
                    _ => "/realtime/refresh_on_context_change",
                };
                if config
                    .pointer(pointer)
                    .and_then(Value::as_bool)
                    .unwrap_or(true)
                {
                    "on"
                } else {
                    "off"
                }
                .into()
            }
        }
    }

    pub fn matches_edits(self, config: &Value, choice: &str) -> bool {
        self.edits(choice).is_ok_and(|edits| {
            edits.iter().all(|(path, expected)| {
                let pointer = format!("/{}", path.replace('.', "/"));
                let actual = config.pointer(&pointer).unwrap_or(&Value::Null);
                actual == expected
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn questions_outside_plan_requires_explicit_opt_in_and_reads_back_the_same_flag() {
        let setting = CliSetting::QuestionsOutsidePlan;
        assert_eq!(setting.configured_choice(&json!({})), "off");
        for (choice, enabled) in [("on", true), ("off", false)] {
            assert_eq!(
                setting.edits(choice).unwrap(),
                vec![("features.default_mode_request_user_input", json!(enabled))]
            );
            let config = json!({"features": {"default_mode_request_user_input": enabled}});
            assert_eq!(setting.configured_choice(&config), choice);
            assert!(setting.matches_edits(&config, choice));
            assert!(!setting.matches_edits(&config, if enabled { "off" } else { "on" }));
        }
    }

    #[test]
    fn runtime_readback_checks_all_controls_not_just_label() {
        let config = json!({"features": {"code_mode": {"enabled": false, "runtime": "v8", "notebook_profile": "keep"}, "code_mode_only": true}});
        assert!(!CliSetting::Runtime.matches_edits(&config, "off"));
        assert_eq!(CliSetting::Runtime.configured_choice(&config), "v8");
        assert!(CliSetting::Context.edits("none").is_err());
        assert!(CliSetting::Retention.edits("1").is_err());
        assert_eq!(
            CliSetting::Acknowledgements.edits("default").unwrap(),
            vec![("realtime.delegation_ack_filler", Value::Null)]
        );
    }
}
