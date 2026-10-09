use anyhow::Result;
use codex_config::config_toml::RealtimeWsVersion;
use codex_protocol::protocol::CodexResponseHandoffMode;
use codex_protocol::protocol::ConversationStartParams;
use codex_protocol::protocol::ConversationTextParams;
use codex_protocol::protocol::ConversationTextRole;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::RealtimeConversationRealtimeEvent;
use codex_protocol::protocol::RealtimeConversationVersion;
use codex_protocol::protocol::RealtimeEvent;
use codex_protocol::protocol::RealtimeOutputModality;
use codex_utils_absolute_path::AbsolutePathBuf;
use core_test_support::responses;
use core_test_support::responses::start_mock_server;
use core_test_support::responses::start_websocket_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use core_test_support::wait_for_event_match;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn universal_preferences_and_current_thread_seed_reach_text_and_voice_payloads() -> Result<()>
{
    skip_if_no_network!(Ok(()));
    let dir = tempfile::tempdir()?;
    let path = AbsolutePathBuf::try_from(dir.path().join("codex_personality.md"))?;
    tokio::fs::write(path.as_path(), "Prefer concise, direct communication.").await?;
    let api_server = start_mock_server().await;
    let mut commentary =
        responses::ev_assistant_message("commentary", "private commentary sentinel");
    commentary["item"]["phase"] = json!("commentary");
    let response_mock = responses::mount_sse_once(
        &api_server,
        responses::sse(vec![
            responses::ev_response_created("response"),
            commentary,
            responses::ev_assistant_message("answer", "Public settled answer."),
            responses::ev_completed("response"),
        ]),
    )
    .await;
    let realtime_server = start_websocket_server(vec![vec![vec![json!({
        "type": "session.started", "session": {"id": "current_thread"}
    })]]])
    .await;
    let mut builder = test_codex().with_config({
        let base_url = realtime_server.uri().to_string();
        move |config| {
            config.experimental_realtime_ws_base_url = Some(base_url);
            config.model_provider.supports_websockets = false;
            config.personality_file = path;
            config.personality_file_required = true;
            config.communication_preferences = Some("Prefer concise, direct communication.".into());
            config.base_instructions = Some("Native text baseline".into());
        }
    });
    let test = builder.build_with_auto_env(&api_server).await?;
    test.submit_turn("The task in this exact thread.").await?;
    let instructions = response_mock.single_request().instructions_text();
    assert!(instructions.starts_with("Native text baseline"));
    assert!(instructions.contains("user's preferred communication styles"));
    assert!(instructions.contains("Prefer concise, direct communication."));
    let mut params = start_params(RealtimeConversationVersion::V3);
    params.initial_items.clear();
    test.codex
        .submit(Op::RealtimeConversationStart(params))
        .await?;
    wait_for_event(&test.codex, |msg| {
        matches!(msg, EventMsg::RealtimeConversationStarted(_))
    })
    .await;
    let request = timeout(
        Duration::from_secs(2),
        realtime_server.wait_for_request(0, 0),
    )
    .await?;
    let voice_request = request.body_json();
    let instructions = voice_request["session"]["instructions"].as_str().unwrap();
    assert!(instructions.starts_with("backend prompt"));
    assert!(instructions.contains("Prefer concise, direct communication."));
    let seed = voice_request["session"]["initial_items"].to_string();
    assert!(seed.contains("The task in this exact thread."));
    assert!(seed.contains("Public settled answer."));
    assert!(!seed.contains("private commentary sentinel"));
    assert!(!seed.contains("Native text baseline"));
    test.codex.submit(Op::RealtimeConversationClose).await?;
    wait_for_event(&test.codex, |msg| {
        matches!(msg, EventMsg::RealtimeConversationClosed(_))
    })
    .await;
    realtime_server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voice_personality_file_and_ack_preference_reach_new_call_payloads() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let dir = tempfile::tempdir()?;
    let path = AbsolutePathBuf::try_from(dir.path().join("codex_personality.md"))?;
    let api_server = start_mock_server().await;
    let realtime_server = start_websocket_server(vec![
        vec![
            vec![json!({"type": "session.started", "session": {"id": "first"}})],
            vec![],
            vec![],
        ],
        vec![vec![
            json!({"type": "session.started", "session": {"id": "second"}}),
        ]],
        vec![vec![
            json!({"type": "session.started", "session": {"id": "third"}}),
        ]],
    ])
    .await;
    let mut builder = test_codex().with_config({
        let base_url = realtime_server.uri().to_string();
        let path = path.clone();
        move |config| {
            config.experimental_realtime_ws_base_url = Some(base_url);
            config.personality_file = path;
            config.personality_file_required = true;
            config.realtime.delegation_ack_filler = Some(true);
        }
    });
    let test = builder.build_with_auto_env(&api_server).await?;
    for (index, text) in ["Speak warmly.", "Speak tersely.", ""]
        .into_iter()
        .enumerate()
    {
        tokio::fs::write(path.as_path(), text).await?;
        let mut params = start_params(RealtimeConversationVersion::V3);
        params.include_startup_context = false;
        if index == 1 {
            params.delegation_ack_filler = Some(false);
        }
        test.codex
            .submit(Op::RealtimeConversationStart(params))
            .await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::RealtimeConversationStarted(_))
        })
        .await;
        let request = timeout(
            Duration::from_secs(2),
            realtime_server.wait_for_request(index, 0),
        )
        .await?;
        let body = request.body_json();
        let instructions = body["session"]["instructions"].as_str().unwrap();
        assert!(instructions.starts_with("backend prompt"));
        if text.is_empty() {
            assert_eq!(instructions, "backend prompt");
        } else {
            assert!(instructions.contains(text));
            assert!(instructions.contains("user's preferred communication styles"));
        }
        assert_eq!(body["session"]["delegation"]["ack_filler"], index != 1);
        if index == 0 {
            test.codex
                .submit(Op::RealtimeConversationText(ConversationTextParams {
                    role: ConversationTextRole::Developer,
                    text: "Already-admitted typed input; context only.".into(),
                }))
                .await?;
            let context = timeout(
                Duration::from_secs(2),
                realtime_server.wait_for_request(index, 1),
            )
            .await?;
            assert_eq!(
                context.body_json(),
                json!({
                    "type": "session.context.append",
                    "channel": "commentary",
                    "content": [{"type": "input_text", "text": "Already-admitted typed input; context only."}],
                })
            );
            // The TUI greeting uses appendSpeech's literal output contract,
            // never a "say hello" instruction or a synthetic host input.
            test.codex
                .submit(Op::RealtimeConversationSpeech(
                    codex_protocol::protocol::ConversationSpeechParams {
                        text: "Hi. What would you like to work on?".into(),
                    },
                ))
                .await?;
            let greeting = timeout(
                Duration::from_secs(2),
                realtime_server.wait_for_request(index, 2),
            )
            .await?;
            assert_eq!(
                greeting.body_json(),
                json!({
                    "type": "session.context.append", "channel": "speakable",
                    "content": [{"type": "input_text", "text": "Hi. What would you like to work on?"}],
                })
            );
        }
        test.codex.submit(Op::RealtimeConversationClose).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::RealtimeConversationClosed(_))
        })
        .await;
    }
    realtime_server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frameless_v3_sends_initial_items_in_session_bootstrap() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let api_server = start_mock_server().await;
    let realtime_server = start_websocket_server(vec![vec![vec![json!({
        "type": "session.started",
        "session": { "id": "sess_initial_items", "instructions": "backend prompt" }
    })]]])
    .await;
    let mut builder = test_codex().with_config({
        let realtime_base_url = realtime_server.uri().to_string();
        move |config| {
            config.experimental_realtime_ws_base_url = Some(realtime_base_url);
            config.experimental_realtime_ws_startup_context = Some(String::new());
            config.realtime.version = RealtimeWsVersion::V3;
        }
    });
    let test = builder.build_with_auto_env(&api_server).await?;

    test.codex
        .submit(Op::RealtimeConversationStart(start_params(
            RealtimeConversationVersion::V3,
        )))
        .await?;

    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::RealtimeConversationStarted(_))
    })
    .await;
    let request = timeout(
        Duration::from_secs(2),
        realtime_server.wait_for_request(/*connection_index*/ 0, /*request_index*/ 0),
    )
    .await?;
    let body = request.body_json();

    assert_eq!(body["type"], "session.update");
    assert_eq!(body["session"]["instructions"], "backend prompt");
    assert_eq!(
        body["session"]["initial_items"],
        json!([
            {
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": "Remember this."}],
            },
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "What do you remember?"}],
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "I remember."}],
            },
        ])
    );

    realtime_server.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_items_require_frameless_v3() -> Result<()> {
    skip_if_no_network!(Ok(()));

    assert_start_error(
        start_params(RealtimeConversationVersion::V2),
        "initial realtime items require realtime v3",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_items_enforce_count_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut params = start_params(RealtimeConversationVersion::V3);
    params.initial_items = vec![
        ConversationTextParams {
            text: "item".to_string(),
            role: ConversationTextRole::User,
        };
        129
    ];
    assert_start_error(
        params,
        "initial realtime items must contain no more than 128 items",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_items_enforce_per_item_token_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut params = start_params(RealtimeConversationVersion::V3);
    params.initial_items = vec![ConversationTextParams {
        text: "x".repeat(8_192 * 4 + 1),
        role: ConversationTextRole::User,
    }];
    assert_start_error(
        params,
        "each initial realtime item must not exceed 8192 estimated tokens",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initial_items_enforce_aggregate_token_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut params = start_params(RealtimeConversationVersion::V3);
    params.initial_items = vec![
        ConversationTextParams {
            text: "x".repeat(8_192 * 2 + 1),
            role: ConversationTextRole::User,
        };
        2
    ];
    assert_start_error(
        params,
        "initial realtime items must not exceed 8192 estimated tokens in total",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn realtime_start_instructions_enforce_token_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut params = start_params(RealtimeConversationVersion::V3);
    params.realtime_start_instructions = Some("x".repeat(8_192 * 4 + 1));
    assert_start_error(
        params,
        "realtime start instructions must not exceed 8192 estimated tokens",
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn realtime_end_instructions_enforce_token_limit() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let mut params = start_params(RealtimeConversationVersion::V3);
    params.realtime_end_instructions = Some("x".repeat(8_192 * 4 + 1));
    assert_start_error(
        params,
        "realtime end instructions must not exceed 8192 estimated tokens",
    )
    .await
}

async fn assert_start_error(params: ConversationStartParams, expected_error: &str) -> Result<()> {
    let api_server = start_mock_server().await;
    let test = test_codex().build_with_auto_env(&api_server).await?;
    test.codex
        .submit(Op::RealtimeConversationStart(params))
        .await?;
    let error = wait_for_event_match(&test.codex, |msg| match msg {
        EventMsg::RealtimeConversationRealtime(RealtimeConversationRealtimeEvent {
            payload: RealtimeEvent::Error(message),
        }) => Some(message.clone()),
        _ => None,
    })
    .await;
    assert!(
        error.contains(expected_error),
        "expected error to contain {expected_error:?}, got {error:?}"
    );
    Ok(())
}

fn start_params(version: RealtimeConversationVersion) -> ConversationStartParams {
    ConversationStartParams {
        client_managed_handoffs: false,
        delegation_ack_filler: None,
        flush_transcript_tail_on_session_end: false,
        codex_responses_as_items: false,
        codex_response_item_prefix: None,
        codex_response_handoff_mode: CodexResponseHandoffMode::Thinking,
        backend_reasoning_status: false,
        codex_response_handoff_channel_prefixes: None,
        model: None,
        output_modality: RealtimeOutputModality::Audio,
        include_startup_context: true,
        initial_items: vec![
            ConversationTextParams {
                text: "Remember this.".to_string(),
                role: ConversationTextRole::Developer,
            },
            ConversationTextParams {
                text: "What do you remember?".to_string(),
                role: ConversationTextRole::User,
            },
            ConversationTextParams {
                text: "I remember.".to_string(),
                role: ConversationTextRole::Assistant,
            },
        ],
        realtime_start_instructions: None,
        realtime_end_instructions: None,
        prompt: Some(Some("backend prompt".to_string())),
        realtime_session_id: None,
        transport: None,
        version: Some(version),
        voice: None,
    }
}
