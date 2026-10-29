//! Background work items tracked per agent.
//!
//! A background item is work an agent started and left running (a subagent,
//! a background shell, a monitor) while its own turn is idle. Items expire on
//! a deadline so a missed completion hook cannot pin an agent forever.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::multiplexer::AgentStatus;

/// Deadline extension for a subagent each time it reports activity.
pub const SUBAGENT_IDLE_SECS: u64 = 1800;
/// Grace period for a non-persistent monitor after its last observed output.
pub const MONITOR_GRACE_SECS: u64 = 120;
/// Deadline for a persistent monitor.
pub const PERSISTENT_MONITOR_SECS: u64 = 86400;
/// Deadline for a background shell.
pub const SHELL_SECS: u64 = 43200;
/// Most transcript bytes read by one scan, counted back from the end.
pub const TRANSCRIPT_SCAN_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// Transcript bytes before a launch that its first scan reads again.
pub const LAUNCH_RESCAN_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackgroundKind {
    Subagent,
    Shell,
    Monitor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundItem {
    pub id: String,
    pub kind: BackgroundKind,
    pub started_ts: u64,
    /// Unix timestamp after which the item no longer counts as live.
    pub expires_ts: u64,
    /// Next transcript byte to scan for this item's completion.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transcript_offset: Option<u64>,
}

/// Whether any item is still live at `now`.
pub fn has_live(items: &[BackgroundItem], now: u64) -> bool {
    items.iter().any(|item| item.expires_ts > now)
}

/// Drop items whose deadline has passed.
pub fn prune_expired(items: &mut Vec<BackgroundItem>, now: u64) {
    items.retain(|item| item.expires_ts > now);
}

/// Insert an item, or merge it into the tracked item with the same id.
///
/// A merge keeps the original start, takes the later deadline, and keeps the
/// stored transcript offset unless the new item carries one.
pub fn upsert(items: &mut Vec<BackgroundItem>, item: BackgroundItem) {
    match items.iter_mut().find(|existing| existing.id == item.id) {
        Some(existing) => {
            existing.expires_ts = existing.expires_ts.max(item.expires_ts);
            if item.transcript_offset.is_some() {
                existing.transcript_offset = item.transcript_offset;
            }
        }
        None => items.push(item),
    }
}

/// Remove the item with `id`. Returns whether it was tracked.
pub fn remove(items: &mut Vec<BackgroundItem>, id: &str) -> bool {
    let before = items.len();
    items.retain(|item| item.id != id);
    items.len() != before
}

/// A background work item started by a tool call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    pub id: String,
    pub kind: BackgroundKind,
    /// Requested timeout of a monitor, in seconds.
    pub timeout_secs: Option<u64>,
    pub persistent: bool,
}

/// An agent hook event, reduced to what status tracking needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    PromptSubmit {
        prompt: Option<String>,
    },
    ToolUse {
        /// Set when a subagent made the call.
        agent_id: Option<String>,
        launch: Option<Launch>,
        /// Task or shell id the agent asked to stop.
        stopped: Option<String>,
    },
    SubagentStart {
        agent_id: String,
    },
    SubagentStop {
        agent_id: String,
    },
    Waiting,
    Stop,
}

const NOTIFICATION_OPEN: &str = "<task-notification>";
const NOTIFICATION_CLOSE: &str = "</task-notification>";

/// Text between `<tag>` and `</tag>` in `block`, trimmed.
fn tag_value<'a>(block: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = block.find(&open)? + open.len();
    let end = block[start..].find(&close)? + start;
    Some(block[start..end].trim())
}

