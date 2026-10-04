use super::*;
use crate::GuardianAuthorizationVersion;
use crate::context::GuardianReviewEvidence;
use crate::session::step_context::StepContext;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_rx;
use crate::session::tests::update_selected_settings_for_test;
use crate::session::tests::update_turn_settings_for_test;
use crate::state::ActiveTurn;
use crate::tools::context::ToolInvocation;
use crate::tools::context::ToolPayload;
use crate::turn_diff_tracker::TurnDiffTracker;
use codex_guardian_context::RenderedVerifiedAnswers;
use codex_protocol::ThreadId;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::request_user_input::RequestUserInputAnswer;
use codex_protocol::request_user_input::RequestUserInputResponse;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use test_case::test_case;
use tokio::sync::Mutex;

#[test_case("async", false, vec![ModeKind::Default], "request_user_input async delivery is unavailable for this model"; "async_model_gate")]
#[test_case("wait", true, vec![], "request_user_input is unavailable in Default mode"; "wait_config_gate")]
#[tokio::test]
async fn request_user_input_delivery_respects_availability(
    delivery: &str,
    async_enabled: bool,
    available_modes: Vec<ModeKind>,
    message: &str,
) {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let mut turn = turn;
    Arc::make_mut(&mut Arc::get_mut(&mut turn).unwrap().config)
        .features
        .enable(Feature::DefaultModeRequestUserInput)
        .unwrap();
    let result = RequestUserInputHandler { available_modes, async_enabled }.handle(ToolInvocation {
        session,
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "unavailable".to_string(),
        tool_name: codex_tools::ToolName::plain(REQUEST_USER_INPUT_TOOL_NAME),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: json!({"delivery": delivery, "questions": [{"id": "confirm", "header": "Confirm", "question": "Proceed?"}]}).to_string(),
        },
    }).await;
    let Err(error) = result else {
        panic!("unavailable delivery must fail")
    };
    assert_eq!(
        error,
        FunctionCallError::RespondToModel(message.to_string())
    );
    assert!(
        events.try_recv().is_err(),
        "unavailable questions must not reach the UI"
    );
}

#[test_case("wait"; "wait")]
#[test_case("async"; "async_delivery")]
#[tokio::test]
async fn multi_agent_v2_request_user_input_rejects_subagent_threads(delivery: &str) {
    let (session, mut turn) = make_session_and_context().await;
    turn.session_source = SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
        parent_thread_id: ThreadId::new(),
        depth: 1,
        agent_path: None,
        agent_nickname: None,
        agent_role: None,
    });
    let turn = Arc::new(turn);

    let result = RequestUserInputHandler {
        available_modes: Vec::new(),
        async_enabled: true,
    }
    .handle(ToolInvocation {
        session: Arc::new(session),
        step_context: StepContext::for_test(Arc::clone(&turn)),
        turn,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "call-1".to_string(),
        tool_name: codex_tools::ToolName::plain(REQUEST_USER_INPUT_TOOL_NAME),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: json!({
                "delivery": delivery,
                "questions": [{
                    "header": "Hdr",
                    "question": "Pick one",
                    "id": "pick_one",
                    "options": [
                        {
                            "label": "A",
                            "description": "A"
                        },
                        {
                            "label": "B",
                            "description": "B"
                        }
                    ]
                }]
            })
            .to_string(),
        },
    })
    .await;

    let Err(err) = result else {
        panic!("sub-agent request_user_input should fail");
    };
    assert_eq!(
        err,
        FunctionCallError::RespondToModel(
            "request_user_input can only be used by the root thread".to_string(),
        )
    );
}

