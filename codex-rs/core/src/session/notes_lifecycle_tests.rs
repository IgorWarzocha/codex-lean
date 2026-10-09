use super::*;
use crate::config::Config;
use crate::config::test_config;
use crate::rollout::recorder::RolloutRecorder;
use crate::thread_manager::ForkSnapshot;
use crate::thread_manager::NewThread;
use crate::thread_manager::StartThreadOptions;
use crate::thread_manager::ThreadManager;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_protocol::mcp::ClientMcpExtensions;
use codex_protocol::models::ContentItem;
use codex_protocol::protocol::ErrorEvent;
use codex_protocol::protocol::NotesCheckpoint;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::TurnAbortedEvent;
use codex_protocol::protocol::TurnCompleteEvent;
use codex_protocol::protocol::TurnStartedEvent;
use codex_thread_store::ForkBoundary;
use codex_thread_store::PrepareForkParams;
use std::path::Path;

fn started(turn_id: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnStarted(TurnStartedEvent {
        turn_attribution: None,
        turn_id: turn_id.to_owned(),
        root_turn_id: None,
        trace_id: None,
        started_at: None,
        model_context_window: None,
        collaboration_mode_kind: ModeKind::Default,
    }))
}

fn user(text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(codex_history::ResponseItemEnvelope::new(
        ResponseItem::Message {
            id: None,
            role: "user".to_owned(),
            content: vec![ContentItem::InputText {
                text: text.to_owned(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    ))
}

fn complete(turn_id: &str, checkpoint: Option<NotesCheckpoint>, failed: bool) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::TurnComplete(TurnCompleteEvent {
        turn_id: turn_id.to_owned(),
        root_turn_id: None,
        notes_checkpoint: checkpoint,
        last_agent_message: Some("done".to_owned()),
        error: failed.then(|| ErrorEvent {
            message: "failed".to_owned(),
            codex_error_info: None,
            misalignment: None,
        }),
        started_at: None,
        completed_at: None,
        duration_ms: None,
        time_to_first_token_ms: None,
    }))
}

#[tokio::test]
async fn replay_only_reuses_notes_from_the_selected_settled_run() {
    let (session, turn) = super::tests::make_session_and_context().await;
    let checkpoint = NotesCheckpoint {
        thread_id: Some(session.thread_id()),
        window_id: session
            .state
            .lock()
            .await
            .auto_compact_window_ids()
            .window_id
            .to_string(),
        settled_at_ms: 100,
        fresh: true,
    };
    let saved = vec![
        started("saved"),
        user("old work"),
        complete("saved", Some(checkpoint.clone()), false),
    ];
    let legacy_checkpoint: NotesCheckpoint = serde_json::from_value(serde_json::json!({
        "window_id": checkpoint.window_id,
        "settled_at_ms": checkpoint.settled_at_ms,
        "fresh": true,
    }))
    .expect("legacy settlement without owner remains readable");
    assert_eq!(
        session
            .reconstruct_history_from_rollout(&turn, &saved)
            .await
            .notes_checkpoint,
        Some(checkpoint.clone())
    );

    for tail in [
        vec![started("unfinished")],
        vec![
            started("cancelled"),
            RolloutItem::EventMsg(EventMsg::TurnAborted(TurnAbortedEvent {
                turn_id: Some("cancelled".to_owned()),
                root_turn_id: None,
                reason: TurnAbortReason::Interrupted,
                error: None,
                started_at: None,
                completed_at: None,
                duration_ms: None,
            })),
        ],
        vec![
            started("missing"),
            user("new work"),
            complete("missing", None, false),
        ],
        vec![
            started("failed"),
            user("new work"),
            complete("failed", Some(checkpoint.clone()), true),
        ],
        vec![
            started("foreign"),
            user("inherited work"),
            complete(
                "foreign",
                Some(NotesCheckpoint {
                    thread_id: Some(ThreadId::new()),
                    ..checkpoint.clone()
                }),
                false,
            ),
        ],
        vec![
            started("legacy"),
            user("legacy work"),
            complete("legacy", Some(legacy_checkpoint), false),
        ],
        vec![user("injected after completion")],
    ] {
        let mut history = saved.clone();
        history.extend(tail);
        assert_eq!(
            session
                .reconstruct_history_from_rollout(&turn, &history)
                .await
                .notes_checkpoint,
            None
        );
    }

    let mut rolled_back = saved;
    rolled_back.extend([
        started("removed"),
        user("discarded work"),
        complete("removed", None, false),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
    ]);
    assert_eq!(
        session
            .reconstruct_history_from_rollout(&turn, &rolled_back)
            .await
            .notes_checkpoint,
        Some(checkpoint)
    );
}

#[tokio::test]
async fn rollback_cannot_restore_an_ancestor_notes_checkpoint() {
    let (session, turn) = super::tests::make_session_and_context().await;
    let checkpoint = NotesCheckpoint {
        thread_id: Some(ThreadId::new()),
        window_id: session
            .state
            .lock()
            .await
            .auto_compact_window_ids()
            .window_id
            .to_string(),
        settled_at_ms: 100,
        fresh: true,
    };
    let history = vec![
        started("parent"),
        user("parent work"),
        complete("parent", Some(checkpoint.clone()), false),
        started("child"),
        user("child work"),
        complete(
            "child",
            Some(NotesCheckpoint {
                thread_id: Some(session.thread_id()),
                ..checkpoint
            }),
            false,
        ),
        RolloutItem::EventMsg(EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
            num_turns: 1,
        })),
    ];
    assert_eq!(
        session
            .reconstruct_history_from_rollout(&turn, &history)
            .await
            .notes_checkpoint,
        None
    );
}

