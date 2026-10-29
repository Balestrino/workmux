//! Worktree tab: navigation, removal, sweep, project picker, and preview.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context as _;

use crate::git;
use crate::multiplexer::{AgentPane, Multiplexer};
use crate::util::canon_or_self;
use crate::workflow;
use crate::workspaces::{self, WorkspaceRepo};

use super::super::agent;
use super::super::sort::WorktreeSortMode;
use super::super::unified::{self, DisplayRow, RowInputs};
use super::super::worktree_info::{self, OrphanEntry};
use super::App;
use super::types::*;

/// Project name for a repository: the name computed for its repository key,
/// else derived from the path.
pub(in crate::command::dashboard) fn resolve_project_name(
    names: &HashMap<PathBuf, String>,
    repo_key: &Path,
    path: &Path,
) -> String {
    names
        .get(repo_key)
        .cloned()
        .unwrap_or_else(|| agent::extract_project_name(path))
}

/// Repository roots to list: discovered workspace repositories plus the
/// roots of repositories that have agents, deduplicated by canonical path.
fn fetch_roots(workspace_repos: &[WorkspaceRepo], agent_roots: &[PathBuf]) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    workspace_repos
        .iter()
        .map(|r| &r.main_root)
        .chain(agent_roots)
        .filter(|root| seen.insert(canon_or_self(root)))
        .cloned()
        .collect()
}

/// Worktrees of every repository in `roots`. A root that cannot be listed
/// (not a repository any more, broken config) is skipped.
fn list_all_repos(
    config: &crate::config::Config,
    mux: &dyn Multiplexer,
    roots: &[PathBuf],
) -> Vec<workflow::types::WorktreeInfo> {
    let mut seen_keys = HashSet::new();
    let mut all = Vec::new();
    for root in roots {
        let root_config = crate::config::Config::load_with_location_from(root, None)
            .map(|(c, _)| c)
            .unwrap_or_else(|_| config.clone());
        let Ok(list) = workflow::list_in(&root_config, mux, false, &[], Some(root)) else {
            continue;
        };
        if let Some(first) = list.first()
            && !seen_keys.insert(first.repo_key.clone())
        {
            continue;
        }
        all.extend(list);
    }
    all
}

/// Project names sent with a worktree list. Without workspaces the map is
/// empty, so every row falls back to the name derived from its own path
/// (upstream behavior); with workspaces it covers every known repository.
fn fetched_project_names(
    workspace_dirs: &[PathBuf],
    agent_roots: &[PathBuf],
) -> HashMap<PathBuf, String> {
    if workspace_dirs.is_empty() {
        return HashMap::new();
    }
    // Full set even when narrowed, so names do not depend on the view.
    all_project_names(workspace_dirs, agent_roots)
}

/// Project names for `(repo_key, main_root)` pairs, keyed by repository key.
/// Always call this with every known repository (not a narrowed view) so
/// repositories sharing a first path component keep their full relative name.
fn names_for_repos(
    workspace_dirs: &[PathBuf],
    repos: &[(PathBuf, PathBuf)],
) -> HashMap<PathBuf, String> {
    let roots: Vec<PathBuf> = repos.iter().map(|(_, root)| root.clone()).collect();
    let by_root = workspaces::project_names(workspace_dirs, &roots);
    repos
        .iter()
        .filter_map(|(key, root)| {
            by_root
                .get(&canon_or_self(root))
                .map(|name| (key.clone(), name.clone()))
        })
        .collect()
}

/// Names for every repository under the workspaces plus the agent roots.
/// Runs git, so call it from a background thread.
fn all_project_names(
    workspace_dirs: &[PathBuf],
    agent_roots: &[PathBuf],
) -> HashMap<PathBuf, String> {
    let discovered = workspaces::discover_repos(workspace_dirs);
    let mut seen = HashSet::new();
    let mut repos: Vec<(PathBuf, PathBuf)> = Vec::new();
    for r in &discovered {
        if seen.insert(r.repo_key.clone()) {
            repos.push((r.repo_key.clone(), r.main_root.clone()));
        }
    }
    for root in fetch_roots(&discovered, agent_roots) {
        if discovered
            .iter()
            .any(|r| canon_or_self(&r.main_root) == canon_or_self(&root))
        {
            continue;
        }
        if let Ok(key) = git::get_repository_key_in(Some(&root))
            && seen.insert(key.clone())
        {
            repos.push((key, root));
        }
    }
    names_for_repos(workspace_dirs, &repos)
}

/// Picker selection as a worktree-view override; "All projects" clears it.
fn override_for(entry: &ProjectEntry) -> Option<(String, PathBuf)> {
    entry
        .repo_key
        .as_ref()
        .map(|_| (entry.name.clone(), entry.path.clone()))
}

/// Picker entries for `(repo_key, main_root)` pairs, one per repository,
/// sorted by name. With workspaces configured "All projects" comes first.
fn project_entries(
    workspace_dirs: &[PathBuf],
    repos: Vec<(PathBuf, PathBuf)>,
) -> Vec<ProjectEntry> {
    let mut seen = HashSet::new();
    let repos: Vec<(PathBuf, PathBuf)> = repos
        .into_iter()
        .filter(|(key, _)| seen.insert(key.clone()))
        .collect();
    let roots: Vec<PathBuf> = repos.iter().map(|(_, root)| root.clone()).collect();
    let names = workspaces::project_names(workspace_dirs, &roots);

    let mut entries: Vec<ProjectEntry> = repos
        .into_iter()
        .map(|(key, root)| ProjectEntry {
            name: names
                .get(&canon_or_self(&root))
                .cloned()
                .unwrap_or_else(|| agent::extract_project_name(&root)),
            path: root,
            repo_key: Some(key),
        })
        .collect();
    entries.sort_by(|a, b| a.name.cmp(&b.name).then(a.path.cmp(&b.path)));
    if !workspace_dirs.is_empty() {
        entries.insert(
            0,
            ProjectEntry {
                name: ALL_PROJECTS.to_string(),
                path: PathBuf::new(),
                repo_key: None,
            },
        );
    }
    entries
}

const ALL_PROJECTS: &str = "All projects";

fn merge_worktree_prs(
    worktrees: &mut [workflow::types::WorktreeInfo],
    statuses: &std::collections::HashMap<
        PathBuf,
        std::collections::HashMap<String, crate::github::PrSummary>,
    >,
) {
    for wt in worktrees {
        if wt.pr_info.is_some() || wt.is_main {
            continue;
        }
        wt.pr_info = statuses
            .get(&wt.repo_key)
            .and_then(|prs| prs.get(&wt.branch))
            .cloned();
    }
}

/// Delete the last word from a string (Emacs Ctrl+w behavior).
fn delete_word_backward(s: &mut String) {
    // Trim trailing whitespace first
    let trimmed_len = s.trim_end().len();
    s.truncate(trimmed_len);
    // Then delete back to the previous word boundary
    if let Some(pos) = s.rfind(|c: char| c == '/' || c == '-' || c.is_whitespace()) {
        s.truncate(pos);
    } else {
        s.clear();
    }
}

fn default_add_worktree_base(repo_path: &Path) -> String {
    crate::config::Config::load_with_location_from(repo_path, None)
        .ok()
        .and_then(|(config, _)| {
            workflow::resolve_configured_base_branch(&config, repo_path)
                .ok()
                .flatten()
        })
        .or_else(|| {
            git::get_current_branch_in(repo_path)
                .ok()
                .map(|branch| branch.trim().to_string())
                .filter(|branch| !branch.is_empty())
        })
        .or_else(|| git::get_default_branch_in(Some(repo_path)).ok())
        .unwrap_or_else(|| "main".to_string())
}

fn list_branches_for_status(app: &mut App, repo_path: &Path) -> Option<Vec<String>> {
    match git::list_local_branches_in(Some(repo_path)) {
        Ok(branches) => Some(branches),
        Err(_) => {
            app.status_message = Some((
                "Failed to list branches".to_string(),
                std::time::Instant::now(),
            ));
            None
        }
    }
}

fn run_add_worktree_job(
    job: AddWorktreeJob,
    repo_path: PathBuf,
    mux: Arc<dyn Multiplexer>,
) -> anyhow::Result<String> {
    match job {
        AddWorktreeJob::CreateBranch { name, base_branch } => {
            let (config, config_location) =
                crate::config::Config::load_with_location_from(&repo_path, None)?;
            let ctx = workflow::WorkflowContext::new_in(
                &repo_path,
                config.clone(),
                mux,
                config_location,
            )?;
            let handle = crate::naming::derive_handle(&name, None, &config)?;
            let mut options = workflow::types::SetupOptions::new(true, true, true);
            options.focus_window = false;
            options.mode = config.mode();

            let result = workflow::create(
                &ctx,
                workflow::CreateArgs {
                    branch_name: &name,
                    handle: &handle,
                    base_branch: base_branch.as_deref(),
                    remote_branch: None,
                    checkout_ref: None,
                    prompt: None,
                    options,
                    mode_override: None,
                    agent: None,
                    is_explicit_name: false,
                    prompt_file_only: false,
                    fork_source: None,
                },
            )?;
            Ok(result.branch_name)
        }
        AddWorktreeJob::CheckoutPr { pr_number } => {
            let (config, config_location) =
                crate::config::Config::load_with_location_from(&repo_path, None)?;

            let pr_details = crate::github::get_pr_details_in(Some(&repo_path), pr_number)
                .with_context(|| format!("Failed to fetch PR #{}", pr_number))?;

            let current_repo_owner = git::get_repo_owner_in(Some(&repo_path))
                .context("Failed to determine repository owner")?;
            let is_fork = pr_details.is_fork(&current_repo_owner);
            let fork_owner = &pr_details.head_repository_owner.login;

            let remote_name = if is_fork {
                git::ensure_fork_remote_in(fork_owner, Some(&repo_path))?
            } else {
                "origin".to_string()
            };

            let local_branch = if is_fork {
                format!("{}-{}", fork_owner, pr_details.head_ref_name)
            } else {
                pr_details.head_ref_name.clone()
            };
            let remote_branch = format!("{}/{}", remote_name, pr_details.head_ref_name);
            // Compare the checkout against the PR target branch in the target
            // repository (origin), not the head it was populated from.
            let base_branch = crate::workflow::pr::pr_base_branch(&pr_details.base_ref_name);

            let ctx = workflow::WorkflowContext::new_in(
                &repo_path,
                config.clone(),
                mux,
                config_location,
            )?;
            let handle = crate::naming::derive_handle(&local_branch, None, &config)?;
            let mut options = workflow::types::SetupOptions::new(true, true, true);
            options.focus_window = false;
            options.mode = config.mode();

            let result = workflow::create(
                &ctx,
                workflow::CreateArgs {
                    branch_name: &local_branch,
                    handle: &handle,
                    base_branch: base_branch.as_deref(),
                    remote_branch: Some(&remote_branch),
                    checkout_ref: Some(crate::workflow::pr::CheckoutRef {
                        number: pr_number,
                        forge: crate::workflow::pr::Forge::Github,
                    }),
                    prompt: None,
                    options,
                    mode_override: None,
                    agent: None,
                    is_explicit_name: false,
                    prompt_file_only: false,
                    fork_source: None,
                },
            )?;
            Ok(result.branch_name)
        }
    }
}