#[test_case("wait", false, ModeKind::Default, false, true, false; "wait_default_off")]
#[test_case("async", false, ModeKind::Default, false, true, false; "async_default_off")]
#[test_case("async", true, ModeKind::Default, false, true, false; "legacy_default_off")]
#[test_case("async", false, ModeKind::Plan, false, true, true; "captured_plan")]
#[test_case("async", true, ModeKind::Plan, false, true, true; "legacy_captured_plan")]
#[test_case("async", false, ModeKind::Default, true, true, true; "default_opt_in")]
#[test_case("async", true, ModeKind::Default, true, true, true; "legacy_default_opt_in")]
#[test_case("async", true, ModeKind::Plan, false, false, false; "legacy_model_gate")]
#[tokio::test]
async fn question_dispatch_uses_captured_mode_and_cannot_bypass_gates(
    delivery: &str,
    legacy: bool,
    mode: ModeKind,
    opt_in: bool,
    model_capable: bool,
    accepted: bool,
) {
    let (session, mut turn, events) = make_session_and_context_with_rx().await;
    let config = Arc::make_mut(&mut Arc::get_mut(&mut turn).unwrap().config);
    if opt_in {
        config
            .features
            .enable(Feature::DefaultModeRequestUserInput)
            .unwrap();
    }
    update_turn_settings_for_test(Arc::get_mut(&mut turn).unwrap(), |settings| {
        update_selected_settings_for_test(settings, |selected| {
            selected.collaboration_mode.mode = if mode == ModeKind::Plan {
                ModeKind::Default
            } else {
                ModeKind::Plan
            };
        });
    });
    let mut step_context = StepContext::for_test(Arc::clone(&turn));
    let settings = Arc::make_mut(&mut Arc::get_mut(&mut step_context).unwrap().settings);
    update_selected_settings_for_test(settings, |selected| {
        selected.collaboration_mode.mode = mode;
    });
    Arc::make_mut(&mut settings.model_info).experimental_supported_tools = if model_capable {
        vec!["request_user_input_async".to_string()]
    } else {
        Vec::new()
    };
    let invocation = ToolInvocation {
        session,
        turn,
        step_context,
        cancellation_token: tokio_util::sync::CancellationToken::new(),
        tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
        call_id: "stale-call".to_string(),
        tool_name: codex_tools::ToolName::plain(if legacy { "request_user_input_async" } else { REQUEST_USER_INPUT_TOOL_NAME }),
        source: crate::tools::context::ToolCallSource::Direct,
        payload: ToolPayload::Function {
            arguments: if legacy {
                json!({"questions": [{"title": "Proceed?"}]})
            } else {
                json!({"delivery": delivery, "questions": [{"id": "confirm", "header": "Confirm", "question": "Proceed?"}]})
            }.to_string(),
        },
    };
    let result = if legacy {
        crate::tools::handlers::RequestUserInputAsyncHandler
            .handle(invocation)
            .await
    } else {
        // Stale handlers may still claim both delivery and Default are available.
        RequestUserInputHandler {
            available_modes: vec![ModeKind::Default, ModeKind::Plan],
            async_enabled: true,
        }
        .handle(invocation)
        .await
    };
    if accepted {
        assert!(result.is_ok());
        assert!(events.try_recv().is_ok());
    } else {
        let Err(error) = result else {
            panic!("gated invocation must fail")
        };
        let message = if mode == ModeKind::Default && !opt_in {
            "request_user_input is unavailable in Default mode"
        } else {
            "request_user_input async delivery is unavailable for this model"
        };
        assert_eq!(
            error,
            FunctionCallError::RespondToModel(message.to_string())
        );
        assert!(
            events.try_recv().is_err(),
            "gated invocation must not reach the UI"
        );
    }
}

