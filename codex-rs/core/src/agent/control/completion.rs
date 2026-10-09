//! Delivers terminal child results and completion activity to the agent tree.
//!
//! Sessions capture terminal state; the controller owns routing and completion delivery.
//! Delivery remains best effort; metrics distinguish queue acceptance from delivery failure.

use super::LocalAgentControl;
use super::residency::V2RuntimeGuard;
use crate::TurnStartOptions;
use crate::agent::api::AgentTurnOutcome;
use crate::agent_communication::AgentCommunicationContext;
use crate::agent_communication::AgentCommunicationKind;
use crate::session_prefix::format_guardian_interruption_message;
use crate::session_prefix::format_inter_agent_completion_message;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::AgentPath;
use codex_protocol::ThreadId;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::items::SubAgentActivityItem;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentActivityKind;
use codex_protocol::protocol::SubAgentSource;
use codex_rollout_trace::AgentResultTracePayload;
use codex_rollout_trace::ThreadTraceContext;
use std::sync::Arc;
use tracing::debug;

impl LocalAgentControl {
    /// Restore only an automatically evicted, unstopped parent. The terminal task owns this
    /// wait after releasing its active reservation. No detached retry or polling is needed.
    pub(super) async fn completion_parent_guard(
        &self,
        parent: ThreadId,
        state: &Arc<ThreadManagerState>,
    ) -> CodexResult<V2RuntimeGuard> {
        let mut activity = self.runtime.residency.watch_activity();
        loop {
            let gate = self
                .runtime
                .registry
                .runtime_gate(parent)
                .ok_or(CodexErr::ThreadNotFound(parent))?;
            let guard = tokio::select! {
                biased;
                _ = self.runtime.shutdown.cancelled() => {
                    return Err(CodexErr::InvalidRequest("agent runtime is shutting down".into()));
                }
                guard = gate.lock_owned() => guard,
            };
            if state.get_thread(parent).await.is_err()
                && let Some(config) = self.runtime.registry.evicted_completion_config(parent)
            {
                match self
                    .ensure_v2_agent_loaded_under_gate((*config).clone(), parent, None)
                    .await
                {
                    Err(err)
                        if matches!(err.details(), CodexErrorDetails::AgentLimitReached { .. }) =>
                    {
                        // Release the gate so an explicit Stop or close can revoke eligibility.
                        drop(guard);
                        tokio::select! {
                            _ = self.runtime.shutdown.cancelled() => {
                                return Err(CodexErr::InvalidRequest("agent runtime is shutting down".into()));
                            }
                            update = activity.changed() => {
                                update.map_err(|_| CodexErr::InternalAgentDied)?;
                            }
                        }
                        continue;
                    }
                    result => result?,
                }
            }
            return Ok(self.runtime.track_runtime_guard(guard));
        }
    }

    /// Routes a captured terminal outcome without retaining the child's live turn context.
    pub(crate) async fn notify_parent_of_terminal_turn(
        &self,
        outcome: AgentTurnOutcome,
        trace: &ThreadTraceContext,
    ) {
        let SessionSource::SubAgent(SubAgentSource::ThreadSpawn {
            parent_thread_id,
            agent_path: Some(child_agent_path),
            ..
        }) = &outcome.source
        else {
            return;
        };
        let parent_thread_id = *parent_thread_id;
        let status = outcome.status;
        let Some(parent_agent_path) = child_agent_path
            .as_str()
            .rsplit_once('/')
            .and_then(|(parent, _)| AgentPath::try_from(parent).ok())
        else {
            return;
        };

        if matches!(status, AgentStatus::Completed(_))
            && let Some(parent_turn_id) = outcome.parent_turn_id
        {
            let initiating_thread_id = match outcome.initiating_agent_path.as_ref() {
                Some(initiating_agent_path) if initiating_agent_path != &parent_agent_path => {
                    self.runtime.resolve_agent_reference(
                        outcome.thread_id,
                        &outcome.source,
                        initiating_agent_path.as_str(),
                    )
                    .await
                    .inspect_err(|err| {
                        debug!(
                            "failed to resolve completed activity initiator {initiating_agent_path}: {err}"
                        );
                    })
                    .ok()
                }
                _ => Some(parent_thread_id),
            };
            if let Some(initiating_thread_id) = initiating_thread_id
                && let Err(err) = self
                    .emit_sub_agent_activity(
                        initiating_thread_id,
                        parent_turn_id,
                        SubAgentActivityItem {
                            model: None,
                            reasoning_effort: None,
                            id: format!("subagent-completed-{}", outcome.turn_id),
                            kind: SubAgentActivityKind::Completed,
                            agent_thread_id: outcome.thread_id,
                            agent_path: child_agent_path.clone(),
                        },
                    )
                    .await
            {
                debug!(
                    "failed to emit completed activity to initiating thread {initiating_thread_id}: {err}"
                );
            }
        }

        let message = match (&status, outcome.error_info) {
            (AgentStatus::Errored(error), Some(CodexErrorInfo::TooManyDenials)) => {
                Some(format_guardian_interruption_message(
                    parent_agent_path.clone(),
                    child_agent_path.clone(),
                    error,
                ))
            }
            _ => format_inter_agent_completion_message(
                parent_agent_path.clone(),
                child_agent_path.clone(),
                &status,
            ),
        };
        let Some(message) = message else {
            return;
        };
        // `communication` owns the message. Keep a second copy only when the
        // recorder will actually need it after parent delivery succeeds.
        let trace_message = trace.is_enabled().then(|| message.clone());
        let communication = InterAgentCommunication::new(
            child_agent_path.clone(),
            parent_agent_path,
            Vec::new(),
            message,
            /*trigger_turn*/ false,
        );
        let context =
            AgentCommunicationContext::new(AgentCommunicationKind::Result, outcome.thread_id);
        let delivery = self
            .send_inter_agent_communication(
                parent_thread_id,
                communication,
                context,
                TurnStartOptions {
                    resume_parent_on_completion: true,
                    ..Default::default()
                },
            )
            .await;
        if let Some(metrics) = codex_otel::global() {
            let _ = metrics.counter(
                "codex.multi_agent.result_delivery",
                /*inc*/ 1,
                &[(
                    "outcome",
                    if delivery.is_ok() { "queued" } else { "failed" },
                )],
            );
        }
        if let Err(err) = delivery {
            debug!("failed to notify parent thread {parent_thread_id}: {err}");
            return;
        }
        if let Some(message) = trace_message {
            trace.record_agent_result_interaction(
                outcome.turn_id.as_str(),
                parent_thread_id,
                &AgentResultTracePayload {
                    child_agent_path: child_agent_path.as_str(),
                    message: &message,
                    status: &status,
                },
            );
        }
    }
}
