use anyhow::Result;
use codex_config::types::ContextStrategy;
use codex_core::TurnInputRequest;
use codex_core::compact::SUMMARIZATION_PROMPT;
use codex_core::config::TokenBudgetConfig;
use codex_login::CodexAuth;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::ThreadSettingsOverrides;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::user_input::UserInput;
use core_test_support::responses::ResponsesRequest;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_completed_with_tokens;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::sse_failed;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::test_codex::TestCodex;
use core_test_support::test_codex::TestCodexBuilder;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use test_case::test_case;

const FULL_WINDOW: i64 = 64_000;
const USABLE_WINDOW: i64 = FULL_WINDOW * 80 / 100;
const SUMMARY: &str = "Readable checkpoint: retain job 42 and the tool's budget evidence.";
const URGENT_CHECKPOINT: &str = "STOP ongoing work. Checkpoint the active request";

fn notes_fixture(scope: AutoCompactTokenLimitScope) -> TestCodexBuilder {
    test_codex()
        .with_direct_tools()
        .with_context_strategy(ContextStrategy::Notes)
        .with_auth(
            CodexAuth::from_external_chatgpt_tokens(
                "header.e30.signature",
                "account-123",
                Some("plus"),
            )
            .expect("test backend authentication"),
        )
        .with_model_info_override("gpt-5.2", |model| {
            model.effective_context_window_percent = 80;
            model.max_context_window = Some(FULL_WINDOW);
        })
        .with_config(move |config| {
            let base_url = config
                .model_provider
                .base_url
                .as_ref()
                .expect("mock provider has a base URL");
            config.model_provider.base_url = Some(format!(
                "{}/backend-api/codex",
                base_url
                    .strip_suffix("/v1")
                    .expect("mock provider URL ends in /v1")
            ));
            config.model_context_window = Some(FULL_WINDOW);
            config.model_auto_compact_token_limit = Some(32_000);
            config.model_auto_compact_token_limit_scope = scope;
            config.base_instructions = Some("Complete the user's task.".into());
            config.token_budget = Some(TokenBudgetConfig {
                reminder_threshold_tokens: Some(6_000),
                reminder_message_template: "Native budget: {n_remaining} tokens remain.".into(),
                guidance_message: Some("Save notes before requesting a new context.".into()),
                auto_compact_fallback_prompt: Some("Save the current work to notes.".into()),
                auto_compact_fallback_buffer_tokens: Some(8_000),
                ..TokenBudgetConfig::default()
            });
        })
}

fn reply(id: &str, text: &str) -> String {
    sse(vec![ev_assistant_message(id, text), ev_completed(id)])
}

async fn complete(test: &TestCodex, text: &str) -> Result<TurnCompleteEvent> {
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: text.into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let event = wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let EventMsg::TurnComplete(completion) = event else {
        unreachable!("waited for turn completion")
    };
    Ok(completion)
}

