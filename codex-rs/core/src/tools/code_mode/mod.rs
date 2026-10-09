mod delegate;
mod execute_handler;
pub(crate) mod execute_spec;
#[cfg(test)]
mod model_output_tests;
pub(crate) mod notebook;
pub(crate) mod notebook_handler;
mod notebook_spec;
mod output;
pub(crate) mod prompt;
mod response_adapter;
#[cfg(test)]
mod shutdown_tests;
mod telemetry;
mod tool_definitions;
mod wait_handler;
pub(crate) mod wait_spec;

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;

use codex_code_mode::CellId;
use codex_code_mode::CodeModeNestedToolCall;
use codex_code_mode::CodeModeSession;
use codex_code_mode::CodeModeSessionProvider;
use codex_code_mode::CodeModeToolKind;
use codex_code_mode::RuntimeResponse;
use codex_protocol::ThreadId;
use codex_protocol::models::FunctionCallOutputBody;
use codex_protocol::models::FunctionCallOutputContentItem;
use codex_protocol::models::ResponseInputItem;
use futures::future::join_all;
use serde_json::Value as JsonValue;
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::function_tool::FunctionCallError;
use crate::original_image_detail::can_request_original_image_detail;
use crate::original_image_detail::sanitize_original_image_detail as sanitize_image_detail_items;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tools::ExecutedToolCalls;
use crate::tools::call_trace;
use crate::tools::context::FunctionToolOutput;
use crate::tools::context::SharedTurnDiffTracker;
use crate::tools::context::ToolPayload;
use crate::tools::parallel::ToolCallRuntime;
use crate::tools::router::ToolCall;
use crate::tools::router::ToolCallSource;
use crate::unified_exec::resolve_max_tokens;
use codex_protocol::openai_models::ToolMode;
use codex_tools::ToolName;
use codex_utils_audio::estimate_audio_token_count;
use codex_utils_output_truncation::TruncationPolicy;
use codex_utils_output_truncation::formatted_truncate_text_content_items_with_policy;
use codex_utils_output_truncation::truncate_function_output_items_with_policy;
use codex_utils_output_truncation::with_serialization_allowance;

use delegate::CodeModeCellDelegate;
use delegate::CodeModeDispatchBroker;
use delegate::CodeModeDispatchWorker;
pub(crate) use execute_handler::CodeModeExecuteHandler;
use output::CodeModeToolOutput;
use response_adapter::into_function_call_output_content_items;
pub(crate) use tool_definitions::prepare_code_mode_tool_definitions;
pub(crate) use wait_handler::CodeModeWaitHandler;

pub(crate) const PUBLIC_TOOL_NAME: &str = codex_code_mode::PUBLIC_TOOL_NAME;
pub(crate) const WAIT_TOOL_NAME: &str = codex_code_mode::WAIT_TOOL_NAME;
pub(crate) const DEFAULT_WAIT_YIELD_TIME_MS: u64 = codex_code_mode::DEFAULT_WAIT_YIELD_TIME_MS;

/// Returns true for the code-mode `exec` tool in the default namespace.
pub(crate) fn is_exec_tool_name(tool_name: &ToolName) -> bool {
    tool_name.is_default_namespace() && tool_name.name == PUBLIC_TOOL_NAME
}

#[derive(Clone)]
pub(crate) struct ExecContext {
    pub(super) session: Arc<Session>,
    pub(super) turn: Arc<TurnContext>,
}

pub(crate) struct CodeModeService {
    notebook_cwd: Option<codex_utils_absolute_path::AbsolutePathBuf>,
    notebook_provider: Option<Arc<codex_notebook::DenoNotebookSessionProvider>>,
    session: OnceCell<Arc<dyn CodeModeSession>>,
    session_provider: Arc<dyn CodeModeSessionProvider>,
    availability: Result<(), String>,
    dispatch_broker: Arc<CodeModeDispatchBroker>,
    default_exec_yield_time_ms: u64,
    shutdown_token: CancellationToken,
    unavailable_warning_emitted: AtomicBool,
    // Once unrestricted access has been authorized, later direct-only steps must
    // still enforce revocation for a live or concurrently prewarming Notebook.
    notebook_authority_required: AtomicBool,
}

