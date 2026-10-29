//! Pure logic for the unified worktree view: agents as sub-rows of the
//! worktree they work in.

use std::cmp::Ordering;
use std::collections::HashSet;
use std::path::PathBuf;

use crate::multiplexer::{AgentPane, AgentStatus};
use crate::util::canon_or_self;

/// One row of the unified view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DisplayRow {
    /// Index into the visible (filtered) worktree list.
    Worktree(usize),
    /// An agent sub-row; `parent` is the visible worktree index, or `None`
    /// for agents in the "Outside worktrees" group.
    Agent {
        pane_id: String,
        parent: Option<usize>,
    },
    /// Index into the visible orphans list.
    Orphan(usize),
    /// Non-selectable "Outside worktrees" group header.
    OutsideHeader,
}

pub struct RowInputs<'a> {
    pub visible_worktrees: &'a [PathBuf],
    pub all_worktrees: &'a [PathBuf],
    pub agents: &'a [AgentPane],
    /// `assign_agents(all_worktrees, agents)`, computed once by the caller.
    pub assignment: &'a [Option<usize>],
    pub orphan_count: usize,
    pub filter: &'a str,
    pub outside_group: bool,
}

/// For each agent, the index into `worktree_paths` of the deepest worktree
/// that is an ancestor-or-equal of the agent's path.
pub fn assign_agents(worktree_paths: &[PathBuf], agents: &[AgentPane]) -> Vec<Option<usize>> {
    let canon: Vec<PathBuf> = worktree_paths.iter().map(|p| canon_or_self(p)).collect();
    agents
        .iter()
        .map(|agent| {
            let path = canon_or_self(&agent.path);
            canon
                .iter()
                .enumerate()
                .filter(|(_, wt)| path.starts_with(wt))
                .max_by_key(|(_, wt)| wt.components().count())
                .map(|(i, _)| i)
        })
        .collect()
}

fn status_rank(a: &AgentPane) -> u8 {
    match (a.status, a.background) {
        (Some(AgentStatus::Waiting), _) => 0,
        (Some(AgentStatus::Working), false) => 1,
        (Some(AgentStatus::Working), true) => 2,
        (Some(AgentStatus::Done), _) => 3,
        (None, _) => 4,
    }
}

/// Waiting, Working, Working background, Done, no status; ties by most
/// recent activity first.
pub fn agent_order(a: &AgentPane, b: &AgentPane) -> Ordering {
    status_rank(a)
        .cmp(&status_rank(b))
        .then_with(|| b.activity_ts().cmp(&a.activity_ts()))
}

fn sorted_ids(agents: &[AgentPane], mut idx: Vec<usize>) -> Vec<String> {
    idx.sort_by(|&x, &y| agent_order(&agents[x], &agents[y]));
    idx.into_iter().map(|i| agents[i].pane_id.clone()).collect()
}

/// Case-insensitive title match; `filter` must already be lowercase.
fn title_contains(agent: &AgentPane, filter: &str) -> bool {
    agent
        .pane_title
        .as_deref()
        .is_some_and(|t| t.to_lowercase().contains(filter))
}

fn matches_filter(agent: &AgentPane, filter: &str) -> bool {
    if filter.is_empty() {
        return true;
    }
    let f = filter.to_lowercase();
    title_contains(agent, &f) || agent.path.to_string_lossy().to_lowercase().contains(&f)
}