#[test_case(ContextStrategy::Notes, 272_000; "notes_selected_window")]
#[test_case(ContextStrategy::Notes, 123_456; "notes_custom_window")]
#[test_case(ContextStrategy::Compaction, 272_000; "compaction_selected_window")]
#[test_case(ContextStrategy::Compaction, 123_456; "compaction_custom_window")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn context_events_report_selected_budget_independently_of_admission(
    strategy: ContextStrategy,
    selected: i64,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let notes = strategy == ContextStrategy::Notes;
    let usage = if notes { selected + 1_000 } else { 10_000 };
    let replies = if notes {
        vec![
            sse(vec![
                ev_function_call("reset", "new_context", "{}"),
                ev_completed_with_tokens("reset", usage),
            ]),
            sse(vec![
                ev_assistant_message("final", "done after checkpoint"),
                ev_completed_with_tokens("final", 10_000),
            ]),
        ]
    } else {
        vec![sse(vec![
            ev_assistant_message("final", "done"),
            ev_completed_with_tokens("final", usage),
        ])]
    };
    let responses = mount_sse_sequence(&server, replies).await;
    let test = notes_fixture(AutoCompactTokenLimitScope::Total)
        .with_context_strategy(strategy)
        .with_model_info_override("gpt-5.2", |model| {
            model.max_context_window = Some(872_000);
            model.effective_context_window_percent = 95;
        })
        .with_config(move |config| config.model_context_window = Some(selected))
        .build(&server)
        .await?;
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Save a notes checkpoint and report this request's context budget".into(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut started_window = None;
    let mut usage_window = None;
    let mut saw_usage = false;
    loop {
        let event = wait_for_event(&test.codex, |event| {
            matches!(
                event,
                EventMsg::TurnStarted(_) | EventMsg::TokenCount(_) | EventMsg::TurnComplete(_)
            )
        })
        .await;
        match event {
            EventMsg::TurnStarted(event) => started_window = event.model_context_window,
            EventMsg::TokenCount(event) => {
                if let Some(info) = event.info {
                    usage_window = info.model_context_window;
                    assert_eq!(usage_window, Some(selected));
                    saw_usage |= info.last_token_usage.total_tokens == usage;
                }
            }
            EventMsg::TurnComplete(event) => {
                assert!(event.error.is_none());
                break;
            }
            _ => unreachable!("filtered context events"),
        }
    }
    assert_eq!(started_window, Some(selected));
    assert_eq!(usage_window, Some(selected));
    assert!(
        saw_usage,
        "actual usage must not be clamped to the display budget"
    );
    let requests = responses.requests();
    assert_eq!(requests.len(), if notes { 2 } else { 1 });
    assert!(
        !requests
            .iter()
            .any(|request| request.body_contains_text(SUMMARIZATION_PROMPT))
    );
    if notes {
        assert_ne!(
            requests[0].header("x-codex-window-id"),
            requests[1].header("x-codex-window-id")
        );
    }
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

