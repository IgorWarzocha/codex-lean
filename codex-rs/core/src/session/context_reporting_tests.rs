use super::context_window::context_window_token_status_for_model;
use super::step_context::StepContext;
use super::tests::make_session_and_context;
use super::tests::update_turn_settings_for_test;
use crate::config::ContextStrategy;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::protocol::TokenUsage;
use std::sync::Arc;
use test_case::test_case;

#[test_case(ContextStrategy::Notes, Some(872_000), false, 828_400; "notes_advertised_maximum")]
#[test_case(ContextStrategy::Compaction, Some(872_000), false, 258_400; "compaction_selected_window")]
#[test_case(ContextStrategy::Notes, None, false, 258_400; "notes_unknown_maximum")]
#[test_case(ContextStrategy::Notes, Some(872_000), true, 258_400; "notes_fallback_metadata")]
#[tokio::test]
async fn reporting_preserves_selected_reminder_budget(
    strategy: ContextStrategy,
    maximum: Option<i64>,
    fallback: bool,
    ceiling: i64,
) {
    let (session, mut turn) = make_session_and_context().await;
    let config = Arc::make_mut(&mut turn.config);
    config.context_strategy = strategy;
    config.model_auto_compact_token_limit_scope = AutoCompactTokenLimitScope::Total;
    update_turn_settings_for_test(&mut turn, |settings| {
        let model = Arc::make_mut(&mut settings.model_info);
        model.context_window = Some(272_000);
        model.max_context_window = maximum;
        model.effective_context_window_percent = 95;
        model.used_fallback_model_metadata = fallback;
        model.auto_compact_token_limit = None;
    });
    assert_eq!(turn.model_context_window(), Some(ceiling));
    assert_eq!(turn.model_info().context_window, Some(272_000));
    assert_eq!(turn.model_info().usable_context_window(), Some(258_400));
    let status =
        context_window_token_status_for_model(&session, &turn.config, &turn, turn.model_info())
            .await;
    assert_eq!(status.full_context_window_limit, Some(ceiling));
    assert_eq!(status.auto_compact_scope_limit, Some(244_800));
    assert_eq!(status.base_window_tokens_remaining, Some(244_800));
}

#[test_case(ContextStrategy::Notes, 475_000; "notes_captured_maximum")]
#[test_case(ContextStrategy::Compaction, 180_500; "compaction_captured_selected_window")]
#[tokio::test]
async fn usage_recompute_and_overflow_report_captured_model(
    strategy: ContextStrategy,
    ceiling: i64,
) {
    let (session, mut turn) = make_session_and_context().await;
    Arc::make_mut(&mut turn.config).context_strategy = strategy;
    update_turn_settings_for_test(&mut turn, |settings| {
        let model = Arc::make_mut(&mut settings.model_info);
        model.context_window = Some(272_000);
        model.max_context_window = Some(872_000);
        model.effective_context_window_percent = 95;
        model.used_fallback_model_metadata = false;
    });
    let mut captured = turn.initial_settings.as_ref().clone();
    let model = Arc::make_mut(&mut captured.model_info);
    model.context_window = Some(190_000);
    model.max_context_window = Some(500_000);
    let mut later = captured.clone();
    Arc::make_mut(&mut later.model_info).max_context_window = Some(300_000);
    turn.next_step_settings.store(Arc::new(later));

    session
        .record_token_usage_info(
            &turn,
            &captured,
            Some(&TokenUsage {
                input_tokens: 10_000,
                total_tokens: 10_000,
                ..TokenUsage::default()
            }),
        )
        .await
        .expect("record captured request usage");
    let info = session.state.lock().await.token_info().expect("token info");
    assert_eq!(info.model_context_window, Some(ceiling));
    assert_eq!(info.last_token_usage.total_tokens, 10_000);

    // Replacement history must keep the same captured ceiling, not the initial turn model.
    session
        .recompute_token_usage(&turn, &captured.model_info)
        .await;
    let info = session.state.lock().await.token_info().expect("token info");
    assert_eq!(info.model_context_window, Some(ceiling));

    session
        .set_total_tokens_full(&turn, &captured.model_info)
        .await;
    let info = session.state.lock().await.token_info().expect("token info");
    assert_eq!(info.model_context_window, Some(ceiling));
    assert_eq!(info.total_token_usage.total_tokens, ceiling);

    let mut step = StepContext::for_test(Arc::new(turn));
    Arc::get_mut(&mut step).expect("unique step").settings = Arc::new(captured);
    let world_state = Arc::new(
        session
            .build_world_state_for_step(&step)
            .await
            .expect("world state"),
    );
    session.start_new_context_window(&step, world_state).await;
    let info = session.state.lock().await.token_info().expect("token info");
    assert_eq!(info.model_context_window, Some(ceiling));
    assert!(info.last_token_usage.total_tokens < ceiling);
}