impl CodeModeService {
    pub(crate) fn new(
        thread_id: ThreadId,
        session_provider: Arc<dyn CodeModeSessionProvider>,
        config: &Config,
        executed_tool_calls: ExecutedToolCalls,
    ) -> Self {
        let notebook_cwd = (config.code_mode.runtime == codex_features::CodeModeRuntime::Notebook)
            .then(|| config.cwd.clone());
        let notebook_provider = notebook_cwd.as_ref().map(|_| {
            Arc::new(
                codex_notebook::DenoNotebookSessionProvider::from_config(
                    config.code_mode.deno_program.clone(),
                    config.cwd.to_path_buf(),
                    config.codex_home.to_path_buf(),
                    thread_id.to_string(),
                )
                .with_ephemeral(config.ephemeral)
                .with_max_heap_mib(config.code_mode.notebook_max_heap_mib)
                .with_plain_command_output(config.code_mode.notebook_plain_command_output)
                .with_default_profile(config.code_mode.notebook_profile.clone()),
            )
        });
        let session_provider = notebook_provider
            .as_ref()
            .map(|provider| Arc::clone(provider) as Arc<dyn CodeModeSessionProvider>)
            .unwrap_or(session_provider);
        let dispatch_broker = Arc::new(CodeModeDispatchBroker::new(thread_id, executed_tool_calls));
        let availability = session_provider.availability();
        Self {
            notebook_cwd,
            notebook_provider,
            session: OnceCell::new(),
            session_provider,
            availability,
            dispatch_broker,
            default_exec_yield_time_ms: config.code_mode.default_exec_yield_time_ms,
            shutdown_token: CancellationToken::new(),
            unavailable_warning_emitted: AtomicBool::new(false),
            notebook_authority_required: AtomicBool::new(false),
        }
    }

    pub(crate) fn is_available(&self) -> bool {
        self.availability.is_ok()
    }

    pub(crate) fn can_prewarm(&self) -> bool {
        self.is_available()
    }

    pub(crate) async fn prewarm(&self, turn: &TurnContext) -> Result<(), String> {
        if self.shutdown_token.is_cancelled()
            || crate::tools::effective_tool_mode(turn, turn.model_info()) == ToolMode::Direct
        {
            return Ok(());
        }
        match &self.notebook_provider {
            Some(provider) => {
                // Launching even a bare executable is unrestricted execution. Require the same
                // full-access local environment authority before resolving PATH or spawning it.
                let Some(cwd) = &self.notebook_cwd else {
                    return Ok(());
                };
                if turn.config.code_mode.runtime != codex_features::CodeModeRuntime::Notebook {
                    return Ok(());
                }
                if let Err(error) = notebook::validate_prewarm_access(turn, cwd) {
                    tracing::debug!(%error, "Notebook prewarm deferred until authorized access");
                    return Ok(());
                }
                self.notebook_authority_required
                    .store(true, Ordering::Release);
                // Saved-code restoration still waits for a captured, validated step.
                provider.prewarm().await
            }
            None => self.session().await.map(|_| ()),
        }
    }

    pub(crate) fn take_unavailable_warning(&self, tool_mode: ToolMode) -> Option<String> {
        let error = self.availability.as_ref().err()?;
        let behavior = match tool_mode {
            ToolMode::Direct => "Falling back to direct tools",
            ToolMode::CodeMode | ToolMode::CodeModeOnly => "Code mode will fail closed",
        };
        (!self
            .unavailable_warning_emitted
            .swap(true, Ordering::Relaxed))
        .then(|| {
            if self.notebook_cwd.is_some() {
                format!("Notebook is unavailable because {error}. Check features.code_mode.deno_program and restart the thread.")
            } else {
                format!(
                    "Code Mode is unavailable because {error}. {behavior}; enable `features.code_mode_host` and install `codex-code-mode-host`."
                )
            }
        })
    }

    pub(crate) fn session_provider(&self) -> Arc<dyn CodeModeSessionProvider> {
        Arc::clone(&self.session_provider)
    }