/// Status message for a worktree action on an agent outside every worktree.
const NOT_IN_WORKTREE: &str = "not in a worktree";

/// Rows of the Worktrees table. Without the unified view: the worktrees
/// then the orphans, as upstream.
fn display_rows_for(unified_view: bool, inputs: &RowInputs) -> Vec<DisplayRow> {
    if unified_view {
        return unified::build_display_rows(inputs);
    }
    (0..inputs.visible_worktrees.len())
        .map(DisplayRow::Worktree)
        .chain((0..inputs.orphan_count).map(DisplayRow::Orphan))
        .collect()
}

/// The worktree a row stands for: its own, or an agent sub-row's parent.
fn worktree_index_of(row: &DisplayRow) -> Option<usize> {
    match row {
        DisplayRow::Worktree(i)
        | DisplayRow::Agent {
            parent: Some(i), ..
        } => Some(*i),
        _ => None,
    }
}

/// An agent sub-row of the "Outside worktrees" group.
fn is_outside_agent(row: &DisplayRow) -> bool {
    matches!(row, DisplayRow::Agent { parent: None, .. })
}

fn first_selectable(rows: &[DisplayRow]) -> Option<usize> {
    rows.iter().position(unified::is_selectable)
}

/// Next (or previous) selectable row after `current`, wrapping around and
/// skipping the group header. Nothing selected counts as row 0.
fn step_selectable(rows: &[DisplayRow], current: Option<usize>, forward: bool) -> Option<usize> {
    let count = rows.len();
    if count == 0 {
        return None;
    }
    let mut i = current.unwrap_or(0).min(count - 1);
    for _ in 0..count {
        i = if forward {
            (i + 1) % count
        } else {
            (i + count - 1) % count
        };
        if unified::is_selectable(&rows[i]) {
            return Some(i);
        }
    }
    None
}

/// Index of the `n`-th worktree row (0-based); agent sub-rows, orphans and
/// the header have no jump number.
fn nth_worktree_row(rows: &[DisplayRow], n: usize) -> Option<usize> {
    rows.iter()
        .enumerate()
        .filter(|(_, r)| matches!(r, DisplayRow::Worktree(_)))
        .nth(n)
        .map(|(i, _)| i)
}

/// Where the selection goes after the rows were rebuilt.
#[derive(Debug, PartialEq, Eq)]
enum Restored {
    /// The selected agent (or, failing that, the selected path) is at this row.
    Found(usize),
    /// The selected row is gone: the first selectable row, if any.
    Lost(Option<usize>),
    /// Nothing was selected by identity.
    NoIdentity,
}

/// Find the previous selection in `rows`: by pane id for an agent sub-row,
/// then by path (`path_match`) for worktree and orphan rows.
fn restore_row(
    rows: &[DisplayRow],
    agent: Option<&str>,
    path_match: Option<&dyn Fn(&DisplayRow) -> bool>,
) -> Restored {
    if let Some(id) = agent
        && let Some(i) = rows
            .iter()
            .position(|r| matches!(r, DisplayRow::Agent { pane_id, .. } if pane_id == id))
    {
        return Restored::Found(i);
    }
    if let Some(matches) = path_match
        && let Some(i) = rows.iter().position(|r| {
            matches!(r, DisplayRow::Worktree(_) | DisplayRow::Orphan(_)) && matches(r)
        })
    {
        return Restored::Found(i);
    }
    if agent.is_some() || path_match.is_some() {
        Restored::Lost(first_selectable(rows))
    } else {
        Restored::NoIdentity
    }
}

/// What Enter on a row jumps to.
#[derive(Debug, PartialEq, Eq)]
enum EnterTarget {
    /// An agent's pane.
    Pane(String),
    /// The worktree's own window or session (the existing open logic).
    Worktree(usize),
}

/// Enter on an agent sub-row targets its pane; on a worktree row, its first
/// sub-row's pane, else the worktree itself. Orphans and the header: nothing.
fn enter_target(rows: &[DisplayRow], selected: usize) -> Option<EnterTarget> {
    match rows.get(selected)? {
        DisplayRow::Agent { pane_id, .. } => Some(EnterTarget::Pane(pane_id.clone())),
        DisplayRow::Worktree(i) => match rows.get(selected + 1) {
            Some(DisplayRow::Agent {
                pane_id,
                parent: Some(parent),
            }) if parent == i => Some(EnterTarget::Pane(pane_id.clone())),
            _ => Some(EnterTarget::Worktree(*i)),
        },
        DisplayRow::Orphan(_) | DisplayRow::OutsideHeader => None,
    }
}

/// First agent (in list order) assigned to `worktree` among `all_worktrees`
/// (deepest ancestor-or-equal), so agents in a subdirectory are found too.
fn first_agent_in_worktree(
    worktree: &Path,
    all_worktrees: &[PathBuf],
    agents: &[AgentPane],
) -> Option<String> {
    let target = canon_or_self(worktree);
    let index = all_worktrees
        .iter()
        .position(|p| canon_or_self(p) == target)?;
    unified::assign_agents(all_worktrees, agents)
        .iter()
        .zip(agents)
        .find(|(a, _)| **a == Some(index))
        .map(|(_, agent)| agent.pane_id.clone())
}

/// Tab after a Tab press: the unified view has no Agents tab.
fn next_tab(current: DashboardTab, unified_view: bool) -> DashboardTab {
    if unified_view {
        return DashboardTab::Worktrees;
    }
    match current {
        DashboardTab::Agents => DashboardTab::Worktrees,
        DashboardTab::Worktrees => DashboardTab::Agents,
    }
}

/// Footer message for a row action whose repository config failed to load.
fn row_config_error(handle: &str, error: &anyhow::Error) -> String {
    format!("Cannot load config for '{handle}': {error:#}")
}

/// Orphan rows shown for the current view: only the selected project's when
/// the picker narrowed the view, and only those whose project or name
/// contains the (lowercase) text filter.
fn filter_orphans(
    orphans: &[OrphanEntry],
    project: Option<&str>,
    filter_text: &str,
) -> Vec<OrphanEntry> {
    let filter = filter_text.to_lowercase();
    orphans
        .iter()
        .filter(|o| project.is_none_or(|p| o.project == p))
        .filter(|o| {
            filter.is_empty()
                || o.project.to_lowercase().contains(&filter)
                || o.name.to_lowercase().contains(&filter)
        })
        .cloned()
        .collect()
}

/// What the active tab's selection says about the repository to act on.
#[derive(Debug)]
enum RepoSelection {
    /// Worktrees tab: a listed worktree row.
    Worktree(PathBuf),
    /// Worktrees tab: an orphan row, which belongs to no repository.
    Orphan,
    /// Agents tab: the selected agent's repository root, when known.
    Agent(Option<PathBuf>),
    /// Nothing selected.
    Nothing,
}

/// Pick the repository an action targets. An orphan row targets nothing.
/// Otherwise: the project override, else (in all mode) the selection, else
/// the dashboard's current worktree, else the first listed row. The first
/// row is not used for an agent selection in all mode: it belongs to an
/// arbitrary repository the user is not looking at.
fn choose_repo_path(
    override_path: Option<&PathBuf>,
    all_mode: bool,
    selection: RepoSelection,
    current: Option<PathBuf>,
    first_row_path: Option<PathBuf>,
) -> Option<PathBuf> {
    let (selected, first_row_path) = match selection {
        RepoSelection::Orphan => return None,
        RepoSelection::Worktree(path) => (Some(path), first_row_path),
        RepoSelection::Agent(root) if all_mode => (root, None),
        RepoSelection::Agent(root) => (root, first_row_path),
        RepoSelection::Nothing => (None, first_row_path),
    };
    override_path
        .cloned()
        .or_else(|| selected.filter(|_| all_mode))
        .or(current)
        .or(first_row_path)
}

/// Branches that already have a worktree in the repository `repo_key`.
/// Without a key (it could not be resolved) every listed worktree counts.
fn occupied_branches(
    worktrees: &[workflow::types::WorktreeInfo],
    repo_key: Option<&Path>,
) -> HashSet<String> {
    let target = repo_key.map(canon_or_self);
    worktrees
        .iter()
        .filter(|w| {
            target
                .as_ref()
                .is_none_or(|key| canon_or_self(&w.repo_key) == *key)
        })
        .map(|w| w.branch.clone())
        .collect()
}

fn remove_worktree(
    handle: &str,
    path: &Path,
    keep_branch: bool,
    mux: Arc<dyn Multiplexer>,
) -> anyhow::Result<workflow::types::RemoveResult> {
    let (config, config_location) = crate::config::Config::load_with_location_from(path, None)?;
    let ctx = workflow::WorkflowContext::new_in(path, config, mux, config_location)?;
    workflow::remove_quiet(handle, true, keep_branch, &ctx)
}

fn spawn_add_worktree_job(
    job: AddWorktreeJob,
    repo_path: PathBuf,
    mux: Arc<dyn Multiplexer>,
    tx: mpsc::Sender<AppEvent>,
) {
    std::thread::spawn(move || {
        let result = run_add_worktree_job(job, repo_path, mux).map_err(|e| e.to_string());
        let _ = tx.send(AppEvent::AddWorktreeResult(result));
    });
}

impl App {
    /// Reset the worktree fetch timer to trigger an immediate refetch
    pub fn trigger_worktree_refetch(&mut self) {
        self.last_worktree_fetch = std::time::Instant::now() - Duration::from_secs(60);
    }

    /// Switch between Agents and Worktrees tabs (no-op in the unified view)
    pub fn switch_tab(&mut self) {
        let next = next_tab(self.active_tab, self.unified);
        if next == self.active_tab {
            return;
        }
        self.active_tab = next;
        if self.active_tab == DashboardTab::Worktrees {
            // Trigger immediate fetch on switch
            self.last_worktree_fetch = std::time::Instant::now();
            self.spawn_worktree_fetch();
            // First time the tab is shown: do not wait for the interval.
            self.maybe_spawn_worktree_info_fetch();
        }
    }

