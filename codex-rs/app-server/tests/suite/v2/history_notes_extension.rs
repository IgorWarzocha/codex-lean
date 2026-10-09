use std::collections::HashMap;
use std::time::Duration;

use anyhow::Result;
use app_test_support::MockResponsesConfig;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::RequestId;
use codex_app_server_protocol::ThreadLoadedListResponse;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_features::Feature;
use core_test_support::load_default_config_for_test;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tempfile::TempDir;
use test_case::test_case;
use tokio::time::timeout;
use wiremock::Mock;
use wiremock::Request;
use wiremock::ResponseTemplate;
use wiremock::matchers::body_partial_json;
use wiremock::matchers::method;
use wiremock::matchers::path;

use super::analytics::mount_analytics_capture;
use super::analytics::wait_for_matching_analytics_event;

const DEFAULT_READ_TIMEOUT: Duration = Duration::from_secs(30);

const THREAD_HINT: &str =
    "Recent notes (up to 5, most-recent first):\n- /root/notes/latest.md (2 lines, 14 UTF-8 bytes)";
const BRIDGE_HINT: &str = "unstructured notes/thread_hint fixture result";

#[tokio::test]
async fn context_strategy_notes_default_rejects_api_key_auth_without_loading_a_thread() -> Result<()>
{
    let server = responses::start_mock_server().await;
    let codex_home = TempDir::new()?;
    MockResponsesConfig::new(&server.uri())
        .with_root_config("context_strategy = 'notes'")
        .with_model_provider("openai-custom")
        .with_provider_name("OpenAI")
        .with_provider_base_url(&format!("{}/backend-api/codex", server.uri()))
        .with_provider_config("requires_openai_auth = true\nsupports_websockets = false")
        .write(codex_home.path())?;
    let mut app = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", Some("test-api-key"))])
        .build_initialized()
        .await?;
    let id = app
        .send_thread_start_request_with_auto_env(ThreadStartParams::default())
        .await?;
    let error = app
        .read_stream_until_error_message(RequestId::Integer(id))
        .await?;
    assert!(
        error
            .error
            .message
            .contains("context_strategy = 'notes' requires remote notes storage")
    );
    let loaded: ThreadLoadedListResponse = app
        .request(|request_id| ClientRequest::ThreadLoadedList {
            request_id,
            params: Default::default(),
        })
        .await?;
    assert!(loaded.data.is_empty());
    app.start_thread(ThreadStartParams {
        config: Some(HashMap::from([(
            "context_strategy".to_string(),
            json!("compaction"),
        )])),
        ..Default::default()
    })
    .await?;
    Ok(())
}