#[test_case(None, RenderedVerifiedAnswers { fragments: vec![], complete: true }; "empty")]
#[test_case(Some(("other_question", "A".to_owned())), RenderedVerifiedAnswers { fragments: vec![], complete: true }; "unrequested question")]
#[test_case(Some(("pick_one", " ".to_owned())), RenderedVerifiedAnswers { fragments: vec![], complete: true }; "blank answer")]
#[test_case(Some(("pick_one", "A".to_owned())), RenderedVerifiedAnswers {
    fragments: vec!["assistant: Pick one\nassistant: A: A\nuser: A\n".to_owned()],
    complete: true,
}; "genuine answer")]
#[test_case(Some(("pick_one", "A\nB".to_owned())), RenderedVerifiedAnswers {
    fragments: vec!["assistant: Pick one\nuser: A\nuser: B\n".to_owned()],
    complete: true,
}; "short multiline answer")]
#[test_case(Some(("pick_one", "x\n".repeat(/*n*/ 900))), RenderedVerifiedAnswers {
    fragments: vec!["Host notice: some verified user answers are unavailable within the evidence budget. Do not treat the remaining answers as complete authorization for an action.\n".to_owned()],
    complete: false,
}; "oversized multiline answer")]
#[tokio::test]
async fn request_user_input_sets_non_blocking_outside_plan_mode(
    answer: Option<(&str, String)>,
    expected: RenderedVerifiedAnswers,
) {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let mut turn = turn;
    Arc::make_mut(&mut Arc::get_mut(&mut turn).unwrap().config)
        .features
        .enable(Feature::DefaultModeRequestUserInput)
        .unwrap();
    session
        .services
        .thread_extension_data
        .insert(GuardianReviewEvidence::default());
    let original_history = session.conversation_history_snapshot().await;
    *session.active_turn.lock().await = Some(ActiveTurn::default());

    let request = tokio::spawn({
        let session = Arc::clone(&session);
        let turn = Arc::clone(&turn);
        async move {
            RequestUserInputHandler {
                available_modes: vec![ModeKind::Default],
                async_enabled: false,
            }
            .handle(ToolInvocation {
                session,
                step_context: StepContext::for_test(Arc::clone(&turn)),
                turn,
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
                call_id: "call-1".to_string(),
                tool_name: codex_tools::ToolName::plain(REQUEST_USER_INPUT_TOOL_NAME),
                source: crate::tools::context::ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: json!({
                        "questions": [{
                            "header": "Hdr",
                            "question": "Pick one",
                            "id": "pick_one",
                            "options": [
                                {
                                    "label": "A",
                                    "description": "A"
                                },
                                {
                                    "label": "B",
                                    "description": "B"
                                }
                            ]
                        }]
                    })
                    .to_string(),
                },
            })
            .await
        }
    });

    let event = events.recv().await.expect("request_user_input event");
    let EventMsg::RequestUserInput(request_event) = event.msg else {
        panic!("expected request_user_input event");
    };
    assert_eq!(request_event.call_id, "call-1");
    assert!(!request_event.is_blocking);

    session
        .notify_user_input_response(
            &request_event.turn_id,
            RequestUserInputResponse {
                answers: answer
                    .iter()
                    .map(|(question_id, answer)| {
                        (
                            (*question_id).to_owned(),
                            RequestUserInputAnswer {
                                answers: vec![answer.clone()],
                            },
                        )
                    })
                    .collect(),
            },
        )
        .await;

    let output = request
        .await
        .expect("request_user_input handler task should finish")
        .expect("request_user_input handler should succeed");
    assert!(output.success_for_logging());
    let history = session.conversation_history_snapshot().await;
    let RenderedVerifiedAnswers {
        fragments,
        complete,
    } = codex_guardian_context::render_verified_answers(
        history.retained_context().expect("host context snapshot"),
    );
    let expected_fragments = expected
        .fragments
        .iter()
        .map(|fragment| {
            if expected.complete {
                format!("Retained source order: 0\n{fragment}")
            } else {
                fragment.clone()
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(
        (&fragments, complete),
        (&expected_fragments, expected.complete)
    );
    let evidence = session
        .services
        .thread_extension_data
        .get_or_init(GuardianReviewEvidence::default);
    let has_answer = matches!(&answer, Some(("pick_one", answer)) if !answer.trim().is_empty());
    assert_eq!(
        evidence.authorization_version(history.as_ref()),
        GuardianAuthorizationVersion {
            user_message_revision: original_history.user_message_revision() + u64::from(has_answer),
            retained_context_complete: expected.complete,
        },
    );
}

#[tokio::test]
async fn request_user_input_sets_blocking_from_captured_step_mode() {
    let (session, turn, events) = make_session_and_context_with_rx().await;
    let mut step_context = StepContext::for_test(Arc::clone(&turn));
    update_selected_settings_for_test(
        Arc::make_mut(&mut Arc::get_mut(&mut step_context).unwrap().settings),
        |selected| selected.collaboration_mode.mode = ModeKind::Plan,
    );
    *session.active_turn.lock().await = Some(ActiveTurn::default());

    let request = tokio::spawn({
        let session = Arc::clone(&session);
        let turn = Arc::clone(&turn);
        async move {
            RequestUserInputHandler {
                available_modes: vec![ModeKind::Plan],
                async_enabled: false,
            }
            .handle(ToolInvocation {
                session,
                step_context,
                turn,
                cancellation_token: tokio_util::sync::CancellationToken::new(),
                tracker: Arc::new(Mutex::new(TurnDiffTracker::default())),
                call_id: "call-1".to_string(),
                tool_name: codex_tools::ToolName::plain(REQUEST_USER_INPUT_TOOL_NAME),
                source: crate::tools::context::ToolCallSource::Direct,
                payload: ToolPayload::Function {
                    arguments: json!({
                        "questions": [{
                            "header": "Hdr",
                            "question": "Pick one",
                            "id": "pick_one",
                            "options": [
                                {
                                    "label": "A",
                                    "description": "A"
                                },
                                {
                                    "label": "B",
                                    "description": "B"
                                }
                            ]
                        }]
                    })
                    .to_string(),
                },
            })
            .await
        }
    });

    let event = events.recv().await.expect("request_user_input event");
    let EventMsg::RequestUserInput(request_event) = event.msg else {
        panic!("expected request_user_input event");
    };
    assert_eq!(request_event.call_id, "call-1");
    assert!(request_event.is_blocking);

    session
        .notify_user_input_response(
            &request_event.turn_id,
            RequestUserInputResponse {
                answers: HashMap::new(),
            },
        )
        .await;

    request
        .await
        .expect("request_user_input handler task should finish")
        .expect("request_user_input handler should succeed");
}
