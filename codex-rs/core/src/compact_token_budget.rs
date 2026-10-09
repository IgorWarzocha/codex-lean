use std::sync::Arc;

use crate::context::ContextualUserFragment;
use crate::context::DeveloperInstructions;
use crate::context::world_state::WorldState;
use crate::hook_runtime::PostCompactHookOutcome;
use crate::hook_runtime::PreCompactHookOutcome;
use crate::hook_runtime::run_post_compact_hooks;
use crate::hook_runtime::run_pre_compact_hooks;
use crate::session::TurnInput;
use crate::session::session::Session;
use crate::session::step_context::StepContext;
use crate::session::turn_context::TurnContext;
use crate::tasks::RegularTask;
use crate::tasks::SessionTask;
use codex_analytics::CompactionTrigger;
use codex_async_utils::OrCancelExt;
use codex_history::ResponseItemEnvelope;
use codex_protocol::error::CodexErr;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::ContextCompactionItem;
use codex_protocol::items::TurnItem;
use tokio_util::sync::CancellationToken;

/// Notes-only manual compact reuses a successful settled checkpoint or samples one
/// before invoking compact hooks and installing a fresh window.
pub(crate) async fn run_manual_compact_task(
    sess: Arc<Session>,
    turn_context: Arc<TurnContext>,
    cancellation_token: CancellationToken,
) -> CodexResult<()> {
    if !sess.has_fresh_notes().await {
        let source_window = sess.current_window().await.2;
        let mut reminder = ContextualUserFragment::into(DeveloperInstructions::new(
            "Checkpoint the active request, decisions, progress, useful findings and next steps in notes. Then reply briefly.",
        ));
        reminder.set_turn_id_if_missing(&turn_context.sub_id);
        let input = vec![TurnInput::ResponseItem(ResponseItemEnvelope::new(reminder))];
        // Use native regular preparation and pending-input settlement, not a separate
        // model loop that bypasses lifecycle contributors or attachment preparation.
        let reply = Arc::new(RegularTask::new())
            .run(
                Arc::clone(&sess),
                Arc::clone(&turn_context),
                input,
                cancellation_token.child_token(),
            )
            .await?;
        if cancellation_token.is_cancelled() {
            return Err(CodexErr::TurnAborted);
        }
        if sess.current_window().await.2 != source_window {
            // The agent's native new_context already performed its distinct reset.
            // Never turn that into a second manual reset or reuse pre-reset writes.
            return Ok(());
        }
        sess.settle_manual_notes(&turn_context, reply).await?;
        if !sess.has_fresh_notes().await {
            return Err(CodexErr::InvalidRequest("Notes checkpoint did not complete successfully. Context was not reset; save notes and retry compact.".to_owned()));
        }
    } else {
        sess.emit_turn_started(&turn_context, crate::state::TaskKind::Compact)
            .await;
    }

    // Manual compaction runs outside run_turn, so it captures its own current step.
    let step_context = sess
        .capture_step_context(Arc::clone(&turn_context), &cancellation_token)
        .await?;
    let world_state = Arc::new(
        sess.build_world_state_for_step(&step_context, /*new_window*/ true)
            .await?,
    );
    run_compact_task_inner(
        &sess,
        &step_context,
        world_state,
        CompactionTrigger::Manual,
        &cancellation_token,
    )
    .await
}

/// Runs token-budget inline auto-compaction as a normal compaction lifecycle.
///
/// Token-budget compaction skips model/server summarization and installs a fresh context window
/// instead. It is still modeled as compaction so compact hooks and `ContextCompaction` turn items
/// observe the same lifecycle as local or remote compaction.
pub(crate) async fn run_inline_auto_compact_task(
    sess: Arc<Session>,
    step_context: Arc<StepContext>,
    world_state: Arc<WorldState>,
    cancellation_token: CancellationToken,
) -> CodexResult<()> {
    run_compact_task_inner(
        &sess,
        &step_context,
        world_state,
        CompactionTrigger::Auto,
        &cancellation_token,
    )
    .await
}

async fn run_compact_task_inner(
    sess: &Arc<Session>,
    step_context: &Arc<StepContext>,
    world_state: Arc<WorldState>,
    trigger: CompactionTrigger,
    cancellation_token: &CancellationToken,
) -> CodexResult<()> {
    let turn_context = &step_context.turn;
    let pre_compact_outcome = run_pre_compact_hooks(sess, turn_context, trigger)
        .or_cancel(cancellation_token)
        .await?;
    match pre_compact_outcome {
        PreCompactHookOutcome::Continue => {}
        PreCompactHookOutcome::Stopped => return Err(CodexErr::TurnAborted),
    }
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }

    let compaction_item = TurnItem::ContextCompaction(ContextCompactionItem::new());
    sess.emit_turn_item_started(turn_context, &compaction_item)
        .await;
    let prepared = sess
        .prepare_new_context_window(step_context, world_state)
        .or_cancel(cancellation_token)
        .await?;
    if cancellation_token.is_cancelled() {
        return Err(CodexErr::TurnAborted);
    }
    // Preparation may wait on remote notes hints. Only the fully prepared history
    // crosses the window transition, which is not cancelled halfway through.
    sess.install_new_context_window(step_context, prepared)
        .await;
    sess.begin_notes_run(turn_context).await;
    sess.emit_turn_item_completed(turn_context, compaction_item)
        .await;

    let post_compact_outcome = run_post_compact_hooks(sess, turn_context, trigger)
        .or_cancel(cancellation_token)
        .await?;
    if let PostCompactHookOutcome::Stopped = post_compact_outcome {
        return Err(CodexErr::TurnAborted);
    }

    Ok(())
}