    pub(crate) async fn execute(
        &self,
        mut request: codex_code_mode::ExecuteRequest,
        step_context: Arc<StepContext>,
    ) -> Result<codex_code_mode::StartedCell, String> {
        self.validate_notebook_access(&step_context).await?;
        request
            .yield_time_ms
            .get_or_insert(self.default_exec_yield_time_ms);
        let preempt = step_context.preempt.clone();
        let delegate = Arc::new(CodeModeCellDelegate {
            broker: Arc::clone(&self.dispatch_broker),
            step_context,
            outer_call_id: request.tool_call_id.clone(),
        });
        self.session()
            .await?
            .execute(request, delegate, preempt)
            .await
    }

    pub(crate) async fn validate_sampling_access(&self, step: &StepContext) -> Result<(), String> {
        if step.tool_router.requires_code_mode_worker()
            || self.notebook_authority_required.load(Ordering::Acquire)
        {
            self.validate_notebook_access(step).await
        } else {
            Ok(())
        }
    }

    pub(crate) async fn validate_notebook_access(&self, step: &StepContext) -> Result<(), String> {
        let selected =
            step.turn.config.code_mode.runtime == codex_features::CodeModeRuntime::Notebook;
        let result = match (&self.notebook_cwd, selected) {
            (Some(cwd), true) => notebook::validate_access(step, cwd),
            (None, false) => Ok(()),
            _ => Err("Start a new thread to change the Code Mode runtime".to_string()),
        };
        if let Err(error) = result {
            self.shutdown_with_mode(ShutdownMode::WithoutCleanup)
                .await?;
            return Err(error);
        }
        if selected {
            self.notebook_authority_required
                .store(true, Ordering::Release);
        }
        Ok(())
    }

    pub(crate) async fn control_notebook(
        &self,
        request: codex_notebook::NotebookRequest,
        step: &StepContext,
    ) -> Result<codex_notebook::NotebookControlResult, String> {
        self.validate_notebook_access(step).await?;
        let provider = self
            .notebook_provider
            .as_ref()
            .ok_or_else(|| "The notebook tool requires the Notebook runtime".to_string())?;
        if self.shutdown_token.is_cancelled() {
            return Err("notebook session is shut down".to_string());
        }
        // Reads and recovery must not initialize a cold kernel or replay startup
        // side effects. The provider uses the live session when one exists.
        if matches!(
            &request,
            codex_notebook::NotebookRequest::Diagnostics
                | codex_notebook::NotebookRequest::List { .. }
                | codex_notebook::NotebookRequest::Unpin { .. }
                | codex_notebook::NotebookRequest::Reset
        ) {
            return provider.control(request).await;
        }
        // Initialization may restore bindings. Access must be checked before even creating it.
        self.session().await?;
        provider.control(request).await
    }

    pub(crate) async fn wait(
        &self,
        request: codex_code_mode::WaitRequest,
        preempt: Option<CancellationToken>,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        self.session().await?.wait(request, preempt).await
    }

    pub(crate) async fn terminate(
        &self,
        cell_id: CellId,
    ) -> Result<codex_code_mode::WaitOutcome, String> {
        self.session().await?.terminate(cell_id).await
    }

    pub(crate) async fn interrupt_active_cells(&self) {
        let Some(session) = self.session.get() else {
            return;
        };
        join_all(
            self.dispatch_broker
                .active_cell_ids()
                .into_iter()
                .map(|cell_id| async move {
                    if let Err(error) = session.terminate(cell_id.clone()).await {
                        tracing::warn!(%cell_id, %error, "failed to terminate interrupted code-mode cell");
                    }
                }),
        )
        .await;
    }

    pub(crate) async fn shutdown(&self) -> Result<(), String> {
        self.shutdown_with_mode(ShutdownMode::Graceful).await
    }

    async fn shutdown_with_mode(&self, mode: ShutdownMode) -> Result<(), String> {
        self.shutdown_token.cancel();
        let prewarm_cleanup = match &self.notebook_provider {
            Some(provider) => provider.shutdown_prewarm().await,
            None => Ok(()),
        };
        // Join any initialization already in progress without initializing an unused service.
        let session_cleanup = match self
            .session
            .get_or_try_init(|| async {
                Err::<Arc<dyn CodeModeSession>, String>(
                    "code mode session is shutting down".to_string(),
                )
            })
            .await
        {
            Ok(session) => match mode {
                ShutdownMode::Graceful => session.shutdown().await,
                ShutdownMode::WithoutCleanup => session.shutdown_without_cleanup().await,
            },
            Err(_) => Ok(()),
        };
        session_cleanup.and(prewarm_cleanup)
    }