pub fn build_display_rows(inputs: &RowInputs) -> Vec<DisplayRow> {
    let assignment = inputs.assignment;
    debug_assert_eq!(assignment.len(), inputs.agents.len());
    let canon_visible: Vec<PathBuf> = inputs
        .visible_worktrees
        .iter()
        .map(|p| canon_or_self(p))
        .collect();
    let canon_all: Vec<PathBuf> = inputs
        .all_worktrees
        .iter()
        .map(|p| canon_or_self(p))
        .collect();

    let mut rows = Vec::new();
    for (vi, canon) in canon_visible.iter().enumerate() {
        rows.push(DisplayRow::Worktree(vi));
        let Some(ai) = canon_all.iter().position(|p| p == canon) else {
            continue;
        };
        let members: Vec<usize> = assignment
            .iter()
            .enumerate()
            .filter(|(_, a)| **a == Some(ai))
            .map(|(i, _)| i)
            .collect();
        for pane_id in sorted_ids(inputs.agents, members) {
            rows.push(DisplayRow::Agent {
                pane_id,
                parent: Some(vi),
            });
        }
    }

    rows.extend((0..inputs.orphan_count).map(DisplayRow::Orphan));

    if inputs.outside_group {
        let outside: Vec<usize> = assignment
            .iter()
            .enumerate()
            .filter(|(i, a)| a.is_none() && matches_filter(&inputs.agents[*i], inputs.filter))
            .map(|(i, _)| i)
            .collect();
        if !outside.is_empty() {
            rows.push(DisplayRow::OutsideHeader);
            for pane_id in sorted_ids(inputs.agents, outside) {
                rows.push(DisplayRow::Agent {
                    pane_id,
                    parent: None,
                });
            }
        }
    }
    rows
}

/// Indices into `all_worktrees` (the list `assignment` was computed
/// against) that have an agent whose title contains the (case-insensitive)
/// filter.
pub fn worktrees_visible_by_agent(
    assignment: &[Option<usize>],
    agents: &[AgentPane],
    filter: &str,
) -> HashSet<usize> {
    let f = filter.to_lowercase();
    assignment
        .iter()
        .zip(agents)
        .filter_map(|(a, agent)| a.filter(|_| title_contains(agent, &f)))
        .collect()
}