fn assert_same_window(requests: &[ResponsesRequest]) {
    let first = requests[0]
        .header("x-codex-window-id")
        .expect("request contains context window header");
    assert!(
        requests.iter().all(|request| {
            request.header("x-codex-window-id").as_deref() == Some(first.as_str())
        }),
        "readable rescue must not retire the notes window"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_window_downshift_does_not_summarize_notes_below_execution_cap() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("previous", "Retain job 42"),
                ev_completed_with_tokens("previous", 250_000),
            ]),
            reply("current", "continued without summarizing"),
        ],
    )
    .await;
    let test = notes_fixture(AutoCompactTokenLimitScope::Total)
        .with_model("gpt-daybreak-red-latest")
        .with_config(|config| {
            config.model_context_window = None;
            config.model_auto_compact_token_limit = None;
        })
        .build(&server)
        .await?;
    assert!(complete(&test, "Previous task").await?.error.is_none());
    core_test_support::submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            model: Some("gpt-6-astra".into()),
            ..Default::default()
        },
    )
    .await?;
    assert!(
        complete(&test, "Continue on the larger-capacity model")
            .await?
            .error
            .is_none()
    );
    let requests = responses.requests();
    assert_eq!(
        requests.len(),
        2,
        "a selected-window downshift must not add a Notes summary request"
    );
    assert_eq!(requests[1].body_json()["model"], "gpt-6-astra");
    assert!(!requests[1].body_contains_text(SUMMARIZATION_PROMPT));
    assert!(requests[1].body_contains_text("Retain job 42"));
    assert_same_window(&requests);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn same_window_rescue_preserves_checkpoint_instruction_without_duplicates() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_function_call("overflow", "get_context_remaining", "{}"),
                ev_completed_with_tokens("overflow", USABLE_WINDOW),
            ]),
            reply("summary", SUMMARY),
            sse(vec![
                ev_function_call("checkpoint", "get_context_remaining", "{}"),
                ev_completed_with_tokens("checkpoint", FULL_WINDOW * 75 / 100),
            ]),
            reply("final", "checkpointing after rescue"),
        ],
    )
    .await;
    let test = notes_fixture(AutoCompactTokenLimitScope::Total)
        .build(&server)
        .await?;
    assert!(
        complete(&test, "Keep the mandatory checkpoint instruction")
            .await?
            .error
            .is_none()
    );
    let requests = responses.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[1].body_contains_text(SUMMARIZATION_PROMPT));
    for request in &requests[1..] {
        assert_eq!(
            request
                .message_input_texts("developer")
                .iter()
                .filter(|text| text.contains(URGENT_CHECKPOINT))
                .count(),
            1,
            "same-window summarization must retain one mandatory checkpoint instruction"
        );
        assert!(request.body_contains_text("call new_context IMMEDIATELY before resuming work"));
    }
    assert!(requests[2].body_contains_text(SUMMARY));
    assert_same_window(&requests);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_budget_escalates_once_without_rescue_and_reset_rearms() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_function_call("early", "get_context_remaining", "{}"),
                ev_completed_with_tokens("early", 60_000),
            ]),
            sse(vec![
                ev_function_call("cross", "get_context_remaining", "{}"),
                ev_completed_with_tokens("cross", FULL_WINDOW),
            ]),
            sse(vec![
                ev_function_call("repeat", "get_context_remaining", "{}"),
                ev_completed_with_tokens("repeat", FULL_WINDOW + 1_000),
            ]),
            sse(vec![
                ev_function_call("reset", "new_context", "{}"),
                ev_completed_with_tokens("reset", FULL_WINDOW + 2_000),
            ]),
            sse(vec![
                ev_function_call("rearm", "get_context_remaining", "{}"),
                ev_completed_with_tokens("rearm", FULL_WINDOW),
            ]),
            reply("final", "continued after checkpoint"),
        ],
    )
    .await;
    let test = notes_fixture(AutoCompactTokenLimitScope::Total)
        .with_model_info_override("gpt-5.2", |model| {
            model.max_context_window = Some(FULL_WINDOW * 4)
        })
        .build(&server)
        .await?;
    assert!(
        complete(&test, "Keep working across the selected budget")
            .await?
            .error
            .is_none()
    );
    let requests = responses.requests();
    assert_eq!(requests.len(), 6);
    assert!(
        !requests
            .iter()
            .any(|request| request.body_contains_text(SUMMARIZATION_PROMPT))
    );
    assert!(requests[1].body_contains_text("Native budget:"));
    let urgent_count = |request: &ResponsesRequest| {
        request
            .message_input_texts("developer")
            .iter()
            .filter(|text| text.contains(URGENT_CHECKPOINT))
            .count()
    };
    assert_eq!(
        requests.iter().map(urgent_count).collect::<Vec<_>>(),
        vec![0, 0, 1, 1, 0, 1]
    );
    assert!(requests[2].body_contains_text("call new_context IMMEDIATELY before resuming work"));
    assert_same_window(&requests[..4]);
    assert_ne!(
        requests[3].header("x-codex-window-id"),
        requests[4].header("x-codex-window-id")
    );
    assert_eq!(
        requests[4].header("x-codex-window-id"),
        requests[5].header("x-codex-window-id")
    );
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[test_case(false; "accepted_input")]
#[test_case(true; "injected_context")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn selected_budget_crossing_before_request_instructs_without_rescue(
    inject: bool,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(&server, vec![reply("final", "checkpoint next")]).await;
    let test = notes_fixture(AutoCompactTokenLimitScope::BodyAfterPrefix)
        .with_model_info_override("gpt-5.2", |model| {
            model.max_context_window = Some(FULL_WINDOW * 4)
        })
        .build(&server)
        .await?;
    let payload = format!(
        "Selected-budget crossing {}",
        "x".repeat((FULL_WINDOW * 4 + 4_000) as usize)
    );
    if inject {
        test.codex.inject_response_items(vec![serde_json::from_value(serde_json::json!({
            "type": "message", "role": "developer", "content": [{"type": "input_text", "text": payload}]
        }))?]).await?;
    }
    let input = if inject {
        "Inspect injected evidence"
    } else {
        &payload
    };
    assert!(complete(&test, input).await?.error.is_none());
    let requests = responses.requests();
    assert_eq!(requests.len(), 1);
    assert!(!requests[0].body_contains_text(SUMMARIZATION_PROMPT));
    assert!(requests[0].body_contains_text(URGENT_CHECKPOINT));
    assert!(requests[0].body_contains_text("Selected-budget crossing"));
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[test_case(Some(FULL_WINDOW); "advertised_maximum")]
#[test_case(None; "unknown_maximum")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn near_model_max_escalates_with_checkpoint_runway(maximum: Option<i64>) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_function_call("checkpoint", "get_context_remaining", "{}"),
                ev_completed_with_tokens("checkpoint", FULL_WINDOW * 75 / 100),
            ]),
            reply("final", "checkpointing before the hard cap"),
        ],
    )
    .await;
    let test = notes_fixture(AutoCompactTokenLimitScope::Total)
        .with_model_info_override("gpt-5.2", move |model| model.max_context_window = maximum)
        .build(&server)
        .await?;
    assert!(
        complete(&test, "Preserve execution headroom")
            .await?
            .error
            .is_none()
    );
    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0].body_contains_text(URGENT_CHECKPOINT));
    assert!(requests[1].body_contains_text(URGENT_CHECKPOINT));
    assert!(!requests[1].body_contains_text(SUMMARIZATION_PROMPT));
    assert_same_window(&requests);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[test_case(AutoCompactTokenLimitScope::Total, USABLE_WINDOW; "exact_usable_cap")]