    /// Run the external info provider when configured and due (first run, or
    /// the configured interval elapsed). At most one run at a time; the run
    /// happens on a background thread and never blocks the UI.
    pub(super) fn maybe_spawn_worktree_info_fetch(&mut self) {
        let Some(command) = self
            .config
            .dashboard
            .worktree_info_command()
            .map(str::to_owned)
        else {
            return;
        };
        let due = self
            .last_info_fetch
            .is_none_or(|t| t.elapsed() >= self.config.dashboard.worktree_info_interval());
        if !due {
            return;
        }
        if self
            .is_info_fetching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        self.last_info_fetch = Some(std::time::Instant::now());
        let tx = self.event_tx.clone();
        let is_fetching = self.is_info_fetching.clone();
        std::thread::spawn(move || {
            struct ResetFlag(Arc<AtomicBool>);
            impl Drop for ResetFlag {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _reset = ResetFlag(is_fetching);
            let result = worktree_info::run_provider(&command, worktree_info::PROVIDER_TIMEOUT)
                .map_err(|e| format!("{e:#}"));
            let _ = tx.send(AppEvent::WorktreeInfo(result));
        });
    }

    /// Rebuild the path index from the last good report and the listed worktrees.
    pub fn reindex_worktree_info(&mut self) {
        let Some(report) = &self.worktree_info else {
            return;
        };
        let known: Vec<PathBuf> = self.all_worktrees.iter().map(|w| w.path.clone()).collect();
        self.worktree_info_index = worktree_info::index_report(report, &known);
    }

    /// Spawn background thread to fetch worktree list
    pub(super) fn spawn_worktree_fetch(&self) {
        if self
            .is_worktree_fetching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }

        let tx = self.event_tx.clone();
        let is_fetching = self.is_worktree_fetching.clone();
        let config = self.config.clone();
        let mux = self.mux.clone();
        let repo_override = self
            .worktree_project_override
            .as_ref()
            .map(|(_, p)| p.clone());
        let workspace_dirs = self.config.dashboard.workspaces();
        let agent_roots: Vec<PathBuf> = self.repo_roots.values().cloned().collect();

        std::thread::spawn(move || {
            struct ResetFlag(Arc<AtomicBool>);
            impl Drop for ResetFlag {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _reset = ResetFlag(is_fetching);

            // fetch_pr_status=false: the dashboard fetches PR status separately,
            // and workflow::list's spinner would corrupt the TUI output
            let worktrees = if workspace_dirs.is_empty() || repo_override.is_some() {
                workflow::list_in(&config, mux.as_ref(), false, &[], repo_override.as_deref()).ok()
            } else {
                let repos = workspaces::discover_repos(&workspace_dirs);
                let roots = fetch_roots(&repos, &agent_roots);
                Some(list_all_repos(&config, mux.as_ref(), &roots))
            };
            if let Some(worktrees) = worktrees {
                let names = fetched_project_names(&workspace_dirs, &agent_roots);
                let _ = tx.send(AppEvent::WorktreeList(worktrees, names));
            }
        });
    }

    /// Refresh project names in the background so the Agents tab does not
    /// depend on the Worktrees tab. Only active when workspaces are configured.
    pub(super) fn maybe_spawn_project_names_fetch(&mut self) {
        let workspace_dirs = self.config.dashboard.workspaces();
        if workspace_dirs.is_empty() {
            return;
        }
        let roots_len = self.repo_roots.len();
        if self.project_names_roots_len == Some(roots_len)
            && self.last_project_names_fetch.elapsed() < Duration::from_secs(30)
        {
            return;
        }
        if self
            .is_project_names_fetching
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            return;
        }
        self.project_names_roots_len = Some(roots_len);
        self.last_project_names_fetch = std::time::Instant::now();
        let agent_roots: Vec<PathBuf> = self.repo_roots.values().cloned().collect();
        let tx = self.event_tx.clone();
        let flag = self.is_project_names_fetching.clone();
        std::thread::spawn(move || {
            struct ResetFlag(Arc<AtomicBool>);
            impl Drop for ResetFlag {
                fn drop(&mut self) {
                    self.0.store(false, Ordering::SeqCst);
                }
            }
            let _reset = ResetFlag(flag);
            let names = all_project_names(&workspace_dirs, &agent_roots);
            let _ = tx.send(AppEvent::ProjectNames(names));
        });
    }

    /// Cycle to the next worktree sort mode.
    pub fn cycle_worktree_sort_mode(&mut self) {
        self.worktree_sort_mode = self.worktree_sort_mode.next();
        self.worktree_sort_mode.save();
        self.apply_worktree_filters();
    }

    /// Sort worktrees according to the current sort mode.
    fn sort_worktrees(&mut self) {
        match self.worktree_sort_mode {
            WorktreeSortMode::Natural => {} // Keep original order from git
            WorktreeSortMode::Age => {
                self.worktrees
                    .sort_by_key(|worktree| std::cmp::Reverse(worktree.created_at));
            }
        }
    }

    /// Apply filter text to worktree list and restore selection
    pub(super) fn apply_worktree_filters(&mut self) {
        // Reset from baseline
        self.worktrees = self.all_worktrees.clone();

        // Merge PR data from dashboard's own PR fetching into worktrees
        // (workflow::list is called with fetch_pr_status=false to avoid spinner)
        merge_worktree_prs(&mut self.worktrees, &self.pr_statuses);

        // Unified view: assign agents against every listed worktree once,
        // for both the filter and the rows.
        let (all_paths, assignment) = self.agent_assignment();

        // Apply name filter (unified view: or an agent title of the worktree)
        if !self.worktree_filter_text.is_empty() {
            let filter = self.worktree_filter_text.to_lowercase();
            let by_agent = if self.unified {
                unified::worktrees_visible_by_agent(&assignment, &self.host_agents, &filter)
            } else {
                HashSet::new()
            };
            // `worktrees` is still `all_worktrees` in order: index = position.
            let mut index = 0;
            self.worktrees.retain(|w| {
                let i = index;
                index += 1;
                let handle = w.handle.to_lowercase();
                handle.contains(&filter)
                    || w.branch.to_lowercase().contains(&filter)
                    || by_agent.contains(&i)
            });
        }

        // Sort after filtering
        self.sort_worktrees();

        self.visible_orphans = filter_orphans(
            &self.worktree_info_index.orphans,
            self.worktree_project_override
                .as_ref()
                .map(|(name, _)| name.as_str()),
            &self.worktree_filter_text,
        );

        self.rebuild_display_rows_with(&all_paths, &assignment);

        self.update_worktree_preview();
    }

    /// Paths of every listed worktree and, in the unified view, the agent
    /// assignment against them (empty otherwise). Touches the filesystem:
    /// call it when rebuilding rows, not per frame.
    fn agent_assignment(&self) -> (Vec<PathBuf>, Vec<Option<usize>>) {
        if !self.unified {
            return (Vec::new(), Vec::new());
        }
        let all: Vec<PathBuf> = self.all_worktrees.iter().map(|w| w.path.clone()).collect();
        let assignment = unified::assign_agents(&all, &self.host_agents);
        (all, assignment)
    }

    fn compute_display_rows(
        &self,
        all_paths: &[PathBuf],
        assignment: &[Option<usize>],
    ) -> Vec<DisplayRow> {
        let visible: Vec<PathBuf> = self.worktrees.iter().map(|w| w.path.clone()).collect();
        display_rows_for(
            self.unified,
            &RowInputs {
                visible_worktrees: &visible,
                all_worktrees: all_paths,
                agents: if self.unified { &self.host_agents } else { &[] },
                assignment,
                orphan_count: self.visible_orphans.len(),
                filter: &self.worktree_filter_text,
                outside_group: self.worktree_project_override.is_none() && self.worktrees_loaded,
            },
        )
    }

    /// Rebuild the Worktrees table rows from the current worktrees, orphans
    /// and agents, and restore the selection.
    pub(super) fn rebuild_display_rows(&mut self) {
        let (all_paths, assignment) = self.agent_assignment();
        self.rebuild_display_rows_with(&all_paths, &assignment);
    }

    fn rebuild_display_rows_with(&mut self, all_paths: &[PathBuf], assignment: &[Option<usize>]) {
        self.display_rows = self.compute_display_rows(all_paths, assignment);
        self.restore_worktree_selection();
    }

    /// Restore the selection after a rebuild: by pane id for an agent
    /// sub-row, by path for worktree and orphan rows, else the first
    /// selectable row.
    fn restore_worktree_selection(&mut self) {
        let (worktrees, orphans) = (&self.worktrees, &self.visible_orphans);
        let path_match = self.selected_worktree_path.as_ref().map(|path| {
            move |row: &DisplayRow| match row {
                DisplayRow::Worktree(i) => worktrees.get(*i).is_some_and(|w| &w.path == path),
                DisplayRow::Orphan(i) => orphans.get(*i).is_some_and(|o| &o.path == path),
                _ => false,
            }
        });
        let restored = restore_row(
            &self.display_rows,
            self.selected_agent_pane.as_deref(),
            path_match
                .as_ref()
                .map(|m| m as &dyn Fn(&DisplayRow) -> bool),
        );
        match restored {
            Restored::Found(index) => {
                self.worktree_table_state.select(Some(index));
                self.set_selection_identity(index);
            }
            Restored::Lost(index) => {
                self.worktree_table_state.select(index);
                match index {
                    // Unified rows reorder as agents change: follow the new row.
                    Some(index) if self.unified => self.set_selection_identity(index),
                    // Upstream: keep the index, forget the identity.
                    _ => {
                        self.selected_worktree_path = None;
                        self.selected_agent_pane = None;
                    }
                }
            }
            Restored::NoIdentity => match self.worktree_table_state.selected() {
                None => {
                    if let Some(index) = first_selectable(&self.display_rows) {
                        self.worktree_table_state.select(Some(index));
                        self.set_selection_identity(index);
                    }
                }
                // Keep the index while it is still a selectable row.
                Some(index)
                    if self
                        .display_rows
                        .get(index)
                        .is_some_and(unified::is_selectable) => {}
                Some(_) => self
                    .worktree_table_state
                    .select(first_selectable(&self.display_rows)),
            },
        }
    }

    /// Remember what the row at `index` is, to find it again after a rebuild.
    fn set_selection_identity(&mut self, index: usize) {
        let row = self.display_rows.get(index);
        self.selected_agent_pane = match row {
            Some(DisplayRow::Agent { pane_id, .. }) => Some(pane_id.clone()),
            _ => None,
        };
        // An agent sub-row falls back to its parent worktree.
        self.selected_worktree_path = match row {
            Some(DisplayRow::Orphan(i)) => self.visible_orphans.get(*i).map(|o| o.path.clone()),
            Some(row) => worktree_index_of(row)
                .and_then(|i| self.worktrees.get(i))
                .map(|w| w.path.clone()),
            None => None,
        };
    }

    /// Number of table rows (selectable or not): `display_rows`.
    pub fn worktree_row_count(&self) -> usize {
        self.display_rows.len()
    }

    /// The row under the selection.
    pub fn selected_row(&self) -> Option<&DisplayRow> {
        self.display_rows.get(self.worktree_table_state.selected()?)
    }

    /// The worktree the selection stands for: a worktree row, or the parent
    /// of an agent sub-row. `None` on orphans, outside-group agents and the
    /// header.
    pub fn selected_worktree_index(&self) -> Option<usize> {
        self.selected_row().and_then(worktree_index_of)
    }

    /// The worktree the selection stands for, if it is still listed.
    pub fn selected_worktree(&self) -> Option<&workflow::types::WorktreeInfo> {
        self.worktrees.get(self.selected_worktree_index()?)
    }

    /// The worktree a row action targets. On an agent outside every
    /// worktree, shows "not in a worktree" and returns `None`.
    fn action_worktree_index(&mut self) -> Option<usize> {
        if self.outside_agent_blocked() {
            return None;
        }
        self.selected_worktree_index()
            .filter(|&i| i < self.worktrees.len())
    }

    /// True, with the "not in a worktree" message, when the selection is an
    /// agent outside every worktree (worktree actions do nothing there).
    pub(super) fn outside_agent_blocked(&mut self) -> bool {
        if self.selected_row().is_some_and(is_outside_agent) {
            self.status_message = Some((NOT_IN_WORKTREE.to_string(), std::time::Instant::now()));
            return true;
        }
        false
    }

    /// The orphan row under the selection, if the selection is on one.
    pub fn selected_orphan(&self) -> Option<&OrphanEntry> {
        match self.selected_row()? {
            DisplayRow::Orphan(i) => self.visible_orphans.get(*i),
            _ => None,
        }
    }

