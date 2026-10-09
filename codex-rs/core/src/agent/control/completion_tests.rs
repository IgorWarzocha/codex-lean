use super::*;
use crate::agent::control::spawn::SpawnInitialInput;

/// Queue-only initial mail gives these controller tests registered, task-free runtimes.
/// Real nested inference and natural task completion are covered by multi_agent_resume.
async fn idle_child(
    harness: &AgentControlHarness,
    control: &LocalAgentControl,
    root: &CodexThread,
    name: &str,
) -> Arc<CodexThread> {
    let source = thread_spawn_source(
        root.session.thread_id,
        &root.session_source,
        1,
        None,
        Some(name.to_string()),
    )
    .expect("child source");
    let path = source.get_agent_path().expect("child path");
    let (child, _) = control
        .spawn_agent_internal(
            harness.config.clone(),
            SpawnInitialInput::InterAgentCommunication(
                InterAgentCommunication::new(
                    AgentPath::root(),
                    path,
                    Vec::new(),
                    "fixture mail".into(),
                    false,
                ),
                AgentCommunicationContext::new(
                    AgentCommunicationKind::Message,
                    root.session.thread_id,
                ),
            ),
            Some(source),
            SpawnAgentOptions {
                parent_thread_id: Some(root.session.thread_id),
                ..Default::default()
            },
        )
        .await
        .expect("spawn idle child");
    let thread = harness.manager.get_thread(child.thread_id).await.unwrap();
    let turn = thread.session.new_default_turn().await;
    thread
        .session
        .send_event_without_parent_notification(
            &turn,
            EventMsg::TurnComplete(TurnCompleteEvent {
                turn_id: turn.sub_id.clone(),
                root_turn_id: Some(turn.root_turn_id()),
                started_at: None,
                last_agent_message: Some("done".into()),
                error: None,
                completed_at: None,
                duration_ms: None,
                time_to_first_token_ms: None,
                notes_checkpoint: None,
            }),
        )
        .await;
    assert!(thread.session.active_turn.lock().await.is_none());
    thread
}

async fn evicted_parent_fixture() -> (
    AgentControlHarness,
    LocalAgentControl,
    Arc<CodexThread>,
    Arc<CodexThread>,
) {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::Sqlite).unwrap();
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (_, root) = harness.start_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let parent = idle_child(&harness, &control, &root, "parent").await;
    let pressure = idle_child(&harness, &control, &root, "pressure").await;
    assert!(
        harness
            .manager
            .get_thread(parent.session.thread_id)
            .await
            .is_err()
    );
    assert!(
        control
            .runtime
            .registry
            .evicted_completion_config(parent.session.thread_id)
            .is_some()
    );
    (harness, control, parent, pressure)
}

#[tokio::test]
async fn completion_wake_waits_for_delivery_pin_then_restores_once() {
    check_completion_capacity_release(true).await;
}

#[tokio::test]
async fn completion_wake_waits_for_runtime_gate_then_restores_once() {
    check_completion_capacity_release(false).await;
}

async fn check_completion_capacity_release(use_pin: bool) {
    let (harness, control, parent, pressure) = evicted_parent_fixture().await;
    let state = control.runtime.upgrade().unwrap();
    let (tx_sub, _rx_sub) = async_channel::bounded(1);
    let blocked_io = crate::session::SessionIo {
        tx_sub,
        rx_event: async_channel::unbounded().1,
        agent_status: pressure.io.agent_status.clone(),
        session_loop_termination: pressure.io.session_loop_termination.clone(),
    };
    blocked_io
        .submit(Op::CleanBackgroundTerminals)
        .await
        .unwrap();
    let blocked_submission = if use_pin {
        let pin = control
            .runtime
            .pin_v2_residency(&state, &pressure)
            .await
            .unwrap();
        let mut submission =
            Box::pin(blocked_io.submit_with_trace(Op::Interrupt, None, None, None, pin));
        assert!(
            timeout(Duration::from_millis(20), &mut submission)
                .await
                .is_err()
        );
        Some(submission)
    } else {
        None
    };
    let gate = if use_pin {
        None
    } else {
        Some(
            control.runtime.track_runtime_guard(
                control
                    .runtime
                    .registry
                    .runtime_gate(pressure.session.thread_id)
                    .unwrap()
                    .lock_owned()
                    .await,
            ),
        )
    };
    let parent_id = parent.session.thread_id;
    let wake = control.completion_parent_guard(parent_id, &state);
    tokio::pin!(wake);
    assert!(timeout(Duration::from_secs(1), &mut wake).await.is_err());
    assert!(harness.manager.get_thread(parent_id).await.is_err());
    // Cancellation drops the queued send and its last residency pin outside the handler.
    drop(blocked_submission);
    drop(gate);
    let guard = timeout(Duration::from_secs(5), &mut wake)
        .await
        .expect("capacity release wakes the completion owner")
        .expect("parent restores");
    let restored = harness.manager.get_thread(parent_id).await.unwrap();
    assert!(!Arc::ptr_eq(&parent, &restored));
    assert!(
        harness
            .manager
            .get_thread(pressure.session.thread_id)
            .await
            .is_err()
    );
    drop(guard);
    let _second_guard = control
        .completion_parent_guard(parent_id, &state)
        .await
        .unwrap();
    assert!(Arc::ptr_eq(
        &restored,
        &harness.manager.get_thread(parent_id).await.unwrap(),
    ));
}

