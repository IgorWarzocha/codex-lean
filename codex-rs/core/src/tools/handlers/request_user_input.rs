use crate::function_tool::FunctionCallError;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolOutput;
use crate::tools::context::ToolPayload;
use crate::tools::context::boxed_tool_output;
use crate::tools::handlers::parse_arguments;
use crate::tools::handlers::request_user_input_spec::REQUEST_USER_INPUT_TOOL_NAME;
use crate::tools::handlers::request_user_input_spec::RequestUserInputToolArgs;
use crate::tools::handlers::request_user_input_spec::UserInputDelivery;
use crate::tools::handlers::request_user_input_spec::create_request_user_input_tool;
use crate::tools::handlers::request_user_input_spec::normalize_request_user_input_tool_args;
use crate::tools::handlers::request_user_input_spec::request_user_input_async_available;
use crate::tools::handlers::request_user_input_spec::request_user_input_tool_description;
use crate::tools::handlers::request_user_input_spec::request_user_input_unavailable_message;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_features::Feature;
use codex_history::RetainedContextEvent;
use codex_history::VerifiedAnswer;
use codex_history::VerifiedQuestionAnswer;
use codex_protocol::config_types::ModeKind;
use codex_protocol::items::AgentMessageContent;
use codex_protocol::items::AgentMessageDelivery;
use codex_protocol::items::AgentMessageItem;
use codex_protocol::items::AsyncUserInputQuestion;
use codex_protocol::items::TurnItem;
use codex_protocol::models::MessagePhase;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::request_user_input::RequestUserInputArgs;
use codex_tools::ToolName;
use codex_tools::ToolSpec;

pub struct RequestUserInputHandler {
    pub available_modes: Vec<ModeKind>,
    pub async_enabled: bool,
}

/// Native responses keep their serialized payload; exec receives the same JSON as a value.
struct RequestUserInputOutput {
    native: FunctionToolOutput,
    value: serde_json::Value,
}

impl RequestUserInputOutput {
    fn new(content: String) -> Result<Self, FunctionCallError> {
        let value = serde_json::from_str(&content).map_err(|err| {
            FunctionCallError::Fatal(format!(
                "failed to decode request_user_input response: {err}"
            ))
        })?;
        Ok(Self {
            native: FunctionToolOutput::from_text(content, Some(true)),
            value,
        })
    }
}

impl ToolOutput for RequestUserInputOutput {
    fn log_output(&self) -> String {
        self.native.log_output()
    }
    fn success_for_logging(&self) -> bool {
        self.native.success_for_logging()
    }
    fn to_response_item(&self, call_id: &str, payload: &ToolPayload) -> ResponseInputItem {
        self.native.to_response_item(call_id, payload)
    }
    fn code_mode_result(&self, _payload: &ToolPayload) -> serde_json::Value {
        self.value.clone()
    }
}

impl ToolExecutor<ToolInvocation> for RequestUserInputHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain(REQUEST_USER_INPUT_TOOL_NAME)
    }

    fn spec(&self) -> ToolSpec {
        create_request_user_input_tool(
            request_user_input_tool_description(&self.available_modes, self.async_enabled),
            self.async_enabled,
        )
    }

    fn handle<'a>(&'a self, invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(self.handle_call(invocation))
    }
}