#[test_case(AutoCompactTokenLimitScope::BodyAfterPrefix, USABLE_WINDOW + 1_000; "above_cap_with_body_after_prefix")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn usable_hard_cap_rescues_tool_continuation_on_a_larger_backend(
    scope: AutoCompactTokenLimitScope,
    usage: i64,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    // Every request succeeds at this backend, including requests above the model's cap.
    // Usage is below the full window but reaches its usable, headroom-reserving limit.
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_function_call("budget-evidence", "get_context_remaining", "{}"),
                ev_completed_with_tokens("tool", usage),
            ]),
            reply("summary", SUMMARY),
            reply("final", "job 42 recovered"),
        ],
    )
    .await;
    let test = notes_fixture(scope).build(&server).await?;
    let completion = complete(&test, "Retain job 42").await?;
    assert!(completion.error.is_none(), "{completion:?}");
    assert_eq!(
        completion.last_agent_message.as_deref(),
        Some("job 42 recovered")
    );
    let requests = responses.requests();
    assert_eq!(
        requests.len(),
        3,
        "summary must precede normal continuation"
    );
    assert!(requests[1].body_contains_text(SUMMARIZATION_PROMPT));
    assert!(
        requests[1].input().iter().any(|item| {
            item["type"] == "function_call_output" && item["call_id"] == "budget-evidence"
        }),
        "the readable summarizer must receive active tool evidence"
    );
    assert!(requests[2].body_contains_text(SUMMARY));
    assert!(requests[2].body_contains_text("Retain job 42"));
    assert_same_window(&requests);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn exhausted_previous_turn_preserves_accepted_input_before_sampling() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse(vec![
                ev_assistant_message("previous", "job 42 evidence"),
                ev_completed_with_tokens("previous", FULL_WINDOW + 1_000),
            ]),
            reply("summary", SUMMARY),
            reply("final", "continued"),
        ],
    )
    .await;
    let test = notes_fixture(AutoCompactTokenLimitScope::Total)
        .build(&server)
        .await?;
    assert!(complete(&test, "Previous task").await?.error.is_none());
    assert!(complete(&test, "New accepted task").await?.error.is_none());
    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    assert!(requests[1].body_contains_text(SUMMARIZATION_PROMPT));
    assert!(requests[1].body_contains_text("job 42 evidence"));
    assert!(requests[1].body_contains_text("New accepted task"));
    assert!(requests[2].body_contains_text("New accepted task"));
    assert!(requests[2].body_contains_text(SUMMARY));
    assert_same_window(&requests);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn oversized_new_input_is_summarized_before_first_normal_request() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![reply("summary", SUMMARY), reply("final", "admitted")],
    )
    .await;
    let test = notes_fixture(AutoCompactTokenLimitScope::BodyAfterPrefix)
        .build(&server)
        .await?;
    let input = format!(
        "Accepted job 42 input {}",
        "x".repeat((FULL_WINDOW * 4 + 4_000) as usize)
    );
    let completion = complete(&test, &input).await?;
    assert!(completion.error.is_none(), "{completion:?}");
    assert_eq!(completion.last_agent_message.as_deref(), Some("admitted"));
    let requests = responses.requests();
    assert_eq!(requests.len(), 2);
    assert!(requests[0].body_contains_text(SUMMARIZATION_PROMPT));
    assert!(requests[0].body_contains_text("Accepted job 42 input"));
    assert!(requests[1].body_contains_text(SUMMARY));
    assert!(requests[1].body_contains_text("Accepted job 42 input"));
    assert_same_window(&requests);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn backend_overflow_below_advertised_max_retains_one_same_window_rescue() -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![
            sse_failed(
                "overflow",
                "context_length_exceeded",
                "Input exceeds context",
            ),
            reply("summary", SUMMARY),
            reply("final", "recovered from backend overflow"),
        ],
    )
    .await;
    let test = notes_fixture(AutoCompactTokenLimitScope::Total)
        .with_model_info_override("gpt-5.2", |model| {
            model.max_context_window = Some(FULL_WINDOW * 4)
        })
        .build(&server)
        .await?;
    assert!(complete(&test, "Retain job 42").await?.error.is_none());
    let requests = responses.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.body_contains_text(SUMMARIZATION_PROMPT))
            .count(),
        1
    );
    assert!(requests[2].body_contains_text(SUMMARY));
    assert_same_window(&requests);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}

