//! Residency cleanup must not strand a nested delegation or erase idle Stop.

use super::*;
use codex_core::config::AgentRoleConfig;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::turn_input::SteerSubmission;
use pretty_assertions::assert_eq;

const GRANDCHILD_MODEL: &str = "gpt-5.4";
const GRANDCHILD_CALL: &str = "spawn-nested-child";
const GRANDCHILD_TASK: &str = "complete the nested async work";
const PARENT_RESULT: &str = "Parent delegated nested work";
const PRESSURE_TASK: &str = "Keep the second residency slot busy";

async fn wait_idle(receiver: &mut mpsc::UnboundedReceiver<String>, level: &str) -> Result<()> {
    timeout(Duration::from_secs(10), async {
        while receiver.recv().await.expect("idle lifecycle channel") != level {}
    })
    .await
    .context("thread did not reach idle cleanup")
}

async fn redirect_once(
    server: &wiremock::MockServer,
    matcher: impl wiremock::Match + Send + Sync + 'static,
    stream: &core_test_support::streaming_sse::StreamingSseServer,
) {
    wiremock::Mock::given(wiremock::matchers::method("POST"))
        .and(wiremock::matchers::path("/v1/responses"))
        .and(matcher)
        .respond_with(
            wiremock::ResponseTemplate::new(307)
                .insert_header("location", format!("{}/v1/responses", stream.uri())),
        )
        .up_to_n_times(1)
        .mount(server)
        .await;
}

