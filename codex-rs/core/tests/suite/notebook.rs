use anyhow::Result;
use codex_code_mode::CodeModeSessionProvider;
use codex_config::types::ContextStrategy;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_features::CodeModeRuntime;
use codex_features::Feature;
use codex_login::CodexAuth;
use codex_notebook::DenoNotebookSessionProvider;
use codex_protocol::dynamic_tools::DynamicToolCallOutputContentItem;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolResponse;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::TurnItem;
use codex_protocol::models::PermissionProfile;
use codex_protocol::openai_models::ToolMode;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event_match;
use serde_json::Value;
use serde_json::json;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn codex_app_json_reply_is_filterable_in_notebook_with_native_dispatch() -> Result<()> {
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_custom_tool_call(
                    "desktop-query",
                    "exec",
                    "const result = await tools.codex_app__list_artifacts({}); text(result.kept);",
                ),
                ev_completed("desktop-query-response"),
            ]),
            sse(vec![
                ev_assistant_message("done", "Done."),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    let mut test = test_codex().build_with_auto_env(&server).await?;
    let fixture: Value = serde_json::from_str(include_str!("../../assets/tools/codex_app.json"))?;
    let namespace: DynamicToolNamespaceSpec = serde_json::from_value(fixture["namespace"].clone())?;
    test.codex.shutdown_and_wait().await?;
    let thread = test
        .thread_manager
        .start_thread(StartThreadOptions {
            dynamic_tools: vec![DynamicToolSpec::Namespace(namespace)],
            environments: Some(vec![test.executor_environment().selection().clone()]),
            ..StartThreadOptions::new(test.config.clone())
        })
        .await?;
    test.codex = thread.thread;
    test.session_configured = thread.session_configured;
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Inspect the attached artifacts.".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                permission_profile: Some(PermissionProfile::Disabled),
                ..Default::default()
            }),
        )
        .await?;
    let call = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::ItemStarted(event) => match &event.item {
            TurnItem::DynamicToolCall(call) => Some(call.clone()),
            _ => None,
        },
        EventMsg::Error(error) => panic!("failed before dynamic dispatch: {}", error.message),
        EventMsg::TurnComplete(_) => panic!("completed without dynamic dispatch"),
        _ => None,
    })
    .await;
    assert_eq!(call.namespace.as_deref(), Some("codex_app"));
    assert_eq!(call.tool, "list_artifacts");
    assert_eq!(call.arguments, json!({}));
    // Synthetic data tests our JSON-to-notebook contract, not the Desktop's
    // undocumented per-tool result fields. Native client execution stays intact.
    test.codex
        .submit(Op::DynamicToolResponse {
            id: call.id,
            response: DynamicToolResponse {
                content_items: vec![DynamicToolCallOutputContentItem::InputText {
                    text: json!({"kept": "selected-value", "bulk": "not-printed"}).to_string(),
                }],
                success: true,
            },
        })
        .await?;
    wait_for_event_match(&test.codex, |event| match event {
        EventMsg::TurnComplete(_) => Some(()),
        EventMsg::Error(error) => panic!("notebook failed: {}", error.message),
        _ => None,
    })
    .await;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1]
        .custom_tool_call_output("desktop-query")
        .to_string();
    assert!(output.contains("selected-value"), "{output}");
    assert!(!output.contains("not-printed"), "{output}");
    assert!(!output.contains("Script error"), "{output}");
    assert!(
        requests[0].body_json()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .all(|tool| tool["name"] != "codex_app")
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_notebook_rejects_sandboxed_sampling_before_provider_request() -> Result<()> {
    let server = start_mock_server().await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path_regex(".*/responses$"))
        .respond_with(wiremock::ResponseTemplate::new(200))
        .expect(0)
        .mount(&server)
        .await;
    let test = test_codex().build_with_auto_env(&server).await?;
    assert!(test.config.features.enabled(Feature::CodeMode));
    assert_eq!(test.config.code_mode.runtime, CodeModeRuntime::Notebook);
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Continue with restricted permissions.".to_string(),
                text_elements: Vec::new(),
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                permission_profile: Some(PermissionProfile::workspace_write()),
                ..Default::default()
            }),
        )
        .await?;
    let error = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::Error(error) => Some(error.clone()),
        _ => None,
    })
    .await;
    assert!(
        error.message.contains("danger-full-access"),
        "{}",
        error.message
    );
    assert!(
        server.received_requests().await.unwrap().is_empty(),
        "permission rejection must precede sampling"
    );
    assert!(!test.home.path().join("notebook").exists());
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn default_notebook_emits_native_view_image_result() -> Result<()> {
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_custom_tool_call(
                    "view-image",
                    "exec",
                    "image(await tools.view_image({path: 'image.bin', detail: 'original'}));",
                ),
                ev_completed("image-response"),
            ]),
            sse(vec![
                ev_assistant_message("done", "Done."),
                ev_completed("done"),
            ]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.code_mode.deno_program = std::env::var_os("DENO_PROGRAM").map(Into::into);
        })
        .build(&server)
        .await?;
    assert_eq!(test.config.code_mode.runtime, CodeModeRuntime::Notebook);
    assert!(test.config.features.enabled(Feature::CodeMode));
    let mut png = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(1, 1, image::Rgba([255, 0, 0, 255]))
        .write_to(&mut png, image::ImageFormat::Png)?;
    // MIME must come from the validated bytes, not the file extension.
    std::fs::write(test.cwd_path().join("image.bin"), png.into_inner())?;
    test.submit_turn("Inspect the local image.").await?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    let output = requests[1].custom_tool_call_output("view-image");
    let items = output["output"]
        .as_array()
        .expect("multimodal Notebook output");
    assert!(
        items.iter().any(|item| {
            item["type"] == "input_image"
                && item["image_url"]
                    .as_str()
                    .is_some_and(|url| url.starts_with("data:image/png;base64,"))
        }),
        "{output}"
    );
    assert!(!output.to_string().contains("Script error"), "{output}");
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[test_case::test_case(PermissionProfile::workspace_write(); "sandboxed")]
#[test_case::test_case(PermissionProfile::Disabled; "full_access")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn disabled_code_mode_does_not_acquire_default_notebook(
    profile: PermissionProfile,
) -> Result<()> {
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(&server, vec![sse(vec![ev_completed("done")])]).await;
    let test = test_codex()
        .with_model_info_override("gpt-5.4", |model| model.tool_mode = Some(ToolMode::Direct))
        .with_config(|config| {
            config.features.disable(Feature::CodeMode).unwrap();
            config.code_mode.deno_program = Some("/nonexistent/default-notebook-deno".into());
        })
        .build_with_auto_env(&server)
        .await?;
    assert_eq!(test.config.code_mode.runtime, CodeModeRuntime::Notebook);
    assert!(test.config.features.enabled(Feature::CodeModePrewarm));
    test.submit_turn_with_permission_profile("Continue.", profile)
        .await?;
    let request = mock.single_request();
    assert!(request.tool_by_name("functions", "exec").is_none());
    assert!(request.tool_by_name("functions", "notebook").is_none());
    assert!(!request.instructions_text().contains("<exec_tools>"));
    assert!(
        !request
            .message_input_texts("developer")
            .join("\n")
            .contains("Notebook")
    );
    assert!(!test.home.path().join("notebook").exists());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn direct_model_transition_revokes_live_notebook_without_running_disposers() -> Result<()> {
    let deno = std::env::var_os("DENO_PROGRAM")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "deno".into());
    if let Err(error) =
        DenoNotebookSessionProvider::new(deno.clone(), std::env::current_dir()?).availability()
    {
        eprintln!("skipping Notebook integration test: {error}");
        return Ok(());
    }
    let server = start_mock_server().await;
    let mock = mount_sse_sequence(&server, vec![
        sse(vec![ev_custom_tool_call("seed-disposer", "exec", "var resource = {[Symbol.dispose]() { Deno.writeTextFileSync('disposed-after-revocation', 'x'); }}; text('ready')"), ev_completed("seed")]),
        sse(vec![ev_assistant_message("ready", "Ready."), ev_completed("ready")]),
    ]).await;
    let test = test_codex()
        .with_model_info_override("gpt-5.2", |model| model.tool_mode = Some(ToolMode::Direct))
        .with_model_info_override("gpt-5.4", |model| {
            model.tool_mode = Some(ToolMode::CodeMode)
        })
        .with_config(move |config| {
            config.features.disable(Feature::CodeMode).unwrap();
            config.code_mode.deno_program = Some(deno);
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Start the assigned work.").await?;
    assert_eq!(mock.requests().len(), 2);
    let (output, success) = mock.requests()[1]
        .custom_tool_call_output_content_and_success("seed-disposer")
        .unwrap();
    assert_ne!(success, Some(false));
    assert!(output.unwrap().contains("ready"));
    test.codex
        .start_or_steer_turn(
            TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Continue with restricted permissions.".to_string(),
                text_elements: vec![],
            }])
            .with_thread_settings(ThreadSettingsOverrides {
                model: Some("gpt-5.2".to_string()),
                permission_profile: Some(PermissionProfile::workspace_write()),
                ..Default::default()
            }),
        )
        .await?;
    let error = wait_for_event_match(&test.codex, |event| match event {
        EventMsg::Error(error) => Some(error.clone()),
        _ => None,
    })
    .await;
    assert!(
        error.message.contains("danger-full-access"),
        "{}",
        error.message
    );
    assert_eq!(
        mock.requests().len(),
        2,
        "revocation must precede direct-model sampling"
    );
    test.codex.shutdown_and_wait().await?;
    assert!(!test.cwd_path().join("disposed-after-revocation").exists());
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn notebook_context_restores_status_after_rollover_without_exposing_values() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let deno = std::env::var_os("DENO_PROGRAM")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| "deno".into());
    if let Err(error) =
        DenoNotebookSessionProvider::new(deno.clone(), std::env::current_dir()?).availability()
    {
        eprintln!("skipping Notebook integration test: {error}");
        return Ok(());
    }
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("resp-1"),
                ev_custom_tool_call(
                    "create-binding",
                    "exec",
                    r#"globalThis.retainedNotebookBinding = "private-secret-marker"; text("created")"#,
                ),
                ev_completed("resp-1"),
            ]),
            sse(vec![
                ev_response_created("resp-2"),
                ev_function_call("rollover", "new_context", "{}"),
                ev_completed("resp-2"),
            ]),
            sse(vec![
                ev_response_created("resp-3"),
                ev_custom_tool_call(
                    "read-binding",
                    "exec",
                    r#"let rejected = false;
try { await tools.notebook({action: "prune", query: "*"}); } catch { rejected = true; }
text({value: globalThis.retainedNotebookBinding, rejected, status: (await tools.notebook({action: "status"})).message});"#,
                ),
                ev_completed("resp-3"),
            ]),
            sse(vec![
                ev_response_created("resp-4"),
                ev_function_call("checkpoint", "notebook", r#"{"action":"checkpoint"}"#),
                ev_completed("resp-4"),
            ]),
            sse(vec![
                ev_assistant_message("done", "done"),
                ev_completed("resp-5"),
            ]),
        ],
    )
    .await;
    let backend_url = format!("{}/backend-api/codex", server.uri());
    let test = test_codex()
        .with_context_strategy(ContextStrategy::Notes)
        .with_auth(CodexAuth::from_external_chatgpt_tokens(
            "header.e30.signature",
            "account-123",
            Some("plus"),
        )?)
        .with_config(move |config| {
            config.model_provider.base_url = Some(backend_url);
            config.code_mode.deno_program = Some(deno);
            config.ephemeral = true;
            config.base_instructions = Some("Keep this explicit base unchanged.".to_string());
            config.features.enable(Feature::TokenBudget).unwrap();
        })
        .build(&server)
        .await?;
    assert!(test.config.features.enabled(Feature::CodeMode));
    assert!(!test.config.features.enabled(Feature::CodeModeOnly));
    assert_eq!(test.config.code_mode.runtime, CodeModeRuntime::Notebook);
    assert!(test.config.code_mode.disable_in_process_fallback);
    test.submit_turn("Retain a binding across a new context window")
        .await?;
    let requests = responses.requests();
    assert_eq!(requests.len(), 5);
    let instructions = requests[0].instructions_text();
    assert!(instructions.starts_with("Keep this explicit base unchanged.\n\n<exec_tools>\n"));
    assert!(instructions.contains("tools.exec_command("));
    assert!(instructions.contains(
        "Filter ALL_TOOLS by name or description. Print names only. Read tools.NAME.description for selected tools."
    ));
    for request in &requests {
        // Repeated samples and context rollover must not accumulate tool catalogs.
        assert_eq!(request.instructions_text(), instructions);
        assert_eq!(
            request.instructions_text().matches("<exec_tools>").count(),
            1
        );
        assert!(
            !request
                .message_input_texts("developer")
                .join("\n")
                .contains("<exec_tools>")
        );
        let body = request.body_json();
        let exec = body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "exec")
            .expect("Notebook exec declaration");
        let description = exec["description"].as_str().unwrap();
        assert!(!description.contains("tools.exec_command("));
        assert!(!description.contains("<exec_tools>"));
    }
    let startup = requests[0].message_input_texts("developer").join("\n");
    assert!(startup.contains("Notebook idle"), "{startup}");
    assert!(!startup.contains("Notebook status unavailable"));
    let rollover = requests[2].message_input_texts("developer").join("\n");
    assert!(rollover.contains("retainedNotebookBinding"), "{rollover}");
    assert!(!rollover.contains("private-secret-marker"));
    let output = requests[3]
        .custom_tool_call_output("read-binding")
        .to_string();
    assert!(output.contains("private-secret-marker"), "{output}");
    assert!(
        output.contains("rejected") && output.contains("true"),
        "{output}"
    );
    assert!(output.contains("Notebook running (cached)"), "{output}");
    let checkpoint = requests[4].function_call_output("checkpoint").to_string();
    assert!(checkpoint.contains("Notebook checkpoint"), "{checkpoint}");
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[test_case::test_case(false; "json")]
#[test_case::test_case(true; "plain")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires a local Deno executable and Unix shell"]
#[cfg(unix)]
async fn notebook_command_output_projects_only_the_displayed_result(plain: bool) -> Result<()> {
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_response_created("command-response"),
                ev_custom_tool_call(
                    "command-output",
                    "exec",
                    r#"var commandResult = await tools.exec_command({cmd: "printf 'first\\nsecond\\n'; exit 7", login: false});
text(commandResult);
text({...commandResult});
text(commandResult.output);"#,
                ),
                ev_completed("command-response"),
            ]),
            sse(vec![ev_assistant_message("done", "done"), ev_completed("done")]),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(move |config| {
            config.code_mode.runtime = CodeModeRuntime::Notebook;
            config.code_mode.deno_program = Some(
                std::env::var_os("DENO_PROGRAM")
                    .map(std::path::PathBuf::from)
                    .unwrap_or_else(|| "deno".into()),
            );
            config.code_mode.notebook_plain_command_output = plain;
            config.ephemeral = true;
            config.features.enable(Feature::CodeModeOnly).unwrap();
        })
        .build(&server)
        .await?;
    test.submit_turn("Run the command and inspect the returned object")
        .await?;
    let requests = responses.requests();
    let result = requests[1].custom_tool_call_output("command-output");
    let items = result["output"].as_array().expect("three text emissions");
    assert_eq!(items.len(), 3, "{result}");
    let projected = items[0]["text"].as_str().unwrap();
    let projected_metadata: serde_json::Value = serde_json::from_str(if plain {
        let (metadata, output) = projected.split_once("\nOutput:\n").unwrap();
        assert_eq!(output, "first\nsecond\n");
        metadata
    } else {
        projected
    })?;
    assert_eq!(projected_metadata["exit_code"], 7);
    for key in ["chunk_id", "wall_time_seconds", "original_token_count"] {
        assert!(projected_metadata.get(key).is_none(), "{projected}");
    }
    if !plain {
        assert_eq!(projected_metadata["output"], "first\nsecond\n");
    }
    let raw: serde_json::Value = serde_json::from_str(items[1]["text"].as_str().unwrap())?;
    assert_eq!(raw["exit_code"], 7);
    assert_eq!(raw["output"], "first\nsecond\n");
    assert!(raw.get("chunk_id").is_some(), "{raw}");
    assert!(raw.get("wall_time_seconds").is_some(), "{raw}");
    assert_eq!(items[2]["text"], "first\nsecond\n");
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