    pub(crate) fn mark_cell_ready_for_dispatch(
        &self,
        cell_id: &codex_code_mode::CellId,
        originating_call: Option<crate::tools::context::ToolCallOrigin>,
    ) {
        self.dispatch_broker
            .mark_cell_ready_for_dispatch(cell_id, originating_call);
    }

    pub(crate) fn cell_originating_call(
        &self,
        cell_id: &codex_code_mode::CellId,
    ) -> Option<crate::tools::context::ToolCallOrigin> {
        self.dispatch_broker.cell_originating_call(cell_id)
    }

    pub(crate) fn finish_cell_dispatch(&self, cell_id: &CellId) {
        self.dispatch_broker.close_cell(cell_id);
    }

    pub(crate) fn start_turn_worker(
        &self,
        session: &Arc<Session>,
        step_context: Arc<StepContext>,
        tracker: SharedTurnDiffTracker,
    ) -> Option<CodeModeDispatchWorker> {
        if !step_context.tool_router.requires_code_mode_worker() {
            return None;
        }

        Some(
            self.dispatch_broker
                .start_turn_worker(Arc::clone(session), step_context, tracker),
        )
    }

    pub(crate) async fn session(&self) -> Result<Arc<dyn CodeModeSession>, String> {
        if self.shutdown_token.is_cancelled() {
            return Err("code mode session is shutting down".to_string());
        }
        self.session
            .get_or_try_init(|| async {
                if self.shutdown_token.is_cancelled() {
                    return Err("code mode session is shutting down".to_string());
                }
                let session = tokio::select! {
                    biased;
                    _ = self.shutdown_token.cancelled() => {
                        return Err("code mode session is shutting down".to_string());
                    }
                    session = self
                        .session_provider
                        .create_session() => session?,
                };
                if self.shutdown_token.is_cancelled() {
                    let _ = session.shutdown_without_cleanup().await;
                    return Err("code mode session is shutting down".to_string());
                }
                Ok(session)
            })
            .await
            .map(Arc::clone)
    }
}

enum ShutdownMode {
    Graceful,
    WithoutCleanup,
}

fn handle_runtime_response(
    model_info: &codex_protocol::openai_models::ModelInfo,
    response: RuntimeResponse,
    max_output_tokens: Option<usize>,
    wall_time: Duration,
    experimental_show_cell_overhead: bool,
) -> CodeModeToolOutput {
    let script_status = format_script_status(&response);
    let supports_original = can_request_original_image_detail(model_info);
    let host_duration = response
        .code_mode_host_duration()
        .filter(|_| experimental_show_cell_overhead);

    let (content_items, error_text) = match response {
        RuntimeResponse::Yielded { content_items, .. }
        | RuntimeResponse::Terminated { content_items, .. } => (content_items, None),
        RuntimeResponse::Result {
            content_items,
            error_text,
            ..
        } => (content_items, error_text),
    };
    let mut content_items = into_function_call_output_content_items(content_items);
    sanitize_image_detail_items(supports_original, &mut content_items);
    let success = error_text.is_none();
    if let Some(error_text) = error_text {
        content_items.push(FunctionCallOutputContentItem::InputText {
            text: format!("Script error:\n{error_text}"),
        });
    }
    content_items = truncate_code_mode_result(content_items, max_output_tokens);
    CodeModeToolOutput::new(
        FunctionToolOutput::from_content(content_items, Some(success)),
        script_status,
        wall_time,
        host_duration,
    )
}

fn format_script_status(response: &RuntimeResponse) -> String {
    match response {
        RuntimeResponse::Yielded { cell_id, .. } => {
            format!("Script running with cell ID {cell_id}")
        }
        RuntimeResponse::Terminated { .. } => "Script terminated".to_string(),
        RuntimeResponse::Result { error_text, .. } => {
            if error_text.is_none() {
                "Script completed".to_string()
            } else {
                "Script failed".to_string()
            }
        }
    }
}