    /// Path identifying the row at `index` (worktree or orphan).
    pub fn row_path(&self, index: usize) -> Option<PathBuf> {
        match self.display_rows.get(index)? {
            DisplayRow::Worktree(i) => self.worktrees.get(*i).map(|w| w.path.clone()),
            DisplayRow::Orphan(i) => self.visible_orphans.get(*i).map(|o| o.path.clone()),
            DisplayRow::Agent { .. } | DisplayRow::OutsideHeader => None,
        }
    }

    pub fn select_worktree(&mut self, index: usize) {
        if !self
            .display_rows
            .get(index)
            .is_some_and(unified::is_selectable)
        {
            return;
        }
        self.worktree_table_state.select(Some(index));
        self.set_selection_identity(index);
        self.update_worktree_preview();
    }

    pub fn worktree_next(&mut self) {
        let current = self.worktree_table_state.selected();
        if let Some(index) = step_selectable(&self.display_rows, current, true) {
            self.select_worktree(index);
        }
    }

    pub fn worktree_previous(&mut self) {
        let current = self.worktree_table_state.selected();
        if let Some(index) = step_selectable(&self.display_rows, current, false) {
            self.select_worktree(index);
        }
    }

    /// Jump to the `index`-th worktree row. Only worktree rows have jump numbers.
    pub fn worktree_jump_to_index(&mut self, index: usize) {
        if let Some(index) = nth_worktree_row(&self.display_rows, index) {
            self.worktree_table_state.select(Some(index));
            self.set_selection_identity(index);
            self.jump_to_selected_worktree();
        }
    }

    /// Show the remove confirmation modal for the selected worktree.
    /// Always shows the modal (even for clean worktrees). Skips main worktree.
    /// Works from both the worktrees tab (uses selected worktree) and agents tab
    /// (finds the worktree matching the selected agent's path).
    pub fn remove_selected_worktree(&mut self) {
        let worktree = match self.active_tab {
            DashboardTab::Worktrees => {
                let Some(selected) = self.action_worktree_index() else {
                    return;
                };
                self.worktrees.get(selected).cloned()
            }
            DashboardTab::Agents => {
                let Some(selected) = self.table_state.selected() else {
                    return;
                };
                let Some(agent) = self.agents.get(selected) else {
                    return;
                };
                let agent_path = &agent.path;
                self.worktrees
                    .iter()
                    .find(|w| w.path == *agent_path)
                    .cloned()
            }
        };

        let Some(worktree) = worktree else {
            return;
        };

        // Block removal of main worktree
        if worktree.is_main || self.removals.contains(&worktree.path) {
            return;
        }

        let is_dirty = git::has_uncommitted_changes(&worktree.path).unwrap_or(true);

        self.pending_remove = Some(RemovePlan {
            handle: worktree.handle.clone(),
            path: worktree.path.clone(),
            is_dirty,
            is_unmerged: worktree.has_unmerged,
            keep_branch: false,
            force_armed: false,
        });
    }

    /// Toggle keep-branch in the pending remove plan.
    pub fn toggle_remove_keep_branch(&mut self) {
        if let Some(ref mut plan) = self.pending_remove {
            plan.keep_branch = !plan.keep_branch;
        }
    }

    /// Arm force mode for dirty worktree removal.
    pub fn arm_remove_force(&mut self) {
        if let Some(ref mut plan) = self.pending_remove
            && plan.is_dirty
        {
            plan.force_armed = true;
        }
    }

    /// Execute the pending remove confirmation.
    pub fn confirm_remove(&mut self) {
        let Some(plan) = self.pending_remove.take() else {
            return;
        };

        // Dirty worktrees require force to be armed
        if plan.is_dirty && !plan.force_armed {
            self.pending_remove = Some(plan);
            return;
        }

        if !self.removals.start(plan.path.clone()) {
            return;
        }
        let mux = self.mux.clone();
        let tx = self.event_tx.clone();
        std::thread::spawn(move || {
            let result = remove_worktree(&plan.handle, &plan.path, plan.keep_branch, mux)
                .map_err(|error| format!("{error:#}"));
            let _ = tx.send(AppEvent::RemoveWorktreeResult(plan.path, result));
        });
    }

    pub(super) fn handle_remove_worktree_result(
        &mut self,
        path: PathBuf,
        result: Result<workflow::types::RemoveResult, String>,
    ) {
        let handle = path.file_name().unwrap_or_default().to_string_lossy();
        let removed = result
            .as_ref()
            .is_ok_and(|result| !result.cleanup_scheduled);
        self.removals.finish(&path, removed);
        match result {
            Ok(result) if result.cleanup_scheduled => {
                self.status_message = Some((
                    format!("Removal scheduled for '{handle}'"),
                    std::time::Instant::now(),
                ));
            }
            Ok(_) => {
                self.all_worktrees.retain(|wt| wt.path != path);
                self.apply_worktree_filters();
            }
            Err(error) => {
                self.status_message = Some((
                    format!("Failed to remove '{handle}': {error}"),
                    std::time::Instant::now(),
                ));
            }
        }
        self.trigger_worktree_refetch();
    }

    /// Close the mux window/session for the selected worktree without removing it.
    pub fn close_selected_worktree_window(&mut self) {
        let Some(selected) = self.action_worktree_index() else {
            return;
        };
        let Some(worktree) = self.worktrees.get(selected).cloned() else {
            return;
        };

        if worktree.is_main || !worktree.has_mux_window || self.removals.contains(&worktree.path) {
            return;
        }

        // The window name prefix comes from the row repository's config.
        let row_config = match crate::config::Config::load_with_location_from(&worktree.path, None)
        {
            Ok((config, _)) => config,
            Err(e) => {
                self.set_row_config_error(&worktree.handle, &e);
                return;
            }
        };
        let prefix = row_config.window_prefix();
        if worktree.mode == crate::config::MuxMode::Window {
            let repo = Some(worktree.path.as_path());
            let target_name = crate::git::get_worktree_target_window_in(&worktree.handle, repo)
                .unwrap_or_else(|| worktree.handle.clone());
            let full_name = crate::multiplexer::util::prefixed(prefix, &target_name);
            let parent_session = crate::git::get_worktree_window_session_in(&worktree.handle, repo);
            let window_token = self
                .mux
                .supports_window_ownership()
                .then(|| crate::git::get_worktree_window_token_in(&worktree.handle, repo))
                .flatten();
            let target = if let Some(token) = window_token {
                self.mux
                    .resolve_owned_window_targets(
                        &token,
                        &full_name,
                        parent_session.as_deref(),
                        &worktree.path,
                    )
                    .ok()
                    .and_then(|targets| {
                        targets
                            .iter()
                            .find(|owned| owned.is_primary)
                            .map(|owned| owned.target.clone())
                    })
            } else {
                Some(crate::multiplexer::WindowTarget::new(
                    full_name,
                    parent_session,
                ))
            };
            if let Some(target) = target {
                let _ = self.mux.kill_window_target(&target);
            }
        } else {
            let target_name = crate::git::get_worktree_target_session_in(
                &worktree.handle,
                Some(worktree.path.as_path()),
            )
            .unwrap_or_else(|| worktree.handle.clone());
            let full_name = crate::multiplexer::util::prefixed(prefix, &target_name);
            let _ = crate::multiplexer::handle::MuxHandle::kill_full(
                self.mux.as_ref(),
                worktree.mode,
                &full_name,
            );
        }
        self.trigger_worktree_refetch();
    }

    /// Build the sweep candidate list and open the sweep modal.
    /// If worktree data hasn't been loaded yet, triggers a background fetch
    /// and opens an empty sweep modal (data will arrive on next refresh).
    pub fn start_sweep(&mut self) {
        // Ensure worktree data is loaded (may not be if called from agents view)
        if self.worktrees.is_empty() {
            self.spawn_worktree_fetch();
        }

        // Resolve gone branches per repository: rows may span several repos.
        let mut gone_by_repo: HashMap<PathBuf, HashSet<String>> = HashMap::new();
        for wt in &self.worktrees {
            gone_by_repo
                .entry(wt.repo_key.clone())
                .or_insert_with(|| git::get_gone_branches_in(Some(&wt.path)).unwrap_or_default());
        }
        let no_gone = HashSet::new();

        let mut candidates: Vec<SweepCandidate> = Vec::new();

        for wt in &self.worktrees {
            if wt.is_main || self.removals.contains(&wt.path) {
                continue;
            }

            let gone = gone_by_repo.get(&wt.repo_key).unwrap_or(&no_gone);
            let status = self.git_statuses.get(&wt.path);
            let is_dirty = status.is_some_and(|s| s.is_dirty);
            let has_upstream = status.is_some_and(|s| s.has_upstream);

            // Determine reason: PR merged > PR closed > upstream gone > merged locally
            let reason = if let Some(ref pr) = wt.pr_info {
                match pr.state.as_str() {
                    "MERGED" => Some(SweepReason::PrMerged),
                    "CLOSED" => Some(SweepReason::PrClosed),
                    _ => {
                        if gone.contains(&wt.branch) {
                            Some(SweepReason::UpstreamGone)
                        } else {
                            None
                        }
                    }
                }
            } else if gone.contains(&wt.branch) {
                Some(SweepReason::UpstreamGone)
            } else if !has_upstream && !wt.has_unmerged {
                Some(SweepReason::MergedLocally)
            } else {
                None
            };

            let Some(reason) = reason else { continue };

            candidates.push(SweepCandidate {
                handle: wt.handle.clone(),
                path: wt.path.clone(),
                reason,
                is_dirty,
                selected: !is_dirty, // Pre-select non-dirty candidates
            });
        }

        self.pending_sweep = Some(SweepState {
            candidates,
            cursor: 0,
        });
    }

    /// Toggle selection of the current sweep candidate.
    pub fn sweep_toggle(&mut self) {
        if let Some(ref mut sweep) = self.pending_sweep
            && let Some(candidate) = sweep.candidates.get_mut(sweep.cursor)
            && !candidate.is_dirty
        {
            candidate.selected = !candidate.selected;
        }
    }

    /// Move cursor up in sweep modal.
    pub fn sweep_up(&mut self) {
        if let Some(ref mut sweep) = self.pending_sweep {
            sweep.cursor = sweep.cursor.saturating_sub(1);
        }
    }

    /// Move cursor down in sweep modal.
    pub fn sweep_down(&mut self) {
        if let Some(ref mut sweep) = self.pending_sweep
            && sweep.cursor + 1 < sweep.candidates.len()
        {
            sweep.cursor += 1;
        }
    }

    /// Execute sweep: remove all selected candidates in a background thread.
    pub fn confirm_sweep(&mut self) {
        let Some(sweep) = self.pending_sweep.take() else {
            return;
        };

        let paths_to_remove: Vec<(String, PathBuf)> = sweep
            .candidates
            .into_iter()
            .filter(|c| c.selected)
            .map(|c| (c.handle, c.path))
            .collect();

        if paths_to_remove.is_empty() {
            return;
        }

        let total = paths_to_remove.len();
        let mux = self.mux.clone();
        let tx = self.event_tx.clone();

        self.sweep_progress = Some(SweepProgress {
            total,
            current: 1,
            handle: paths_to_remove[0].0.clone(),
        });

        std::thread::spawn(move || {
            let mut outcome = SweepOutcome {
                completed: 0,
                scheduled: 0,
            };
            let mut failures = 0;
            for (i, (handle, path)) in paths_to_remove.iter().enumerate() {
                let _ = tx.send(AppEvent::SweepProgressUpdate(i + 1, total, handle.clone()));

                match remove_worktree(handle, path, false, mux.clone()) {
                    Ok(result) if result.cleanup_scheduled => outcome.scheduled += 1,
                    Ok(_) => outcome.completed += 1,
                    Err(_) => failures += 1,
                }
            }

            if failures > 0 {
                let _ = tx.send(AppEvent::SweepComplete(Err(format!(
                    "Completed {}, scheduled {}, failed {} of {} worktrees",
                    outcome.completed, outcome.scheduled, failures, total
                ))));
            } else {
                let _ = tx.send(AppEvent::SweepComplete(Ok(outcome)));
            }
        });
    }

