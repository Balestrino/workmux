//! Pure logic for the unified worktree view: agents as sub-rows of the
//! worktree they work in.

use std::cmp::{Ordering, Reverse};
use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::multiplexer::{AgentPane, AgentStatus, RemoteAgent};
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
    /// `$HOME`, to match outside agents by their displayed (`~/…`) directory.
    pub home: Option<&'a Path>,
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
                // The first of equally deep (duplicate) paths, as the row
                // builder looks worktrees up by first position.
                .min_by_key(|(_, wt)| Reverse(wt.components().count()))
                .map(|(i, _)| i)
        })
        .collect()
}

/// Directory with `$HOME` shortened to `~`, as outside-group rows show it.
pub fn short_dir(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
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

/// Case-insensitive match of the title or, for a remote agent, its label;
/// `filter` must already be lowercase.
fn title_contains(agent: &AgentPane, filter: &str) -> bool {
    agent
        .pane_title
        .as_deref()
        .is_some_and(|t| t.to_lowercase().contains(filter))
        || remote_label(agent).is_some_and(|label| label.to_lowercase().contains(filter))
}

/// Title, raw directory or displayed (`~/…`) directory match.
fn matches_filter(agent: &AgentPane, filter: &str, home: Option<&Path>) -> bool {
    if filter.is_empty() {
        return true;
    }
    let f = filter.to_lowercase();
    title_contains(agent, &f)
        || agent.path.to_string_lossy().to_lowercase().contains(&f)
        || short_dir(&agent.path, home).to_lowercase().contains(&f)
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
    // A duplicate path lists its agents once, under its first row.
    let mut listed = HashSet::new();
    for (vi, canon) in canon_visible.iter().enumerate() {
        rows.push(DisplayRow::Worktree(vi));
        let Some(ai) = canon_all.iter().position(|p| p == canon) else {
            continue;
        };
        if !listed.insert(ai) {
            continue;
        }
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
            .filter(|(i, a)| {
                a.is_none() && matches_filter(&inputs.agents[*i], inputs.filter, inputs.home)
            })
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

/// The label of a remote agent, `handle@remote` as the Agents tab shows it.
fn remote_label(agent: &AgentPane) -> Option<String> {
    agent
        .remote
        .as_ref()
        .map(|remote| crate::remote::agents::label(&remote.handle, &remote.at.remote))
}

/// Title an agent's sub-row shows. A remote agent's starts with its label,
/// then says when the remote is unreachable or its execution ended, as the
/// Agents tab does; a local agent's is `title` as it is.
pub fn sub_row_title(agent: &AgentPane, title: &str, now: u64) -> String {
    let (Some(label), Some(remote)) = (remote_label(agent), agent.remote.as_ref()) else {
        return title.to_string();
    };
    let title = crate::remote::agents::row_title(remote, now, Some(title))
        .unwrap_or_else(|| title.to_string());
    if title.is_empty() {
        label
    } else {
        format!("{label} \u{b7} {title}")
    }
}

/// Where Enter on an agent's sub-row goes.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentJump {
    /// The agent's pane.
    Pane(String),
    /// A remote agent no local pane relays: its window is opened first.
    Detached(Box<RemoteAgent>),
}

/// What jumping to the agent in `pane_id` does: a remote agent no local
/// pane relays has no pane to switch to.
pub fn agent_jump(agents: &[AgentPane], pane_id: &str) -> AgentJump {
    agents
        .iter()
        .find(|agent| agent.pane_id == pane_id)
        .filter(|agent| crate::remote::agents::is_detached(agent))
        .and_then(|agent| agent.remote.clone())
        .map_or_else(
            || AgentJump::Pane(pane_id.to_string()),
            |remote| AgentJump::Detached(Box::new(remote)),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn remote_agent(id: &str, attached: bool, connected: bool) -> AgentPane {
        let mut a = agent(id, "/ws/repo/feat-x");
        a.pane_title = Some("do x".into());
        a.remote = Some(RemoteAgent {
            handle: "feat-x".into(),
            attached,
            connected,
            ..RemoteAgent::new(crate::multiplexer::RemoteRef {
                remote: "devbox".into(),
                ws_id: "ws1".into(),
                ex_id: "ex1".into(),
                pane: 0,
            })
        });
        a
    }

    #[test]
    fn a_remote_sub_row_names_its_workspace_and_remote() {
        let local = agent("%1", "/ws/repo/feat-x");
        assert_eq!(sub_row_title(&local, "do x", 100), "do x");

        let attached = remote_agent("%2", true, true);
        assert_eq!(
            sub_row_title(&attached, "do x", 100),
            "feat-x@devbox \u{b7} do x"
        );
        assert_eq!(sub_row_title(&attached, "", 100), "feat-x@devbox");

        // An unreachable remote says so ahead of the last title, as the
        // Agents tab does.
        let lost = remote_agent("%remote-ws1-0", false, false);
        assert_eq!(
            sub_row_title(&lost, "do x", 100),
            "feat-x@devbox \u{b7} disconnected \u{b7} do x"
        );
    }

    #[test]
    fn a_remote_sub_row_matches_the_filter_by_its_label() {
        let agents = [remote_agent("%2", true, true)];
        let rows = build_display_rows(&RowInputs {
            visible_worktrees: &[],
            all_worktrees: &[],
            agents: &agents,
            assignment: &[None],
            orphan_count: 0,
            filter: "devbox",
            outside_group: true,
            home: None,
        });
        assert!(
            rows.contains(&DisplayRow::Agent {
                pane_id: "%2".into(),
                parent: None
            }),
            "{rows:?}"
        );
    }

    #[test]
    fn jumping_to_a_detached_remote_agent_opens_its_window() {
        let agents = [
            agent("%1", "/ws/repo"),
            remote_agent("%2", true, true),
            remote_agent("%remote-ws1-0", false, true),
        ];
        assert_eq!(agent_jump(&agents, "%1"), AgentJump::Pane("%1".into()));
        // A pane relays it: that pane is the target.
        assert_eq!(agent_jump(&agents, "%2"), AgentJump::Pane("%2".into()));
        // No pane relays it: there is no pane to switch to.
        assert_eq!(
            agent_jump(&agents, "%remote-ws1-0"),
            AgentJump::Detached(Box::new(agents[2].remote.clone().unwrap()))
        );
        // An agent that is gone falls back to the pane id.
        assert_eq!(agent_jump(&agents, "%9"), AgentJump::Pane("%9".into()));
    }

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
            remote: None,
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
            home: None,
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
    fn duplicate_worktree_paths_assign_and_list_under_the_first() {
        let wts = paths(&["/ws/a", "/ws/a"]);
        let agents = [agent("A", "/ws/a/src")];
        assert_eq!(assign_agents(&wts, &agents), vec![Some(0)]);
        assert_eq!(
            rows(&wts, &wts, &agents, "", true),
            vec![
                DisplayRow::Worktree(0),
                ag("A", Some(0)),
                DisplayRow::Worktree(1),
                DisplayRow::Orphan(0),
            ]
        );
    }

    #[test]
    fn outside_filter_matches_the_home_shortened_dir() {
        let wts = paths(&["/ws/a"]);
        let agents = [agent("B", "/home/u/work/x")];
        let assignment = assign_agents(&wts, &agents);
        let rows_for = |filter: &str| {
            build_display_rows(&RowInputs {
                visible_worktrees: &wts,
                all_worktrees: &wts,
                agents: &agents,
                assignment: &assignment,
                orphan_count: 0,
                filter,
                outside_group: true,
                home: Some(Path::new("/home/u")),
            })
        };
        assert!(rows_for("~/work").contains(&ag("B", None)));
        assert!(rows_for("/home/u/work").contains(&ag("B", None)));
        assert!(!rows_for("~/other").contains(&ag("B", None)));
    }

    #[test]
    fn short_dir_shortens_home() {
        let home = Path::new("/home/u");
        assert_eq!(
            short_dir(Path::new("/home/u/work/x"), Some(home)),
            "~/work/x"
        );
        assert_eq!(short_dir(home, Some(home)), "~");
        assert_eq!(short_dir(Path::new("/srv/x"), Some(home)), "/srv/x");
        assert_eq!(
            short_dir(Path::new("/home/user2/x"), Some(home)),
            "/home/user2/x"
        );
        assert_eq!(short_dir(Path::new("/home/u/x"), None), "/home/u/x");
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