/// Ids of tasks that a `<task-notification>` block reports as finished.
///
/// A block counts when it carries a `<status>` other than `running`;
/// monitor event blocks have no status and are ignored. The text may be a
/// JSON-escaped transcript line, so common escapes are undone first.
pub fn terminal_task_ids(text: &str) -> Vec<String> {
    let text = if text.contains("\\n") || text.contains("\\\"") {
        text.replace("\\n", "\n").replace("\\\"", "\"")
    } else {
        text.to_string()
    };
    let mut ids = Vec::new();
    let mut rest = text.as_str();
    // Each closing tag pairs with the nearest opener before it, so a stray
    // opener (a literal in quoted code) cannot swallow the block after it.
    while let Some(end) = rest.find(NOTIFICATION_CLOSE) {
        if let Some(start) = rest[..end].rfind(NOTIFICATION_OPEN) {
            let block = &rest[start + NOTIFICATION_OPEN.len()..end];
            if let (Some(id), Some(status)) =
                (tag_value(block, "task-id"), tag_value(block, "status"))
                && !id.is_empty()
                && status != "running"
            {
                ids.push(id.to_string());
            }
        }
        rest = &rest[end + NOTIFICATION_CLOSE.len()..];
    }
    ids
}

/// Unix timestamp at which work started by `launch` at `now` expires.
fn launch_expiry(launch: &Launch, now: u64) -> u64 {
    match launch.kind {
        BackgroundKind::Subagent => now + SUBAGENT_IDLE_SECS,
        BackgroundKind::Shell => now + SHELL_SECS,
        BackgroundKind::Monitor if launch.persistent => now + PERSISTENT_MONITOR_SECS,
        BackgroundKind::Monitor => now + launch.timeout_secs.unwrap_or(0) + MONITOR_GRACE_SECS,
    }
}

/// Apply a hook event to an agent's background state.
///
/// Returns the status to store. `previous` is the status stored before the
/// event; `transcript` is the agent's transcript file when the hook names one.
/// Expired items are dropped first, then the transcript bytes appended since
/// the last scan are checked for completed tasks, then the event is applied.
pub fn transition(
    turn_active: &mut bool,
    items: &mut Vec<BackgroundItem>,
    event: &HookEvent,
    previous: Option<AgentStatus>,
    transcript: Option<&Path>,
    now: u64,
) -> AgentStatus {
    prune_expired(items, now);
    if let Some(path) = transcript
        && items.iter().any(|item| item.transcript_offset.is_some())
    {
        scan_transcript(items, path);
    }

    match event {
        HookEvent::SessionStart => {
            items.clear();
            *turn_active = false;
            AgentStatus::Done
        }
        HookEvent::PromptSubmit { prompt } => {
            if let Some(prompt) = prompt {
                for id in terminal_task_ids(prompt) {
                    remove(items, &id);
                }
            }
            *turn_active = true;
            AgentStatus::Working
        }
        HookEvent::ToolUse {
            agent_id: None,
            launch,
            stopped,
        } => {
            *turn_active = true;
            if let Some(launch) = launch {
                upsert(
                    items,
                    BackgroundItem {
                        id: launch.id.clone(),
                        kind: launch.kind,
                        started_ts: now,
                        expires_ts: launch_expiry(launch, now),
                        // Start a little before the end: the notification
                        // of a quick task can land before this hook runs.
                        // Ids are unique, so rescanning is harmless.
                        transcript_offset: transcript
                            .and_then(|path| std::fs::metadata(path).ok())
                            .map(|meta| meta.len().saturating_sub(LAUNCH_RESCAN_BYTES)),
                    },
                );
            }
            if let Some(id) = stopped {
                remove(items, id);
            }
            AgentStatus::Working
        }
        HookEvent::ToolUse {
            agent_id: Some(id), ..
        } => match items.iter_mut().find(|item| &item.id == id) {
            Some(item) => {
                item.expires_ts = item.expires_ts.max(now + SUBAGENT_IDLE_SECS);
                AgentStatus::Working
            }
            // A foreground subagent: never tracked, since it has no completion
            // event. After the turn it must not pin the agent in `Working`.
            None if *turn_active => AgentStatus::Working,
            None => previous.unwrap_or(AgentStatus::Done),
        },
        HookEvent::SubagentStart { agent_id } => {
            upsert(
                items,
                BackgroundItem {
                    id: agent_id.clone(),
                    kind: BackgroundKind::Subagent,
                    started_ts: now,
                    expires_ts: now + SUBAGENT_IDLE_SECS,
                    transcript_offset: None,
                },
            );
            AgentStatus::Working
        }
        HookEvent::SubagentStop { agent_id } => {
            if !remove(items, agent_id) {
                // Untracked: a Claude internal agent, or a Codex subagent
                // whose SubagentStart was missed. The latter must not leave
                // an idle agent working with nothing live.
                if !*turn_active && previous == Some(AgentStatus::Working) && !has_live(items, now)
                {
                    AgentStatus::Done
                } else {
                    previous.unwrap_or(AgentStatus::Done)
                }
            } else if *turn_active || has_live(items, now) {
                AgentStatus::Working
            } else {
                AgentStatus::Done
            }
        }
        HookEvent::Waiting => AgentStatus::Waiting,
        HookEvent::Stop => {
            *turn_active = false;
            if has_live(items, now) {
                AgentStatus::Working
            } else {
                AgentStatus::Done
            }
        }
    }
}