async fn notes_thread_manager(config: &Config) -> ThreadManager {
    let state_db =
        codex_state::StateRuntime::init(config.sqlite.clone(), config.model_provider_id.clone())
            .await
            .expect("initialize persistent thread store");
    ThreadManager::with_models_provider_home_and_state_for_tests(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        config.model_provider.clone(),
        config.codex_home.to_path_buf(),
        Arc::new(codex_exec_server::EnvironmentManager::default_for_tests()),
        Some(state_db),
    )
}

async fn cold_resume_notes_thread(config: &Config, path: &Path) -> (ThreadManager, NewThread) {
    let manager = notes_thread_manager(config).await;
    let history = RolloutRecorder::get_rollout_history(path)
        .await
        .expect("read cold history");
    let thread = manager
        .resume_thread_with_history(
            config.clone(),
            history,
            AuthManager::from_auth_for_testing(CodexAuth::create_dummy_chatgpt_auth_for_testing()),
            None,
            ClientMcpExtensions::default(),
        )
        .await
        .expect("cold resume");
    (manager, thread)
}

async fn save_settled_notes(session: &Arc<Session>) {
    let turn = session.new_default_turn().await;
    session.begin_notes_run(&turn).await;
    // Exercise native trusted-write settlement without a model or a remote notes backend.
    let tracker = session
        .services
        .thread_extension_data
        .get::<codex_extension_api::NotesCheckpointTracker>()
        .unwrap();
    tracker.finish_write(tracker.begin_write(&turn.sub_id).unwrap(), true);
    assert!(
        session
            .persist_rollout_items(&[started(&turn.sub_id), user("checkpoint work")])
            .await
    );
    session
        .settle_manual_notes(&turn, Some("saved".to_owned()))
        .await
        .expect("settle saved notes");
}

async fn open_notes_window(session: &Arc<Session>) {
    let turn = session.new_default_turn().await;
    let step = session
        .capture_step_context(turn, &CancellationToken::new())
        .await
        .expect("capture step");
    let world_state = Arc::new(
        session
            .build_world_state_for_step(&step, /*new_window*/ true)
            .await
            .expect("build world state"),
    );
    session.start_new_context_window(&step, world_state).await;
    session.flush_rollout().await.expect("flush new window");
}