#[test_case(false; "summary_still_exceeds_hard_cap")]
#[test_case(true; "backend_overflow_after_hard_cap_rescue")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hard_cap_rescue_is_bounded_and_fails_visibly(backend_overflows: bool) -> Result<()> {
    skip_if_no_network!(Ok(()));
    let server = start_mock_server().await;
    let mut replies = vec![sse(vec![
        ev_function_call("budget-evidence", "get_context_remaining", "{}"),
        ev_completed_with_tokens("tool", FULL_WINDOW + 1_000),
    ])];
    replies.push(reply(
        "summary",
        &if backend_overflows {
            SUMMARY.into()
        } else {
            "x".repeat((FULL_WINDOW * 4 + 4_000) as usize)
        },
    ));
    if backend_overflows {
        replies.push(sse_failed(
            "overflow",
            "context_length_exceeded",
            "Input exceeds context",
        ));
    }
    let responses = mount_sse_sequence(&server, replies).await;
    let test = notes_fixture(AutoCompactTokenLimitScope::Total)
        .build(&server)
        .await?;
    let completion = complete(&test, "Retain job 42").await?;
    let error = completion
        .error
        .expect("oversized rescue must stop visibly");
    assert_eq!(
        error.codex_error_info,
        Some(CodexErrorInfo::ContextWindowExceeded)
    );
    let requests = responses.requests();
    assert_eq!(requests.len(), if backend_overflows { 3 } else { 2 });
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.body_contains_text(SUMMARIZATION_PROMPT))
            .count(),
        1,
        "configured exhaustion and backend overflow must share one rescue budget"
    );
    assert_same_window(&requests);
    test.codex.shutdown_and_wait().await?;
    Ok(())
}