pub fn is_selectable(row: &DisplayRow) -> bool {
    !matches!(row, DisplayRow::OutsideHeader)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(id: &str, path: &str) -> AgentPane {
        AgentPane {
            session: "s".into(),
            window_name: format!("wm-{id}"),
            pane_id: id.into(),
            window_id: String::new(),
            window_index: None,
            path: PathBuf::from(path),
            pane_title: None,
            status: None,
            status_ts: None,
            activity_ts: None,
            updated_ts: None,
            window_cmd: None,
            agent_command: None,
            agent_kind: None,
            prompt: None,
            background: false,
        }
    }

    fn st(id: &str, status: Option<AgentStatus>, bg: bool, ts: u64) -> AgentPane {
        let mut a = agent(id, "/ws/w");
        a.status = status;
        a.background = bg;
        a.activity_ts = Some(ts);
        a
    }

    fn paths(v: &[&str]) -> Vec<PathBuf> {
        v.iter().map(PathBuf::from).collect()
    }

    #[test]
    fn deepest_worktree_wins() {
        let wts = paths(&["/ws/repo/primary-clone", "/ws/repo/primary-clone/feat-x"]);
        let agents = [
            agent("%1", "/ws/repo/primary-clone/feat-x/src"),
            agent("%2", "/ws/repo/primary-clone/docs"),
        ];
        assert_eq!(assign_agents(&wts, &agents), vec![Some(1), Some(0)]);
    }

    #[test]
    fn subdirectory_and_unassigned() {
        let wts = paths(&["/ws/repo/feat-y"]);
        let agents = [
            agent("%1", "/ws/repo/feat-y/backend"),
            agent("%2", "/home/u/.claude/projects/x/memory"),
            agent("%3", "/ws/repo/feat-yy"),
        ];
        assert_eq!(assign_agents(&wts, &agents), vec![Some(0), None, None]);
    }

    #[test]
    fn agents_sorted_by_status_then_recency() {
        use AgentStatus::*;
        let mut v = [
            st("done", Some(Done), false, 50),
            st("bg", Some(Working), true, 40),
            st("wait", Some(Waiting), false, 10),
            st("w30", Some(Working), false, 30),
            st("w35", Some(Working), false, 35),
            st("none", None, false, 60),
        ];
        v.sort_by(agent_order);
        let ids: Vec<_> = v.iter().map(|a| a.pane_id.as_str()).collect();
        assert_eq!(ids, ["wait", "w35", "w30", "bg", "done", "none"]);
    }

    fn fixture() -> (Vec<PathBuf>, Vec<AgentPane>) {
        let wts = paths(&["/ws/a", "/ws/b"]);
        let mut a = agent("A", "/ws/b/src");
        a.pane_title = Some("Fork workmux".into());
        let mut b = agent("B", "/elsewhere/x");
        b.pane_title = Some("Stray".into());
        (wts, vec![a, b])
    }

    fn rows(
        wts: &[PathBuf],
        all: &[PathBuf],
        agents: &[AgentPane],
        filter: &str,
        outside_group: bool,
    ) -> Vec<DisplayRow> {
        let assignment = assign_agents(all, agents);
        build_display_rows(&RowInputs {
            visible_worktrees: wts,
            all_worktrees: all,
            agents,
            assignment: &assignment,
            orphan_count: 1,
            filter,
            outside_group,
        })
    }

    fn ag(id: &str, parent: Option<usize>) -> DisplayRow {
        DisplayRow::Agent {
            pane_id: id.into(),
            parent,
        }
    }

    #[test]
    fn rows_interleave_agents_orphans_and_outside_group() {
        let (wts, agents) = fixture();
        assert_eq!(
            rows(&wts, &wts, &agents, "", true),
            vec![
                DisplayRow::Worktree(0),
                DisplayRow::Worktree(1),
                ag("A", Some(1)),
                DisplayRow::Orphan(0),
                DisplayRow::OutsideHeader,
                ag("B", None),
            ]
        );
    }

    #[test]
    fn outside_group_hidden_when_disabled_or_filtered_out() {
        let (wts, agents) = fixture();
        let no_group = rows(&wts, &wts, &agents, "", false);
        assert!(!no_group.contains(&DisplayRow::OutsideHeader));
        assert!(!no_group.contains(&ag("B", None)));
        let filtered = rows(&wts, &wts, &agents, "zzz", true);
        assert!(!filtered.contains(&DisplayRow::OutsideHeader));
        assert!(!filtered.contains(&ag("B", None)));
        let matching = rows(&wts, &wts, &agents, "STRAY", true);
        assert!(matching.contains(&DisplayRow::OutsideHeader));
        assert!(matching.contains(&ag("B", None)));
    }

    #[test]
    fn agents_of_hidden_worktrees_are_dropped() {
        let all = paths(&["/ws/a", "/ws/b"]);
        let visible = paths(&["/ws/a"]);
        let agents = [agent("A", "/ws/b/src")];
        let r = rows(&visible, &all, &agents, "", true);
        assert_eq!(r, vec![DisplayRow::Worktree(0), DisplayRow::Orphan(0)]);
    }

    #[test]
    fn header_is_not_selectable() {
        assert!(!is_selectable(&DisplayRow::OutsideHeader));
        assert!(is_selectable(&DisplayRow::Worktree(0)));
        assert!(is_selectable(&ag("A", None)));
        assert!(is_selectable(&DisplayRow::Orphan(0)));
    }

    #[test]
    fn worktree_visible_by_agent_title() {
        let wts = paths(&["/ws/v", "/ws/w"]);
        let mut a = agent("A", "/ws/w");
        a.pane_title = Some("Fork workmux".into());
        let mut b = agent("B", "/elsewhere");
        b.pane_title = Some("Fork too".into());
        let agents = [a, b];
        let assignment = assign_agents(&wts, &agents);
        assert_eq!(
            worktrees_visible_by_agent(&assignment, &agents, "fork"),
            HashSet::from([1])
        );
        assert!(worktrees_visible_by_agent(&assignment, &agents, "zzz").is_empty());
    }
}
