use super::context_window::context_window_token_status_for_model;
use super::step_context::StepContext;
use super::tests::make_session_and_context;
use super::tests::update_turn_settings_for_test;
use crate::config::ContextStrategy;
use codex_protocol::config_types::AutoCompactTokenLimitScope;
use codex_protocol::protocol::TokenUsage;
use std::sync::Arc;
use test_case::test_case;

#[test_case(ContextStrategy::Notes, Some(272_000), Some(872_000), false, 272_000, 828_400; "notes_advertised_maximum")]
#[test_case(ContextStrategy::Compaction, Some(272_000), Some(872_000), false, 272_000, 258_400; "compaction_selected_window")]
#[test_case(ContextStrategy::Notes, Some(123_456), Some(872_000), false, 123_456, 828_400; "notes_custom_window")]
#[test_case(ContextStrategy::Compaction, Some(123_456), Some(872_000), false, 123_456, 117_283; "compaction_custom_window")]
#[test_case(ContextStrategy::Notes, Some(272_000), None, false, 272_000, 258_400; "notes_unknown_maximum")]
#[test_case(ContextStrategy::Notes, Some(272_000), Some(872_000), true, 272_000, 258_400; "notes_fallback_metadata")]
#[test_case(ContextStrategy::Notes, None, Some(500_000), false, 500_000, 475_000; "notes_resolved_maximum")]
#[tokio::test]
async fn reporting_preserves_selected_reminder_budget(
    strategy: ContextStrategy,
    selected: Option<i64>,
    maximum: Option<i64>,
    fallback: bool,
    displayed: i64,
    ceiling: i64,
) {
    let (session, mut turn) = make_session_and_context().await;
    let config = Arc::make_mut(&mut turn.config);
    config.context_strategy = strategy;
    config.model_auto_compact_token_limit_scope = AutoCompactTokenLimitScope::Total;
    update_turn_settings_for_test(&mut turn, |settings| {
        let model = Arc::make_mut(&mut settings.model_info);
        model.context_window = selected;
        model.max_context_window = maximum;
        model.effective_context_window_percent = 95;
        model.used_fallback_model_metadata = fallback;
        model.auto_compact_token_limit = None;
    });
    assert_eq!(turn.model_context_window(), Some(displayed));
    assert_eq!(turn.model_info().context_window, selected);
    assert_eq!(
        turn.model_info().usable_context_window(),
        Some(displayed * 95 / 100)
    );
    let status =
        context_window_token_status_for_model(&session, &turn.config, &turn, turn.model_info())
            .await;
    assert_eq!(status.full_context_window_limit, Some(ceiling));
    assert_eq!(status.auto_compact_scope_limit, Some(displayed * 90 / 100));
    assert_eq!(
        status.base_window_tokens_remaining,
        Some(displayed * 90 / 100)
    );
}

#[test_case(ContextStrategy::Notes, 190_000, 300_000, 475_000; "notes_captured_maximum")]
#[test_case(ContextStrategy::Notes, 123_456, 600_000, 475_000; "notes_retains_observed_overflow")]
#[test_case(ContextStrategy::Compaction, 190_000, 10_000, 180_500; "compaction_captured_selected_window")]
#[tokio::test]
async fn usage_recompute_and_overflow_report_captured_model(
    strategy: ContextStrategy,
    selected: i64,
    observed: i64,
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
    model.context_window = Some(selected);
    model.max_context_window = Some(500_000);
    let mut later = captured.clone();
    let later_model = Arc::make_mut(&mut later.model_info);
    later_model.context_window = Some(90_000);
    later_model.max_context_window = Some(300_000);
    turn.next_step_settings.store(Arc::new(later));

    session
        .record_token_usage_info(
            &turn,
            &captured,
            Some(&TokenUsage {
                input_tokens: observed,
                total_tokens: observed,
                ..TokenUsage::default()
            }),
        )
        .await
        .expect("record captured request usage");
    let info = session.state.lock().await.token_info().expect("token info");
    assert_eq!(info.model_context_window, Some(selected));
    assert_eq!(info.last_token_usage.total_tokens, observed);
    let observed_info = info.clone();
    if strategy == ContextStrategy::Notes {
        assert!(observed > selected);
        let status = context_window_token_status_for_model(
            &session,
            &turn.config,
            &turn,
            &captured.model_info,
        )
        .await;
        assert_eq!(status.token_limit_reached, observed >= ceiling);
        assert!(status.notes_checkpoint_due);
    }

    // Replacement history must keep the captured budget, not the initial or pending model.
    session
        .recompute_token_usage(&turn, &captured.model_info)
        .await;
    let info = session.state.lock().await.token_info().expect("token info");
    assert_eq!(info.model_context_window, Some(selected));

    session
        .state
        .lock()
        .await
        .set_token_info(Some(observed_info));
    session
        .set_total_tokens_full(&turn, &captured.model_info)
        .await;
    let info = session.state.lock().await.token_info().expect("token info");
    assert_eq!(info.model_context_window, Some(selected));
    assert_eq!(info.total_token_usage.total_tokens, ceiling.max(observed));
    if strategy == ContextStrategy::Notes {
        assert_eq!(info.last_token_usage.total_tokens, ceiling.max(observed));
        let status = context_window_token_status_for_model(
            &session,
            &turn.config,
            &turn,
            &captured.model_info,
        )
        .await;
        assert!(status.full_context_window_limit_reached);
        assert!(status.token_limit_reached);
    }

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
    assert_eq!(info.model_context_window, Some(selected));
    assert!(info.last_token_usage.total_tokens < selected);
}