fn truncate_code_mode_result(
    items: Vec<FunctionCallOutputContentItem>,
    max_output_tokens: Option<usize>,
) -> Vec<FunctionCallOutputContentItem> {
    let max_output_tokens = resolve_max_tokens(max_output_tokens);
    let policy = TruncationPolicy::Tokens(max_output_tokens);
    if items
        .iter()
        .all(|item| matches!(item, FunctionCallOutputContentItem::InputText { .. }))
    {
        let (truncated_items, _) =
            formatted_truncate_text_content_items_with_policy(&items, policy);
        return truncated_items;
    }

    truncate_function_output_items_with_policy(&items, policy, estimate_audio_token_count)
}

// Submit synchronously so the recorder sees the call before the cell's dispatch gate closes.
fn submit_nested_tool(
    session: Arc<Session>,
    step_context: Arc<StepContext>,
    tool_runtime: ToolCallRuntime,
    invocation: CodeModeNestedToolCall,
    call_id: String,
    outer_call_id: String,
    cancellation_token: CancellationToken,
) -> Result<
    impl std::future::Future<Output = Result<JsonValue, FunctionCallError>> + Send + 'static,
    FunctionCallError,
> {
    let CodeModeNestedToolCall {
        cell_id,
        runtime_tool_call_id,
        tool_name,
        tool_kind,
        input,
    } = invocation;
    let thread_id = session.thread_id;
    let turn_id = step_context.turn.sub_id.clone();
    let tool_name = tool_name.with_default_namespace();
    // A cell can outlive a turn; the broker records arrival before a dispatching turn is known.
    tracing::event!(
        name: "codex.code_mode.nested_tool_dispatched",
        target: "codex_otel.trace_safe",
        tracing::Level::INFO,
        event.name = "codex.code_mode.nested_tool_dispatched",
        conversation.id = %thread_id,
        turn_id = turn_id.as_str(),
        cell.id = telemetry::trace_id(cell_id.as_str()),
        runtime_tool_call_id = telemetry::trace_id(&runtime_tool_call_id),
        call_id = call_id.as_str(),
    );
    let payload = if is_exec_tool_name(&tool_name) {
        Err(format!("{PUBLIC_TOOL_NAME} cannot invoke itself"))
    } else {
        build_nested_tool_payload(tool_kind, &tool_name, input)
    };
    let payload = match payload {
        Ok(payload) => payload,
        Err(error) => {
            call_trace::result_ready(
                thread_id,
                &turn_id,
                &tool_name,
                &call_id,
                call_trace::Source::CodeMode,
            );
            return Err(FunctionCallError::RespondToModel(error));
        }
    };

    let call = ToolCall {
        tool_name: tool_name.clone(),
        call_id,
        payload,
        encrypted_function_args: None,
    };
    let output_token_limit =
        with_serialization_allowance(step_context.settings.model_info.truncation_policy.into())
            .token_budget();
    session
        .services
        .analytics_events_client
        .track_code_mode_tool_call(codex_analytics::CodeModeToolCallFact::ChildStarted {
            thread_id: session.thread_id.to_string(),
            turn_id: step_context.turn.sub_id.clone(),
            call_id: call.call_id.clone(),
            cell_id: cell_id.to_string(),
        });
    let result = tool_runtime.handle_tool_call_with_source(
        step_context,
        call,
        ToolCallSource::CodeMode {
            cell_id: cell_id.to_string(),
            runtime_tool_call_id,
        },
        cancellation_token.clone(),
        Arc::default(),
    );
    Ok(async move {
        let result = result.await?;
        if let Some(mut output) = result.code_mode_model_output() {
            if cancellation_token.is_cancelled() {
                return Err(FunctionCallError::RespondToModel(
                    "code mode model-only output relay cancelled".to_string(),
                ));
            }
            let qualified_name = match &tool_name.namespace {
                Some(namespace) if !tool_name.is_default_namespace() => {
                    format!("{namespace}.{}", tool_name.name)
                }
                _ => tool_name.name.clone(),
            };
            let mut items = vec![FunctionCallOutputContentItem::InputText {
                text: format!(
                    "Nested tool {qualified_name}, call_id {}: model-only output",
                    result.call_id
                ),
            }];
            match output.body {
                FunctionCallOutputBody::Text(text) => {
                    items.push(FunctionCallOutputContentItem::InputText { text })
                }
                FunctionCallOutputBody::ContentItems(content) => items.extend(content),
            }
            let encrypted = items
                .iter()
                .any(|item| matches!(item, FunctionCallOutputContentItem::EncryptedContent { .. }));
            output.body = FunctionCallOutputBody::ContentItems(items);
            // Responses accepts encrypted content only in function outputs.
            // Legacy plaintext results and attachments keep exec's normal kind.
            let item = if encrypted {
                ResponseInputItem::FunctionCallOutput {
                    call_id: outer_call_id,
                    output,
                }
            } else {
                ResponseInputItem::CustomToolCallOutput {
                    call_id: outer_call_id,
                    name: Some(PUBLIC_TOOL_NAME.to_string()),
                    output,
                }
            };
            delegate::inject_output(
                &session,
                &cell_id,
                item.into(),
                result
                    .result
                    .fallback_token_limit_override()
                    .unwrap_or(output_token_limit),
            )
            .await
            .map_err(FunctionCallError::RespondToModel)?;
        }
        Ok(result.code_mode_result())
    })
}