    // ── Project picker methods ─────────────────────────────────────

    /// Discover projects from cached repo roots and open the picker modal.
    pub fn show_project_picker(&mut self) {
        let workspace_dirs = self.config.dashboard.workspaces();
        let mut repos: Vec<(PathBuf, PathBuf)> = workspaces::discover_repos(&workspace_dirs)
            .into_iter()
            .map(|r| (r.repo_key, r.main_root))
            .collect();
        for (agent_path, root) in &self.repo_roots {
            if let Some(key) = self.repo_keys.get(agent_path) {
                repos.push((key.clone(), root.clone()));
            }
        }
        let projects = project_entries(&workspace_dirs, repos);

        let current_name = match &self.worktree_project_override {
            Some((name, _)) => Some(name.clone()),
            None if !workspace_dirs.is_empty() => Some(ALL_PROJECTS.to_string()),
            None => self
                .current_worktree
                .as_deref()
                .map(agent::extract_project_name),
        };

        let initial_cursor = current_name
            .as_ref()
            .and_then(|name| projects.iter().position(|p| &p.name == name))
            .unwrap_or(0);

        self.pending_project_picker = Some(ProjectPicker {
            projects,
            cursor: initial_cursor,
            filter: String::new(),
            current_name,
        });
    }

    /// Move cursor down in project picker.
    pub fn project_picker_down(&mut self) {
        if let Some(ref mut picker) = self.pending_project_picker {
            let filtered = picker.filtered();
            if !filtered.is_empty() && picker.cursor + 1 < filtered.len() {
                picker.cursor += 1;
            }
        }
    }

    /// Move cursor up in project picker.
    pub fn project_picker_up(&mut self) {
        if let Some(ref mut picker) = self.pending_project_picker {
            picker.cursor = picker.cursor.saturating_sub(1);
        }
    }

    /// Append a character to the project picker filter.
    pub fn project_picker_filter_append(&mut self, c: char) {
        if let Some(ref mut picker) = self.pending_project_picker {
            picker.filter.push(c);
            picker.cursor = 0;
        }
    }

    /// Delete the last character from the project picker filter.
    pub fn project_picker_filter_delete(&mut self) {
        if let Some(ref mut picker) = self.pending_project_picker {
            picker.filter.pop();
            picker.cursor = 0;
        }
    }

    /// Confirm project picker selection: set override and trigger refetch.
    pub fn confirm_project_picker(&mut self) {
        let Some(picker) = self.pending_project_picker.take() else {
            return;
        };
        let filtered = picker.filtered();
        let Some(&idx) = filtered.get(picker.cursor) else {
            return;
        };
        let selected = &picker.projects[idx];

        self.worktree_project_override = override_for(selected);
        self.worktrees.clear();
        self.all_worktrees.clear();
        self.worktrees_loaded = false;
        // Rows only: the selection is restored when the new list arrives.
        let (all_paths, assignment) = self.agent_assignment();
        self.display_rows = self.compute_display_rows(&all_paths, &assignment);
        self.last_worktree_fetch = std::time::Instant::now();
        self.spawn_worktree_fetch();

        // Switch to worktrees tab to show the result
        if self.active_tab != DashboardTab::Worktrees {
            self.active_tab = DashboardTab::Worktrees;
        }
    }

    // ── Base branch picker methods ──────────────────────────────────

    /// Open the base branch picker for the selected worktree (works from both tabs).
    pub fn show_base_branch_picker(&mut self) {
        // Resolve repo path, branch, and current base from whichever tab is active
        let (repo_path, worktree_branch, current_base) = match self.active_tab {
            DashboardTab::Worktrees => {
                let Some(selected) = self.action_worktree_index() else {
                    return;
                };
                let Some(worktree) = self.worktrees.get(selected) else {
                    return;
                };
                if worktree.is_main || worktree.branch == "(detached)" {
                    return;
                }
                (
                    worktree.path.clone(),
                    worktree.branch.clone(),
                    worktree.base_branch.clone(),
                )
            }
            DashboardTab::Agents => {
                let Some(selected) = self.table_state.selected() else {
                    return;
                };
                let Some(agent) = self.agents.get(selected) else {
                    return;
                };
                let path = agent.path.clone();
                let branch = match git::get_current_branch_in(&path) {
                    Ok(b) if !b.is_empty() && b != "(detached)" => b,
                    _ => return,
                };
                let base = git::get_branch_base_in(&branch, Some(&path)).ok();
                (path, branch, base)
            }
        };

        // List local branches, excluding the worktree's own branch
        let Some(branches) = list_branches_for_status(self, &repo_path) else {
            return;
        };
        let mut branches: Vec<_> = branches
            .into_iter()
            .filter(|b| *b != worktree_branch)
            .collect();

        // Pin current base to the top if it exists in the list
        if let Some(ref base) = current_base
            && let Some(pos) = branches.iter().position(|b| b == base)
        {
            let pinned = branches.remove(pos);
            branches.insert(0, pinned);
        }

        let initial_cursor = 0;

        self.pending_base_picker = Some(BaseBranchPicker {
            branches,
            cursor: initial_cursor,
            filter: String::new(),
            current_base,
            worktree_branch,
            repo_path,
        });
    }

    /// Move cursor down in base branch picker.
    pub fn base_picker_down(&mut self) {
        if let Some(ref mut picker) = self.pending_base_picker {
            let filtered = picker.filtered();
            if !filtered.is_empty() && picker.cursor + 1 < filtered.len() {
                picker.cursor += 1;
            }
        }
    }

    /// Move cursor up in base branch picker.
    pub fn base_picker_up(&mut self) {
        if let Some(ref mut picker) = self.pending_base_picker {
            picker.cursor = picker.cursor.saturating_sub(1);
        }
    }

    /// Append a character to the base branch picker filter.
    pub fn base_picker_filter_append(&mut self, c: char) {
        if let Some(ref mut picker) = self.pending_base_picker {
            picker.filter.push(c);
            picker.cursor = 0;
        }
    }

    /// Delete the last character from the base branch picker filter.
    pub fn base_picker_filter_delete(&mut self) {
        if let Some(ref mut picker) = self.pending_base_picker {
            picker.filter.pop();
            picker.cursor = 0;
        }
    }

    /// Confirm base branch picker selection: set the base and trigger refetch.
    pub fn confirm_base_picker(&mut self) {
        let Some(picker) = self.pending_base_picker.take() else {
            return;
        };
        let filtered = picker.filtered();
        let Some(&idx) = filtered.get(picker.cursor) else {
            return;
        };
        let new_base = &picker.branches[idx];

        if let Err(e) =
            git::set_branch_base_in(&picker.worktree_branch, new_base, Some(&picker.repo_path))
        {
            self.status_message = Some((
                format!("Failed to set base: {}", e),
                std::time::Instant::now(),
            ));
            return;
        }

        // Update in-memory state immediately so the UI reflects the change
        let new_base_owned = new_base.to_string();
        for wt in self
            .all_worktrees
            .iter_mut()
            .chain(self.worktrees.iter_mut())
        {
            if wt.branch == picker.worktree_branch {
                wt.base_branch = Some(new_base_owned.clone());
            }
        }
        // Also update the cached GitStatus so the Git column and detail panel refresh
        if let Some(status) = self.git_statuses.get_mut(&picker.repo_path) {
            status.base_branch = new_base_owned.clone();
        }

        self.status_message = Some((
            format!(
                "Base for '{}' set to '{}'",
                picker.worktree_branch, new_base
            ),
            std::time::Instant::now(),
        ));

        self.trigger_worktree_refetch();
    }

    /// Footer message for a row action whose repository config failed to load.
    fn set_row_config_error(&mut self, handle: &str, error: &anyhow::Error) {
        self.status_message = Some((row_config_error(handle, error), std::time::Instant::now()));
    }

    /// Open a multiplexer window or session for the selected worktree.
    pub fn open_selected_worktree(&mut self) {
        let Some(selected) = self.action_worktree_index() else {
            return;
        };
        let Some(worktree) = self.worktrees.get(selected) else {
            return;
        };

        if self.removals.contains(&worktree.path) {
            return;
        }

        let handle = worktree.handle.clone();
        let path = worktree.path.clone();

        // Act in the row's repository, with its configuration, not the
        // dashboard's cwd.
        let (config, location) = match crate::config::Config::load_with_location_from(&path, None) {
            Ok(loaded) => loaded,
            Err(e) => {
                self.set_row_config_error(&handle, &e);
                return;
            }
        };
        let mode = config.mode();
        let ctx = match workflow::WorkflowContext::new_in(&path, config, self.mux.clone(), location)
        {
            Ok(ctx) => ctx,
            Err(e) => {
                self.status_message = Some((
                    format!("Cannot open '{handle}': {e:#}"),
                    std::time::Instant::now(),
                ));
                return;
            }
        };

        let mut options = workflow::types::SetupOptions::new(false, false, true);
        options.mode = mode;
        if workflow::open(&handle, &ctx, options, false, None, None, None).is_ok()
            && self.should_close_after_jump()
        {
            self.should_jump = true;
        }
    }

    /// Jump to the selected row's agent or mux window.
    /// An agent sub-row jumps to its pane; a worktree row to its first agent
    /// (sub-row in the unified view, assigned agent otherwise), else falls
    /// back to workflow::open which switches to an existing window/session
    /// or creates one.
    pub fn jump_to_selected_worktree(&mut self) {
        let Some(selected) = self.worktree_table_state.selected() else {
            return;
        };
        if self
            .selected_worktree()
            .is_some_and(|w| self.removals.contains(&w.path))
        {
            return;
        }

        match enter_target(&self.display_rows, selected) {
            Some(EnterTarget::Pane(target)) => self.switch_to_pane_and_track(&target),
            Some(EnterTarget::Worktree(index)) => {
                let Some(worktree) = self.worktrees.get(index) else {
                    return;
                };
                // Agents tab mode: no sub-rows, so look the agent up (an
                // agent in a subdirectory of the worktree counts too).
                if !self.unified {
                    let all: Vec<PathBuf> =
                        self.all_worktrees.iter().map(|w| w.path.clone()).collect();
                    if let Some(target) =
                        first_agent_in_worktree(&worktree.path, &all, &self.all_agents)
                    {
                        self.switch_to_pane_and_track(&target);
                        return;
                    }
                }
                // Fall back to workflow::open (switches to existing or creates new)
                self.open_selected_worktree();
            }
            None => {}
        }
    }

    // ── Add worktree methods ───────────────────────────────────────