#[tokio::test]
async fn completion_wake_stop_during_capacity_wait_retains_result_without_reload() {
    let (harness, control, parent, pressure) = evicted_parent_fixture().await;
    let _pin = Arc::clone(&pressure.residency_gate).read_owned().await;
    let parent_id = parent.session.thread_id;
    let result = InterAgentCommunication::new(
        AgentPath::root().join("child").unwrap(),
        AgentPath::root().join("parent").unwrap(),
        Vec::new(),
        "retained terminal result".into(),
        false,
    );
    let send = control.send_inter_agent_communication(
        parent_id,
        result.clone(),
        AgentCommunicationContext::new(AgentCommunicationKind::Result, pressure.session.thread_id),
        TurnStartOptions {
            resume_parent_on_completion: true,
            ..Default::default()
        },
    );
    tokio::pin!(send);
    assert!(timeout(Duration::from_secs(1), &mut send).await.is_err());
    let _ = control.interrupt_agent(parent_id).await;
    timeout(Duration::from_secs(5), &mut send)
        .await
        .expect("Stop wakes the completion owner")
        .expect("result remains queue-only");
    assert!(harness.manager.get_thread(parent_id).await.is_err());
    let retained = control.runtime.mailboxes.take(parent_id);
    let result_mail = retained
        .iter()
        .find(|mail| mail.communication == result)
        .unwrap();
    assert!(result_mail.start_options.resume_parent_on_completion);
    assert!(!result_mail.communication.trigger_turn);
}

#[tokio::test]
async fn completion_wake_close_during_capacity_wait_cannot_restore_parent() {
    let (harness, control, parent, pressure) = evicted_parent_fixture().await;
    let state = control.runtime.upgrade().unwrap();
    let _pin = Arc::clone(&pressure.residency_gate).read_owned().await;
    let parent_id = parent.session.thread_id;
    let wake = control.completion_parent_guard(parent_id, &state);
    tokio::pin!(wake);
    assert!(timeout(Duration::from_secs(1), &mut wake).await.is_err());
    let _ = control.shutdown_live_agent(parent_id).await;
    let err = timeout(Duration::from_secs(5), &mut wake)
        .await
        .expect("close wakes the completion owner")
        .err()
        .expect("closed identity cannot reload");
    assert!(matches!(err.details(), CodexErrorDetails::ThreadNotFound(id) if *id == parent_id));
    assert!(harness.manager.get_thread(parent_id).await.is_err());
    assert!(control.runtime.mailboxes.take(parent_id).is_empty());
}

#[tokio::test]
async fn completion_wake_shutdown_cancels_wait_for_parent_runtime_gate() {
    let (harness, control, parent, _pressure) = evicted_parent_fixture().await;
    let state = control.runtime.upgrade().unwrap();
    let parent_id = parent.session.thread_id;
    let _gate = control
        .runtime
        .registry
        .runtime_gate(parent_id)
        .unwrap()
        .lock_owned()
        .await;
    let wake = control.completion_parent_guard(parent_id, &state);
    tokio::pin!(wake);
    assert!(timeout(Duration::from_millis(20), &mut wake).await.is_err());
    let _shutdown = control.runtime.request_shutdown();
    let err = timeout(Duration::from_secs(1), &mut wake)
        .await
        .expect("tree shutdown cancels gate acquisition without unlocking")
        .err()
        .expect("shutdown rejects completion delivery");
    assert!(matches!(
        err.details(),
        CodexErrorDetails::InvalidRequest(_)
    ));
    assert!(harness.manager.get_thread(parent_id).await.is_err());
}

