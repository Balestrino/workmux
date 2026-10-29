//! Background work items tracked per agent.
//!
//! A background item is work an agent started and left running (a subagent,
//! a background shell, a monitor) while its own turn is idle. Items expire on
//! a deadline so a missed completion hook cannot pin an agent forever.

// The hook handlers that consume these arrive in later changes.
#![allow(dead_code)]

use serde::{Deserialize, Serialize};

/// Deadline extension for a subagent each time it reports activity.
pub const SUBAGENT_IDLE_SECS: u64 = 1800;
/// Grace period for a non-persistent monitor after its last observed output.
pub const MONITOR_GRACE_SECS: u64 = 120;
/// Deadline for a persistent monitor.
pub const PERSISTENT_MONITOR_SECS: u64 = 86400;
/// Deadline for a background shell.
pub const SHELL_SECS: u64 = 43200;

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
    while let Some(start) = rest.find(NOTIFICATION_OPEN) {
        let body = &rest[start + NOTIFICATION_OPEN.len()..];
        let end = body.find(NOTIFICATION_CLOSE).unwrap_or(body.len());
        let block = &body[..end];
        if let (Some(id), Some(status)) = (tag_value(block, "task-id"), tag_value(block, "status"))
            && !id.is_empty()
            && status != "running"
        {
            ids.push(id.to_string());
        }
        rest = &body[end..];
    }
    ids
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
}
