//! Checks native role guidance against the tools actually sent on the first step.

use super::done;
use super::tool;
use anyhow::Context;
use codex_agent_message_board_extension::AGENT_BOARD_TOOL_NAME;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_features::Feature;
use codex_features::RemoteMessageBoardConfigToml;
use codex_protocol::models::PermissionProfile;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::InternalSessionSource;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::user_input::UserInput;
use core_test_support::ThreadIdle;
use core_test_support::responses;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_custom_tool_call;
use core_test_support::responses::sse;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

const BOARD_GUIDANCE: &str = "In authorized substantial multi-agent workflows, use `agent_board` for shared decisions, dependencies, and findings. Parents pass relevant channel names and thread IDs in assignments. Children read and update those threads. Board posts do not assign work or wake idle agents.";
const DIRECT_GUIDANCE: &str = "Use direct messages for targeted coordination. Use `followup_task` to start work for idle agents";

fn board_is_callable(request: &ResponsesRequest) -> bool {
    request
        .tool_by_name("collaboration", AGENT_BOARD_TOOL_NAME)
        .is_some()
        || request
            .instructions_text()
            .contains("tools.collaboration__agent_board(")
}

#[derive(Clone, Copy)]
enum Availability {
    Default,
    Disabled,
    Ephemeral,
    FailedStartup,
    V1,
    Internal,
    BoardOnly,
}

