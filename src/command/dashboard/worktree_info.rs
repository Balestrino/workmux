//! External worktree info provider: a user-configured shell command prints
//! JSON describing per-worktree environments; this module parses, runs and
//! indexes that report.

use std::collections::HashMap;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Deserializer};

/// Maximum time a provider run may take before the child is killed.
pub const PROVIDER_TIMEOUT: Duration = Duration::from_secs(10);

const SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Success,
    Warning,
    Error,
    Info,
    Muted,
}

impl<'de> Deserialize<'de> for Tone {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(match s.as_str() {
            "success" => Tone::Success,
            "warning" => Tone::Warning,
            "error" => Tone::Error,
            "info" => Tone::Info,
            _ => Tone::Muted,
        })
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct InfoStatus {
    pub label: String,
    pub tone: Tone,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct InfoDetail {
    pub label: String,
    pub value: String,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct InfoEntry {
    pub path: PathBuf,
    pub status: InfoStatus,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub details: Vec<InfoDetail>,
}

#[derive(Clone, Debug, Deserialize, PartialEq)]
pub struct OrphanEntry {
    pub project: String,
    pub name: String,
    pub path: PathBuf,
    pub status: InfoStatus,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub details: Vec<InfoDetail>,
    #[serde(default)]
    pub hint: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct InfoReport {
    pub worktrees: Vec<InfoEntry>,
    pub orphans: Vec<OrphanEntry>,
}

#[derive(Deserialize)]
struct RawReport {
    #[serde(rename = "schemaVersion")]
    schema_version: u32,
    #[serde(default)]
    worktrees: Vec<InfoEntry>,
    #[serde(default)]
    orphans: Vec<OrphanEntry>,
}

pub fn parse_report(json: &str) -> Result<InfoReport> {
    let raw: RawReport = serde_json::from_str(json).context("invalid worktree info JSON")?;
    if raw.schema_version != SCHEMA_VERSION {
        bail!(
            "unsupported worktree info schemaVersion {} (expected {})",
            raw.schema_version,
            SCHEMA_VERSION
        );
    }
    Ok(InfoReport {
        worktrees: raw.worktrees,
        orphans: raw.orphans,
    })
}

/// Run the provider via `sh -c`, reading stdout only (stderr is discarded).
///
/// Stdout is drained by a dedicated thread while the caller polls the child,
/// so a provider that writes more than a pipe buffer cannot deadlock. The
/// child runs in its own process group so a timeout kills grandchildren too,
/// and the reader is never joined after a timeout (a stray process holding
/// the pipe cannot block us).
pub fn run_provider(command: &str, timeout: Duration) -> Result<InfoReport> {
    let mut child = Command::new("sh")
        .arg("-c")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("failed to start worktree info command")?;
    let mut stdout = child.stdout.take().ok_or_else(|| anyhow!("no stdout"))?;
    let (tx, rx) = mpsc::channel::<std::io::Result<Vec<u8>>>();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = tx.send(stdout.read_to_end(&mut buf).map(|_| buf));
    });

    let deadline = Instant::now() + timeout;
    let kill_group = |child: &mut std::process::Child| {
        // SAFETY: plain syscall on the process group we created.
        unsafe {
            libc::kill(-(child.id() as i32), libc::SIGKILL);
        }
        let _ = child.kill();
        let _ = child.wait();
    };
    let status = loop {
        match child.try_wait().context("failed to wait for command")? {
            Some(status) => break status,
            None if Instant::now() >= deadline => {
                kill_group(&mut child);
                bail!("worktree info command timed out after {timeout:?}");
            }
            None => std::thread::sleep(Duration::from_millis(20)),
        }
    };
    if !status.success() {
        bail!("worktree info command failed ({status})");
    }
    let remaining = deadline.saturating_duration_since(Instant::now());
    let bytes = match rx.recv_timeout(remaining) {
        Ok(res) => res.context("failed to read command output")?,
        Err(_) => {
            kill_group(&mut child);
            bail!("worktree info command output not closed after {timeout:?}");
        }
    };
    let text = String::from_utf8(bytes).context("command output is not UTF-8")?;
    parse_report(&text)
}

#[derive(Clone, Debug, Default)]
pub struct InfoIndex {
    pub by_path: HashMap<PathBuf, InfoEntry>,
    pub orphans: Vec<OrphanEntry>,
}

fn canon(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Index a report by canonical path. The first entry for a path wins. An
/// orphan whose canonical path matches a known (listed) worktree is attached
/// to that worktree instead of staying an orphan.
pub fn index_report(report: &InfoReport, known_paths: &[PathBuf]) -> InfoIndex {
    let known: std::collections::HashSet<PathBuf> = known_paths.iter().map(|p| canon(p)).collect();
    let mut index = InfoIndex::default();
    for entry in &report.worktrees {
        index
            .by_path
            .entry(canon(&entry.path))
            .or_insert_with(|| entry.clone());
    }
    for orphan in &report.orphans {
        let path = canon(&orphan.path);
        if known.contains(&path) {
            index.by_path.entry(path).or_insert_with(|| InfoEntry {
                path: orphan.path.clone(),
                status: orphan.status.clone(),
                id: orphan.id.clone(),
                details: orphan.details.clone(),
            });
        } else {
            index.orphans.push(orphan.clone());
        }
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"{
  "schemaVersion": 1,
  "worktrees": [
    {
      "path": "/home/tore/workspaces/orkestra-gaterei/work-linkedin-connector",
      "status": { "label": "ready", "tone": "success" },
      "id": "00000000-0000-4000-8000-000000000001",
      "details": [
        { "label": "backend", "value": "http://127.0.0.1:18600" },
        { "label": "console", "value": "http://dev-host.example:18601" }
      ]
    }
  ],
  "orphans": [
    {
      "project": "orkestra-commons",
      "name": "dev-sandbox",
      "path": "/home/tore/workspaces/orkestra-commons/dev-sandbox",
      "status": { "label": "unknown", "tone": "warning" },
      "id": "00000000-0000-4000-8000-000000000002",
      "details": [],
      "hint": "forget --environment 00000000-0000-4000-8000-000000000002"
    }
  ]
}"#;

    fn status(label: &str) -> InfoStatus {
        InfoStatus {
            label: label.into(),
            tone: Tone::Info,
        }
    }

    fn entry(path: &Path, label: &str) -> InfoEntry {
        InfoEntry {
            path: path.to_path_buf(),
            status: status(label),
            id: None,
            details: vec![],
        }
    }

    fn orphan(path: &Path) -> OrphanEntry {
        OrphanEntry {
            project: "p".into(),
            name: "n".into(),
            path: path.to_path_buf(),
            status: status("gone"),
            id: Some("x".into()),
            details: vec![],
            hint: None,
        }
    }

    #[test]
    fn parse_full_report() {
        let r = parse_report(FULL).unwrap();
        assert_eq!(r.worktrees.len(), 1);
        assert_eq!(r.worktrees[0].details.len(), 2);
        assert_eq!(r.worktrees[0].status.tone, Tone::Success);
        assert_eq!(r.orphans.len(), 1);
        assert_eq!(
            r.orphans[0].hint.as_deref(),
            Some("forget --environment 00000000-0000-4000-8000-000000000002")
        );
    }

    #[test]
    fn parse_rejects_other_schema() {
        assert!(parse_report(r#"{"schemaVersion":2,"worktrees":[],"orphans":[]}"#).is_err());
    }

    #[test]
    fn parse_unknown_tone_is_muted() {
        let r = parse_report(
            r#"{"schemaVersion":1,"worktrees":[{"path":"/a","status":{"label":"x","tone":"purple"}}]}"#,
        )
        .unwrap();
        assert_eq!(r.worktrees[0].status.tone, Tone::Muted);
    }

    #[test]
    fn parse_ignores_unknown_fields() {
        let r = parse_report(
            r#"{"schemaVersion":1,"extra":1,"worktrees":[{"path":"/a","zzz":true,"status":{"label":"x","tone":"info","more":1}}]}"#,
        )
        .unwrap();
        assert_eq!(r.worktrees.len(), 1);
    }

    #[test]
    fn index_first_entry_wins() {
        let report = InfoReport {
            worktrees: vec![
                entry(Path::new("/nope/a"), "first"),
                entry(Path::new("/nope/a"), "second"),
            ],
            orphans: vec![],
        };
        let idx = index_report(&report, &[]);
        assert_eq!(idx.by_path[Path::new("/nope/a")].status.label, "first");
    }

    #[test]
    fn index_attaches_orphan_matching_known_path() {
        let dir = tempfile::tempdir().unwrap();
        let report = InfoReport {
            worktrees: vec![],
            orphans: vec![orphan(dir.path()), orphan(Path::new("/nope/missing"))],
        };
        let idx = index_report(&report, &[dir.path().to_path_buf()]);
        assert!(
            idx.by_path
                .contains_key(&std::fs::canonicalize(dir.path()).unwrap())
        );
        assert_eq!(idx.orphans.len(), 1);
        assert_eq!(idx.orphans[0].path, Path::new("/nope/missing"));
    }

    #[test]
    fn index_canonicalizes_paths() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real");
        std::fs::create_dir(&real).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let report = InfoReport {
            worktrees: vec![entry(&link, "ok")],
            orphans: vec![],
        };
        let idx = index_report(&report, std::slice::from_ref(&real));
        assert_eq!(
            idx.by_path[&std::fs::canonicalize(&real).unwrap()]
                .status
                .label,
            "ok"
        );
    }

    #[test]
    fn run_provider_timeout() {
        let start = Instant::now();
        let res = run_provider("sleep 5", Duration::from_millis(200));
        assert!(res.is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    #[test]
    fn run_provider_nonzero_exit() {
        assert!(run_provider("echo '{}'; exit 3", Duration::from_secs(2)).is_err());
    }

    #[test]
    fn run_provider_ok() {
        let cmd = format!("printf '%s' '{}'", FULL.replace('\'', ""));
        let r = run_provider(&cmd, Duration::from_secs(2)).unwrap();
        assert_eq!(r.worktrees.len(), 1);
    }

    #[test]
    fn run_provider_large_output_does_not_deadlock() {
        // ~1 MiB of padding inside valid JSON, well past the pipe buffer.
        let cmd = r#"printf '{"schemaVersion":1,"pad":"'; head -c 1048576 /dev/zero | tr '\0' x; printf '"}'"#;
        let r = run_provider(cmd, Duration::from_secs(5)).unwrap();
        assert!(r.worktrees.is_empty());
    }
}