    /// Get the repo path for the current worktree view context.
    fn worktree_repo_path(&self) -> Option<PathBuf> {
        let all_mode = !self.config.dashboard.workspaces().is_empty();
        choose_repo_path(
            self.worktree_project_override.as_ref().map(|(_, p)| p),
            all_mode,
            self.repo_selection(),
            self.current_worktree.clone(),
            self.worktrees.first().map(|w| w.path.clone()),
        )
    }

    /// The active tab's selection, as far as it identifies a repository.
    fn repo_selection(&self) -> RepoSelection {
        match self.active_tab {
            DashboardTab::Worktrees => {
                if let Some(worktree) = self.selected_worktree() {
                    return RepoSelection::Worktree(worktree.path.clone());
                }
                match self.selected_row() {
                    Some(DisplayRow::Orphan(_)) => RepoSelection::Orphan,
                    _ => RepoSelection::Nothing,
                }
            }
            DashboardTab::Agents => {
                let root = self
                    .table_state
                    .selected()
                    .and_then(|i| self.agents.get(i))
                    .and_then(|agent| self.repo_roots.get(&agent.path))
                    .cloned();
                RepoSelection::Agent(root)
            }
        }
    }

    /// Open the add-worktree modal with unified picker (branches pre-fetched).
    pub fn show_add_worktree(&mut self) {
        if matches!(self.repo_selection(), RepoSelection::Orphan) {
            self.status_message = Some((
                "Missing worktree: no repository to add to".to_string(),
                std::time::Instant::now(),
            ));
            return;
        }
        let Some(repo_path) = self.worktree_repo_path() else {
            self.status_message = Some((
                "No project context available".to_string(),
                std::time::Instant::now(),
            ));
            return;
        };

        let Some(branches) = list_branches_for_status(self, &repo_path) else {
            return;
        };

        let default_branch = default_add_worktree_base(&repo_path);

        // Collect branches that already have worktrees in the target repository
        let repo_key = git::get_repository_key_in(Some(&repo_path)).ok();
        let occupied_branches = occupied_branches(&self.worktrees, repo_key.as_deref());

        self.pending_add_worktree = Some(AddWorktreeState {
            branches,
            occupied_branches,
            cursor: 0,
            filter: String::new(),
            tab_prefix: None,
            base_branch: default_branch,
            editing_base: false,
            base_filter: String::new(),
            base_tab_prefix: None,
            repo_path,
            mode: AddWorktreeMode::Branch,
            pr_list: None,
            pr_request_counter: 0,
        });
    }