impl RequestUserInputHandler {
    async fn handle_call(
        &self,
        invocation: ToolInvocation,
    ) -> Result<Box<dyn crate::tools::context::ToolOutput>, FunctionCallError> {
        let ToolInvocation {
            session,
            turn,
            step_context,
            call_id,
            payload,
            ..
        } = invocation;

        let arguments = match payload {
            ToolPayload::Function { arguments } => arguments,
            _ => {
                return Err(FunctionCallError::RespondToModel(format!(
                    "{REQUEST_USER_INPUT_TOOL_NAME} handler received unsupported payload"
                )));
            }
        };

        if turn.session_source.is_non_root_agent() {
            return Err(FunctionCallError::RespondToModel(
                "request_user_input can only be used by the root thread".to_string(),
            ));
        }

        let mode = step_context.settings.effective_collaboration_mode().mode;
        let modes = codex_tools::request_user_input_available_modes(&turn.config.features);
        if let Some(message) = request_user_input_unavailable_message(mode, &modes) {
            return Err(FunctionCallError::RespondToModel(message));
        }
        let args: RequestUserInputToolArgs = parse_arguments(&arguments)?;
        let args = normalize_request_user_input_tool_args(args)
            .map_err(FunctionCallError::RespondToModel)?;
        if args.delivery == UserInputDelivery::Async {
            if !self.async_enabled
                || !request_user_input_async_available(
                    &turn.session_source,
                    &step_context.settings.model_info,
                    &turn.config.features,
                    mode,
                )
            {
                return Err(FunctionCallError::RespondToModel(
                    "request_user_input async delivery is unavailable for this model".to_string(),
                ));
            }
            let item = async_user_input_item(call_id, args.questions);
            session.emit_turn_item_started(turn.as_ref(), &item).await;
            session.emit_turn_item_completed(turn.as_ref(), item).await;
            return Ok(boxed_tool_output(RequestUserInputOutput::new(
                r#"{"accepted":true}"#.to_string(),
            )?));
        }

        if let Some(message) = request_user_input_unavailable_message(mode, &self.available_modes) {
            return Err(FunctionCallError::RespondToModel(message));
        }
        let args = RequestUserInputArgs {
            questions: args.questions,
            is_blocking: mode == ModeKind::Plan,
            auto_resolution_ms: None,
        };
        let questions = args.questions.clone();
        let accepted = session
            .request_user_input(turn.as_ref(), call_id.clone(), args)
            .await
            .ok_or_else(|| {
                FunctionCallError::RespondToModel(format!(
                    "{REQUEST_USER_INPUT_TOOL_NAME} was cancelled before receiving a response"
                ))
            })?;

        let response = accepted.response;

        let content = serde_json::to_string(&response).map_err(|err| {
            FunctionCallError::Fatal(format!(
                "failed to serialize {REQUEST_USER_INPUT_TOOL_NAME} response: {err}"
            ))
        })?;
        // Persist answers even while an older checkpoint needs compatibility review.
        if turn.config.features.enabled(Feature::GuardianApproval) {
            let user_input = questions
                .iter()
                .filter_map(|question| {
                    let response = response.answers.get(&question.id)?;
                    let answers = response
                        .answers
                        .iter()
                        .filter(|answer| !answer.trim().is_empty())
                        .cloned()
                        .collect::<Vec<_>>();
                    if answers.is_empty() {
                        return None;
                    }
                    let mut question_text = question.question.clone();
                    for option in question
                        .options
                        .iter()
                        .flatten()
                        .filter(|option| response.answers.contains(&option.label))
                    {
                        question_text
                            .push_str(&format!("\n{}: {}", option.label, option.description));
                    }
                    Some(VerifiedQuestionAnswer {
                        question: question_text,
                        answer: answers.join("\n"),
                    })
                })
                .collect::<Vec<_>>();
            if !user_input.is_empty() {
                session
                    .record_retained_context(RetainedContextEvent::VerifiedAnswer {
                        answer: VerifiedAnswer {
                            turn_id: turn.sub_id.clone(),
                            call_id,
                            questions: user_input,
                        },
                        acceptance_order: Some(accepted.acceptance_order),
                    })
                    .await;
            }
        }

        Ok(boxed_tool_output(RequestUserInputOutput::new(content)?))
    }
}

fn async_user_input_item(
    call_id: String,
    questions: Vec<codex_protocol::request_user_input::RequestUserInputQuestion>,
) -> TurnItem {
    // Keep the consumer's async AgentMessage contract and index-based answer IDs.
    let questions = questions
        .into_iter()
        .map(|question| AsyncUserInputQuestion {
            title: question.question,
            options: question.options.map(|options| {
                options
                    .into_iter()
                    .map(|option| {
                        if option.description.trim().is_empty() {
                            option.label
                        } else {
                            format!("{}: {}", option.label, option.description)
                        }
                    })
                    .collect()
            }),
        })
        .collect::<Vec<_>>();
    let text = questions
        .iter()
        .map(|question| {
            let mut lines = vec![question.title.clone()];
            lines.extend(
                question
                    .options
                    .iter()
                    .flatten()
                    .map(|option| format!("- {option}")),
            );
            lines.join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    TurnItem::AgentMessage(AgentMessageItem {
        id: call_id,
        content: vec![AgentMessageContent::Text { text }],
        phase: Some(MessagePhase::FinalAnswer),
        memory_citation: None,
        delivery: Some(AgentMessageDelivery::Async),
        questions: Some(questions),
    })
}

impl CoreToolRuntime for RequestUserInputHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}

#[cfg(test)]
#[path = "request_user_input_tests.rs"]
mod tests;
