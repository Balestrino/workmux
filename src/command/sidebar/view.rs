//! Named scopes shared by the sidebar and its CLI chooser.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::cmd::Cmd;
use crate::config::{Config, SidebarView};
use crate::multiplexer::AgentPane;

pub const OPTION: &str = "@workmux_sidebar_view";

pub fn selected<'a>(views: &'a [SidebarView], name: Option<&str>) -> Option<&'a SidebarView> {
    views.iter().find(|view| Some(view.name.as_str()) == name)
}

fn next_name<'a>(views: &'a [SidebarView], current: Option<&str>) -> &'a str {
    match views
        .iter()
        .position(|view| Some(view.name.as_str()) == current)
    {
        Some(index) => views.get(index + 1),
        None => views.first(),
    }
    .map_or("All", |view| view.name.as_str())
}

pub fn select(name: Option<&str>, list: bool) -> Result<()> {
    let config = Config::load(None)?;
    let views = config.sidebar.views.as_deref().unwrap_or_default();
    if list {
        println!("All");
        for view in views {
            println!("{}", view.name);
        }
        return Ok(());
    }
    let current = Cmd::new("tmux")
        .args(&["show-option", "-gqv", OPTION])
        .run_and_capture_stdout()?;
    let name = name.unwrap_or_else(|| next_name(views, Some(current.trim())));
    if name != "All" && selected(views, Some(name)).is_none() {
        bail!("unknown sidebar view {name:?}; use workmux sidebar view --list");
    }
    Cmd::new("tmux")
        .args(&["set-option", "-g", OPTION, name])
        .run()?;
    super::signal_daemon();
    Ok(())
}

/// Normalize nonexistent paths lexically too, without confusing sibling names.
fn normalize(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(component),
        }
    }
    result
}

fn matches(view: &SidebarView, session: &str, project: &Path) -> bool {
    view.sessions.iter().any(|name| name == session)
        || view
            .project_dirs
            .iter()
            .any(|dir| project.starts_with(normalize(&crate::util::expand_tilde(dir))))
}

fn includes(views: &[SidebarView], view: &SidebarView, session: &str, project: &Path) -> bool {
    if view.other {
        !views
            .iter()
            .filter(|v| !v.other)
            .any(|v| matches(v, session, project))
    } else {
        matches(view, session, project)
    }
}

/// Cache Git discovery across ticks; refresh so moved repositories recover.
#[derive(Default)]
pub struct ProjectCache(HashMap<PathBuf, (Instant, PathBuf)>);

impl ProjectCache {
    pub fn retain(
        &mut self,
        agents: &mut Vec<AgentPane>,
        views: &[SidebarView],
        name: Option<&str>,
    ) {
        let Some(view) = selected(views, name) else {
            return;
        };
        self.0
            .retain(|path, _| agents.iter().any(|agent| &agent.path == path));
        let needs_paths = if view.other {
            views.iter().any(|v| !v.project_dirs.is_empty())
        } else {
            !view.project_dirs.is_empty()
        };
        agents.retain(|agent| {
            let project = if needs_paths {
                let entry = self
                    .0
                    .entry(agent.path.clone())
                    .or_insert_with(|| (Instant::now(), project_root(&agent.path)));
                if entry.0.elapsed() > Duration::from_secs(30) {
                    *entry = (Instant::now(), project_root(&agent.path));
                }
                entry.1.as_path()
            } else {
                agent.path.as_path()
            };
            includes(views, view, &agent.session, project)
        });
    }
}

fn project_root(path: &Path) -> PathBuf {
    normalize(
        &crate::git::get_main_worktree_root_in(Some(path)).unwrap_or_else(|_| path.to_path_buf()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn views() -> Vec<SidebarView> {
        serde_yaml::from_str("- name: Named\n  sessions: [foo, bar, xyz]\n- name: Code\n  project_dirs: [/home/me/code]\n- name: Work\n  project_dirs: [/home/me/work]\n- name: Other\n  other: true\n").unwrap()
    }

    #[test]
    fn matching_and_complement() {
        let views = views();
        let code = Path::new("/home/me/code/app");
        assert!(includes(&views, &views[0], "foo", code));
        assert!(includes(&views, &views[1], "foo", code));
        assert!(!includes(&views, &views[3], "foo", Path::new("/elsewhere")));
        assert!(!includes(&views, &views[3], "misc", code));
        assert!(includes(
            &views,
            &views[3],
            "misc",
            Path::new("/home/me/code-other")
        ));
        assert!(matches(&views[1], "misc", Path::new("/home/me/code")));
        assert!(!matches(&views[0], "Foo", code));
        assert!(!includes(
            &views,
            &views[3],
            "misc",
            Path::new("/home/me/work/app")
        ));
    }

    #[test]
    fn order_and_missing_selection() {
        let views = views();
        assert_eq!(next_name(&views, None), "Named");
        assert_eq!(next_name(&views, Some("Named")), "Code");
        assert_eq!(next_name(&views, Some("Other")), "All");
        assert_eq!(next_name(&[], None), "All");
        assert!(selected(&views, Some("deleted")).is_none());
    }

    #[test]
    fn home_expansion_and_directory_boundaries() {
        let view = SidebarView {
            name: "Home".into(),
            project_dirs: vec!["~/code/../work".into()],
            ..Default::default()
        };
        let home = crate::util::expand_tilde("~");
        assert!(matches(&view, "", &normalize(&home.join("work/app"))));
        assert!(!matches(
            &view,
            "",
            &normalize(&home.join("work-other/app"))
        ));
    }

    #[test]
    fn linked_worktree_uses_main_project_location() {
        let temp = tempfile::tempdir().unwrap();
        let main = temp.path().join("code/project");
        let linked = temp.path().join("elsewhere/feature");
        std::fs::create_dir_all(&main).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "initial",
                "--allow-empty",
            ],
            vec![
                "worktree",
                "add",
                "-qb",
                "feature",
                linked.to_str().unwrap(),
            ],
        ] {
            Cmd::new("git").args(&args).workdir(&main).run().unwrap();
        }
        assert_eq!(project_root(&linked), main.canonicalize().unwrap());
        let view = SidebarView {
            name: "Code".into(),
            project_dirs: vec![temp.path().join("code").to_string_lossy().into_owned()],
            ..Default::default()
        };
        assert!(matches(&view, "", &project_root(&linked)));
        assert_eq!(
            project_root(&temp.path().join("non-repo")),
            normalize(&temp.path().join("non-repo"))
        );
    }

    #[test]
    fn validate_config() {
        crate::config::validate_sidebar_views(&views()).unwrap();
        for yaml in [
            "- {name: All, sessions: [foo]}",
            "- {name: empty}",
            "- {name: Other, other: true, sessions: [foo]}",
            "- {name: A, other: true}\n- {name: B, other: true}",
            "- {name: A, sessions: [foo]}\n- {name: A, sessions: [bar]}",
            "- {name: A, project_dirs: [relative]}",
        ] {
            let views: Vec<SidebarView> = serde_yaml::from_str(yaml).unwrap();
            assert!(
                crate::config::validate_sidebar_views(&views).is_err(),
                "{yaml}"
            );
        }
    }
}