#[test_case(true, true, 200, THREAD_HINT; "native_hint")]
#[test_case(true, false, 200, THREAD_HINT; "native_hint_without_experimental_capability")]
#[test_case(true, true, 200, ""; "no_notes")]
#[test_case(true, true, 503, THREAD_HINT; "native_failure_does_not_use_bridge")]
#[test_case(false, true, 200, THREAD_HINT; "legacy_bridge_setting_cannot_disable_notes")]
#[tokio::test]
async fn app_server_uses_configured_notes_backend_for_context_window_hints(
    use_history_notes_extension: bool,
    supports_experimental_context: bool,
    hint_status: u16,
    hint_text: &str,
) -> Result<()> {
    let server = responses::start_mock_server().await;
    let backend = responses::start_mock_server().await;
    let response_mock = responses::mount_sse_sequence(
        &server,
        vec![
            responses::sse(vec![
                responses::ev_response_created("resp-1"),
                responses::ev_custom_tool_call(
                    "notes-catalog",
                    "exec",
                    "text(ALL_TOOLS.map(tool => tool.name));",
                ),
                responses::ev_completed("resp-1"),
            ]),
            responses::sse(vec![
                responses::ev_assistant_message("msg-1", "done"),
                responses::ev_completed("resp-2"),
            ]),
        ],
    )
    .await;
    // HTTP MCP stays on the app-server host, including with a remote executor.
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(|request: &Request| {
            let body: Value = request.body_json().expect("MCP JSON-RPC request");
            let Some(id) = body.get("id") else {
                return ResponseTemplate::new(202);
            };
            let result = match body["method"].as_str() {
                Some("initialize") => json!({
                    "protocolVersion": body["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "notes-fixture", "version": "1.0.0"},
                }),
                Some("tools/list") => json!({
                    "tools": [{
                        "name": "thread_hint",
                        "inputSchema": {"type": "object", "properties": {}},
                        "annotations": {"readOnlyHint": true},
                        "_meta": {"ui": {"visibility": []}},
                    }],
                }),
                Some("tools/call") => {
                    assert_eq!(body["params"]["name"], "thread_hint");
                    let thread_id = body["params"]["_meta"]["threadId"]
                        .as_str()
                        .expect("threadId metadata");
                    json!({"content": [
                        {"type": "text", "text": format!("manual history hint for thread {thread_id}")},
                        {"type": "text", "text": BRIDGE_HINT},
                    ]})
                }
                _ => {
                    return ResponseTemplate::new(200).set_body_json(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "error": {"code": -32601, "message": "Method not found"},
                    }));
                }
            };
            ResponseTemplate::new(200).set_body_json(json!({
                "jsonrpc": "2.0", "id": id, "result": result,
            }))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/backend-api/codex/alpha/notes/v2/thread_hint"))
        .respond_with(
            ResponseTemplate::new(hint_status).set_body_json(serde_json::json!({
                "text": hint_text
            })),
        )
        .mount(&server)
        .await;

    let codex_home = TempDir::new()?;
    let config = load_default_config_for_test(&codex_home).await;
    let mut model = codex_core::test_support::construct_model_info_offline("mock-model", &config);
    model.supports_experimental_context = supports_experimental_context;
    let catalog_path = codex_home.path().join("models.json");
    std::fs::write(
        &catalog_path,
        serde_json::to_vec(&json!({"models": [model]}))?,
    )?;
    MockResponsesConfig::new(&server.uri())
        // Exercise the actual Notes and Notebook defaults with their required
        // backend authentication and unsandboxed runtime, rather than opting out.
        .with_sandbox_mode("danger-full-access")
        .with_root_config("context_strategy = 'notes'")
        .with_extra_config(&format!(
            "[features.code_mode]\nenabled = true\nruntime = 'notebook'\ndeno_program = {}",
            serde_json::to_string(&std::env::var("DENO_PROGRAM").unwrap_or_else(|_| "deno".into()))?
        ))
        .with_root_config(&format!("chatgpt_base_url = \"{}\"", backend.uri()))
        .with_root_config(&format!(
            "model_catalog_json = {}",
            serde_json::to_string(&catalog_path)?
        ))
        .with_model_provider("openai-custom")
        .with_provider_name("OpenAI")
        .with_provider_base_url(&format!("{}/backend-api/codex", server.uri()))
        .with_provider_config("supports_websockets = false\nrequires_openai_auth = true")
        .with_extra_config(&format!(
            "[features.context_management]\nexperimental_mode = false\n\n[features.token_budget]\nenabled = true\nuse_history_notes_extension = {use_history_notes_extension}\n\n[mcp_servers.notes]\nurl = \"{}/mcp\"\nstartup_timeout_sec = 10\n",
            server.uri(),
        ))
        .write(codex_home.path())?;
    mount_analytics_capture(&backend, codex_home.path()).await?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .build_initialized()
        .await?;
    // The Notebook kernel and selected local executor must begin in the same
    // directory. This fixture selects an automatic environment explicitly.
    let cwd = app_server.auto_env_params()?.cwd;
    let thread = app_server
        .start_thread(ThreadStartParams {
            cwd: Some(cwd.to_string()),
            ..Default::default()
        })
        .await?
        .thread;
    let completed = timeout(
        Duration::from_secs(10),
        app_server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "inspect history and notes".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);

    let model_requests = response_mock.requests();
    assert_eq!(model_requests.len(), 2);
    let request = &model_requests[0];
    let (catalog, _) = model_requests[1]
        .custom_tool_call_output_content_and_success("notes-catalog")
        .expect("Notebook catalog output");
    let catalog = catalog.expect("Notebook must execute and return its nested tool catalog");
    let body = request.body_json();
    let exec = body["tools"]
        .as_array()
        .expect("model tool declarations")
        .iter()
        .find(|tool| tool["name"] == "exec")
        .expect("Notebook exec declaration");
    assert!(
        exec["description"]
            .as_str()
            .expect("exec description")
            .contains("Run JavaScript/TypeScript. Source only")
    );
    assert!(
        request
            .message_input_texts("developer")
            .join("\n")
            .contains("Persistent Deno/TypeScript notebook")
    );
    assert!(catalog.contains("collaboration__agent_board"));
    // MAv2 coordination is direct-model-only by default. The shared board is
    // callable from Notebook, but spawning and messaging must stay outside exec.
    for name in ["spawn_agent", "send_message", "followup_task"] {
        assert!(request.tool_by_name("collaboration", name).is_some());
        assert!(!catalog.contains(&format!("collaboration__{name}")));
    }
    assert!(!catalog.contains("collaboration__wait_agent"));
    assert!(
        request
            .tool_by_name("collaboration", "wait_agent")
            .is_none()
    );
    // Remote Notes capability is selected by the authenticated backend, not the
    // legacy use_history_notes_extension preference. Notes continuity requires it.
    for (namespace, tool_name) in [
        ("history", "list_windows"),
        ("history", "list_items"),
        ("history", "read_item"),
        ("history", "search_contents"),
        ("notes", "list_files_by_prefix"),
        ("notes", "read_file"),
        ("notes", "search_contents"),
        ("notes", "append_to_file"),
        ("notes", "write_file"),
    ] {
        assert!(
            catalog.contains(&format!("{namespace}__{tool_name}")),
            "Notebook should expose {namespace}.{tool_name} through exec"
        );
        assert!(request.tool_by_name(namespace, tool_name).is_none());
    }
    assert!(!catalog.contains("notes__thread_hint"));
    assert!(request.tool_by_name("notes", "thread_hint").is_none());

    let input = request.input();
    let bridge_hint_present = request
        .message_input_texts("developer")
        .iter()
        .any(|text| text.contains(BRIDGE_HINT));
    assert!(!bridge_hint_present);
    let requests = server.received_requests().await.expect("recorded requests");
    let native_requests = requests
        .iter()
        .filter(|request| request.url.path() == "/backend-api/codex/alpha/notes/v2/thread_hint")
        .count();
    assert_eq!(native_requests, 1);
    let bridge_calls = requests
        .iter()
        .filter(|request| request.url.path() == "/mcp")
        .filter(|request| {
            request
                .body_json::<Value>()
                .is_ok_and(|body| body["method"] == "tools/call")
        })
        .count();
    assert_eq!(bridge_calls, 0);
    let developer_messages = request.message_input_texts("developer");
    let context_window = developer_messages
        .iter()
        .find(|text| text.contains("<context_window>"))
        .expect("context-window developer message");
    let window_body = context_window
        .split_once("<context_window>")
        .expect("opening tag")
        .1
        .split_once("</context_window>")
        .expect("closing tag")
        .0;
    assert_eq!(
        window_body.contains(THREAD_HINT),
        hint_status == 200 && !hint_text.is_empty(),
    );
    assert!(!input.iter().any(|item| {
        item["type"] == "function_call"
            && item["namespace"] == "notes"
            && item["name"] == "thread_hint"
    }));

    let event = wait_for_matching_analytics_event(&backend, DEFAULT_READ_TIMEOUT, |event| {
        event["event_type"] == "codex_thread_hint_status"
            && event["event_params"]["thread_id"] == thread.id
    })
    .await?;
    assert_eq!(
        event["event_params"]["status"],
        if hint_status == 200 {
            "succeeded"
        } else {
            "failed"
        },
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn history_notes_and_async_message_emit_control_tool_analytics() -> Result<()> {
    let encrypted_query = format!("enc_query_{}", "x".repeat(1_001));
    let calls = [
        ("history", "list_windows", json!({})),
        ("history", "list_items", json!({})),
        (
            "history",
            "read_item",
            json!({"window_id": "PRIVATE_WINDOW", "item_id": "PRIVATE_ITEM"}),
        ),
        (
            "history",
            "search_contents",
            json!({"query": encrypted_query, "limit": 2, "recent_first": false}),
        ),
        (
            "notes",
            "list_files_by_prefix",
            json!({"prefix": "PRIVATE_PATH"}),
        ),
        ("notes", "read_file", json!({"path": "PRIVATE_PATH"})),
        (
            "notes",
            "search_contents",
            json!({"query": encrypted_query, "max_files": 2}),
        ),
        (
            "notes",
            "append_to_file",
            json!({"path": "PRIVATE_PATH", "text": "enc_append_text"}),
        ),
        (
            "notes",
            "write_file",
            json!({"path": "PRIVATE_PATH", "text": "enc_write_text"}),
        ),
        (
            "functions",
            "request_user_input_async",
            json!({"questions": [{"title": "PRIVATE_MESSAGE"}]}),
        ),
        (
            "notes",
            "list_files_by_prefix",
            json!(["PRIVATE_INVALID_ARGUMENTS"]),
        ),
        (
            "functions",
            "request_user_input_async",
            json!({"questions": [{"title": " "}]}),
        ),
    ];
    let server = responses::start_mock_server().await;
    let backend = responses::start_mock_server().await;
    for (namespace, tool, arguments) in &calls[..9] {
        Mock::given(method("POST"))
            .and(path(format!(
                "/backend-api/codex/alpha/{namespace}/v2/{tool}"
            )))
            .and(body_partial_json(arguments.clone()))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"text": "PRIVATE_RESULT"})),
            )
            .expect(1)
            .mount(&server)
            .await;
    }
    let mut sequence = calls
        .iter()
        .enumerate()
        .map(|(index, (namespace, tool, args))| {
            let response_id = format!("resp-{index}");
            responses::sse(vec![
                responses::ev_response_created(&response_id),
                responses::ev_function_call_with_namespace(
                    &format!("call-{index}"),
                    namespace,
                    tool,
                    &args.to_string(),
                ),
                responses::ev_completed(&response_id),
            ])
        })
        .collect::<Vec<_>>();
    sequence.push(responses::sse(vec![responses::ev_completed("resp-final")]));
    let response_mock = responses::mount_sse_sequence(&server, sequence).await;

    let codex_home = TempDir::new()?;
    let config = load_default_config_for_test(&codex_home).await;
    let mut model = codex_core::test_support::construct_model_info_offline("mock-model", &config);
    model.supports_experimental_context = true;
    model
        .experimental_supported_tools
        .push("send_user_message_async".to_string());
    let catalog_path = codex_home.path().join("models.json");
    std::fs::write(
        &catalog_path,
        serde_json::to_vec(&json!({"models": [model]}))?,
    )?;
    MockResponsesConfig::new(&server.uri())
        .with_model_provider("openai-custom")
        .with_provider_name("OpenAI")
        .with_provider_base_url(&format!("{}/backend-api/codex", server.uri()))
        .with_provider_config("supports_websockets = false\nrequires_openai_auth = true")
        .with_root_config(&format!("chatgpt_base_url = \"{}\"", backend.uri()))
        .with_root_config(&format!(
            "model_catalog_json = {}",
            serde_json::to_string(&catalog_path)?
        ))
        .with_extra_config(
            "[features.token_budget]\nenabled = true\nuse_history_notes_extension = true",
        )
        // This test sends direct function calls and verifies their individual
        // item IDs and exact control-tool counts. Notebook coverage lives above.
        .with_extra_config("[features.code_mode]\nruntime = 'v8'")
        .enable_feature(Feature::DefaultModeRequestUserInput)
        .write(codex_home.path())?;
    mount_analytics_capture(&backend, codex_home.path()).await?;

    let mut app_server = TestAppServer::builder()
        .with_codex_home(codex_home.path())
        .with_env_overrides(&[("OPENAI_API_KEY", None)])
        .without_managed_config()
        .build_initialized()
        .await?;
    let thread = app_server
        .start_thread(ThreadStartParams::default())
        .await?
        .thread;
    let completed = app_server
        .start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.id.clone(),
            input: vec![UserInput::Text {
                text: "exercise control tools".to_string(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    assert_eq!(response_mock.requests().len(), calls.len() + 1);

    let request = &response_mock.requests()[0];
    for (namespace, name, field) in [
        ("history", "search_contents", "query"),
        ("notes", "search_contents", "query"),
        ("notes", "append_to_file", "text"),
        ("notes", "write_file", "text"),
    ] {
        let tool = request
            .tool_by_name(namespace, name)
            .expect("declared history or notes tool");
        assert_eq!(tool["parameters"]["properties"][field]["encrypted"], true);
    }

    for (index, (namespace, tool, _)) in calls.iter().enumerate() {
        let event = wait_for_matching_analytics_event(&backend, DEFAULT_READ_TIMEOUT, |event| {
            event["event_type"] == "codex_control_tool_call_event"
                && event["event_params"]["item_id"] == format!("call-{index}")
        })
        .await?;
        let params = &event["event_params"];
        assert_eq!(
            json!({
                "tool": params["tool_name"],
                "thread": params["thread_id"],
                "turn": params["turn_id"],
                "status": params["terminal_status"],
                "origin": params["originating_response_id"],
                "duration": params["execution_duration_ms"].is_u64(),
            }),
            json!({
                "tool": if *namespace == "functions" { tool.to_string() } else { format!("{namespace}.{tool}") },
                "thread": thread.id,
                "turn": completed.turn.id,
                "status": if index < 10 { "completed" } else { "failed" },
                "origin": format!("resp-{index}"),
                "duration": true,
            })
        );
        assert!(!event.to_string().contains("PRIVATE_"));
    }
    let turn_event = wait_for_matching_analytics_event(&backend, DEFAULT_READ_TIMEOUT, |event| {
        event["event_type"] == "codex_turn_event"
            && event["event_params"]["turn_id"] == completed.turn.id
    })
    .await?;
    assert_eq!(
        json!({
            "total": turn_event["event_params"]["total_tool_call_count"],
            "dynamic": turn_event["event_params"]["dynamic_tool_call_count"],
        }),
        json!({"total": calls.len(), "dynamic": 0})
    );
    assert_eq!(
        response_mock.requests()[10].function_call_output_text("call-9"),
        Some(r#"{"accepted":true}"#.to_string())
    );
    assert_eq!(
        response_mock.requests()[11].function_call_output_text("call-10"),
        Some("History tool arguments must be a JSON object".to_string())
    );

    Ok(())
}
