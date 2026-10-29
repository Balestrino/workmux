//! Discovery of git repositories under configured workspace directories, and
//! the project names used to label them.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::git;
use crate::util::canon_or_self;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceRepo {
    pub repo_key: PathBuf,
    pub main_root: PathBuf,
    pub project: String,
}

/// Repositories under the workspace dirs, sorted by project name.
pub fn discover_repos(workspaces: &[PathBuf]) -> Vec<WorkspaceRepo> {
    let mut seen_keys: HashSet<PathBuf> = HashSet::new();
    let mut found: Vec<(PathBuf, PathBuf)> = Vec::new(); // (repo_key, main_root)

    for ws in workspaces {
        let ws = canon_or_self(ws);
        for dir in candidate_dirs(&ws) {
            if !dir.join(".git").is_dir() {
                continue;
            }
            let Ok(repo_key) = git::get_repository_key_in(Some(&dir)) else {
                continue;
            };
            if seen_keys.insert(repo_key.clone()) {
                found.push((repo_key, dir));
            }
        }
    }

    let roots: Vec<PathBuf> = found.iter().map(|(_, root)| root.clone()).collect();
    let names = project_names(workspaces, &roots);

    let mut repos: Vec<WorkspaceRepo> = found
        .into_iter()
        .map(|(repo_key, main_root)| {
            let project = names
                .get(&main_root)
                .cloned()
                .unwrap_or_else(|| crate::agent_display::extract_project_name(&main_root));
            WorkspaceRepo {
                repo_key,
                main_root,
                project,
            }
        })
        .collect();
    repos.sort_by(|a, b| {
        a.project
            .cmp(&b.project)
            .then(a.main_root.cmp(&b.main_root))
    });
    repos
}

/// Visible directories at depth 1 and 2 below `ws`, without descending into
/// directories that are themselves repositories (`.git` is a directory).
fn candidate_dirs(ws: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for d1 in visible_subdirs(ws) {
        out.push(d1.clone());
        if d1.join(".git").is_dir() {
            continue;
        }
        out.extend(visible_subdirs(&d1));
    }
    out
}

fn visible_subdirs(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| !e.file_name().to_string_lossy().starts_with("."))
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs
}

/// Project name for each main root (the key is the canonical main root).
///
/// Inside a workspace the name is the first path component below it; when
/// several roots share that component, the full relative path is used.
/// Outside any workspace the existing `extract_project_name` applies.
pub fn project_names(workspaces: &[PathBuf], main_roots: &[PathBuf]) -> HashMap<PathBuf, String> {
    let workspaces: Vec<PathBuf> = workspaces.iter().map(|w| canon_or_self(w)).collect();

    // (canonical root, relative path below its workspace if any)
    let entries: Vec<(PathBuf, Option<PathBuf>)> = main_roots
        .iter()
        .map(|root| {
            let root = canon_or_self(root);
            let rel = workspaces
                .iter()
                .filter_map(|ws| root.strip_prefix(ws).ok())
                .find(|rel| rel.components().next().is_some())
                .map(Path::to_path_buf);
            (root, rel)
        })
        .collect();

    let first_component = |rel: &Path| -> String {
        rel.components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .unwrap_or_default()
    };

    let mut sharing: HashMap<String, HashSet<&PathBuf>> = HashMap::new();
    for (root, rel) in &entries {
        if let Some(rel) = rel {
            sharing
                .entry(first_component(rel))
                .or_default()
                .insert(root);
        }
    }

    entries
        .iter()
        .map(|(root, rel)| {
            let name = match rel {
                Some(rel) => {
                    let first = first_component(rel);
                    if sharing.get(&first).is_some_and(|roots| roots.len() > 1) {
                        rel.to_string_lossy().to_string()
                    } else {
                        first
                    }
                }
                None => crate::agent_display::extract_project_name(root),
            };
            (root.clone(), name)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{init_repo, run_git};

    fn make_repo(path: &Path) {
        std::fs::create_dir_all(path).unwrap();
        init_repo(path);
    }

    fn projects(repos: &[WorkspaceRepo]) -> Vec<&str> {
        repos.iter().map(|r| r.project.as_str()).collect()
    }

    #[test]
    fn discovers_primary_clone_layout() {
        let ws = tempfile::tempdir().unwrap();
        make_repo(&ws.path().join("alpha/primary-clone"));
        let repos = discover_repos(&[ws.path().to_path_buf()]);
        assert_eq!(projects(&repos), ["alpha"]);
        assert!(repos[0].main_root.ends_with("alpha/primary-clone"));
    }

    #[test]
    fn discovers_depth_one_repo() {
        let ws = tempfile::tempdir().unwrap();
        make_repo(&ws.path().join("beta"));
        let repos = discover_repos(&[ws.path().to_path_buf()]);
        assert_eq!(projects(&repos), ["beta"]);
    }

    #[test]
    fn shared_first_component_uses_relative_path() {
        let ws = tempfile::tempdir().unwrap();
        make_repo(&ws.path().join("sync/one"));
        make_repo(&ws.path().join("sync/two"));
        let repos = discover_repos(&[ws.path().to_path_buf()]);
        assert_eq!(projects(&repos), ["sync/one", "sync/two"]);
    }

    #[test]
    fn ignores_linked_worktrees_and_hidden() {
        let ws = tempfile::tempdir().unwrap();
        let primary = ws.path().join("alpha/primary-clone");
        make_repo(&primary);
        run_git(
            &primary,
            &[
                "worktree",
                "add",
                "-b",
                "feat",
                ws.path().join("alpha/feature").to_str().unwrap(),
            ],
        );
        make_repo(&ws.path().join(".cache/x"));
        let repos = discover_repos(&[ws.path().to_path_buf()]);
        assert_eq!(projects(&repos), ["alpha"]);
    }

    #[test]
    fn does_not_descend_into_repo() {
        let ws = tempfile::tempdir().unwrap();
        make_repo(&ws.path().join("gamma"));
        make_repo(&ws.path().join("gamma/vendor/dep"));
        let repos = discover_repos(&[ws.path().to_path_buf()]);
        assert_eq!(projects(&repos), ["gamma"]);
    }

    #[test]
    fn missing_workspace_dir_is_skipped() {
        let ws = tempfile::tempdir().unwrap();
        make_repo(&ws.path().join("beta"));
        let repos = discover_repos(&[ws.path().to_path_buf(), ws.path().join("nope")]);
        assert_eq!(projects(&repos), ["beta"]);
    }

    #[test]
    fn outside_workspace_falls_back() {
        let ws = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let delta = other.path().join("delta");
        make_repo(&delta);
        let names = project_names(&[ws.path().to_path_buf()], std::slice::from_ref(&delta));
        assert_eq!(names.get(&canon_or_self(&delta)).unwrap(), "delta");
    }
}
