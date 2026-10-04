use crate::function_tool::FunctionCallError;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::tools::handlers::RequestUserInputHandler;
use crate::tools::handlers::parse_arguments;
use crate::tools::registry::CoreToolRuntime;
use crate::tools::registry::ToolExecutor;
use codex_protocol::items::AsyncUserInputQuestion;
use codex_protocol::request_user_input::RequestUserInputQuestion;
use codex_protocol::request_user_input::RequestUserInputQuestionOption;
use codex_tools::ToolName;
use codex_tools::ToolSpec;
use serde::Deserialize;

/// Hidden dispatch compatibility for recorded calls using the previous schema.
pub struct RequestUserInputAsyncHandler;

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestUserInputAsyncArgs {
    questions: Vec<AsyncUserInputQuestion>,
}

impl ToolExecutor<ToolInvocation> for RequestUserInputAsyncHandler {
    fn tool_name(&self) -> ToolName {
        ToolName::plain("request_user_input_async")
    }

    fn spec(&self) -> ToolSpec {
        let ToolSpec::Function(mut spec) = RequestUserInputHandler {
            available_modes: Vec::new(),
            async_enabled: true,
        }
        .spec() else {
            unreachable!("question tool is a function")
        };
        spec.name = self.tool_name().name;
        ToolSpec::Function(spec)
    }

    fn handle<'a>(&'a self, mut invocation: ToolInvocation) -> codex_tools::ToolExecutorFuture<'a>
    where
        ToolInvocation: 'a,
    {
        Box::pin(async move {
            let ToolPayload::Function { arguments } = &invocation.payload else {
                return Err(FunctionCallError::RespondToModel(
                    "request_user_input_async handler received unsupported payload".to_string(),
                ));
            };
            let args: RequestUserInputAsyncArgs = parse_arguments(arguments)?;
            if args
                .questions
                .iter()
                .any(|question| question.title.trim().is_empty())
            {
                return Err(FunctionCallError::RespondToModel(
                    "question titles must not be empty".to_string(),
                ));
            }
            let questions = args
                .questions
                .into_iter()
                .enumerate()
                .map(|(index, question)| RequestUserInputQuestion {
                    id: format!("question_{index}"),
                    header: String::new(),
                    question: question.title,
                    is_other: true,
                    is_secret: false,
                    options: question.options.map(|options| {
                        options
                            .into_iter()
                            .map(|label| RequestUserInputQuestionOption {
                                label,
                                description: String::new(),
                            })
                            .collect()
                    }),
                })
                .collect::<Vec<_>>();
            invocation.payload = ToolPayload::Function {
                arguments: serde_json::json!({"questions": questions, "delivery": "async"})
                    .to_string(),
            };
            RequestUserInputHandler {
                available_modes: Vec::new(),
                async_enabled: true,
            }
            .handle(invocation)
            .await
        })
    }
}

impl CoreToolRuntime for RequestUserInputAsyncHandler {
    fn is_builtin_control_tool(&self) -> bool {
        true
    }
}
