//! Filesystem-based state storage for workmux agents.
//!
//! This module provides persistent state storage that works across all
//! terminal multiplexer backends (tmux, WezTerm, Zellij).

pub mod background;
pub mod run;
pub mod store;
#[cfg(test)]
pub(crate) mod test_support;
mod types;

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing::warn;

use crate::agent_identity::classify_agent_kind;
use crate::multiplexer::{AgentStatus, Multiplexer};
use background::{BackgroundItem, HookEvent};

pub use store::StateStore;
pub(crate) use store::{AgentStateCache, AgentStateSource, ResurrectionAgentState};
pub use types::{AgentState, LastDoneCycleState, PaneKey, RuntimeState};

/// How a state write changes the agent's latest user prompt.
#[derive(Debug, PartialEq, Eq)]
pub enum PromptUpdate {
    /// Preserve the stored prompt.
    Keep,
    /// Replace the stored prompt.
    Set(String),
    /// Remove the stored prompt.
    Clear,
}

/// A hook event to apply to the agent's background state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusEvent {
    pub event: HookEvent,
    /// The agent's transcript file, when the hook names one.
    pub transcript: Option<PathBuf>,
}

/// Status stored by a write, and whether the agent is left in the
/// background state (turn idle, background work still running).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResolvedStatus {
    pub status: AgentStatus,
    pub background: bool,
}

/// Persist an agent state update to the StateStore.
///
/// For tmux, merges state from the same pane process and server lifecycle so
/// partial updates don't wipe other fields. Other backends merge by pane key:
/// - If `status` is Some, updates the agent's status. If None, preserves existing.
/// - If `title_override` is Some, uses it. If None, preserves existing stored title,
///   falling back to the live pane title.
/// - If `agent_session_id` is Some, uses it. If None, preserves the existing binding.
/// - `prompt` keeps, replaces or removes the stored prompt.
/// - If `cwd` (the agent's reported directory) is Some, uses it. If None,
///   preserves the existing one.
/// - If `event` is Some, it is applied to the background state and the
///   resulting status replaces `status`. If None, `status` is stored as given
///   and the background state is preserved.
///
/// Returns the stored status, or `None` when nothing with a status was
/// persisted. Logs warnings on failure without propagating errors
/// (best-effort persistence).
#[allow(clippy::too_many_arguments)]
pub fn persist_agent_update(
    mux: &dyn Multiplexer,
    pane_id: &str,
    status: Option<AgentStatus>,
    title_override: Option<String>,
    agent_session_id: Option<String>,
    prompt: PromptUpdate,
    cwd: Option<PathBuf>,
    event: Option<StatusEvent>,
) -> Option<ResolvedStatus> {
    let has_event = event.is_some();
    let resolved = persist_agent_snapshot(
        mux,
        pane_id,
        status,
        title_override,
        agent_session_id,
        prompt,
        cwd,
        event,
        true,
    );
    if should_wake(status, has_event, resolved) && mux.name() == "tmux" {
        let tmux = crate::multiplexer::TmuxBackend::for_socket(&mux.instance_id());
        if let Err(error) = tmux.wake_agent(pane_id) {
            warn!(%pane_id, %error, "failed to clear agent sleep state");
        }
    }
    resolved
}

/// Whether a status write clears the pane's manual sleep flag. An event
/// decides the status itself, and an agent left in the background state is
/// idle, so it does not wake a pane the user put to sleep. Without an event,
/// or when nothing was stored, the passed status decides.
fn should_wake(
    status: Option<AgentStatus>,
    has_event: bool,
    resolved: Option<ResolvedStatus>,
) -> bool {
    match resolved {
        Some(resolved) if has_event => {
            resolved.status == AgentStatus::Working && !resolved.background
        }
        _ => status == Some(AgentStatus::Working),
    }
}

/// Register a live agent pane without assigning it an activity status.
///
/// Registration snapshots only live pane data, preventing state from an
/// unrelated agent process in the same pane from leaking into this record. A
/// hook-provided session ID binds later pane-less events to this registration.
pub fn persist_agent_registration(
    mux: &dyn Multiplexer,
    pane_id: &str,
    agent_session_id: Option<String>,
    prompt: PromptUpdate,
    cwd: Option<PathBuf>,
) {
    persist_agent_snapshot(
        mux,
        pane_id,
        None,
        None,
        agent_session_id,
        prompt,
        cwd,
        None,
        false,
    );
}