    /// Append a character to the add-worktree filter/name input.
    pub fn add_worktree_append(&mut self, c: char) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.filter.push(c);
            state.tab_prefix = None;
            state.cursor = 0;
        }
    }

    /// Delete the last character from the add-worktree filter/name input.
    pub fn add_worktree_delete(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.filter.pop();
            state.tab_prefix = None;
            state.cursor = 0;
        }
    }

    /// Delete the last word from the add-worktree filter (Ctrl+w).
    pub fn add_worktree_delete_word(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            delete_word_backward(&mut state.filter);
            state.tab_prefix = None;
            state.cursor = 0;
        }
    }

    /// Clear the add-worktree filter (Ctrl+u).
    pub fn add_worktree_clear(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.filter.clear();
            state.tab_prefix = None;
            state.cursor = 0;
        }
    }

    /// Move cursor down in the add-worktree picker.
    pub fn add_worktree_down(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            let max_idx = match state.mode {
                AddWorktreeMode::Branch => {
                    let has_create_row = !state.filter.trim().is_empty();
                    if has_create_row {
                        state.selectable_count()
                    } else {
                        state.selectable_count().saturating_sub(1)
                    }
                }
                AddWorktreeMode::Pr => state.filtered_prs().len().saturating_sub(1),
            };
            if state.cursor < max_idx {
                state.cursor += 1;
            }
        }
    }

    /// Move cursor up in the add-worktree picker.
    pub fn add_worktree_up(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.cursor = state.cursor.saturating_sub(1);
        }
    }

    /// Tab-complete: cycle through matching branch names.
    /// First press saves the typed prefix and fills the first match.
    /// Subsequent presses cycle to the next match, wrapping around.
    pub fn add_worktree_tab_complete(&mut self) {
        let Some(ref mut state) = self.pending_add_worktree else {
            return;
        };
        if state.mode == AddWorktreeMode::Pr {
            return;
        }

        if let Some(completion) = cycle_branch_completion(
            &state.branches,
            &mut state.tab_prefix,
            &mut state.filter,
            Some(&state.occupied_branches),
        ) {
            state.filter = completion.branch;
            if let Some(cursor) = completion.picker_cursor {
                state.cursor = cursor;
            }
        }
    }

    /// Toggle between Branch and PR modes (Ctrl+p).
    pub fn add_worktree_toggle_pr_mode(&mut self) {
        let Some(ref mut state) = self.pending_add_worktree else {
            return;
        };

        match state.mode {
            AddWorktreeMode::Branch => {
                state.mode = AddWorktreeMode::Pr;
                state.cursor = 0;
                state.editing_base = false;
                // Start async fetch if not already loaded
                if state.pr_list.is_none() {
                    state.pr_request_counter += 1;
                    let request_id = state.pr_request_counter;
                    state.pr_list = Some(PrListState::Loading);
                    let tx = self.event_tx.clone();
                    let repo_path = state.repo_path.clone();
                    std::thread::spawn(move || {
                        let result = crate::github::list_open_prs(&repo_path);
                        match result {
                            Ok(prs) => {
                                let _ = tx.send(AppEvent::AddWorktreePrList(request_id, Ok(prs)));
                            }
                            Err(e) => {
                                let _ = tx.send(AppEvent::AddWorktreePrList(
                                    request_id,
                                    Err(e.to_string()),
                                ));
                            }
                        }
                    });
                }
            }
            AddWorktreeMode::Pr => {
                state.mode = AddWorktreeMode::Branch;
                state.cursor = 0;
            }
        }
    }

    /// Toggle base branch editing mode (Ctrl+b).
    pub fn add_worktree_toggle_base(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            // No base editing in PR mode
            if state.mode == AddWorktreeMode::Pr {
                return;
            }
            if state.editing_base {
                // Accept current base_filter as the base branch if non-empty
                let text = state.base_filter.trim().to_string();
                if !text.is_empty() {
                    state.base_branch = text;
                }
                state.editing_base = false;
                state.base_filter.clear();
                state.base_tab_prefix = None;
            } else {
                state.editing_base = true;
                state.base_filter = state.base_branch.clone();
                state.base_tab_prefix = None;
            }
        }
    }

    /// Tab-complete for the base branch field.
    pub fn add_worktree_base_tab_complete(&mut self) {
        let Some(ref mut state) = self.pending_add_worktree else {
            return;
        };

        if let Some(completion) = cycle_branch_completion(
            &state.branches,
            &mut state.base_tab_prefix,
            &mut state.base_filter,
            None,
        ) {
            state.base_filter = completion.branch;
        }
    }

    /// Append a character to the base branch filter.
    pub fn add_worktree_base_append(&mut self, c: char) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.base_filter.push(c);
            state.base_tab_prefix = None;
        }
    }

    /// Delete last character from the base branch filter.
    pub fn add_worktree_base_delete(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.base_filter.pop();
            state.base_tab_prefix = None;
        }
    }

    /// Delete last word from the base branch filter (Ctrl+w).
    pub fn add_worktree_base_delete_word(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            delete_word_backward(&mut state.base_filter);
            state.base_tab_prefix = None;
        }
    }

    /// Clear the base branch filter (Ctrl+u).
    pub fn add_worktree_base_clear(&mut self) {
        if let Some(ref mut state) = self.pending_add_worktree {
            state.base_filter.clear();
            state.base_tab_prefix = None;
        }
    }

    /// Handle Enter - create the worktree or checkout a PR.
    pub fn add_worktree_confirm_selection(&mut self) {
        let Some(ref mut state) = self.pending_add_worktree else {
            return;
        };

        // If editing base, confirm the base field first
        if state.editing_base {
            self.add_worktree_toggle_base();
            return;
        }

        match state.mode {
            AddWorktreeMode::Branch => {
                if state.cursor == 0 {
                    // Check for PR number detection (e.g. "#123" or "123")
                    if let Some(pr_number) = state.detected_pr_number() {
                        let repo_path = state.repo_path.clone();
                        self.pending_add_worktree = None;
                        self.do_checkout_pr(pr_number, repo_path);
                        return;
                    }

                    // "Create new branch" selected
                    let name = state.filter.trim().to_string();
                    if name.is_empty() {
                        return;
                    }
                    let base = state.base_branch.clone();
                    let repo_path = state.repo_path.clone();
                    self.pending_add_worktree = None;
                    self.do_create_worktree(name, Some(base), repo_path);
                } else {
                    // Existing branch selected
                    let filtered = state.filtered();
                    let Some(&idx) = filtered.get(state.cursor - 1) else {
                        return;
                    };
                    let branch = state.branches[idx].clone();
                    let repo_path = state.repo_path.clone();
                    self.pending_add_worktree = None;
                    self.do_create_worktree(branch, None, repo_path);
                }
            }
            AddWorktreeMode::Pr => {
                let filtered = state.filtered_prs();
                let Some(&idx) = filtered.get(state.cursor) else {
                    return;
                };
                let prs = match &state.pr_list {
                    Some(PrListState::Loaded { prs, .. }) => prs,
                    _ => return,
                };
                let pr_number = prs[idx].number;
                let repo_path = state.repo_path.clone();
                self.pending_add_worktree = None;
                self.do_checkout_pr(pr_number, repo_path);
            }
        }
    }

    /// Checkout a PR in a background thread (quiet, no stdout/spinner).
    fn do_checkout_pr(&mut self, pr_number: u32, repo_path: PathBuf) {
        spawn_add_worktree_job(
            AddWorktreeJob::CheckoutPr { pr_number },
            repo_path,
            self.mux.clone(),
            self.event_tx.clone(),
        );

        self.status_message = Some((
            format!("Checking out PR #{}...", pr_number),
            std::time::Instant::now(),
        ));
    }

    /// Execute worktree creation in a background thread.
    fn do_create_worktree(
        &mut self,
        name: String,
        base_branch: Option<String>,
        repo_path: PathBuf,
    ) {
        let status_name = name.clone();
        spawn_add_worktree_job(
            AddWorktreeJob::CreateBranch { name, base_branch },
            repo_path,
            self.mux.clone(),
            self.event_tx.clone(),
        );

        self.status_message = Some((
            format!("Creating worktree '{}'...", status_name),
            std::time::Instant::now(),
        ));
    }

    /// Handle the result of a background add-worktree operation.
    pub fn handle_add_worktree_result(&mut self, result: Result<String, String>) {
        match result {
            Ok(branch) => {
                self.status_message = Some((
                    format!("Created worktree '{}'", branch),
                    std::time::Instant::now(),
                ));
                self.trigger_worktree_refetch();
            }
            Err(e) => {
                self.status_message = Some((
                    format!("Failed to create worktree: {}", e),
                    std::time::Instant::now(),
                ));
            }
        }
    }

    /// Update the preview for the selected worktree (git log)
    pub(super) fn update_worktree_preview(&mut self) {
        let current_path = self.selected_worktree().map(|w| w.path.clone());

        if current_path != self.worktree_preview_path {
            self.worktree_preview_path = current_path.clone();
            self.worktree_preview = None;

            if let Some(path) = current_path {
                let tx = self.event_tx.clone();
                std::thread::spawn(move || {
                    let output = crate::git::unattended_git(Some(&path)).and_then(|mut command| {
                        command
                            .args(["log", "--format=%h\t%ar\t%s", "-n", "20"])
                            .output()
                            .map_err(anyhow::Error::from)
                    });
                    if let Ok(out) = output {
                        let log = String::from_utf8_lossy(&out.stdout).to_string();
                        let _ = tx.send(AppEvent::WorktreeLog(path, log));
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::MuxMode;
    use crate::github::{ClosingIssue, PrSummary};
    use crate::workflow::types::WorktreeInfo;
    use std::collections::HashMap;

    fn worktree(repo: &str, name: &str, is_main: bool) -> WorktreeInfo {
        WorktreeInfo {
            repo_key: format!("{repo}/.git").into(),
            handle: name.into(),
            branch: name.into(),
            path: format!("{repo}/worktrees/{name}").into(),
            is_main,
            mode: MuxMode::Window,
            has_mux_window: false,
            has_unmerged: false,
            pr_info: None,
            agent_status: None,
            created_at: None,
            base_branch: None,
        }
    }

    fn repo(key: &str, root: &str) -> WorkspaceRepo {
        WorkspaceRepo {
            repo_key: key.into(),
            main_root: root.into(),
            project: String::new(),
        }
    }

    #[test]
    fn repo_path_follows_selected_row_in_all_mode() {
        use RepoSelection::*;
        let b = PathBuf::from("/b/wt");
        let a = PathBuf::from("/a");
        let ov = PathBuf::from("/o");
        assert_eq!(
            choose_repo_path(None, true, Worktree(b.clone()), Some(a.clone()), None),
            Some(b.clone())
        );
        assert_eq!(
            choose_repo_path(None, false, Worktree(b.clone()), Some(a.clone()), None),
            Some(a.clone())
        );
        assert_eq!(
            choose_repo_path(Some(&ov), true, Worktree(b), Some(a.clone()), None),
            Some(ov)
        );
        assert_eq!(
            choose_repo_path(None, true, Nothing, Some(a.clone()), None),
            Some(a)
        );
    }

    #[test]
    fn repo_path_is_none_for_an_orphan_row() {
        let a = PathBuf::from("/a");
        let first = PathBuf::from("/first");
        let ov = PathBuf::from("/o");
        for all_mode in [true, false] {
            assert_eq!(
                choose_repo_path(
                    None,
                    all_mode,
                    RepoSelection::Orphan,
                    Some(a.clone()),
                    Some(first.clone())
                ),
                None
            );
        }
        assert_eq!(
            choose_repo_path(Some(&ov), true, RepoSelection::Orphan, None, None),
            None
        );
    }

    #[test]
    fn repo_path_on_agents_tab_follows_the_selected_agent() {
        use RepoSelection::*;
        let agent_root = PathBuf::from("/b");
        let current = PathBuf::from("/a");
        let first = PathBuf::from("/first");
        // All mode: the agent's repository, never the Worktrees selection.
        assert_eq!(
            choose_repo_path(
                None,
                true,
                Agent(Some(agent_root.clone())),
                Some(current.clone()),
                Some(first.clone())
            ),
            Some(agent_root.clone())
        );
        // Unknown agent repository: the current worktree...
        assert_eq!(
            choose_repo_path(
                None,
                true,
                Agent(None),
                Some(current.clone()),
                Some(first.clone())
            ),
            Some(current.clone())
        );
        // ...and not an arbitrary first row.
        assert_eq!(
            choose_repo_path(None, true, Agent(None), None, Some(first.clone())),
            None
        );
        // Without workspaces: upstream order (current, then first row).
        assert_eq!(
            choose_repo_path(
                None,
                false,
                Agent(Some(agent_root)),
                None,
                Some(first.clone())
            ),
            Some(first)
        );
    }

    #[test]
    fn occupied_branches_are_those_of_the_target_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let key_a = tmp.path().join("a/.git");
        let key_b = tmp.path().join("b/.git");
        std::fs::create_dir_all(&key_a).unwrap();
        std::fs::create_dir_all(&key_b).unwrap();
        let mut in_a = worktree("a", "feat-a", false);
        in_a.repo_key = key_a.clone();
        let mut in_b = worktree("b", "feat-b", false);
        in_b.repo_key = key_b.clone();
        let rows = vec![in_a, in_b];

        let occupied = occupied_branches(&rows, Some(&key_a));
        assert_eq!(occupied, HashSet::from(["feat-a".to_string()]));
        // The key is compared after canonicalization.
        let via_dots = tmp.path().join("b/../a/.git");
        assert_eq!(
            occupied_branches(&rows, Some(&via_dots)),
            HashSet::from(["feat-a".to_string()])
        );
        // Unknown target key: every listed worktree counts (upstream).
        assert_eq!(occupied_branches(&rows, None).len(), 2);
    }

    #[test]
    fn orphans_follow_the_picker_and_text_filter() {
        let orphan = |project: &str, name: &str| OrphanEntry {
            project: project.into(),
            name: name.into(),
            path: format!("/ws/{project}/{name}").into(),
            status: worktree_info::InfoStatus {
                label: "gone".into(),
                tone: worktree_info::Tone::Muted,
            },
            id: None,
            details: vec![],
            hint: None,
        };
        let all = vec![
            orphan("alpha", "old-env"),
            orphan("beta", "stale"),
            orphan("sync/one", "x"),
        ];
        let names = |v: Vec<OrphanEntry>| v.into_iter().map(|o| o.name).collect::<Vec<_>>();
        assert_eq!(names(filter_orphans(&all, None, "")).len(), 3);
        assert_eq!(names(filter_orphans(&all, Some("beta"), "")), ["stale"]);
        assert_eq!(names(filter_orphans(&all, Some("sync/one"), "")), ["x"]);
        // Text filter matches project or name, case-insensitively.
        assert_eq!(names(filter_orphans(&all, None, "OLD")), ["old-env"]);
        assert_eq!(names(filter_orphans(&all, None, "bet")), ["stale"]);
        assert!(filter_orphans(&all, Some("alpha"), "stale").is_empty());
    }

    #[test]
    fn row_config_error_names_the_row() {
        let msg = row_config_error("feat-x", &anyhow::anyhow!("bad yaml"));
        assert!(msg.contains("feat-x") && msg.contains("bad yaml"), "{msg}");
    }

    #[test]
    fn fetch_roots_dedupes_by_canonical_path() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a");
        let b = tmp.path().join("b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let repos = vec![repo("a/.git", a.to_str().unwrap())];
        let agent_roots = vec![a.join("../a"), b.clone()];
        assert_eq!(fetch_roots(&repos, &agent_roots), vec![a, b]);
    }

    #[test]
    fn project_picker_keeps_same_named_repos_apart() {
        let ws = vec![PathBuf::from("/nonexistent-ws")];
        let entries = project_entries(
            &ws,
            vec![
                ("/x/a/.git".into(), "/x/a/primary-clone".into()),
                ("/x/b/.git".into(), "/x/b/primary-clone".into()),
                ("/x/a/.git".into(), "/x/a/primary-clone".into()),
            ],
        );
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[0].name, "All projects");
        assert!(entries[0].repo_key.is_none());
        assert_eq!(entries[1].name, entries[2].name);
        assert_ne!(entries[1].path, entries[2].path);
    }

    #[test]
    fn project_picker_has_no_all_entry_without_workspaces() {
        let entries = project_entries(&[], vec![("/x/a/.git".into(), "/x/a/proj".into())]);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].repo_key.is_some());
    }

    #[test]
    fn names_for_repos_uses_workspace_relative_names() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().canonicalize().unwrap();
        let a = ws.join("cross/commons");
        let b = ws.join("cross/hermes");
        let c = ws.join("solo/primary-clone");
        let mut repos = Vec::new();
        for (root, key) in [(&a, "ka"), (&b, "kb"), (&c, "kc")] {
            std::fs::create_dir_all(root).unwrap();
            repos.push((PathBuf::from(key), root.clone()));
        }
        let names = names_for_repos(&[ws], &repos);
        assert_eq!(names[Path::new("ka")], "cross/commons");
        assert_eq!(names[Path::new("kb")], "cross/hermes");
        assert_eq!(names[Path::new("kc")], "solo");
    }

    #[test]
    fn without_workspaces_rows_use_their_own_path_name() {
        // Sibling layout: alpha/primary-clone (main) + alpha/feat-x (linked).
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("alpha/primary-clone");
        let linked = tmp.path().join("alpha/feat-x");
        std::fs::create_dir_all(main.join(".git")).unwrap();
        std::fs::create_dir_all(&linked).unwrap();
        std::fs::write(linked.join(".git"), "gitdir: elsewhere\n").unwrap();

        let names = fetched_project_names(&[], std::slice::from_ref(&main));
        assert!(names.is_empty());
        let key = main.join(".git");
        assert_eq!(resolve_project_name(&names, &key, &linked), "feat-x");
        assert_eq!(resolve_project_name(&names, &key, &main), "primary-clone");
    }

    #[test]
    fn names_use_full_set_so_narrowed_view_keeps_relative_names() {
        let ws = vec![PathBuf::from("/nonexistent-ws")];
        let one = ("k1".into(), PathBuf::from("/nonexistent-ws/sync/one"));
        let two = ("k2".into(), PathBuf::from("/nonexistent-ws/sync/two"));
        let full = names_for_repos(&ws, &[one.clone(), two]);
        assert_eq!(full[Path::new("k1")], "sync/one");
        // A single-repo input is what a narrowed view would see: wrong name.
        let narrowed = names_for_repos(&ws, &[one]);
        assert_eq!(narrowed[Path::new("k1")], "sync");
    }

    #[test]
    fn selecting_all_projects_clears_override() {
        let all = ProjectEntry {
            name: ALL_PROJECTS.into(),
            path: PathBuf::new(),
            repo_key: None,
        };
        assert_eq!(override_for(&all), None);
        let one = ProjectEntry {
            name: "sync/one".into(),
            path: "/ws/sync/one".into(),
            repo_key: Some("k1".into()),
        };
        assert_eq!(
            override_for(&one),
            Some(("sync/one".to_string(), PathBuf::from("/ws/sync/one")))
        );
    }

    #[test]
    fn project_entries_use_relative_names_for_shared_first_component() {
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().canonicalize().unwrap();
        let a = ws.join("sync/one");
        let b = ws.join("sync/two");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let entries = project_entries(
            &[ws],
            vec![("ka".into(), a.clone()), ("kb".into(), b.clone())],
        );
        let names: Vec<&str> = entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["All projects", "sync/one", "sync/two"]);
        assert_eq!(entries[1].path, a);
        assert_eq!(entries[2].repo_key.as_deref(), Some(Path::new("kb")));
    }

    #[test]
    fn resolve_project_name_prefers_map_then_path() {
        let mut names = HashMap::new();
        names.insert(PathBuf::from("k/.git"), "alpha".to_string());
        let wt = worktree("k", "main", true);
        assert_eq!(
            resolve_project_name(&names, Path::new("k/.git"), &wt.path),
            "alpha"
        );
        assert_eq!(
            resolve_project_name(&names, Path::new("other"), Path::new("/x/proj")),
            agent::extract_project_name(Path::new("/x/proj"))
        );
    }

    fn pr(number: u32) -> PrSummary {
        PrSummary {
            number,
            title: format!("PR {number}"),
            state: "OPEN".into(),
            is_draft: false,
            checks: None,
            check_meta: None,
            url: None,
            closing_issues: vec![ClosingIssue {
                number: 201,
                repository: "test/project".into(),
            }],
        }
    }

    #[test]
    fn bare_worktree_prs_survive_first_checkout_removal() {
        let first = worktree("/project", "branch-1", false);
        let second = worktree("/project", "branch-2", false);
        let statuses = HashMap::from([(
            first.repo_key.clone(),
            HashMap::from([
                (first.branch.clone(), pr(101)),
                (second.branch.clone(), pr(102)),
            ]),
        )]);
        let mut rows = vec![first, second.clone()];
        merge_worktree_prs(&mut rows, &statuses);
        assert_eq!(rows[0].pr_info, Some(pr(101)));
        assert_eq!(rows[1].pr_info, Some(pr(102)));
        let mut refreshed = vec![second];
        merge_worktree_prs(&mut refreshed, &statuses);
        assert_eq!(refreshed[0].pr_info, Some(pr(102)));
    }

    #[test]
    fn matching_project_and_branch_names_do_not_mix_prs() {
        let mut rows = vec![
            worktree("/a/project", "feature", false),
            worktree("/b/project", "feature", false),
        ];
        let statuses = HashMap::from([
            (
                rows[0].repo_key.clone(),
                HashMap::from([("feature".into(), pr(101))]),
            ),
            (
                rows[1].repo_key.clone(),
                HashMap::from([("feature".into(), pr(102))]),
            ),
        ]);
        merge_worktree_prs(&mut rows, &statuses);
        assert_eq!(rows[0].pr_info, Some(pr(101)));
        assert_eq!(rows[1].pr_info, Some(pr(102)));
    }

    #[test]
    fn ordinary_main_checkout_keeps_existing_pr_suppression() {
        let mut rows = vec![
            worktree("/project", "main", true),
            worktree("/project", "feature", false),
        ];
        let statuses = HashMap::from([(
            rows[0].repo_key.clone(),
            HashMap::from([("main".into(), pr(101)), ("feature".into(), pr(102))]),
        )]);
        merge_worktree_prs(&mut rows, &statuses);
        assert_eq!(rows[0].pr_info, None);
        assert_eq!(rows[1].pr_info, Some(pr(102)));
    }

    fn ag(id: &str, parent: Option<usize>) -> DisplayRow {
        DisplayRow::Agent {
            pane_id: id.into(),
            parent,
        }
    }

    fn pane(id: &str, path: &str) -> crate::multiplexer::AgentPane {
        crate::multiplexer::AgentPane {
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

    #[test]
    fn navigation_reaches_orphans() {
        // 2 worktrees + 1 orphan = 3 rows.
        let rows = [
            DisplayRow::Worktree(0),
            DisplayRow::Worktree(1),
            DisplayRow::Orphan(0),
        ];
        assert_eq!(step_selectable(&rows, Some(0), true), Some(1));
        assert_eq!(step_selectable(&rows, Some(1), true), Some(2));
        assert_eq!(step_selectable(&rows, Some(2), true), Some(0));
        assert_eq!(step_selectable(&rows, Some(0), false), Some(2));
        // Nothing selected: like today, counting from row 0.
        assert_eq!(step_selectable(&rows, None, true), Some(1));
        assert_eq!(step_selectable(&[], None, true), None);
    }

    #[test]
    fn actions_noop_on_orphan() {
        // Worktree actions resolve their target with `worktree_index_of`,
        // which has no worktree for an orphan row.
        assert_eq!(worktree_index_of(&DisplayRow::Orphan(0)), None);
        assert_eq!(worktree_index_of(&DisplayRow::OutsideHeader), None);
        assert_eq!(worktree_index_of(&DisplayRow::Worktree(1)), Some(1));
        // Jump numbers (1-9) address real rows only.
        let rows = [
            DisplayRow::Worktree(0),
            DisplayRow::Worktree(1),
            DisplayRow::Orphan(0),
        ];
        assert_eq!(nth_worktree_row(&rows, 2), None);
        assert_eq!(nth_worktree_row(&rows, 1), Some(1));
    }

    #[test]
    fn selection_follows_agent_across_rebuild() {
        let after = [
            DisplayRow::Worktree(0),
            DisplayRow::Worktree(1),
            ag("p1", Some(1)),
        ];
        assert_eq!(restore_row(&after, Some("p1"), None), Restored::Found(2));
        // By path for worktree rows.
        let by_path = |r: &DisplayRow| *r == DisplayRow::Worktree(1);
        assert_eq!(
            restore_row(&after, None, Some(&by_path)),
            Restored::Found(1)
        );
        // A vanished agent falls back to its parent worktree's path.
        assert_eq!(
            restore_row(&after, Some("gone"), Some(&by_path)),
            Restored::Found(1)
        );
        assert_eq!(restore_row(&after, None, None), Restored::NoIdentity);
    }

    #[test]
    fn selection_never_lands_on_header() {
        let rows = [
            DisplayRow::Orphan(0),
            DisplayRow::OutsideHeader,
            ag("p2", None),
        ];
        let nothing = |_: &DisplayRow| false;
        assert_eq!(
            restore_row(&rows, Some("p1"), Some(&nothing)),
            Restored::Lost(Some(0))
        );
        assert_eq!(step_selectable(&rows, Some(0), true), Some(2));
        assert_eq!(step_selectable(&rows, Some(2), false), Some(0));
        // A header first is never the fallback.
        let header_first = [DisplayRow::OutsideHeader, ag("p2", None)];
        assert_eq!(first_selectable(&header_first), Some(1));
        assert_eq!(
            restore_row(&header_first, Some("p1"), None),
            Restored::Lost(Some(1))
        );
        assert_eq!(restore_row(&[], Some("p1"), None), Restored::Lost(None));
    }

    #[test]
    fn enter_on_worktree_targets_first_agent_row() {
        let rows = [
            DisplayRow::Worktree(0),
            ag("p1", Some(0)),
            ag("p2", Some(0)),
        ];
        assert_eq!(enter_target(&rows, 0), Some(EnterTarget::Pane("p1".into())));
        assert_eq!(enter_target(&rows, 2), Some(EnterTarget::Pane("p2".into())));
        // No sub-row: the worktree's own window logic.
        let bare = [
            DisplayRow::Worktree(0),
            DisplayRow::Worktree(1),
            ag("p1", Some(1)),
        ];
        assert_eq!(enter_target(&bare, 0), Some(EnterTarget::Worktree(0)));
        assert_eq!(enter_target(&bare, 1), Some(EnterTarget::Pane("p1".into())));
        assert_eq!(enter_target(&[DisplayRow::Orphan(0)], 0), None);
        assert_eq!(enter_target(&bare, 9), None);
        // The next row is only a sub-row when it belongs to this worktree.
        let other_parent = [DisplayRow::Worktree(0), ag("p1", Some(1))];
        assert_eq!(
            enter_target(&other_parent, 0),
            Some(EnterTarget::Worktree(0))
        );
        let outside_next = [DisplayRow::Worktree(0), ag("p2", None)];
        assert_eq!(
            enter_target(&outside_next, 0),
            Some(EnterTarget::Worktree(0))
        );
    }

    #[test]
    fn panel_toggles_work_on_outside_agent_rows() {
        // `i`/`l` are view toggles: the dispatcher flips `worktree_panels`
        // directly, whatever the selected row (here an outside-group agent).
        let selected = ag("p2", None);
        assert!(is_outside_agent(&selected));
        let mut panels = WorktreePanels::default();
        panels.toggle_info();
        panels.toggle_log();
        assert!(panels.info && panels.log);
        panels.toggle_info();
        assert!(!panels.info && panels.log);
    }

    #[test]
    fn enter_on_worktree_targets_agent_in_subdirectory() {
        let all = vec![PathBuf::from("/ws/r/main"), PathBuf::from("/ws/r/feat")];
        let agents = [pane("%1", "/ws/r/main"), pane("%2", "/ws/r/feat/backend")];
        assert_eq!(
            first_agent_in_worktree(Path::new("/ws/r/feat"), &all, &agents),
            Some("%2".to_string())
        );
        assert_eq!(
            first_agent_in_worktree(Path::new("/ws/r/main"), &all, &agents),
            Some("%1".to_string())
        );
        assert_eq!(
            first_agent_in_worktree(Path::new("/ws/r/other"), &all, &agents),
            None
        );
    }

    #[test]
    fn actions_from_agent_row_use_parent_worktree() {
        assert_eq!(worktree_index_of(&ag("p1", Some(1))), Some(1));
        assert_eq!(worktree_index_of(&ag("p2", None)), None);
        assert!(is_outside_agent(&ag("p2", None)));
        assert!(!is_outside_agent(&ag("p1", Some(1))));
        assert!(!is_outside_agent(&DisplayRow::Orphan(0)));
    }

    #[test]
    fn jump_to_index_counts_worktree_rows_only() {
        let rows = [
            DisplayRow::Worktree(0),
            ag("a", Some(0)),
            DisplayRow::Worktree(1),
            ag("b", Some(1)),
            DisplayRow::Worktree(2),
        ];
        assert_eq!(nth_worktree_row(&rows, 2), Some(4));
        assert_eq!(nth_worktree_row(&rows, 0), Some(0));
        assert_eq!(nth_worktree_row(&rows, 3), None);
    }

    #[test]
    fn agents_tab_mode_has_no_agent_rows() {
        let wts = vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/b")];
        let agents = [pane("A", "/ws/b/src"), pane("B", "/elsewhere")];
        let assignment = unified::assign_agents(&wts, &agents);
        let inputs = RowInputs {
            visible_worktrees: &wts,
            all_worktrees: &wts,
            agents: &agents,
            assignment: &assignment,
            orphan_count: 1,
            filter: "",
            outside_group: true,
        };
        assert_eq!(
            display_rows_for(false, &inputs),
            vec![
                DisplayRow::Worktree(0),
                DisplayRow::Worktree(1),
                DisplayRow::Orphan(0),
            ]
        );
        // The same inputs in unified mode do carry agent rows.
        let unified_rows = display_rows_for(true, &inputs);
        assert!(unified_rows.contains(&ag("A", Some(1))));
        assert!(unified_rows.contains(&DisplayRow::OutsideHeader));
    }

    #[test]
    fn unified_filter_keeps_worktree_matched_by_agent_title() {
        let all = vec![PathBuf::from("/ws/a"), PathBuf::from("/ws/b")];
        let mut a = pane("A", "/ws/b/src");
        a.pane_title = Some("Fork workmux".into());
        let agents = [a];
        let assignment = unified::assign_agents(&all, &agents);
        let keep = unified::worktrees_visible_by_agent(&assignment, &agents, "FORK");
        assert_eq!(keep, HashSet::from([1]));
    }

    #[test]
    fn tab_switch_is_a_noop_in_unified_mode() {
        use DashboardTab::*;
        assert_eq!(next_tab(Worktrees, true), Worktrees);
        assert_eq!(next_tab(Agents, true), Worktrees);
        assert_eq!(next_tab(Worktrees, false), Agents);
        assert_eq!(next_tab(Agents, false), Worktrees);
    }
}