fn build_nested_tool_payload(
    tool_kind: CodeModeToolKind,
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    match tool_kind {
        CodeModeToolKind::Function => build_function_tool_payload(tool_name, input),
        CodeModeToolKind::Freeform => build_freeform_tool_payload(tool_name, input),
    }
}

fn build_function_tool_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    let arguments = serialize_function_tool_arguments(tool_name, input)?;
    Ok(ToolPayload::Function { arguments })
}

fn serialize_function_tool_arguments(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<String, String> {
    match input {
        None => Ok("{}".to_string()),
        Some(JsonValue::Object(map)) => serde_json::to_string(&JsonValue::Object(map))
            .map_err(|err| format!("failed to serialize tool `{tool_name}` arguments: {err}")),
        Some(_) => Err(format!(
            "tool `{tool_name}` expects a JSON object for arguments"
        )),
    }
}

fn build_freeform_tool_payload(
    tool_name: &ToolName,
    input: Option<JsonValue>,
) -> Result<ToolPayload, String> {
    match input {
        Some(JsonValue::String(input)) => Ok(ToolPayload::Custom { input }),
        _ => Err(format!("tool `{tool_name}` expects a string input")),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use super::build_nested_tool_payload;
    use super::truncate_code_mode_result;
    use crate::session::step_context::StepContext;
    use crate::session::tests::make_session_and_context;
    use crate::tools::context::ToolPayload;
    use crate::tools::registry::ToolRegistry;
    use crate::tools::router::ToolRouter;
    use crate::turn_diff_tracker::TurnDiffTracker;
    use codex_code_mode::CodeModeToolKind;
    use codex_protocol::models::FunctionCallOutputContentItem;
    use codex_protocol::openai_models::ToolMode;
    use codex_tools::ToolName;
    use serde_json::json;

    #[tokio::test]
    async fn notebook_control_checks_access_before_session_initialization() {
        for deno_program in [
            Some(std::path::PathBuf::from("/nonexistent/notebook-deno")),
            None,
        ] {
            let (session, mut turn) = make_session_and_context().await;
            let mut config = (*turn.config).clone();
            config.code_mode.runtime = codex_features::CodeModeRuntime::Notebook;
            // Both explicit and managed runtimes must report the permission boundary first.
            config.code_mode.deno_program = deno_program;
            let notebook_home = config.codex_home.join("notebook");
            assert!(!notebook_home.exists());
            turn.config = Arc::new(config);
            let service = super::CodeModeService::new(
                codex_protocol::ThreadId::new(),
                Arc::new(codex_code_mode::DisabledCodeModeSessionProvider),
                &turn.config,
                session.services.executed_tool_calls.clone(),
            );
            let step = StepContext::for_test(Arc::new(turn));
            let error = service
                .control_notebook(codex_notebook::NotebookRequest::Reset, &step)
                .await
                .expect_err("restricted control must fail");
            assert!(error.contains("danger-full-access"), "{error}");
            assert!(service.session.get().is_none());
            assert!(!notebook_home.exists());
        }
    }

    #[tokio::test]
    async fn notebook_core_cold_ephemeral_recovery_needs_neither_runtime_nor_storage() {
        let home = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let (session, turn, _rx) =
            crate::session::tests::make_session_and_context_with_auth_and_config_and_rx(
                codex_login::CodexAuth::from_api_key("Test API Key"),
                Vec::new(),
                |config| {
                    config.codex_home =
                        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(home.path())
                            .unwrap();
                    config.cwd = codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                        project.path(),
                    )
                    .unwrap();
                    config.ephemeral = true;
                    config.code_mode.runtime = codex_features::CodeModeRuntime::Notebook;
                    config.code_mode.deno_program = Some("/nonexistent/notebook-deno".into());
                    config
                        .permissions
                        .set_permission_profile(codex_protocol::models::PermissionProfile::Disabled)
                        .unwrap();
                },
            )
            .await;
        let step = StepContext::for_test(turn);
        let service = super::CodeModeService::new(
            codex_protocol::ThreadId::new(),
            Arc::new(codex_code_mode::DisabledCodeModeSessionProvider),
            &step.turn.config,
            session.services.executed_tool_calls.clone(),
        );
        let reset = service
            .control_notebook(codex_notebook::NotebookRequest::Reset, &step)
            .await
            .unwrap();
        assert_eq!(reset.details["reset"], true);
        let unpin = service
            .control_notebook(
                codex_notebook::NotebookRequest::Unpin {
                    names: vec!["missing".into()],
                },
                &step,
            )
            .await
            .unwrap();
        assert_eq!(unpin.details["unpinned"], json!([]));
        assert_eq!(unpin.details["missing"], json!(["missing"]));
        let error = service
            .control_notebook(
                codex_notebook::NotebookRequest::Unpin {
                    names: vec!["invalid-name".into()],
                },
                &step,
            )
            .await
            .unwrap_err();
        assert!(error.contains("valid binding names"));
        assert!(service.session.get().is_none());
        assert!(!home.path().join("notebook").exists());
        assert_eq!(std::fs::read_dir(project.path()).unwrap().count(), 0);
    }

    #[tokio::test]
    #[ignore = "requires a local Deno Jupyter executable"]
    async fn notebook_core_cold_recovery_never_replays_startup_side_effects() {
        use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;
        use codex_protocol::models::PermissionProfile;

        for fail_first in [false, true] {
            for action in ["unpin", "reset"] {
                let home = tempfile::tempdir().unwrap();
                let project = tempfile::tempdir().unwrap();
                let (session, turn, _rx) = make_session_and_context_with_auth_and_config_and_rx(
                    codex_login::CodexAuth::from_api_key("Test API Key"),
                    Vec::new(),
                    |config| {
                        config.codex_home =
                            codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                                home.path(),
                            )
                            .unwrap();
                        config.cwd =
                            codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(
                                project.path(),
                            )
                            .unwrap();
                        config.code_mode.runtime = codex_features::CodeModeRuntime::Notebook;
                        config.code_mode.deno_program = Some(
                            std::env::var_os("DENO_PROGRAM")
                                .map(std::path::PathBuf::from)
                                .unwrap_or_else(|| "deno".into()),
                        );
                        config
                            .permissions
                            .set_permission_profile(PermissionProfile::Disabled)
                            .unwrap();
                    },
                )
                .await;
                let step = StepContext::for_test(turn);
                let thread = codex_protocol::ThreadId::new();
                let make_service = || {
                    super::CodeModeService::new(
                        thread,
                        Arc::new(codex_code_mode::DisabledCodeModeSessionProvider),
                        &step.turn.config,
                        session.services.executed_tool_calls.clone(),
                    )
                };
                let original = make_service();
                let started = original.execute(codex_code_mode::ExecuteRequest {
                    tool_call_id: "seed-hook".into(),
                    source: "function brokenStartup() { Deno.writeTextFileSync('hook-runs', 'x', {append:true}); throw new Error('expected startup failure'); }".into(),
                    enabled_tools: Vec::new(),
                    yield_time_ms: Some(10_000),
                    max_output_tokens: None,
                }, step.clone()).await.unwrap();
                assert!(matches!(
                    started.initial_response().await.unwrap(),
                    codex_code_mode::RuntimeResponse::Result {
                        error_text: None,
                        ..
                    }
                ));
                original
                    .control_notebook(
                        serde_json::from_value(
                            json!({"action":"pin","names":["brokenStartup"],"hook":"startup"}),
                        )
                        .unwrap(),
                        &step,
                    )
                    .await
                    .unwrap();
                original.shutdown().await.unwrap();

                let resumed = make_service();
                if fail_first {
                    let error = resumed.session().await.err().expect("startup must fail");
                    assert!(error.contains("expected startup failure"), "{error}");
                }
                let request = if action == "unpin" {
                    json!({"action":"unpin","names":["brokenStartup"]})
                } else {
                    json!({"action":"reset"})
                };
                resumed
                    .control_notebook(serde_json::from_value(request).unwrap(), &step)
                    .await
                    .unwrap();
                assert!(resumed.session.get().is_none());
                assert_eq!(
                    std::fs::read_to_string(project.path().join("hook-runs")).unwrap_or_default(),
                    if fail_first { "x" } else { "" },
                    "{action} must not start or retry the hook"
                );
                resumed.shutdown().await.unwrap();
            }
        }
    }

    #[tokio::test]
    async fn turn_worker_uses_step_router_mode_instead_of_admitted_turn() {
        let (session, mut turn) = make_session_and_context().await;
        Arc::make_mut(&mut turn.config)
            .features
            .disable(codex_features::Feature::CodeMode)
            .expect("admit a direct-mode turn");
        assert_eq!(
            crate::tools::effective_tool_mode(&turn, turn.model_info()),
            ToolMode::Direct
        );
        let session = Arc::new(session);
        let step_context = StepContext::for_test(Arc::new(turn));
        let router = Arc::new(ToolRouter::from_parts(
            ToolRegistry::empty_for_test(),
            Vec::new(),
            ToolMode::CodeModeOnly,
            BTreeMap::new(),
            /*code_mode_instructions*/ None,
            /*tool_namespaces_info*/ None,
            &[],
        ));
        let step_context = step_context.with_tool_router_for_test(router);
        let tracker = Arc::new(tokio::sync::Mutex::new(TurnDiffTracker::new()));

        let worker =
            session
                .services
                .code_mode_service
                .start_turn_worker(&session, step_context, tracker);

        assert!(worker.is_some());
    }

    #[test]
    fn build_nested_tool_payload_uses_function_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Function,
            &ToolName::plain("example"),
            Some(json!({ "value": 1 })),
        )
        .expect("function payload should serialize");

        match payload {
            ToolPayload::Function { arguments } => {
                assert_eq!(arguments, r#"{"value":1}"#.to_string());
            }
            other => panic!("expected function payload, got {other:?}"),
        }
    }

    #[test]
    fn build_nested_tool_payload_uses_freeform_kind() {
        let payload = build_nested_tool_payload(
            CodeModeToolKind::Freeform,
            &ToolName::plain("example"),
            Some(json!("hello")),
        )
        .expect("freeform payload should preserve string input");

        match payload {
            ToolPayload::Custom { input } => {
                assert_eq!(input, "hello".to_string());
            }
            other => panic!("expected freeform payload, got {other:?}"),
        }
    }

    #[test]
    fn truncated_text_output_starts_with_warning() {
        let items = vec![FunctionCallOutputContentItem::InputText {
            text: "0123456789012345678901234567890123456789".to_string(),
        }];

        assert_eq!(
            truncate_code_mode_result(items, Some(5)),
            vec![FunctionCallOutputContentItem::InputText {
                text: concat!(
                    "Warning: truncated output (original token count: 10)\n",
                    "Total output lines: 1\n\n",
                    "0123456789…5 tokens truncated…0123456789"
                )
                .to_string(),
            }]
        );
    }

    #[test]
    fn over_budget_audio_output_is_omitted() {
        let items = vec![FunctionCallOutputContentItem::InputAudio {
            audio_url: format!("data:audio/wav;base64,{}", "A".repeat(100)),
        }];

        assert_eq!(
            truncate_code_mode_result(items, Some(5)),
            vec![FunctionCallOutputContentItem::InputText {
                text: "[omitted 1 audio items ...]".to_string(),
            }]
        );
    }
}