#[test_case::test_case(false; "evicted_completed_parent_resumes")]
#[test_case::test_case(true; "evicted_idle_stopped_parent_does_not_resume")]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn nested_completion_preserves_handoff_across_residency_eviction(stop: bool) -> Result<()> {
    let server = start_mock_server().await;
    let base_url = format!("{}/v1", server.uri());
    let (idle_tx, mut idle_rx) = mpsc::unbounded_channel();
    let mut extensions = ExtensionRegistryBuilder::new();
    extensions.thread_lifecycle_contributor(Arc::new(IdleRecorder(idle_tx)));
    let test = test_codex()
        .with_direct_tools()
        .with_extensions(Arc::new(extensions.build()))
        .with_config(move |config| {
            configure_multi_agent_v2_with_role(config, &base_url);
            // Two subagent residents. Restoring the parent must reclaim the finishing child.
            config.multi_agent_v2.max_concurrent_threads_per_session = 3;
            let path = config.codex_home.join("grandchild-role.toml");
            std::fs::write(&path, format!("model = \"{GRANDCHILD_MODEL}\"\n"))
                .expect("grandchild role");
            config.agent_roles.insert(
                "grandchild".to_string(),
                AgentRoleConfig {
                    config_file: Some(path.to_path_buf()),
                    description: None,
                    nickname_candidates: None,
                },
            );
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
        &json!({"message": INITIAL_TASK, "task_name": "worker", "agent_type": ROLE_NAME,
            "fork_turns": "none"})
        .to_string(),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| request_has_model(request, ROLE_MODEL),
        sse(vec![
            ev_function_call_with_namespace(
                GRANDCHILD_CALL,
                COLLABORATION_NAMESPACE,
                "spawn_agent",
                &json!({"message": GRANDCHILD_TASK, "task_name": "nested",
                "agent_type": "grandchild", "fork_turns": "none"})
                .to_string(),
            ),
            ev_completed("parent-spawned-child"),
        ]),
    )
    .await;
    let (release_parent, parent_gate) = oneshot::channel();
    let (parent_stream, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: Some(parent_gate),
        body: sse(vec![
            ev_assistant_message("parent-result", PARENT_RESULT),
            ev_completed("parent-completed"),
        ]),
    }]])
    .await;
    redirect_once(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, ROLE_MODEL) && body_contains(request, GRANDCHILD_CALL)
        },
        &parent_stream,
    )
    .await;
    let (release_child, child_gate) = oneshot::channel();
    let (child_stream, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: Some(child_gate),
        body: sse(vec![
            ev_assistant_message("nested-result", CHILD_RESULT),
            ev_completed("nested-completed"),
        ]),
    }]])
    .await;
    redirect_once(
        &server,
        |request: &wiremock::Request| request_has_model(request, GRANDCHILD_MODEL),
        &child_stream,
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            !request_has_model(request, ROLE_MODEL)
                && !request_has_model(request, GRANDCHILD_MODEL)
                && body_contains(request, PARENT_RESULT)
        },
        sse(vec![ev_completed("root-acknowledged-parent")]),
    )
    .await;
    let resumed = mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, ROLE_MODEL) && body_contains(request, CHILD_RESULT)
        },
        sse(vec![
            ev_assistant_message("nested-resumed", "Nested result received"),
            ev_completed("nested-parent-resumed"),
        ]),
    )
    .await;
    mount_sse_once_match(
        &server,
        |request: &wiremock::Request| {
            !request_has_model(request, ROLE_MODEL)
                && !request_has_model(request, GRANDCHILD_MODEL)
                && body_contains(request, "Nested result received")
        },
        sse(vec![ev_completed("root-acknowledged-resumed-parent")]),
    )
    .await;

    test.submit_text_turn(INITIAL_PROMPT).await?;
    wait_idle(&mut idle_rx, &root_level).await?;
    let parent_id = created.recv().await?;
    let parent = test.thread_manager.get_thread(parent_id).await?;
    let child_id = created.recv().await?;
    let child = test.thread_manager.get_thread(child_id).await?;
    timeout(
        Duration::from_secs(10),
        child_stream.wait_for_request_count(1),
    )
    .await?;
    release_parent
        .send(())
        .expect("finish the naturally idle parent");
    wait_for_event(parent.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    wait_idle(&mut idle_rx, parent.thread_extension_data().level_id()).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    wait_idle(&mut idle_rx, &root_level).await?;
    if stop {
        parent.submit(Op::Interrupt).await?;
        // A rejected steer is an ordered submission barrier, not new work that resets Stop.
        assert!(matches!(
            parent
                .steer_turn(
                    TurnInputRequest::user_input(vec![UserInput::Text {
                        text: "barrier only".to_string(),
                        text_elements: Vec::new(),
                    }]),
                    "idle-parent".to_string()
                )
                .await?,
            SteerSubmission::NotSubmitted { .. }
        ));
        assert!(
            matches!(parent.agent_status().await, AgentStatus::Completed(_)),
            "idle Stop must survive eviction even when display status remains Completed"
        );
    }
    let (_pressure_release, pressure_gate) = oneshot::channel();
    let (pressure_stream, _) = start_streaming_sse_server(vec![vec![StreamingSseChunk {
        gate: Some(pressure_gate),
        body: sse(vec![ev_completed("pressure-stopped")]),
    }]])
    .await;
    redirect_once(
        &server,
        |request: &wiremock::Request| {
            request_has_model(request, ROLE_MODEL) && body_contains(request, PRESSURE_TASK)
        },
        &pressure_stream,
    )
    .await;
    mount_root_collaboration_call(
        &server,
        "Create residency pressure",
        "spawn-pressure",
        "spawn_agent",
        &json!({"message": PRESSURE_TASK, "task_name": "pressure", "agent_type": ROLE_NAME,
            "fork_turns": "none"})
        .to_string(),
    )
    .await;
    test.submit_text_turn("Create residency pressure").await?;
    wait_idle(&mut idle_rx, &root_level).await?;
    timeout(
        Duration::from_secs(10),
        pressure_stream.wait_for_request_count(1),
    )
    .await?;
    assert!(
        test.thread_manager.get_thread(parent_id).await.is_err(),
        "actual residency eviction"
    );
    assert!(matches!(parent.agent_status().await, AgentStatus::Shutdown));
    release_child
        .send(())
        .expect("finish the child after parent eviction");
    wait_for_event(child.as_ref(), |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    wait_idle(&mut idle_rx, child.thread_extension_data().level_id()).await?;
    if stop {
        assert!(
            test.thread_manager.get_thread(parent_id).await.is_err(),
            "Stop forbids auto-reload"
        );
        assert!(!resumed.requests().iter().any(request_has_result));
    } else {
        let restored = test.thread_manager.get_thread(parent_id).await?;
        assert!(!Arc::ptr_eq(&parent, &restored), "fresh parent runtime");
        wait_for_event(restored.as_ref(), |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        assert_eq!(
            resumed
                .requests()
                .iter()
                .filter(|request| request_has_result(request))
                .count(),
            1
        );
        assert!(
            test.thread_manager.get_thread(child_id).await.is_err(),
            "tight resident limit requires reclaiming the completed child's slot"
        );
    }
    test.codex.submit(Op::Shutdown).await?;
    Ok(())
}

fn request_has_result(request: &core_test_support::responses::ResponsesRequest) -> bool {
    request
        .input()
        .iter()
        .any(|item| item["type"] == "agent_message" && item.to_string().contains(CHILD_RESULT))
}