#[test_case::test_case(Availability::Default, true; "default_on")]
#[test_case::test_case(Availability::Disabled, false; "explicit_off")]
#[test_case::test_case(Availability::Ephemeral, false; "ephemeral_local_unavailable")]
#[test_case::test_case(Availability::FailedStartup, false; "startup_failure")]
#[test_case::test_case(Availability::V1, false; "legacy_runtime")]
#[test_case::test_case(Availability::Internal, false; "internal_worker")]
#[test_case::test_case(Availability::BoardOnly, true; "board_only_coordination")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn first_request_guidance_tracks_actual_board_availability(
    availability: Availability,
    expected: bool,
) -> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_sequence(&server, vec![done(), done()]).await;
    let test = test_codex()
        .with_config(move |config| {
            super::configure(config);
            assert!(config.features.enabled(Feature::AgentMessageBoard));
            match availability {
                Availability::Disabled => {
                    config
                        .features
                        .disable(Feature::AgentMessageBoard)
                        .expect("configure test feature flags");
                }
                Availability::Ephemeral => config.ephemeral = true,
                Availability::FailedStartup => {
                    // The real extension fails closed before registering the tool.
                    config.multi_agent_v2.message_board_remote =
                        Some(RemoteMessageBoardConfigToml {
                            url: "http://127.0.0.1:1".to_string(),
                            bearer_token: None,
                            bearer_token_env_var: None,
                        });
                }
                Availability::V1 => {
                    config
                        .features
                        .disable(Feature::MultiAgentV2)
                        .expect("configure test feature flags");
                }
                Availability::BoardOnly => config.multi_agent_v2.disable_direct_message = true,
                Availability::Default | Availability::Internal => {}
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let internal = if matches!(availability, Availability::Internal) {
        Some(
            test.thread_manager
                .start_thread(StartThreadOptions {
                    session_source: Some(SessionSource::Internal(InternalSessionSource::Guardian)),
                    ..StartThreadOptions::new(test.config.clone())
                })
                .await?,
        )
    } else {
        None
    };
    let thread = internal
        .as_ref()
        .map_or(test.codex.as_ref(), |internal| internal.thread.as_ref());
    for _ in 0..2 {
        thread
            .start_or_steer_turn(
                TurnInputRequest::user_input(vec![UserInput::Text {
                    text: "Continue the existing task.".to_string(),
                    text_elements: vec![],
                }])
                .with_thread_settings(ThreadSettingsOverrides {
                    permission_profile: Some(PermissionProfile::Disabled),
                    ..Default::default()
                }),
            )
            .await?;
        wait_for_event(thread, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    }
    let requests = mock.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(board_is_callable(request), expected);
        let developer = request.message_input_texts("developer");
        assert_eq!(
            developer
                .iter()
                .filter(|text| text.contains(BOARD_GUIDANCE))
                .count(),
            usize::from(expected)
        );
        assert_eq!(
            developer.iter().any(|text| text.contains(DIRECT_GUIDANCE)),
            expected && !matches!(availability, Availability::BoardOnly)
        );
        if !matches!(availability, Availability::V1 | Availability::Internal) {
            assert_eq!(
                developer
                    .iter()
                    .filter(|text| text.contains("Role: `/root`"))
                    .count(),
                1
            );
            assert!(request.body_contains_text("Agent spawning only on explicit request"));
        }
    }
    Ok(())
}

#[test_case::test_case(Some("Configured role."), false; "configured_role_verbatim")]
#[test_case::test_case(Some(""), false; "empty_configured_role")]
#[test_case::test_case(None, true; "empty_catalog_role")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn available_board_preserves_role_override_semantics(
    configured: Option<&'static str>,
    empty_catalog: bool,
) -> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let mock = responses::mount_sse_once(&server, done()).await;
    let test = test_codex()
        .with_config(move |config| {
            super::configure(config);
            config.multi_agent_v2.root_agent_usage_hint_text = configured.map(str::to_owned);
        })
        .with_model_info_override("gpt-5.4", move |model| {
            if empty_catalog {
                model
                    .model_messages
                    .as_mut()
                    .expect("fixture model contains model messages")
                    .multi_agent = Some(codex_protocol::openai_models::MultiAgentMessages {
                    role: Some(codex_protocol::openai_models::MultiAgentRoleMessages {
                        root: Some(String::new()),
                        subagent: None,
                    }),
                    mode: None,
                });
            }
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Continue the assigned task.").await?;
    let request = mock.single_request();
    assert!(board_is_callable(&request));
    let developer = request.message_input_texts("developer");
    assert!(
        !developer
            .iter()
            .any(|text| text.contains(BOARD_GUIDANCE) || text.contains("Role: `/root`"))
    );
    if let Some(configured) = configured.filter(|text| !text.is_empty()) {
        assert_eq!(
            developer
                .iter()
                .filter(|text| text.as_str() == configured)
                .count(),
            1
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn board_guidance_has_exact_bounded_prompt_cost() -> anyhow::Result<()> {
    let mut roles = Vec::new();
    for enabled in [false, true] {
        let server = responses::start_mock_server().await;
        let mock = responses::mount_sse_once(&server, done()).await;
        let test = test_codex()
            .with_config(move |config| {
                super::configure(config);
                if !enabled {
                    config.features.disable(Feature::AgentMessageBoard).unwrap();
                }
            })
            .build_with_auto_env(&server)
            .await?;
        test.submit_turn("Continue the existing task.").await?;
        roles.push(
            mock.single_request()
                .message_input_texts("developer")
                .into_iter()
                .find(|text| text.contains("Role: `/root`"))
                .context("native root role")?,
        );
    }
    assert_eq!(
        roles[1],
        format!("{}\n\n{BOARD_GUIDANCE} {DIRECT_GUIDANCE}", roles[0])
    );
    assert_eq!(
        roles[1].len() - roles[0].len(),
        BOARD_GUIDANCE.len() + DIRECT_GUIDANCE.len() + 3
    );
    println!(
        "Board guidance adds {} UTF-8 prompt bytes",
        roles[1].len() - roles[0].len()
    );
    Ok(())
}

#[test_case::test_case("none"; "fresh_child")]
#[test_case::test_case("all"; "full_history_child")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawned_child_receives_board_and_guidance_once(fork_turns: &str) -> anyhow::Result<()> {
    let server = responses::start_mock_server().await;
    let mut extensions = codex_extension_api::ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(std::sync::Arc::new(ThreadIdle));
    let test = test_codex()
        .with_extensions(std::sync::Arc::new(extensions.build()))
        .with_config(|config| {
            super::configure(config);
            // This scenario calls the nested exec facade, unlike the direct board fixtures.
            config
                .features
                .enable(Feature::CodeMode)
                .expect("configure test feature flags");
        })
        .with_model_info_override("gpt-5.5", |model| {
            model.multi_agent_version = Some(codex_protocol::protocol::MultiAgentVersion::V2);
        })
        .build_with_auto_env(&server)
        .await?;
    // Child terminal results legitimately resume the parent; board notices do not.
    // Keep that extra root turn out of the four-step board/guidance choreography.
    wiremock::Mock::given(wiremock::matchers::header(
        "thread-id",
        test.session_configured.thread_id.to_string(),
    ))
    .and(|request: &wiremock::Request| {
        request
            .body_json::<Value>()
            .expect("parent completion request")["input"]
            .as_array()
            .expect("input")
            .iter()
            .any(|item| item["type"] == "agent_message")
    })
    .respond_with(
        wiremock::ResponseTemplate::new(200)
            .insert_header("content-type", "text/event-stream")
            .set_body_string(done()),
    )
    .with_priority(10)
    .mount(&server)
    .await;
    let mock = responses::mount_sse_sequence(&server, vec![
        sse(vec![ev_custom_tool_call("create-design", "exec", r#"text(await tools.collaboration__agent_board({action: 'create_channel', channel_name: 'design'}))"#), ev_completed("create-design")]),
        tool("spawn-worker", "spawn_agent", json!({
            "task_name":"worker", "message":"Read the design channel and update relevant threads.", "fork_turns":fork_turns
        })),
        done(), done(),
    ]).await;
    test.submit_turn("Spawn a worker for the design task.")
        .await?;
    let child_id = test
        .thread_manager
        .list_thread_ids()
        .await
        .into_iter()
        .find(|id| *id != test.session_configured.thread_id)
        .context("spawned child")?;
    let child = test.thread_manager.get_thread(child_id).await?;
    wait_for_event(&child, |event| matches!(event, EventMsg::TurnComplete(_))).await;
    ThreadIdle::wait(&child).await;
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !matches!(
            test.codex.agent_status().await,
            codex_protocol::protocol::AgentStatus::Completed(_)
        ) {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .context("parent completion autoresume should finish")?;
    let requests = mock.requests();
    assert_eq!(requests.len(), 4);
    let child_request = requests
        .iter()
        .find(|request| {
            request
                .message_input_texts("developer")
                .iter()
                .any(|text| text.contains("Complete the assigned task."))
        })
        .context("child request")?;
    let developer = child_request.message_input_texts("developer");
    assert!(board_is_callable(child_request));
    assert_eq!(
        developer
            .iter()
            .filter(|text| text.contains(BOARD_GUIDANCE))
            .count(),
        1
    );
    assert_eq!(
        developer
            .iter()
            .filter(|text| text.contains("Complete the assigned task."))
            .count(),
        1
    );
    assert!(!developer.iter().any(|text| text.contains("Role: `/root`")));
    assert!(
        child_request.body_contains_text("Read the design channel and update relevant threads.")
    );
    // Prove the fresh child sees the parent's board, not merely another tool with the same name.
    let read_start = responses::received_responses_requests(&server).await.len();
    super::mount_thread_sequence(
        &server,
        child_id,
        vec![
            sse(vec![ev_custom_tool_call("child-read", "exec", r#"text(await tools.collaboration__agent_board({action: 'get_channels', query: 'design'}))"#), ev_completed("child-read")]),
            done(),
        ],
    )
    .await;
    let turn_id = super::start_board_turn(&child, "Read the design channel.").await?;
    wait_for_event(
        &child,
        |event| matches!(event, EventMsg::TurnComplete(event) if event.turn_id == turn_id),
    )
    .await;
    ThreadIdle::wait(&child).await;
    let next = responses::received_responses_requests(&server)
        .await
        .into_iter()
        .skip(read_start)
        .filter(|request| request.header("thread-id") == Some(child_id.to_string()))
        .collect::<Vec<_>>();
    assert_eq!(next.len(), 2);
    let (output, success) = next[1]
        .custom_tool_call_output_content_and_success("child-read")
        .context("shared channel result")?;
    assert_ne!(success, Some(false));
    let output = output.context("shared channel text")?;
    let result: Value = serde_json::from_str(&output)?;
    assert!(
        result.to_string().contains("design"),
        "shared channel missing: {result}"
    );
    assert_eq!(
        next[1]
            .message_input_texts("developer")
            .iter()
            .filter(|text| text.contains(BOARD_GUIDANCE))
            .count(),
        1
    );
    child.shutdown_and_wait().await?;
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
