use super::LocalAgentControl;
use super::LocalAgentRuntime;
use super::runtime::AgentTreeMembership;
use crate::agent::AgentStatus;
use crate::codex_thread::CodexThread;
use crate::config::Config;
use crate::session::PendingMailboxCommunication;
use crate::thread_manager::ThreadManagerState;
use codex_protocol::ThreadId;
use codex_protocol::error::AgentErrorContext;
use codex_protocol::error::CodexErr;
use codex_protocol::error::CodexErrKind;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::error::Result as CodexResult;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::Mutex;
use tracing::warn;

#[derive(Default)]
pub(super) struct V2Residency {
    state: Mutex<V2ResidencyState>,
    activity: tokio::sync::watch::Sender<()>,
}

#[derive(Default)]
struct V2ResidencyState {
    residents: VecDeque<ThreadId>,
    pending_slots: usize,
}

pub(super) struct V2ResidencySlot {
    residency: Arc<V2Residency>,
    active: bool,
}

/// A capacity waiter may skip an idle runtime while this gate is held. Notify after
/// unlocking even if startup or delivery is cancelled, not just when slots change.
pub(super) struct V2RuntimeGuard {
    gate: Option<tokio::sync::OwnedMutexGuard<()>>,
    residency: Arc<V2Residency>,
}

impl Drop for V2RuntimeGuard {
    fn drop(&mut self) {
        drop(self.gate.take());
        self.residency.activity.send_replace(());
    }
}

/// Submissions transfer this pin to the queue. Failed or cancelled senders must
/// wake capacity waiters just like a handler releasing the last resident pin.
pub(crate) struct V2ResidencyPin {
    gate: Option<tokio::sync::OwnedRwLockReadGuard<()>>,
    residency: Arc<V2Residency>,
}

impl std::fmt::Debug for V2ResidencyPin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("V2ResidencyPin").finish_non_exhaustive()
    }
}

impl Drop for V2ResidencyPin {
    fn drop(&mut self) {
        drop(self.gate.take());
        self.residency.activity.send_replace(());
    }
}

impl V2ResidencySlot {
    pub(super) fn commit(mut self, thread_id: ThreadId) {
        self.residency.commit_slot(thread_id);
        self.active = false;
    }
}

impl Drop for V2ResidencySlot {
    fn drop(&mut self) {
        if self.active {
            self.residency.release_pending_slot();
        }
    }
}

impl LocalAgentControl {
    pub(super) async fn reserve_v2_residency_slot(
        &self,
        state: &Arc<ThreadManagerState>,
        config: &Config,
        membership: &AgentTreeMembership,
        protected_thread_id: Option<ThreadId>,
    ) -> CodexResult<V2ResidencySlot> {
        let capacity = config
            .effective_agent_max_threads(MultiAgentVersion::V2)
            .unwrap_or(usize::MAX);
        Arc::clone(&self.runtime.residency)
            .reserve_slot(state, capacity, membership, protected_thread_id)
            .await
    }

    pub(super) async fn touch_loaded_v2_residency(
        &self,
        state: &Arc<ThreadManagerState>,
        thread_id: ThreadId,
    ) {
        if let Ok(thread) = state.get_thread(thread_id).await {
            let _ = self.runtime.pin_v2_residency(state, &thread).await;
        }
    }

    pub(super) fn forget_v2_residency(&self, thread_id: ThreadId) {
        self.runtime.residency.remove(thread_id);
    }
}

impl LocalAgentRuntime {
    pub(super) fn track_runtime_guard(
        &self,
        gate: tokio::sync::OwnedMutexGuard<()>,
    ) -> V2RuntimeGuard {
        V2RuntimeGuard {
            gate: Some(gate),
            residency: Arc::clone(&self.residency),
        }
    }