/// Clear an agent's persisted status without deleting its state record.
///
/// An explicit clear is not a status transition, so the pane identity and
/// metadata (PID, command, title, agent session, kind) are preserved. Only the
/// status fields are reset for the exact pane key, so other panes and other
/// multiplexer instances keep their stored status.
///
/// `clear_prompt` also removes the stored prompt.
///
/// Logs warnings on failure without propagating errors (best-effort
/// persistence).
pub fn clear_agent_status(mux: &dyn Multiplexer, pane_id: &str, clear_prompt: bool) {
    let Ok(store) = StateStore::new() else {
        return;
    };
    let pane_key = PaneKey {
        backend: mux.name().to_string(),
        instance: mux.instance_id(),
        pane_id: pane_id.to_string(),
    };
    if let Err(error) = store.clear_agent_status(&pane_key, clear_prompt) {
        warn!(%error, "failed to persist cleared agent status");
    }
}

#[allow(clippy::too_many_arguments)]
fn persist_agent_snapshot(
    mux: &dyn Multiplexer,
    pane_id: &str,
    status: Option<AgentStatus>,
    title_override: Option<String>,
    agent_session_id: Option<String>,
    prompt: PromptUpdate,
    cwd: Option<PathBuf>,
    event: Option<StatusEvent>,
    preserve_existing: bool,
) -> Option<ResolvedStatus> {
    let Ok(store) = StateStore::new() else {
        return None;
    };
    match store.with_agent_lock(|store| {
        Ok(persist_agent_snapshot_locked(
            store,
            mux,
            pane_id,
            status,
            title_override,
            agent_session_id,
            prompt,
            cwd,
            event,
            preserve_existing,
        ))
    }) {
        Ok(resolved) => resolved,
        Err(error) => {
            warn!(%error, "failed to lock agent state persistence");
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn persist_agent_snapshot_locked(
    store: &StateStore,
    mux: &dyn Multiplexer,
    pane_id: &str,
    status: Option<AgentStatus>,
    title_override: Option<String>,
    agent_session_id: Option<String>,
    prompt: PromptUpdate,
    cwd: Option<PathBuf>,
    event: Option<StatusEvent>,
    preserve_existing: bool,
) -> Option<ResolvedStatus> {
    let pane_key = PaneKey {
        backend: mux.name().to_string(),
        instance: mux.instance_id(),
        pane_id: pane_id.to_string(),
    };

    let live_info = match mux.get_live_pane_info(pane_id) {
        Ok(Some(info)) => info,
        Ok(None) => {
            warn!(%pane_id, "pane not found, skipping state persist");
            return None;
        }
        Err(e) => {
            warn!(error = %e, "failed to get live pane info, skipping state persist");
            return None;
        }
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let (live_pid, boot_id) = if mux.name() == "tmux" {
        let Some(live_pid) = live_info.pid else {
            warn!(%pane_id, "tmux pane PID unavailable, skipping state persist");
            return None;
        };
        let boot_id = match mux.server_boot_id() {
            Ok(Some(boot_id)) => Some(boot_id),
            Ok(None) => {
                warn!(%pane_id, "tmux server identity unavailable, skipping state persist");
                return None;
            }
            Err(error) => {
                warn!(%pane_id, %error, "failed to get tmux server identity, skipping state persist");
                return None;
            }
        };
        (live_pid, boot_id)
    } else {
        (
            live_info.pid.unwrap_or(0),
            mux.server_boot_id().unwrap_or(None),
        )
    };

    if let Some(boot_id) = boot_id.as_deref()
        && let Err(error) = store.compact_context_locked(mux.name(), &mux.instance_id(), boot_id)
    {
        warn!(%error, "failed to compact historical agent state");
        return None;
    }

    // tmux pane PIDs and server lifecycles form a stable process identity.
    // Other backends retain their established pane-key merge behavior.
    let existing = preserve_existing
        .then(|| {
            store.get_agent(&pane_key).ok().flatten().filter(|state| {
                mux.name() != "tmux"
                    || (state.pane_pid == live_pid
                        && state
                            .boot_id
                            .as_deref()
                            .zip(boot_id.as_deref())
                            .is_some_and(|(stored, current)| {
                                store::same_server_boot("tmux", stored, current)
                            }))
            })
        })
        .flatten();

    // Apply the hook event, if any: its status replaces the explicit one.
    let (status, turn_active, background) =
        resolve_background(existing.as_ref(), status, event.as_ref(), now);

    // Resolve status: explicit update wins, otherwise preserve existing
    let final_status = status.or(existing.as_ref().and_then(|e| e.status));

    let previous_status = existing.as_ref().and_then(|state| state.status);

    // Preserve existing status_ts if status hasn't changed (avoids resetting timer)
    let status_ts = match final_status {
        None => None,
        Some(_) if final_status == previous_status => {
            Some(existing.as_ref().and_then(|e| e.status_ts).unwrap_or(now))
        }
        Some(_) => Some(now),
    };

    let previous_activity_ts = existing.as_ref().and_then(AgentState::activity_ts);
    let activity_ts = resolve_activity_ts(
        preserve_existing,
        status,
        previous_status,
        previous_activity_ts,
        now,
    );

    // Capture existing agent_kind before `existing` is consumed below.
    let existing_agent_kind = existing.as_ref().and_then(|e| e.agent_kind.clone());
    let agent_session_id = agent_session_id.or_else(|| {
        existing
            .as_ref()
            .and_then(|state| state.agent_session_id.clone())
    });

    let prompt = resolve_prompt(prompt, existing.as_ref());
    let cwd = resolve_cwd(cwd, existing.as_ref());

    // Snapshot the live title for classification before the resolved
    // `pane_title` consumes `live_info.title`.
    let live_title_for_classify = live_info.title.clone();

    // Resolve title: explicit override wins, then existing stored title, then live
    let pane_title = title_override
        .or_else(|| existing.as_ref().and_then(|state| state.pane_title.clone()))
        .or(live_info.title);

    // Classify the agent kind once and lock it in. The classifier sees the
    // *live* title (not the merged `pane_title` above, which prefers the
    // stored value): a stale stored title would otherwise re-confirm the
    // previous identity even after the foreground command has changed.
    // Reconcile clears entries when it observes a foreground command change.
    // This path does not use the command as process identity because a status
    // hook can temporarily become the pane's foreground command.
    let agent_kind = merge_agent_kind(
        classify_agent_kind(
            live_info.current_command.as_deref(),
            live_title_for_classify.as_deref(),
        ),
        existing_agent_kind,
    );

    let state = AgentState {
        pane_key,
        workdir: live_info.working_dir,
        cwd,
        status: final_status,
        status_ts,
        activity_ts,
        pane_title,
        pane_pid: live_pid,
        command: live_info.current_command.unwrap_or_default(),
        updated_ts: now,
        window_name: live_info.window,
        session_name: live_info.session,
        boot_id,
        agent_kind,
        agent_session_id,
        prompt,
        turn_active,
        background,
    };

    if let Err(error) = store.upsert_agent_locked(&state) {
        warn!(%error, "failed to persist agent state");
        return None;
    }
    state.status.map(|status| ResolvedStatus {
        status,
        background: state.in_background(now),
    })
}

/// Background fields to store, and the status that results from the hook
/// event. Without an event, `status` and the background fields of the same
/// agent process are kept as they are. With one, the event is applied to
/// those fields (empty for a new record) and decides the status.
fn resolve_background(
    existing: Option<&AgentState>,
    status: Option<AgentStatus>,
    event: Option<&StatusEvent>,
    now: u64,
) -> (Option<AgentStatus>, bool, Vec<BackgroundItem>) {
    let mut turn_active = existing.is_some_and(|state| state.turn_active);
    let mut items = existing
        .map(|state| state.background.clone())
        .unwrap_or_default();
    let Some(event) = event else {
        return (status, turn_active, items);
    };
    let status = background::transition(
        &mut turn_active,
        &mut items,
        &event.event,
        existing.and_then(|state| state.status),
        event.transcript.as_deref(),
        now,
    );
    (Some(status), turn_active, items)
}

/// `existing` is already limited to the same agent process, so a kept prompt
/// never crosses into a new agent in a reused pane.
fn resolve_prompt(update: PromptUpdate, existing: Option<&AgentState>) -> Option<String> {
    match update {
        PromptUpdate::Keep => existing.and_then(|state| state.prompt.clone()),
        PromptUpdate::Set(prompt) => Some(prompt),
        PromptUpdate::Clear => None,
    }
}

/// Resolve the agent's current directory. Hooks without one (other agents,
/// sandbox RPC) keep the last reported directory of the same agent process.
fn resolve_cwd(reported: Option<PathBuf>, existing: Option<&AgentState>) -> Option<PathBuf> {
    reported.or_else(|| existing.and_then(|state| state.cwd.clone()))
}

fn resolve_activity_ts(
    preserve_existing: bool,
    explicit_status: Option<AgentStatus>,
    previous_status: Option<AgentStatus>,
    previous_activity_ts: Option<u64>,
    now: u64,
) -> Option<u64> {
    if !preserve_existing
        || previous_activity_ts.is_none()
        || (explicit_status.is_some() && explicit_status != previous_status)
    {
        Some(now)
    } else {
        previous_activity_ts
    }
}

/// Merge a freshly classified agent kind with the previously cached one.
///
/// Locks in the first definitive answer: once `existing` is `Some(_)`, that
/// value is preserved. This guards against title drift (a non-agent process
/// printing a substring like "Vibe" or "◇" into the pane title and stealing
/// the cached identity). Pane reuse is handled separately by reconcile,
/// which removes the stored entry when `pane_current_command` changes.
fn merge_agent_kind(new: Option<String>, existing: Option<String>) -> Option<String> {
    existing.or(new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registration_starts_activity_without_status() {
        assert_eq!(
            resolve_activity_ts(false, None, Some(AgentStatus::Done), Some(10), 20),
            Some(20)
        );
    }

    #[test]
    fn first_status_starts_new_activity() {
        assert_eq!(
            resolve_activity_ts(true, Some(AgentStatus::Working), None, Some(10), 20),
            Some(20)
        );
    }

    #[test]
    fn status_transition_starts_new_activity() {
        assert_eq!(
            resolve_activity_ts(
                true,
                Some(AgentStatus::Done),
                Some(AgentStatus::Working),
                Some(10),
                20,
            ),
            Some(20)
        );
    }

    #[test]
    fn repeated_status_preserves_activity() {
        assert_eq!(
            resolve_activity_ts(
                true,
                Some(AgentStatus::Working),
                Some(AgentStatus::Working),
                Some(10),
                20,
            ),
            Some(10)
        );
    }

    #[test]
    fn metadata_update_preserves_activity() {
        assert_eq!(
            resolve_activity_ts(true, None, None, Some(10), 20),
            Some(10)
        );
    }

    #[test]
    fn reported_cwd_wins_and_is_kept_by_writes_without_one() {
        let mut existing = state_with_prompt("p");
        existing.cwd = Some("/ws/repo/feature".into());
        assert_eq!(
            resolve_cwd(Some("/ws/other".into()), Some(&existing)),
            Some(std::path::PathBuf::from("/ws/other"))
        );
        assert_eq!(
            resolve_cwd(None, Some(&existing)),
            Some(std::path::PathBuf::from("/ws/repo/feature"))
        );
        assert_eq!(resolve_cwd(None, None), None);
    }

    fn state_with_prompt(prompt: &str) -> AgentState {
        serde_json::from_value(serde_json::json!({
            "pane_key": {"backend": "tmux", "instance": "default", "pane_id": "%1"},
            "workdir": "/repo",
            "status": "working",
            "status_ts": 1,
            "pane_title": null,
            "pane_pid": 1,
            "command": "claude",
            "updated_ts": 1,
            "prompt": prompt,
        }))
        .unwrap()
    }

    #[test]
    fn prompt_survives_prompt_less_writes() {
        let existing = state_with_prompt("fix the bug");
        assert_eq!(
            resolve_prompt(PromptUpdate::Keep, Some(&existing)),
            Some("fix the bug".to_string())
        );
        assert_eq!(
            resolve_prompt(PromptUpdate::Set("next".into()), Some(&existing)),
            Some("next".to_string())
        );
        assert_eq!(resolve_prompt(PromptUpdate::Clear, Some(&existing)), None);
    }

    #[test]
    fn prompt_is_not_inherited_without_matching_state() {
        // Registration and pane reuse pass no existing state.
        assert_eq!(resolve_prompt(PromptUpdate::Keep, None), None);
    }

    fn live_item(id: &str) -> BackgroundItem {
        BackgroundItem {
            id: id.into(),
            kind: background::BackgroundKind::Subagent,
            started_ts: 10,
            expires_ts: 5000,
            transcript_offset: None,
        }
    }

    #[test]
    fn no_event_keeps_status_and_background_fields() {
        let mut existing = state_with_prompt("p");
        existing.turn_active = false;
        existing.background = vec![live_item("s1")];
        assert_eq!(
            resolve_background(Some(&existing), Some(AgentStatus::Done), None, 1000),
            (Some(AgentStatus::Done), false, vec![live_item("s1")])
        );
        // An active turn must survive an event-less write as well.
        existing.turn_active = true;
        assert_eq!(
            resolve_background(Some(&existing), Some(AgentStatus::Working), None, 1000),
            (Some(AgentStatus::Working), true, vec![live_item("s1")])
        );
    }

    #[test]
    fn wake_follows_the_resolved_status_of_an_event() {
        let resolved = |status, background| Some(ResolvedStatus { status, background });
        let working = Some(AgentStatus::Working);
        // No event: the passed status decides, as before.
        assert!(should_wake(
            working,
            false,
            resolved(AgentStatus::Working, true)
        ));
        assert!(should_wake(working, false, None));
        assert!(!should_wake(Some(AgentStatus::Done), false, None));
        // Event: the resolved status decides; background work is idle.
        assert!(should_wake(
            working,
            true,
            resolved(AgentStatus::Working, false)
        ));
        assert!(!should_wake(
            working,
            true,
            resolved(AgentStatus::Working, true)
        ));
        assert!(!should_wake(
            working,
            true,
            resolved(AgentStatus::Done, false)
        ));
        assert!(!should_wake(
            working,
            true,
            resolved(AgentStatus::Waiting, false)
        ));
        // Event without a stored record: fall back to the passed status.
        assert!(should_wake(working, true, None));
    }

    #[test]
    fn event_overrides_status_from_transition() {
        let mut existing = state_with_prompt("p");
        existing.status = Some(AgentStatus::Working);
        existing.turn_active = true;
        existing.background = vec![live_item("s1")];
        let event = StatusEvent {
            event: background::HookEvent::Stop,
            transcript: None,
        };
        assert_eq!(
            resolve_background(Some(&existing), Some(AgentStatus::Done), Some(&event), 1000),
            (Some(AgentStatus::Working), false, vec![live_item("s1")])
        );
    }

    #[test]
    fn event_without_same_process_record_starts_empty() {
        let event = StatusEvent {
            event: background::HookEvent::SubagentStart {
                agent_id: "s1".into(),
            },
            transcript: None,
        };
        let (status, turn_active, items) =
            resolve_background(None, Some(AgentStatus::Working), Some(&event), 1000);
        assert_eq!(status, Some(AgentStatus::Working));
        assert!(!turn_active);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "s1");
    }

    #[test]
    fn merge_keeps_existing_when_new_is_none() {
        let merged = merge_agent_kind(None, Some("claude".into()));
        assert_eq!(merged, Some("claude".into()));
    }

    #[test]
    fn merge_preserves_existing_against_drift() {
        // Existing was correctly classified; a later tick whose title drifted
        // into another agent's fingerprint must not overwrite it.
        let merged = merge_agent_kind(Some("vibe".into()), Some("claude".into()));
        assert_eq!(merged, Some("claude".into()));
    }

    #[test]
    fn merge_returns_none_when_both_none() {
        assert_eq!(merge_agent_kind(None, None), None);
    }

    #[test]
    fn merge_classifies_when_existing_is_none() {
        let merged = merge_agent_kind(Some("claude".into()), None);
        assert_eq!(merged, Some("claude".into()));
    }
}
