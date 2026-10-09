//! Assembles multi-agent role instructions from selected text and runtime capabilities.
//! The segment owns rendering and attribution; consumers select and capture its inputs.

use crate::without_update_plan_instructions;
use codex_context_fragments::ContextualUserFragment;
use codex_protocol::models::ContentItemKind;

const DEFAULT_MULTI_AGENT_V2_MODEL_OVERRIDE_USAGE_HINT_TEXT: &str = "`model` or `reasoning_effort`: only when explicitly requested by the user, AGENTS.md, or skills. Overrides require `fork_turns: \"none\"` or a positive integer string. Omitted or `\"all\"`: parent model and effort inherited";
const DEFAULT_MULTI_AGENT_V2_WAIT_AGENT_USAGE_HINT_TEXT: &str =
    "`wait_agent`: waits of minutes preferred over busy polling";
const DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT: &str = "Collaboration tools: direct calls, not inside `functions.exec`. Shared filesystem and working directory. Coordinated edits. Others' changes preserved. Continue independent work after delegating. When only child results remain, end your turn without claiming the task is finished. Child completion resumes an idle parent unless stopped or shut down. Do not poll for completion";
const AGENT_MESSAGE_BOARD_USAGE_HINT_TEXT: &str = "In authorized substantial multi-agent workflows, use `agent_board` for shared decisions, dependencies, and findings. Parents pass relevant channel names and thread IDs in assignments. Children read and update those threads. Board posts do not assign work or wake idle agents.";
const DIRECT_AGENT_COORDINATION_USAGE_HINT_TEXT: &str = "Use direct messages for targeted coordination. Use `followup_task` to start work for idle agents";

/// Board workflow guidance selected from the current step's coordination tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentMessageBoardGuidance {
    BoardOnly,
    WithDirectMessaging,
}

/// Multi-agent role text and the captured capabilities used to render its context segment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MultiAgentRoleInstructions {
    /// Complete configured instructions, emitted verbatim without catalog markers.
    Configured(String),
    /// Selected catalog or bundled text, composed with captured runtime guidance.
    Composed {
        base: String,
        marked: bool,
        omit_update_plan_instructions: bool,
        max_concurrency: usize,
        wait_agent_enabled: bool,
        expose_model_overrides: bool,
        agent_message_board: Option<AgentMessageBoardGuidance>,
    },
}

impl MultiAgentRoleInstructions {
    /// Capture actual step availability without changing configured role overrides.
    pub fn with_agent_message_board(mut self, guidance: Option<AgentMessageBoardGuidance>) -> Self {
        if let Self::Composed {
            agent_message_board,
            ..
        } = &mut self
        {
            *agent_message_board = guidance;
        }
        self
    }
}

impl ContextualUserFragment for MultiAgentRoleInstructions {
    fn content_kind(&self) -> ContentItemKind {
        ContentItemKind("multi_agent.role_instructions".to_string())
    }

    fn role(&self) -> &'static str {
        "developer"
    }

    fn markers(&self) -> (&'static str, &'static str) {
        match self {
            Self::Composed { marked: true, .. } => Self::type_markers(),
            Self::Configured(_) | Self::Composed { marked: false, .. } => ("", ""),
        }
    }

    fn type_markers() -> (&'static str, &'static str) {
        ("<multi_agent_role>", "</multi_agent_role>")
    }

    fn body(&self) -> String {
        match self {
            Self::Configured(text) => text.clone(),
            Self::Composed {
                base,
                omit_update_plan_instructions,
                max_concurrency,
                wait_agent_enabled,
                expose_model_overrides,
                agent_message_board,
                ..
            } => {
                let base = if *omit_update_plan_instructions {
                    without_update_plan_instructions(base)
                } else {
                    base.clone()
                };
                let wait_agent_guidance = if *wait_agent_enabled {
                    format!("{DEFAULT_MULTI_AGENT_V2_WAIT_AGENT_USAGE_HINT_TEXT}\n\n")
                } else {
                    String::new()
                };
                let shared = DEFAULT_MULTI_AGENT_V2_SHARED_USAGE_HINT_TEXT;
                let mut text = format!(
                    "{base}\n{shared}\n{wait_agent_guidance}Active-agent limit, including you: {max_concurrency}"
                );
                if *expose_model_overrides {
                    text.push_str("\n\n");
                    text.push_str(DEFAULT_MULTI_AGENT_V2_MODEL_OVERRIDE_USAGE_HINT_TEXT);
                }
                if let Some(guidance) = agent_message_board {
                    text.push_str("\n\n");
                    text.push_str(AGENT_MESSAGE_BOARD_USAGE_HINT_TEXT);
                    if *guidance == AgentMessageBoardGuidance::WithDirectMessaging {
                        text.push(' ');
                        text.push_str(DIRECT_AGENT_COORDINATION_USAGE_HINT_TEXT);
                    }
                }
                text
            }
        }
    }
}

#[cfg(test)]
#[path = "multi_agent_instructions_tests.rs"]
mod tests;