#[tokio::test]
#[expect(
    clippy::await_holding_invalid_type,
    reason = "hold active_turn to block Stop handling while testing its queued eviction reservation"
)]
async fn completion_wake_traced_idle_stop_fences_eviction_and_reload() {
    let (home, mut config) = test_config().await;
    config.features.enable(Feature::MultiAgentV2).unwrap();
    config.features.enable(Feature::Sqlite).unwrap();
    config.multi_agent_v2.max_concurrent_threads_per_session = 2;
    let harness = AgentControlHarness::new_with_config(home, config).await;
    let (_, root) = harness.start_thread().await;
    let control = root
        .session
        .services
        .local_agent_runtime
        .control(root.session.session_id());
    let state = control.runtime.upgrade().unwrap();
    let parent = idle_child(&harness, &control, &root, "parent").await;
    let parent_id = parent.session.thread_id;
    // Hold the handler before it can apply Stop. The traced submission must already
    // carry an eviction pin rather than allowing a Completed snapshot to be captured.
    let active = parent.session.active_turn.lock().await;
    parent
        .submit_with_trace(
            Op::Interrupt,
            Some(codex_protocol::protocol::W3cTraceContext {
                traceparent: Some("00-00000000000000000000000000000011-0000000000000022-01".into()),
                tracestate: Some("vendor=value".into()),
            }),
        )
        .await
        .unwrap();
    let membership = control.runtime.admit_start().unwrap();
    let capacity_error = timeout(
        Duration::from_secs(1),
        control.reserve_v2_residency_slot(&state, &harness.config, &membership, None),
    )
    .await
    .expect("queued traced Stop excludes eviction without waiting for the handler")
    .err()
    .expect("the only resident is pinned by Stop");
    assert!(matches!(
        capacity_error.details(),
        CodexErrorDetails::AgentLimitReached { .. }
    ));
    drop(active);
    // A rejected steer is an ordered barrier without admitting a task or clearing Stop.
    assert_matches!(
        parent
            .steer_turn(
                TurnInputRequest::user_input(text_input("barrier")),
                "missing-turn".into(),
            )
            .await
            .unwrap(),
        codex_protocol::turn_input::SteerSubmission::NotSubmitted { .. }
    );
    let _pressure = idle_child(&harness, &control, &root, "pressure").await;
    assert!(harness.manager.get_thread(parent_id).await.is_err());
    assert!(
        control
            .runtime
            .registry
            .evicted_completion_config(parent_id)
            .is_none()
    );
    let _guard = control
        .completion_parent_guard(parent_id, &state)
        .await
        .unwrap();
    assert!(harness.manager.get_thread(parent_id).await.is_err());
}

#[tokio::test]
async fn completion_wake_traced_lifecycle_route_preserves_trace_carrier() {
    let (_harness, control, _parent, resident) = evicted_parent_fixture().await;
    let state = control.runtime.upgrade().unwrap();
    let (tx_sub, rx_sub) = async_channel::bounded(1);
    // Observe the real lifecycle route at its owned submission-queue boundary.
    let observed = Arc::new(CodexThread::new(
        Arc::clone(&resident.session),
        crate::session::SessionIo {
            tx_sub,
            rx_event: async_channel::unbounded().1,
            agent_status: resident.io.agent_status.clone(),
            session_loop_termination: resident.io.session_loop_termination.clone(),
        },
        crate::thread_startup_metadata::ThreadStartupMetadata::from(
            &resident.startup_metadata().to_session_configured_event(),
        ),
        resident.rollout_path(),
        resident.session_source.clone(),
    ));
    let id = resident.session.thread_id;
    state
        .threads
        .write()
        .await
        .insert(id, Arc::clone(&observed));
    let trace = codex_protocol::protocol::W3cTraceContext {
        traceparent: Some("00-00000000000000000000000000000011-0000000000000022-01".into()),
        tracestate: Some("vendor=value".into()),
    };
    observed
        .submit_with_trace(Op::Interrupt, Some(trace.clone()))
        .await
        .unwrap();
    let submitted = rx_sub.recv().await.unwrap();
    pretty_assertions::assert_eq!(submitted.trace, Some(trace));
    assert!(submitted.residency_guard.is_some());
    state.threads.write().await.insert(id, resident);
}
