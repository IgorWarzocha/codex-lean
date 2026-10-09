use crate::session::tests::make_session_and_context_with_rx;
use codex_features::Feature;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_history::RolloutItem;
use codex_protocol::models::ConfigurationReasoning;
use codex_protocol::models::ResponseItem;
use codex_protocol::openai_models::ReasoningEffort;
use codex_protocol::protocol::EventMsg;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn queued_client_history_reserves_order_before_a_later_user_reply() {
    let (session, turn_context, _) = make_session_and_context_with_rx().await;
    let active = crate::state::ActiveTurn::default();
    let turn_state = active.turn_state.clone();
    *session.active_turn.lock().await = Some(active);
    let items = ["user", "assistant"].map(|role| {
        serde_json::from_value(serde_json::json!({
            "type": "message", "role": role,
            "content": [{"type": "input_text", "text": "Earlier client history"}]
        }))
        .expect("client message")
    });
    session
        .inject_client_response_items(items.to_vec(), &turn_context)
        .await;
    let reply_order = session.reserve_user_input_order().await;
    let queued = session
        .input_queue
        .take_pending_input_for_turn_state(&turn_state)
        .await;
    assert_eq!(queued.len(), 2);
    let mut previous = None;
    for input in queued {
        let super::PendingTurnInput::ResponseItem(envelope) = input else {
            panic!("expected injected client history");
        };
        let order = envelope
            .metadata
            .expect("admission metadata")
            .user_input_order
            .expect("client acceptance order");
        assert!(previous.is_none_or(|previous| previous < order));
        assert!(
            order < reply_order,
            "later reply must remain newer than queued history"
        );
        previous = Some(order);
    }
}

#[tokio::test]
async fn harness_authored_configuration_updates_preserve_metadata_and_resume() {
    let (session, turn_context, rx_event) = make_session_and_context_with_rx().await;
    assert!(!session.enabled(Feature::RetainClientDeveloperMessages));

    let mut expected = ResponseItemEnvelope {
        item: ResponseItem::ConfigurationUpdate {
            reasoning: ConfigurationReasoning {
                effort: ReasoningEffort::High,
            },
        },
        metadata: Some(CodexHarnessMetadata {
            harness_authored_configuration: true,
            ..Default::default()
        }),
    };
    session
        .record_annotated_conversation_items(
            &turn_context,
            turn_context.model_info(),
            vec![expected.clone()],
        )
        .await;

    expected.metadata.as_mut().unwrap().mcp_attribution = Some(
        session
            .services
            .executed_tool_calls
            .mcp_attribution_snapshot(),
    );

    let recorded = session.clone_history().await.into_annotated_items();
    assert_eq!(recorded, vec![expected.clone()]);
    let mut raw_items = Vec::new();
    while let Ok(event) = rx_event.try_recv() {
        if let EventMsg::RawResponseItem(event) = event.msg {
            raw_items.push(event.item);
        }
    }
    assert_eq!(raw_items, vec![expected.item]);

    let rollout_items = recorded
        .iter()
        .cloned()
        .map(RolloutItem::ResponseItem)
        .collect::<Vec<_>>();
    let reconstructed = session
        .reconstruct_history_from_rollout(&turn_context, &rollout_items)
        .await;
    assert_eq!(reconstructed.history, recorded);
}