    pub(crate) fn notify_residency_activity(&self) {
        self.residency.activity.send_replace(());
    }
    /// Pins and touches the registered runtime without waiting for unrelated eviction.
    pub(crate) async fn pin_v2_residency(
        &self,
        state: &ThreadManagerState,
        thread: &Arc<CodexThread>,
    ) -> CodexResult<Option<V2ResidencyPin>> {
        if !is_resident_candidate(thread) {
            return Ok(None);
        }
        let guard = V2ResidencyPin {
            gate: Some(Arc::clone(&thread.residency_gate).read_owned().await),
            residency: Arc::clone(&self.residency),
        };
        let thread_id = thread.session.thread_id;
        if !Arc::ptr_eq(thread, &state.get_thread(thread_id).await?) {
            return Err(CodexErr::ThreadNotFound(thread_id));
        }
        self.residency.touch(thread_id);
        Ok(Some(guard))
    }
}

impl V2Residency {
    pub(super) fn watch_activity(&self) -> tokio::sync::watch::Receiver<()> {
        self.activity.subscribe()
    }
    async fn reserve_slot(
        self: Arc<Self>,
        manager: &Arc<ThreadManagerState>,
        capacity: usize,
        membership: &AgentTreeMembership,
        protected_thread_id: Option<ThreadId>,
    ) -> CodexResult<V2ResidencySlot> {
        loop {
            if self.try_reserve_pending_slot(capacity) {
                return Ok(V2ResidencySlot {
                    residency: self,
                    active: true,
                });
            }
            if !self
                .try_unload_one_resident(manager, membership, protected_thread_id)
                .await
            {
                return Err(CodexErr::new(CodexErrorDetails::AgentLimitReached {
                    max_threads: capacity,
                })
                .with_agent_context(AgentErrorContext::ResidencyCapacity));
            }
        }
    }

