//! Real child terminal delivery resumes a naturally idle parent, but not a stopped parent.

use super::*;
use anyhow::Context;
use codex_extension_api::ExtensionRegistryBuilder;
use codex_extension_api::ThreadLifecycleContributor;
use core_test_support::streaming_sse::StreamingSseChunk;
use core_test_support::streaming_sse::start_streaming_sse_server;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::sync::oneshot;
use tokio::time::timeout;

#[path = "multi_agent_evicted_completion_tests.rs"]
mod evicted_tests;

const CHILD_RESULT: &str = "worker verified the async handoff";

struct IdleRecorder(mpsc::UnboundedSender<String>);

impl ThreadLifecycleContributor<codex_core::config::Config> for IdleRecorder {
    fn on_thread_idle<'a>(
        &'a self,
        input: codex_extension_api::ThreadIdleInput<'a>,
    ) -> codex_extension_api::ExtensionFuture<'a, ()> {
        Box::pin(async move {
            let _ = self.0.send(input.thread_store.level_id().to_string());
        })
    }
}

#[test_case::test_case(false, false; "natural_idle_resumes")]
#[test_case::test_case(false, true; "idle_stop_blocks_resume")]
#[test_case::test_case(true, false; "active_parent_consumes_without_new_turn")]
#[test_case::test_case(true, true; "active_stop_blocks_resume")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn child_completion_resumes_only_unstopped_parent(active: bool, stop: bool) -> Result<()> {
    let server = start_mock_server().await;
    let (release_child, child_gate) = oneshot::channel();
    let (child_stream, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: Some(child_gate),
        body: sse(vec![
            ev_assistant_message("child-result", CHILD_RESULT),
            ev_completed("child-complete"),
        ]),
    }]])
    .await;
    let base_url = format!("{}/v1", server.uri());
    let (idle_tx, mut idle_rx) = mpsc::unbounded_channel();
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(IdleRecorder(idle_tx)));
    let test = test_codex()
        .with_direct_tools()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            configure_multi_agent_v2_with_role(config, &base_url);
            assert!(!config.multi_agent_v2.wait_agent_enabled);
        })
        .build(&server)
        .await?;
    let root_level = test.codex.thread_extension_data().level_id().to_string();
    let mut created = test.thread_manager.subscribe_thread_created();

    mount_root_collaboration_call(
        &server,
        INITIAL_PROMPT,
        SPAWN_CALL_ID,
        "spawn_agent",
        &json!({"message": INITIAL_TASK, "task_name": "worker",
            "agent_type": ROLE_NAME, "fork_turns": "none"})
        .to_string(),
    )
    .await;
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .and(|request: &wiremock::Request| request_has_model(request, ROLE_MODEL))
        .respond_with(
            wiremock::ResponseTemplate::new(307)
                .insert_header("location", format!("{}/v1/responses", child_stream.uri())),
        )
        .up_to_n_times(1)
        .mount(&server)
        .await;
    let resumed = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            !request_has_model(request, ROLE_MODEL) && body_contains(request, CHILD_RESULT)
        },
        sse(vec![
            ev_assistant_message("resumed", "Result received"),
            ev_completed("resumed-complete"),
        ]),
    )
    .await;

    test.submit_text_turn(INITIAL_PROMPT).await?;
    timeout(Duration::from_secs(10), async {
        while idle_rx.recv().await.as_deref() != Some(&root_level) {}
    })
    .await
    .context("root did not become idle")?;
    let child_id = created.recv().await?;
    let child = test.thread_manager.get_thread(child_id).await?;
    timeout(
        Duration::from_secs(10),
        child_stream.wait_for_request_count(1),
    )
    .await
    .context("child inference did not reach stream")?;
    let mut active_parent = None;
    if active {
        let (release_parent, parent_gate) = oneshot::channel();
        let (parent_stream, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
            gate: Some(parent_gate),
            body: sse(vec![ev_completed("independent-work")]),
        }]])
        .await;
        wiremock::Mock::given(wiremock::matchers::method("POST"))
            .and(wiremock::matchers::path("/v1/responses"))
            .and(|request: &wiremock::Request| {
                body_contains(request, "Independent work") && !body_contains(request, CHILD_RESULT)
            })
            .respond_with(
                wiremock::ResponseTemplate::new(307)
                    .insert_header("location", format!("{}/v1/responses", parent_stream.uri())),
            )
            .up_to_n_times(1)
            .mount(&server)
            .await;
        let submission = test
            .codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Independent work".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        let codex_protocol::turn_input::TurnInputSubmission::Started { turn_id, .. } = submission
        else {
            panic!("parent starts independent work while the child runs");
        };
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnStarted(_))
        })
        .await;
        timeout(
            Duration::from_secs(10),
            parent_stream.wait_for_request_count(1),
        )
        .await?;
        active_parent = Some((turn_id, release_parent, parent_stream));
    }
    if stop {
        // Idle Stop must latch even though there is no running task to abort.
        test.codex.submit(Op::Interrupt).await?;
        if active {
            wait_for_event(&test.codex, |event| {
                matches!(event, EventMsg::TurnAborted(_))
            })
            .await;
        }
    }
    release_child
        .send(())
        .expect("release real child completion");
    wait_for_event(child.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let child_level = child.thread_extension_data().level_id().to_string();
    timeout(Duration::from_secs(10), async {
        while idle_rx.recv().await.as_deref() != Some(&child_level) {}
    })
    .await
    .context("child did not become idle after completion")?;
    if stop {
        // Flush the ordered parent submission queue with explicit input. The result must
        // remain available, without a synthetic TurnStarted before the user starts again.
        // Fresh user input is sampled before previously queued mail.
        mount_sse_once_match(
            &server,
            |request: &wiremock::Request| {
                body_contains(request, "Continue explicitly")
                    && !body_contains(request, CHILD_RESULT)
            },
            sse(vec![ev_completed("explicit-input-first")]),
        )
        .await;
        let submission = test
            .codex
            .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
                text: "Continue explicitly".to_string(),
                text_elements: Vec::new(),
            }]))
            .await?;
        let codex_protocol::turn_input::TurnInputSubmission::Started { turn_id, .. } = submission
        else {
            panic!("stopped parent must be idle");
        };
        wait_for_event(&test.codex, |event| {
            if let EventMsg::TurnStarted(started) = event {
                assert_eq!(started.turn_id, turn_id, "no unsolicited completion wake");
            }
            if let EventMsg::TurnComplete(completed) = event {
                assert_eq!(
                    completed.last_agent_message.as_deref(),
                    Some("Result received"),
                    "{completed:?}"
                );
                return true;
            }
            false
        })
        .await;
    } else if let Some((turn_id, release_parent, _parent_stream)) = active_parent {
        release_parent.send(()).expect("release independent work");
        wait_for_event(&test.codex, |event| {
            assert!(
                !matches!(event, EventMsg::TurnStarted(_)),
                "completion must not create a second active turn"
            );
            if let EventMsg::TurnComplete(completed) = event {
                assert_eq!(completed.turn_id, turn_id);
                assert_eq!(
                    completed.last_agent_message.as_deref(),
                    Some("Result received")
                );
                return true;
            }
            false
        })
        .await;
    } else {
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnStarted(_))
        })
        .await;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
    }
    let result_requests = resumed
        .requests()
        .into_iter()
        .filter(|request| {
            request.input().iter().any(|item| {
                item["type"] == "agent_message" && item.to_string().contains(CHILD_RESULT)
            })
        })
        .collect::<Vec<_>>();
    assert_eq!(
        result_requests.len(),
        1,
        "one inference consumes the child result"
    );
    test.codex.submit(Op::Shutdown).await?;
    Ok(())
}