/// Remove items whose completion was written to the transcript since the
/// last scan, and advance every remaining offset past the last complete line.
///
/// Reads from the smallest stored offset, but never more than the last
/// [`TRANSCRIPT_SCAN_MAX_BYTES`]. A line still being written is left for the
/// next scan. Any I/O error, or no complete line, leaves the items untouched.
fn scan_transcript(items: &mut Vec<BackgroundItem>, transcript: &Path) {
    let Some(from) = items.iter().filter_map(|item| item.transcript_offset).min() else {
        return;
    };
    let Ok((end, text)) = read_tail(transcript, from) else {
        return;
    };
    if text.is_empty() {
        return;
    }
    for id in terminal_task_ids(&String::from_utf8_lossy(&text)) {
        remove(items, &id);
    }
    for item in items.iter_mut() {
        if item.transcript_offset.is_some() {
            item.transcript_offset = Some(end);
        }
    }
}

/// Complete lines of `path` from `from` (clamped to the scan window) up to
/// the last newline, together with the offset just past them. Bytes after the
/// last newline belong to a line still being written and are not returned.
fn read_tail(path: &Path, from: u64) -> std::io::Result<(u64, Vec<u8>)> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = from
        .max(len.saturating_sub(TRANSCRIPT_SCAN_MAX_BYTES))
        .min(len);
    file.seek(SeekFrom::Start(start))?;
    let mut text = Vec::new();
    file.take(len - start).read_to_end(&mut text)?;
    let complete = text.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
    text.truncate(complete);
    Ok((start + text.len() as u64, text))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, started_ts: u64, expires_ts: u64) -> BackgroundItem {
        BackgroundItem {
            id: id.into(),
            kind: BackgroundKind::Subagent,
            started_ts,
            expires_ts,
            transcript_offset: None,
        }
    }

    #[test]
    fn upsert_keeps_start_and_extends_expiry() {
        let mut items = Vec::new();
        upsert(&mut items, item("a", 10, 100));
        upsert(&mut items, item("a", 50, 200));
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].started_ts, 10);
        assert_eq!(items[0].expires_ts, 200);
    }

    #[test]
    fn upsert_keeps_offset_unless_replaced() {
        let mut items = Vec::new();
        let mut first = item("a", 10, 100);
        first.transcript_offset = Some(7);
        upsert(&mut items, first);
        upsert(&mut items, item("a", 20, 50));
        assert_eq!(items[0].transcript_offset, Some(7));
        assert_eq!(items[0].expires_ts, 100);
        let mut third = item("a", 30, 60);
        third.transcript_offset = Some(9);
        upsert(&mut items, third);
        assert_eq!(items[0].transcript_offset, Some(9));
    }

    #[test]
    fn expired_items_do_not_count() {
        let mut items = vec![item("a", 10, 100)];
        assert!(has_live(&items, 99));
        assert!(!has_live(&items, 100));
        prune_expired(&mut items, 100);
        assert!(items.is_empty());
    }

    #[test]
    fn remove_reports_whether_tracked() {
        let mut items = Vec::new();
        assert!(!remove(&mut items, "x"));
        upsert(&mut items, item("x", 1, 10));
        assert!(remove(&mut items, "x"));
        assert!(items.is_empty());
    }

    #[test]
    fn terminal_task_ids_pairs_id_with_status() {
        let text = "<task-notification>\n<task-id>b1</task-id>\n<tool-use-id>t</tool-use-id>\n<status>completed</status>\n<summary>done</summary>\n</task-notification>\nnoise\n<task-notification>\n<task-id>m1</task-id>\n<event>tick</event>\n</task-notification>";
        assert_eq!(terminal_task_ids(text), vec!["b1".to_string()]);

        let running = "<task-notification>\n<task-id>r1</task-id>\n<status>running</status>\n</task-notification>";
        assert!(terminal_task_ids(running).is_empty());

        let killed = "<task-notification>\n<task-id>k1</task-id>\n<status>killed</status>\n</task-notification>";
        assert_eq!(terminal_task_ids(killed), vec!["k1".to_string()]);
    }

    #[test]
    fn terminal_task_ids_reads_json_escaped_transcript() {
        let block = "<task-notification>\n<task-id>b1</task-id>\n<status>completed</status>\n<summary>\"x\"</summary>\n</task-notification>";
        let escaped = serde_json::to_string(block).unwrap();
        assert!(escaped.contains("\\n"));
        assert_eq!(terminal_task_ids(&escaped), vec!["b1".to_string()]);
    }

    #[test]
    fn terminal_task_ids_skips_stray_opener() {
        // Literals from a session editing this parser: an opener and a task id
        // that no closing tag follows before the real notifications.
        let stray = "let open = \"<task-notification>\"; tag_value(\"<task-id>x</task-id>\")\n";
        let text = format!(
            "{stray}{}\nnoise\n{}\ntrailing <task-notification>",
            notification_block("b1", "completed"),
            notification_block("b2", "failed"),
        );
        assert_eq!(
            terminal_task_ids(&text),
            vec!["b1".to_string(), "b2".to_string()]
        );
    }

    fn notification_block(id: &str, status: &str) -> String {
        format!(
            "<task-notification>\n<task-id>{id}</task-id>\n<status>{status}</status>\n</task-notification>"
        )
    }

    // --- transition and transcript scan ---

    use std::io::Write as _;

    const NOW: u64 = 1000;

    /// Agent state as the hook handler would hold it between events.
    #[derive(Default)]
    struct Agent {
        turn_active: bool,
        items: Vec<BackgroundItem>,
        status: Option<AgentStatus>,
    }

    impl Agent {
        fn apply(&mut self, event: HookEvent, transcript: Option<&Path>) -> AgentStatus {
            let status = transition(
                &mut self.turn_active,
                &mut self.items,
                &event,
                self.status,
                transcript,
                NOW,
            );
            self.status = Some(status);
            status
        }

        fn ids(&self) -> Vec<&str> {
            self.items.iter().map(|item| item.id.as_str()).collect()
        }
    }

    fn launch(id: &str, kind: BackgroundKind) -> Launch {
        Launch {
            id: id.into(),
            kind,
            timeout_secs: None,
            persistent: false,
        }
    }

    fn launched(id: &str, kind: BackgroundKind) -> HookEvent {
        HookEvent::ToolUse {
            agent_id: None,
            launch: Some(launch(id, kind)),
            stopped: None,
        }
    }

    fn subagent_tool_use(id: &str) -> HookEvent {
        HookEvent::ToolUse {
            agent_id: Some(id.into()),
            launch: None,
            stopped: None,
        }
    }

    fn notification(id: &str) -> String {
        format!(
            "<task-notification>\n<task-id>{id}</task-id>\n<status>completed</status>\n</task-notification>"
        )
    }

    fn prompt(text: Option<String>) -> HookEvent {
        HookEvent::PromptSubmit { prompt: text }
    }

    /// A transcript line carrying a queued notification, as Claude writes it.
    fn queued_line(id: &str) -> String {
        let line = serde_json::json!({
            "type": "attachment",
            "attachment": { "type": "queued_command", "prompt": notification(id) },
        });
        format!("{line}\n")
    }

    fn append(path: &Path, text: &str) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn transcript_with(text: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::NamedTempFile::new().unwrap();
        file.write_all(text.as_bytes()).unwrap();
        file.flush().unwrap();
        file
    }

    #[test]
    fn claude_background_turn_shows_background_then_done() {
        let mut agent = Agent::default();
        assert_eq!(
            agent.apply(prompt(Some("go".into())), None),
            AgentStatus::Working
        );
        agent.apply(launched("b1", BackgroundKind::Shell), None);
        agent.apply(launched("a1", BackgroundKind::Subagent), None);
        assert_eq!(agent.apply(HookEvent::Stop, None), AgentStatus::Working);
        assert!(!agent.turn_active);
        assert!(has_live(&agent.items, NOW));

        let status = agent.apply(prompt(Some(notification("a1"))), None);
        assert_eq!(status, AgentStatus::Working);
        assert!(agent.turn_active);
        assert_eq!(agent.ids(), vec!["b1"]);

        assert_eq!(agent.apply(HookEvent::Stop, None), AgentStatus::Working);
        assert!(!agent.turn_active);
        assert!(has_live(&agent.items, NOW));

        agent.apply(prompt(Some(notification("b1"))), None);
        assert_eq!(agent.apply(HookEvent::Stop, None), AgentStatus::Done);
        assert!(agent.items.is_empty());
    }

    #[test]
    fn codex_last_subagent_stop_after_turn_end_is_done() {
        let mut agent = Agent::default();
        agent.apply(prompt(None), None);
        let start = |id: &str| HookEvent::SubagentStart {
            agent_id: id.into(),
        };
        let stop = |id: &str| HookEvent::SubagentStop {
            agent_id: id.into(),
        };
        assert_eq!(agent.apply(start("s1"), None), AgentStatus::Working);
        agent.apply(start("s2"), None);
        assert_eq!(agent.apply(HookEvent::Stop, None), AgentStatus::Working);
        assert!(!agent.turn_active);
        assert_eq!(
            agent.apply(subagent_tool_use("s1"), None),
            AgentStatus::Working
        );
        assert_eq!(agent.apply(stop("s1"), None), AgentStatus::Working);
        assert_eq!(agent.apply(stop("s2"), None), AgentStatus::Done);
        assert!(agent.items.is_empty());
    }

    #[test]
    fn subagent_activity_extends_its_expiry() {
        let mut agent = Agent::default();
        agent.apply(
            HookEvent::SubagentStart {
                agent_id: "s1".into(),
            },
            None,
        );
        agent.items[0].expires_ts = NOW + 5;
        agent.apply(subagent_tool_use("s1"), None);
        assert_eq!(agent.items[0].expires_ts, NOW + SUBAGENT_IDLE_SECS);
    }

    #[test]
    fn untracked_subagent_stop_is_ignored() {
        let mut agent = Agent::default();
        agent.apply(prompt(None), None);
        assert_eq!(agent.apply(HookEvent::Stop, None), AgentStatus::Done);
        let status = agent.apply(
            HookEvent::SubagentStop {
                agent_id: "x".into(),
            },
            None,
        );
        assert_eq!(status, AgentStatus::Done);
        assert!(agent.items.is_empty());

        agent.status = Some(AgentStatus::Waiting);
        let status = agent.apply(
            HookEvent::SubagentStop {
                agent_id: "x".into(),
            },
            None,
        );
        assert_eq!(status, AgentStatus::Waiting);
    }

    #[test]
    fn untracked_subagent_stop_after_turn_end_is_done() {
        let stop = || HookEvent::SubagentStop {
            agent_id: "x".into(),
        };
        // Its SubagentStart was missed (or the state predates tracking): a
        // stored working with an idle turn and nothing live must not stick.
        let mut agent = Agent {
            status: Some(AgentStatus::Working),
            ..Agent::default()
        };
        assert_eq!(agent.apply(stop(), None), AgentStatus::Done);

        let mut agent = Agent {
            status: Some(AgentStatus::Working),
            items: vec![item("old", 1, NOW)],
            ..Agent::default()
        };
        assert_eq!(agent.apply(stop(), None), AgentStatus::Done);

        // Live background work, or a running turn, keeps working.
        let mut agent = Agent {
            status: Some(AgentStatus::Working),
            items: vec![item("b1", 1, NOW + 10)],
            ..Agent::default()
        };
        assert_eq!(agent.apply(stop(), None), AgentStatus::Working);
        let mut agent = Agent {
            status: Some(AgentStatus::Working),
            turn_active: true,
            ..Agent::default()
        };
        assert_eq!(agent.apply(stop(), None), AgentStatus::Working);
    }

    #[test]
    fn untracked_subagent_tool_use_after_stop_keeps_previous() {
        let mut agent = Agent::default();
        agent.apply(prompt(None), None);
        assert_eq!(agent.apply(HookEvent::Stop, None), AgentStatus::Done);
        assert_eq!(agent.apply(subagent_tool_use("x"), None), AgentStatus::Done);
        assert!(agent.items.is_empty());
        assert!(!agent.turn_active);
    }

    #[test]
    fn foreground_subagent_is_not_tracked() {
        let mut agent = Agent::default();
        agent.apply(prompt(None), None);
        assert_eq!(
            agent.apply(subagent_tool_use("f1"), None),
            AgentStatus::Working
        );
        assert_eq!(
            agent.apply(subagent_tool_use("f1"), None),
            AgentStatus::Working
        );
        assert!(agent.items.is_empty());
        assert_eq!(agent.apply(HookEvent::Stop, None), AgentStatus::Done);
    }

    #[test]
    fn waiting_changes_nothing_else() {
        let mut agent = Agent::default();
        agent.apply(prompt(None), None);
        agent.apply(launched("b1", BackgroundKind::Shell), None);
        assert_eq!(agent.apply(HookEvent::Waiting, None), AgentStatus::Waiting);
        assert!(agent.turn_active);
        assert_eq!(agent.ids(), vec!["b1"]);
    }

    #[test]
    fn stop_scans_transcript_for_queued_notifications() {
        let file = transcript_with("{\"type\":\"user\"}\n");
        let path = file.path();
        let mut agent = Agent::default();
        agent.apply(prompt(None), Some(path));
        agent.apply(launched("b1", BackgroundKind::Shell), Some(path));
        // A short transcript is rescanned from its start.
        assert_eq!(agent.items[0].transcript_offset, Some(0));

        append(path, &queued_line("b1"));
        assert_eq!(agent.apply(HookEvent::Stop, Some(path)), AgentStatus::Done);
        assert!(agent.items.is_empty());
    }

    #[test]
    fn notification_written_before_launch_hook_is_seen() {
        // The task finished, and its notification was queued, before the
        // PostToolUse hook that reports its launch ran.
        let file = transcript_with("{\"type\":\"user\"}\n");
        let path = file.path();
        let mut agent = Agent::default();
        agent.apply(prompt(None), Some(path));
        append(path, &queued_line("b1"));
        agent.apply(launched("b1", BackgroundKind::Shell), Some(path));
        assert_eq!(agent.apply(HookEvent::Stop, Some(path)), AgentStatus::Done);
        assert!(agent.items.is_empty());
    }

    #[test]
    fn launch_offset_rescans_at_most_the_last_64_kib() {
        let file = transcript_with(&"x".repeat(100 * 1024));
        let path = file.path();
        let mut agent = Agent::default();
        agent.apply(launched("b1", BackgroundKind::Shell), Some(path));
        assert_eq!(agent.items[0].transcript_offset, Some((100 - 64) * 1024));
    }

    #[test]
    fn later_hook_rescans_only_new_bytes() {
        let file = transcript_with("{\"type\":\"user\"}\n");
        let path = file.path();
        let mut agent = Agent::default();
        agent.apply(prompt(None), Some(path));
        agent.apply(launched("b1", BackgroundKind::Shell), Some(path));
        agent.apply(launched("m1", BackgroundKind::Monitor), Some(path));
        assert_eq!(
            agent.apply(HookEvent::Stop, Some(path)),
            AgentStatus::Working
        );
        assert_eq!(agent.ids(), vec!["b1", "m1"]);

        append(path, &queued_line("b1"));
        let len = std::fs::metadata(path).unwrap().len();
        assert_eq!(
            agent.apply(HookEvent::Waiting, Some(path)),
            AgentStatus::Waiting
        );
        assert_eq!(agent.ids(), vec!["m1"]);
        assert_eq!(agent.items[0].transcript_offset, Some(len));
    }

    #[test]
    fn scan_tolerates_missing_and_huge_transcripts() {
        let with_offset = |id: &str, offset: u64| BackgroundItem {
            transcript_offset: Some(offset),
            ..item(id, NOW, NOW + 100)
        };

        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing.jsonl");
        let mut items = vec![with_offset("b1", 3)];
        scan_transcript(&mut items, &missing);
        assert_eq!(items, vec![with_offset("b1", 3)]);

        const MIB: usize = 1024 * 1024;
        let filler = "x".repeat(MIB);
        let mut text = queued_line("b1");
        text.push_str(&filler[text.len()..]);
        text.push_str(&filler.repeat(7));
        let tail = queued_line("b2");
        text.push_str(&filler[tail.len()..]);
        text.push_str(&tail);
        assert_eq!(text.len(), 9 * MIB);
        let file = transcript_with(&text);

        let mut items = vec![with_offset("b1", 0), with_offset("b2", 0)];
        scan_transcript(&mut items, file.path());
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "b1");
        assert_eq!(items[0].transcript_offset, Some(text.len() as u64));
    }

    #[test]
    fn scan_waits_for_a_partly_written_line() {
        let head = "{\"type\":\"user\"}\n";
        let file = transcript_with(head);
        let path = file.path();
        let start = head.len() as u64;
        let mut items = vec![BackgroundItem {
            transcript_offset: Some(start),
            ..item("b1", NOW, NOW + 100)
        }];

        // The notification line is split across two writes, with no complete
        // line in between: nothing is consumed.
        let line = queued_line("b1");
        let (first, second) = line.split_at(line.len() / 2);
        append(path, first);
        scan_transcript(&mut items, path);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].transcript_offset, Some(start));

        // A complete line followed by a partial one: the offset stops after
        // the complete line, so the partial one is read whole next time.
        append(path, second);
        let other = queued_line("b2");
        append(path, &other[..10]);
        items.push(BackgroundItem {
            transcript_offset: Some(start),
            ..item("b2", NOW, NOW + 100)
        });
        scan_transcript(&mut items, path);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].id, "b2");
        let after_b1 = start + line.len() as u64;
        assert_eq!(items[0].transcript_offset, Some(after_b1));

        append(path, &other[10..]);
        scan_transcript(&mut items, path);
        assert!(items.is_empty());
    }

    #[test]
    fn task_stop_removes_item() {
        let mut agent = Agent::default();
        agent.apply(prompt(None), None);
        agent.apply(launched("b1", BackgroundKind::Shell), None);
        let status = agent.apply(
            HookEvent::ToolUse {
                agent_id: None,
                launch: None,
                stopped: Some("b1".into()),
            },
            None,
        );
        assert_eq!(status, AgentStatus::Working);
        assert!(agent.items.is_empty());
        assert_eq!(agent.apply(HookEvent::Stop, None), AgentStatus::Done);
    }

    #[test]
    fn session_start_clears_everything() {
        let mut agent = Agent::default();
        agent.apply(prompt(None), None);
        agent.apply(launched("b1", BackgroundKind::Shell), None);
        agent.apply(launched("a1", BackgroundKind::Subagent), None);
        assert_eq!(
            agent.apply(HookEvent::SessionStart, None),
            AgentStatus::Done
        );
        assert!(agent.items.is_empty());
        assert!(!agent.turn_active);
    }

    #[test]
    fn expiry_values_follow_launch_kind() {
        let expiry = |launch: Launch| {
            let mut agent = Agent::default();
            agent.apply(
                HookEvent::ToolUse {
                    agent_id: None,
                    launch: Some(launch),
                    stopped: None,
                },
                None,
            );
            agent.items[0].expires_ts
        };
        let monitor = Launch {
            timeout_secs: Some(60),
            ..launch("m1", BackgroundKind::Monitor)
        };
        assert_eq!(expiry(monitor.clone()), 1180);
        let persistent = Launch {
            persistent: true,
            ..monitor
        };
        assert_eq!(expiry(persistent), 87400);
        assert_eq!(expiry(launch("b1", BackgroundKind::Shell)), 44200);
        assert_eq!(expiry(launch("a1", BackgroundKind::Subagent)), 2800);
    }

    #[test]
    fn expired_items_are_pruned_before_the_event() {
        let mut agent = Agent::default();
        agent.items.push(item("old", 1, NOW));
        agent.apply(prompt(None), None);
        assert!(agent.items.is_empty());
    }
}
