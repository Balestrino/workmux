//! Worktree tab: navigation, removal, sweep, project picker, and preview.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use anyhow::Context as _;

use crate::git;
use crate::multiplexer::Multiplexer;
use crate::util::canon_or_self;
use crate::workflow;
use crate::workspaces::{self, WorkspaceRepo};

use super::super::agent;
use super::super::sort::WorktreeSortMode;
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

/// What a table row index refers to: a listed worktree, an orphan row after
/// them, or nothing.
#[derive(Debug, PartialEq, Eq)]
enum RowKind {
    Worktree(usize),
    Orphan(usize),
    None,
}

fn row_kind(index: usize, worktree_len: usize, orphan_len: usize) -> RowKind {
    if index < worktree_len {
        RowKind::Worktree(index)
    } else if index - worktree_len < orphan_len {
        RowKind::Orphan(index - worktree_len)
    } else {
        RowKind::None
    }
}

fn wrap_next(current: usize, count: usize) -> usize {
    if current + 1 >= count { 0 } else { current + 1 }
}

fn wrap_prev(current: usize, count: usize) -> usize {
    if current == 0 { count - 1 } else { current - 1 }
}

/// Keyboard jump numbers address real worktree rows only.
fn jump_target(index: usize, worktree_len: usize) -> Option<usize> {
    (index < worktree_len).then_some(index)
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

    /// Switch between Agents and Worktrees tabs
    pub fn switch_tab(&mut self) {
        self.active_tab = match self.active_tab {
            DashboardTab::Agents => DashboardTab::Worktrees,
            DashboardTab::Worktrees => DashboardTab::Agents,
        };
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

        // Apply name filter
        if !self.worktree_filter_text.is_empty() {
            let filter = self.worktree_filter_text.to_lowercase();
            self.worktrees.retain(|w| {
                let handle = w.handle.to_lowercase();
                handle.contains(&filter) || w.branch.to_lowercase().contains(&filter)
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

        // Restore selection by path (orphan rows follow the real rows)
        if let Some(ref path) = self.selected_worktree_path {
            let orphan_idx = self.visible_orphans.iter().position(|o| &o.path == path);
            if let Some(idx) = self.worktrees.iter().position(|w| &w.path == path) {
                self.worktree_table_state.select(Some(idx));
            } else if let Some(idx) = orphan_idx {
                self.worktree_table_state
                    .select(Some(self.worktrees.len() + idx));
            } else {
                self.selected_worktree_path = None;
                if self.worktree_row_count() == 0 {
                    self.worktree_table_state.select(None);
                } else {
                    self.worktree_table_state.select(Some(0));
                }
            }
        } else if self.worktree_row_count() > 0 && self.worktree_table_state.selected().is_none() {
            self.worktree_table_state.select(Some(0));
            self.selected_worktree_path = self.row_path(0);
        }

        self.update_worktree_preview();
    }

    /// Number of selectable rows: the listed worktrees plus the orphan rows.
    pub fn worktree_row_count(&self) -> usize {
        self.worktrees.len() + self.visible_orphans.len()
    }

    /// The orphan row under the selection, if the selection is on one.
    pub fn selected_orphan(&self) -> Option<&OrphanEntry> {
        let selected = self.worktree_table_state.selected()?;
        match row_kind(selected, self.worktrees.len(), self.visible_orphans.len()) {
            RowKind::Orphan(i) => self.visible_orphans.get(i),
            _ => None,
        }
    }

    /// Path identifying the row at `index` (worktree or orphan).
    pub fn row_path(&self, index: usize) -> Option<PathBuf> {
        match row_kind(index, self.worktrees.len(), self.visible_orphans.len()) {
            RowKind::Worktree(i) => self.worktrees.get(i).map(|w| w.path.clone()),
            RowKind::Orphan(i) => self.visible_orphans.get(i).map(|o| o.path.clone()),
            RowKind::None => None,
        }
    }

    pub fn select_worktree(&mut self, index: usize) {
        if index >= self.worktree_row_count() {
            return;
        }
        self.worktree_table_state.select(Some(index));
        self.selected_worktree_path = self.row_path(index);
        self.update_worktree_preview();
    }

    pub fn worktree_next(&mut self) {
        let count = self.worktree_row_count();
        if count == 0 {
            return;
        }
        let i = self.worktree_table_state.selected().unwrap_or(0);
        self.select_worktree(wrap_next(i, count));
    }

    pub fn worktree_previous(&mut self) {
        let count = self.worktree_row_count();
        if count == 0 {
            return;
        }
        let i = self.worktree_table_state.selected().unwrap_or(0);
        self.select_worktree(wrap_prev(i, count));
    }

    /// Jump to the worktree at `index`. Only real rows have jump numbers.
    pub fn worktree_jump_to_index(&mut self, index: usize) {
        if let Some(index) = jump_target(index, self.worktrees.len()) {
            self.worktree_table_state.select(Some(index));
            self.selected_worktree_path = self.worktrees.get(index).map(|w| w.path.clone());
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
                let Some(selected) = self.worktree_table_state.selected() else {
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
        let Some(selected) = self.worktree_table_state.selected() else {
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
                let Some(selected) = self.worktree_table_state.selected() else {
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
        let Some(selected) = self.worktree_table_state.selected() else {
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

    /// Jump to the selected worktree's agent or mux window.
    /// Tries the agent pane first, then falls back to workflow::open
    /// which switches to an existing window/session or creates one.
    pub fn jump_to_selected_worktree(&mut self) {
        let Some(selected) = self.worktree_table_state.selected() else {
            return;
        };
        let Some(worktree) = self.worktrees.get(selected) else {
            return;
        };

        if self.removals.contains(&worktree.path) {
            return;
        }

        // Try agent pane first for direct pane targeting
        if let Some(agent) = self.all_agents.iter().find(|a| a.path == worktree.path) {
            let target = agent.pane_id.clone();
            self.switch_to_pane_and_track(&target);
            return;
        }

        // Fall back to workflow::open (switches to existing or creates new)
        self.open_selected_worktree();
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
                let Some(selected) = self.worktree_table_state.selected() else {
                    return RepoSelection::Nothing;
                };
                match row_kind(selected, self.worktrees.len(), self.visible_orphans.len()) {
                    RowKind::Worktree(i) => RepoSelection::Worktree(self.worktrees[i].path.clone()),
                    RowKind::Orphan(_) => RepoSelection::Orphan,
                    RowKind::None => RepoSelection::Nothing,
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
    fn update_worktree_preview(&mut self) {
        let current_path = self
            .worktree_table_state
            .selected()
            .and_then(|idx| self.worktrees.get(idx))
            .map(|w| w.path.clone());

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

    #[test]
    fn navigation_reaches_orphans() {
        // 2 worktrees + 1 orphan = 3 rows.
        assert_eq!(wrap_next(0, 3), 1);
        assert_eq!(wrap_next(1, 3), 2);
        assert_eq!(wrap_next(2, 3), 0);
        assert_eq!(wrap_prev(0, 3), 2);
        assert_eq!(row_kind(1, 2, 1), RowKind::Worktree(1));
        assert_eq!(row_kind(2, 2, 1), RowKind::Orphan(0));
        assert_eq!(row_kind(3, 2, 1), RowKind::None);
    }

    #[test]
    fn actions_noop_on_orphan() {
        // Every worktree action resolves its target with `worktrees.get(selected)`,
        // which is only reachable for `RowKind::Worktree`.
        assert!(!matches!(row_kind(2, 2, 1), RowKind::Worktree(_)));
        // Jump numbers (1-9) address real rows only.
        assert_eq!(row_kind(2, 2, 1), RowKind::Orphan(0));
        assert_eq!(jump_target(2, 2), None);
        assert_eq!(jump_target(1, 2), Some(1));
    }
}
