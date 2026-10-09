use std::num::NonZeroU64;
use std::sync::Arc;

use codex_async_utils::OrCancelExt;
use codex_config::types::ContextStrategy;
use codex_extension_api::NotesCheckpointTracker;
use codex_history::RolloutItem;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::NotesCheckpoint;
use codex_protocol::protocol::TurnCompleteEvent;
use tokio_util::sync::CancellationToken;

use super::session::Session;
use super::turn_context::TurnContext;

pub(super) fn checkpoint_is_fresh(checkpoint: Option<&NotesCheckpoint>, window_id: &str) -> bool {
    checkpoint.is_some_and(|checkpoint| checkpoint.fresh && checkpoint.window_id == window_id)
}

fn idle_rollover_due(
    checkpoint: Option<&NotesCheckpoint>,
    window_id: &str,
    minutes: Option<NonZeroU64>,
    now_ms: i64,
) -> bool {
    let Some(minutes) = minutes else {
        return false;
    };
    checkpoint_is_fresh(checkpoint, window_id)
        && checkpoint.is_some_and(|checkpoint| {
            u64::try_from(now_ms.saturating_sub(checkpoint.settled_at_ms))
                .is_ok_and(|elapsed_ms| elapsed_ms / 60_000 >= minutes.get())
        })
}

impl Session {
    pub(crate) async fn begin_notes_run(&self, turn_context: &TurnContext) {
        self.state.lock().await.notes_checkpoint = None;
        if turn_context.config.context_strategy != ContextStrategy::Notes {
            return;
        }
        self.services
            .thread_extension_data
            .get_or_init(NotesCheckpointTracker::default)
            .begin_run(&turn_context.sub_id);
    }

    pub(crate) async fn has_fresh_notes(&self) -> bool {
        let state = self.state.lock().await;
        checkpoint_is_fresh(
            state.notes_checkpoint.as_ref(),
            &state.auto_compact_window_ids().window_id.to_string(),
        )
    }

    pub(crate) async fn notes_settlement(
        &self,
        turn_context: &TurnContext,
        completed: bool,
    ) -> NotesCheckpoint {
        let saved = self
            .services
            .thread_extension_data
            .get::<NotesCheckpointTracker>()
            .is_some_and(|tracker| tracker.has_successful_notes(&turn_context.sub_id));
        NotesCheckpoint {
            thread_id: Some(self.thread_id()),
            window_id: self
                .state
                .lock()
                .await
                .auto_compact_window_ids()
                .window_id
                .to_string(),
            settled_at_ms: crate::turn_timing::now_unix_timestamp_ms(),
            fresh: completed && saved,
        }
    }

    pub(crate) async fn install_notes_settlement(
        &self,
        checkpoint: NotesCheckpoint,
    ) -> CodexResult<()> {
        self.flush_rollout().await.map_err(|error| {
            CodexErr::InvalidRequest(format!("Failed to persist notes settlement: {error}"))
        })?;
        self.state.lock().await.notes_checkpoint = Some(checkpoint);
        Ok(())
    }

    /// Manual checkpoint sampling has settled before the outer compact task ends. Persist
    /// that settlement before replacing history, without publishing a second UI completion.
    pub(crate) async fn settle_manual_notes(
        &self,
        turn_context: &TurnContext,
        last_agent_message: Option<String>,
    ) -> CodexResult<()> {
        let completed = last_agent_message.is_some()
            && turn_context.terminal_error.lock().await.is_none()
            && !self.input_queue.has_pending_input(&self.active_turn).await;
        let checkpoint = self.notes_settlement(turn_context, completed).await;
        let event = TurnCompleteEvent {
            turn_id: turn_context.sub_id.clone(),
            root_turn_id: Some(turn_context.root_turn_id()),
            notes_checkpoint: Some(checkpoint.clone()),
            last_agent_message,
            error: turn_context.terminal_error.lock().await.clone(),
            started_at: None,
            completed_at: None,
            duration_ms: None,
            time_to_first_token_ms: None,
        };
        if !self
            .persist_rollout_items(&[RolloutItem::EventMsg(EventMsg::TurnComplete(event))])
            .await
        {
            return Err(CodexErr::InvalidRequest(
                "Failed to persist notes settlement; context was not reset.".to_owned(),
            ));
        }
        self.install_notes_settlement(checkpoint).await
    }

    /// Runs at native admission, while the caller still owns the exact original input.
    /// No synthetic checkpoint request is made for idle rollover.
    pub(crate) async fn maybe_idle_notes_rollover(
        self: &Arc<Self>,
        turn_context: &Arc<TurnContext>,
        cancellation_token: &CancellationToken,
    ) -> CodexResult<()> {
        if turn_context.config.context_strategy != ContextStrategy::Notes
            || crate::guardian::is_basic_session_source(&turn_context.session_source)
            || self.live_thread().is_none()
        {
            return Ok(());
        }
        let due = {
            let state = self.state.lock().await;
            idle_rollover_due(
                state.notes_checkpoint.as_ref(),
                &state.auto_compact_window_ids().window_id.to_string(),
                turn_context.config.context_idle_rollover_minutes,
                crate::turn_timing::now_unix_timestamp_ms(),
            )
        };
        if due {
            let step = self
                .capture_step_context(Arc::clone(turn_context), cancellation_token)
                .or_cancel(cancellation_token)
                .await??;
            let world_state = Arc::new(
                self.build_world_state_for_step(&step, /*new_window*/ true)
                    .or_cancel(cancellation_token)
                    .await??,
            );
            if cancellation_token.is_cancelled() {
                return Err(CodexErr::TurnAborted);
            }
            crate::compact_token_budget::run_inline_auto_compact_task(
                Arc::clone(self),
                Arc::clone(&step),
                world_state,
                cancellation_token.child_token(),
            )
            .await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_requires_selected_settled_fresh_notes_and_is_opt_in() {
        let checkpoint = NotesCheckpoint {
            thread_id: Some(codex_protocol::ThreadId::new()),
            window_id: "window".into(),
            settled_at_ms: 100,
            fresh: true,
        };
        let minutes = NonZeroU64::new(25);
        assert!(!idle_rollover_due(
            Some(&checkpoint),
            "window",
            None,
            1_500_100
        ));
        assert!(!idle_rollover_due(
            Some(&checkpoint),
            "window",
            minutes,
            1_500_099
        ));
        assert!(idle_rollover_due(
            Some(&checkpoint),
            "window",
            minutes,
            1_500_100
        ));
        assert!(!idle_rollover_due(
            Some(&checkpoint),
            "other",
            minutes,
            1_500_100
        ));
        assert!(!idle_rollover_due(None, "window", minutes, 1_500_100));
        assert!(!idle_rollover_due(
            Some(&NotesCheckpoint {
                fresh: false,
                ..checkpoint
            }),
            "window",
            minutes,
            1_500_100
        ));
    }
}