    fn try_reserve_pending_slot(&self, capacity: usize) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.residents.len().saturating_add(state.pending_slots) >= capacity {
            return false;
        }
        state.pending_slots += 1;
        true
    }

    async fn try_unload_one_resident(
        self: &Arc<Self>,
        manager: &Arc<ThreadManagerState>,
        membership: &AgentTreeMembership,
        protected_thread_id: Option<ThreadId>,
    ) -> bool {
        // Keep shutting-down workers counted until removal. Each runtime's write guard
        // excludes delivery and competing evictions without blocking unrelated workers.
        let candidates = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .residents
            .clone();
        for candidate_thread_id in candidates {
            if Some(candidate_thread_id) == protected_thread_id {
                continue;
            }
            let candidate_thread = {
                let threads = manager.threads.read().await;
                match threads.get(&candidate_thread_id) {
                    Some(thread) if is_resident_candidate(thread) => Arc::clone(thread),
                    Some(_) | None => {
                        // A reload cannot publish between the lookup and stale-entry removal.
                        self.remove(candidate_thread_id);
                        return true;
                    }
                }
            };
            let runtime_guard = if let Some(runtime_gate) = candidate_thread
                .session
                .services
                .local_agent_runtime
                .registry
                .runtime_gate(candidate_thread_id)
            {
                let Ok(guard) = runtime_gate.try_lock_owned() else {
                    continue;
                };
                Some(guard)
            } else {
                // Private delegates need no registered identity or automatic reload capability.
                None
            };
            let Ok(residency_guard) =
                Arc::clone(&candidate_thread.residency_gate).try_write_owned()
            else {
                continue;
            };
            if !manager
                .get_thread(candidate_thread_id)
                .await
                .is_ok_and(|registered| Arc::ptr_eq(&registered, &candidate_thread))
                || !is_unloadable(candidate_thread.as_ref()).await
            {
                continue;
            }
            // Once shutdown is submitted, cancellation cannot revoke it. The eviction task
            // must keep delivery excluded and capacity reserved through registry removal.
            let manager = Arc::clone(manager);
            let residency = Arc::clone(self);
            let teardown = membership
                .clone()
                .into_teardown_guard("resident_eviction", Some(candidate_thread_id));
            let eviction = tokio::spawn(async move {
                let _runtime_guard = runtime_guard;
                let _residency_guard = residency_guard;
                // Shutdown used for memory cleanup is not a user Stop. Capture eligibility
                // before it changes the live session's status and cancellation state.
                let completion_config = if matches!(
                    candidate_thread.agent_status().await,
                    AgentStatus::Completed(_)
                ) && candidate_thread
                    .session
                    .can_wake_for_pending_work(false, true)
                    .await
                {
                    Some(candidate_thread.config().await)
                } else {
                    None
                };
                candidate_thread.ensure_rollout_materialized().await;
                if let Err(err) = candidate_thread.shutdown_and_wait().await {
                    teardown
                        .record_shutdown_failure("stop_resident", CodexErrKind::from(&err).into());
                    warn!(
                        "failed to shut down v2 resident thread before unloading {candidate_thread_id}: {err}"
                    );
                    teardown.complete();
                    return false;
                }
                // The submission loop has stopped and the residency guard excludes senders.
                // Preserve unread queue-only mail before dropping the session that held it.
                let mail = candidate_thread
                    .session
                    .input_queue
                    .drain_mailbox()
                    .await
                    .into_iter()
                    .map(PendingMailboxCommunication::into_message)
                    .collect();
                // A concurrent tree shutdown deliberately discards its unread mail.
                let _ = candidate_thread
                    .session
                    .services
                    .local_agent_runtime
                    .mailboxes
                    .enqueue(candidate_thread_id, /*id*/ None, mail);
                let environments = candidate_thread.environment_selections().await;
                let mut threads = manager.threads.write().await;
                if threads
                    .get(&candidate_thread_id)
                    .is_some_and(|registered| !Arc::ptr_eq(registered, &candidate_thread))
                {
                    teardown.complete();
                    return false;
                }
                candidate_thread
                    .session
                    .services
                    .local_agent_runtime
                    .registry
                    .save_evicted_environments(
                        candidate_thread_id,
                        environments,
                        completion_config,
                    );
                // Keep publication excluded until both entries have been removed.
                threads.remove(&candidate_thread_id);
                residency.remove(candidate_thread_id);
                teardown.complete();
                true
            });
            match eviction.await {
                Ok(true) => return true,
                Ok(false) => {}
                Err(err) => warn!("v2 resident eviction task failed: {err}"),
            }
        }
        false
    }

    fn touch(&self, thread_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        touch_resident(&mut state.residents, thread_id);
    }

    fn remove(&self, thread_id: ThreadId) {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .residents
            .retain(|resident_thread_id| *resident_thread_id != thread_id);
        self.activity.send_replace(());
    }

    fn commit_slot(&self, thread_id: ThreadId) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
        touch_resident(&mut state.residents, thread_id);
        drop(state);
        self.activity.send_replace(());
    }

    fn release_pending_slot(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.pending_slots = state.pending_slots.saturating_sub(1);
        drop(state);
        self.activity.send_replace(());
    }
}

fn touch_resident(residents: &mut VecDeque<ThreadId>, thread_id: ThreadId) {
    residents.retain(|resident_thread_id| *resident_thread_id != thread_id);
    residents.push_back(thread_id);
}

fn is_resident_candidate(thread: &CodexThread) -> bool {
    thread.multi_agent_version() == Some(MultiAgentVersion::V2)
        && is_v2_resident_session_source(&thread.session_source)
}

pub(super) fn is_v2_resident_session_source(session_source: &SessionSource) -> bool {
    matches!(session_source, SessionSource::SubAgent(_))
}

async fn is_unloadable(thread: &CodexThread) -> bool {
    matches!(
        thread.agent_status().await,
        AgentStatus::Completed(_) | AgentStatus::Errored(_) | AgentStatus::Interrupted
    ) && thread.session.active_turn.lock().await.is_none()
        && !thread.session.has_outstanding_durable_sleep()
        && !thread
            .session
            .input_queue
            .has_trigger_turn_mailbox_items()
            .await
}

#[cfg(test)]
#[path = "residency_tests.rs"]
mod tests;