#[test_case::test_case(ThreadHistoryMode::Legacy, false; "copied legacy")]
#[test_case::test_case(ThreadHistoryMode::Paginated, false; "copied paginated")]
#[test_case::test_case(ThreadHistoryMode::Paginated, true; "referenced paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fork_notes_ownership_survives_cold_resume(
    history_mode: ThreadHistoryMode,
    referenced: bool,
) {
    let server = wiremock::MockServer::start().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path_regex(".*/models$"))
        .respond_with(
            wiremock::ResponseTemplate::new(200).set_body_json(
                codex_protocol::openai_models::ModelsResponse { models: Vec::new() },
            ),
        )
        .mount(&server)
        .await;
    let home = tempfile::tempdir().expect("codex home");
    let mut config = test_config().await;
    config.codex_home =
        codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(home.path()).unwrap();
    config.cwd = config.codex_home.clone();
    config.sqlite = codex_state::SqliteConfig::new_for_testing(config.codex_home.clone());
    config.chatgpt_base_url = server.uri();
    config.model_provider.base_url = Some(format!("{}/backend-api/codex", server.uri()));
    config.context_strategy = codex_config::types::ContextStrategy::Notes;
    config
        .features
        .enable(Feature::TokenBudget)
        .expect("enable token budget");
    let manager = notes_thread_manager(&config).await;
    let parent = manager
        .start_thread(StartThreadOptions {
            history_mode: Some(history_mode),
            environments: Some(Vec::new()),
            ..StartThreadOptions::new(config.clone())
        })
        .await
        .expect("start parent");
    let parent_session = &parent.thread.session;
    // A first-window-only fixture misses the bug: the fork inherits this Compacted window.
    open_notes_window(parent_session).await;
    save_settled_notes(parent_session).await;
    assert!(parent_session.has_fresh_notes().await);
    let parent_window = parent_session
        .state
        .lock()
        .await
        .auto_compact_window_ids()
        .window_id;
    let parent_path = parent.thread.rollout_path().unwrap();

    let options = StartThreadOptions {
        history_mode: Some(history_mode),
        environments: Some(Vec::new()),
        ..StartThreadOptions::new(config.clone())
    };
    let child = if referenced {
        let prepared = parent_session
            .services
            .thread_store
            .prepare_fork(PrepareForkParams {
                thread_id: parent.thread_id,
                boundary: ForkBoundary::Latest,
            })
            .await
            .expect("freeze parent history");
        manager
            .fork_prepared_thread(options, prepared)
            .await
            .expect("reference-backed fork")
    } else {
        let history = RolloutRecorder::get_rollout_history(&parent_path)
            .await
            .expect("read parent history");
        manager
            .fork_thread_from_history(ForkSnapshot::Interrupted, options, history)
            .await
            .expect("copied fork")
    };
    assert_eq!(
        child
            .thread
            .session
            .state
            .lock()
            .await
            .auto_compact_window_ids()
            .window_id,
        parent_window
    );
    assert!(!child.thread.session.has_fresh_notes().await);
    let child_path = child.thread.rollout_path().unwrap();
    child
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown untouched child");
    parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown parent");
    drop(manager);

    let (_parent_manager, resumed_parent) = cold_resume_notes_thread(&config, &parent_path).await;
    assert!(
        resumed_parent.thread.session.has_fresh_notes().await,
        "same-thread resume retains settlement"
    );
    resumed_parent
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown resumed parent");

    let (_child_manager, resumed_child) = cold_resume_notes_thread(&config, &child_path).await;
    let child_session = &resumed_child.thread.session;
    assert_eq!(
        child_session
            .state
            .lock()
            .await
            .auto_compact_window_ids()
            .window_id,
        parent_window
    );
    assert!(
        !child_session.has_fresh_notes().await,
        "cold fork must not regain ancestor credit before input"
    );
    save_settled_notes(child_session).await;
    assert!(
        child_session.has_fresh_notes().await,
        "child's own checkpoint is fresh"
    );
    resumed_child
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown saved child");

    let (_saved_manager, saved_child) = cold_resume_notes_thread(&config, &child_path).await;
    assert!(
        saved_child.thread.session.has_fresh_notes().await,
        "child settlement survives cold resume"
    );
    // Bounded paginated replay must also retain owned settlement after a child compaction.
    open_notes_window(&saved_child.thread.session).await;
    save_settled_notes(&saved_child.thread.session).await;
    saved_child
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown compacted child");
    let (_compacted_manager, compacted_child) =
        cold_resume_notes_thread(&config, &child_path).await;
    assert!(compacted_child.thread.session.has_fresh_notes().await);
    compacted_child
        .thread
        .shutdown_and_wait()
        .await
        .expect("shutdown final child");
}

#[tokio::test]
async fn only_successful_settlement_authorizes_notes_reuse() {
    let (session, mut turn) = super::tests::make_session_and_context().await;
    turn.config = Arc::new({
        let mut config = (*turn.config).clone();
        config.context_strategy = codex_config::types::ContextStrategy::Notes;
        config
    });
    session.begin_notes_run(&turn).await;
    let tracker = session
        .services
        .thread_extension_data
        .get::<codex_extension_api::NotesCheckpointTracker>()
        .unwrap();
    tracker.finish_write(tracker.begin_write(&turn.sub_id).unwrap(), true);
    let settlement = session.notes_settlement(&turn, true).await;
    assert_eq!(settlement.thread_id, Some(session.thread_id()));
    assert!(settlement.fresh);
    assert!(!session.notes_settlement(&turn, false).await.fresh);
    tracker.finish_write(tracker.begin_write(&turn.sub_id).unwrap(), false);
    assert!(!session.notes_settlement(&turn, true).await.fresh);
    session.begin_notes_run(&turn).await;
    assert!(!session.notes_settlement(&turn, true).await.fresh);
}
