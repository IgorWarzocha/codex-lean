use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use app_test_support::create_final_assistant_message_sse_response;
use app_test_support::create_mock_responses_server_sequence;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::JSONRPCMessage;
use codex_app_server_protocol::ServerRequest;
use codex_app_server_protocol::ServerRequestResolvedNotification;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::ThreadStartResponse;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStartResponse;
use codex_app_server_protocol::UserInput as V2UserInput;
use codex_features::Feature;
use codex_protocol::config_types::CollaborationMode;
use codex_protocol::config_types::ModeKind;
use codex_protocol::config_types::Settings;
use codex_protocol::openai_models::ReasoningEffort;
use core_test_support::responses;
use serde_json::json;
use test_case::test_case;
use tokio::time::timeout;

const DEFAULT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

fn create_request_user_input_sse_response(call_id: &str, notebook: bool) -> anyhow::Result<String> {
    let tool_call_arguments = serde_json::to_string(&json!({
        "questions": [{
            "id": "confirm_path",
            "header": "Confirm",
            "question": "Proceed with the plan?",
            "options": [{
                "label": "Yes (Recommended)",
                "description": "Continue the current plan."
            }, {
                "label": "No",
                "description": "Stop and revisit the approach."
            }]
        }]
    }))?;

    let call = if notebook {
        responses::ev_custom_tool_call(
            call_id,
            "exec",
            &format!("text(await tools.request_user_input({tool_call_arguments}));"),
        )
    } else {
        responses::ev_function_call(call_id, "request_user_input", &tool_call_arguments)
    };
    Ok(responses::sse(vec![
        responses::ev_response_created("resp-1"),
        call,
        responses::ev_completed("resp-1"),
    ]))
}

#[test_case(ModeKind::Plan, false; "direct_plan")]
#[test_case(ModeKind::Default, false; "direct_default")]
#[test_case(ModeKind::Plan, true; "notebook_plan")]
#[test_case(ModeKind::Default, true; "notebook_default")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn request_user_input_round_trip(mode: ModeKind, notebook: bool) -> Result<()> {
    let codex_home = tempfile::TempDir::new()?;
    let responses = vec![
        create_request_user_input_sse_response("call1", notebook)?,
        create_final_assistant_message_sse_response("done")?,
    ];
    let server = create_mock_responses_server_sequence(responses).await;
    let config = MockResponsesConfig::new(&server.uri())
        .with_approval_policy("on-request")
        .with_sandbox_mode("danger-full-access")
        .with_root_config("context_strategy = \"compaction\"");
    let config = if notebook {
        config
            .enable_feature(Feature::CodeModeOnly)
            .with_extra_config(&format!(
                "[features.code_mode]\nruntime = \"notebook\"\ndeno_program = {}",
                serde_json::to_string(
                    &std::env::var("DENO_PROGRAM").unwrap_or_else(|_| "deno".into())
                )?
            ))
    } else {
        config
            .disable_feature(Feature::CodeMode)
            .disable_feature(Feature::CodeModeOnly)
    };
    config.write(codex_home.path())?;

    let mut mcp = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .build_initialized()
        .await?;

    let cwd = mcp.auto_env_params()?.cwd;
    let ThreadStartResponse { thread, .. } = mcp
        .start_thread(ThreadStartParams {
            model: Some("mock-model".to_string()),
            cwd: Some(cwd.to_string()),
            config: (mode == ModeKind::Default).then(|| {
                std::collections::HashMap::from([(
                    "features.default_mode_request_user_input".to_string(),
                    json!(true),
                )])
            }),
            ..Default::default()
        })
        .await?;

    let TurnStartResponse { turn, .. } = mcp
        .request(|request_id| ClientRequest::TurnStart {
            request_id,
            params: TurnStartParams {
                thread_id: thread.id.clone(),
                client_user_message_id: None,
                input: vec![V2UserInput::Text {
                    text: "ask something".to_string(),
                    text_elements: Vec::new(),
                }],
                model: Some("mock-model".to_string()),
                effort: Some(ReasoningEffort::Medium),
                collaboration_mode: Some(CollaborationMode {
                    mode,
                    settings: Settings {
                        model: "mock-model".to_string(),
                        reasoning_effort: Some(ReasoningEffort::Medium),
                        developer_instructions: None,
                    },
                }),
                ..Default::default()
            },
        })
        .await?;

    let server_req = timeout(
        DEFAULT_READ_TIMEOUT,
        mcp.read_stream_until_request_message(),
    )
    .await??;
    let ServerRequest::ToolRequestUserInput { request_id, params } = server_req else {
        panic!("expected ToolRequestUserInput request, got: {server_req:?}");
    };

    assert_eq!(params.thread_id, thread.id);
    assert_eq!(params.turn_id, turn.id);
    assert!(!params.item_id.is_empty());
    if !notebook {
        assert_eq!(params.item_id, "call1");
    }
    assert_eq!(params.questions.len(), 1);
    assert_eq!(params.questions[0].id, "confirm_path");
    assert_eq!(params.questions[0].question, "Proceed with the plan?");
    assert_eq!(params.is_blocking, mode == ModeKind::Plan);
    assert_eq!(params.auto_resolution_ms, None);
    let resolved_request_id = request_id.clone();

    mcp.send_response(
        request_id,
        serde_json::json!({
            "answers": {
                "confirm_path": { "answers": ["Yes (Recommended)"] }
            }
        }),
    )
    .await?;
    let mut saw_resolved = false;
    loop {
        let message = timeout(DEFAULT_READ_TIMEOUT, mcp.read_next_message()).await??;
        let JSONRPCMessage::Notification(notification) = message else {
            continue;
        };
        match notification.method.as_str() {
            "serverRequest/resolved" => {
                let resolved: ServerRequestResolvedNotification = serde_json::from_value(
                    notification
                        .params
                        .clone()
                        .expect("serverRequest/resolved params"),
                )?;
                assert_eq!(resolved.thread_id, thread.id);
                assert_eq!(resolved.request_id, resolved_request_id);
                saw_resolved = true;
            }
            "turn/completed" => {
                assert!(saw_resolved, "serverRequest/resolved should arrive first");
                break;
            }
            _ => {}
        }
    }

    let requests = server.received_requests().await.expect("recorded requests");
    let requests = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/responses"))
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 2);
    let follow_up: serde_json::Value = serde_json::from_slice(&requests[1].body)?;
    let output_type = if notebook {
        "custom_tool_call_output"
    } else {
        "function_call_output"
    };
    let output = follow_up["input"]
        .as_array()
        .expect("input array")
        .iter()
        .find(|item| item["type"] == output_type && item["call_id"] == "call1")
        .expect("question result returned to the model");
    let output = output["output"].to_string();
    assert!(output.contains("confirm_path"), "{output}");
    assert!(output.contains("Yes (Recommended)"), "{output}");
    Ok(())
}
