//! Knowledge-graph state of an agent, shown as a line in the sidebar (fork).
//!
//! Only CodeGraph today: an index is `.codegraph/codegraph.db` at or above
//! the agent's directory, and the agent is connected when a
//! `codegraph serve --mcp` process runs below its pane.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// What the sidebar line shows for one agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum KnowledgeGraphState {
    /// No server connected and no index: `No knowledge graph`.
    None,
    /// Server connected without an index, or an index whose connection
    /// cannot be observed: `○ codegraph`.
    Available,
    /// Needs action: a stale index, or an index with no server: `⚠ codegraph`.
    Attention,
    /// A fresh index and a connected server: blinking `● codegraph`.
    Active,
}

/// Whether the graph's MCP server runs below the agent's pane.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Connection {
    Yes,
    No,
    /// Not observable (no pane pid, unreadable `/proc`): never a warning.
    Unknown,
}

/// The state for an agent, from the spec's decision table.
pub fn decide(has_index: bool, connection: Connection, stale: bool) -> KnowledgeGraphState {
    use KnowledgeGraphState as S;
    match (has_index, connection, stale) {
        (false, Connection::Yes, _) => S::Available,
        (false, _, _) => S::None,
        (true, _, true) => S::Attention,
        (true, Connection::Yes, false) => S::Active,
        (true, Connection::No, false) => S::Attention,
        (true, Connection::Unknown, false) => S::Available,
    }
}

/// The nearest ancestor of `path` (itself included) holding
/// `.codegraph/codegraph.db`. A bare `.codegraph/` (CodeGraph's global
/// `~/.codegraph/` among them) is not an index; a removed directory has none.
pub fn index_root(path: &Path) -> Option<PathBuf> {
    if path.to_string_lossy().ends_with(" (deleted)") || !path.is_dir() {
        return None;
    }
    path.ancestors()
        .find(|dir| dir.join(".codegraph").join("codegraph.db").is_file())
        .map(Path::to_path_buf)
}

/// The fields of `codegraph status --json` the line needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    /// Files added, modified or removed since the last indexing.
    pub pending: u64,
    pub reindex_recommended: bool,
    /// `complete`, `partial`, `indexing`, `failed`, or `None` (old index).
    pub state: Option<String>,
}

/// Parse `codegraph status --json`; `None` for an uninitialized project or
/// output that is not the expected JSON.
pub fn parse_status_json(json: &str) -> Option<ProbeReport> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    if value.get("initialized")?.as_bool() != Some(true) {
        return None;
    }
    let pending = value.get("pendingChanges");
    let count = |key: &str| {
        pending
            .and_then(|p| p.get(key))
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0)
    };
    let index = value.get("index");
    Some(ProbeReport {
        pending: count("added") + count("modified") + count("removed"),
        reindex_recommended: index
            .and_then(|i| i.get("reindexRecommended"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        state: index
            .and_then(|i| i.get("state"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string),
    })
}

/// How long pending changes may wait before the index counts as stale. A
/// connected server's file watcher syncs within seconds.
pub const PENDING_GRACE: Duration = Duration::from_secs(120);

/// Freshness of one index root across probes.
#[derive(Debug, Default)]
pub struct Freshness {
    pending_since: Option<Instant>,
    stale: bool,
}

impl Freshness {
    /// Fold in one probe result; `None` (a failed probe) changes nothing.
    pub fn record(&mut self, report: Option<&ProbeReport>, now: Instant) {
        let Some(report) = report else {
            return;
        };
        let broken = report.reindex_recommended
            || report.state.as_deref().is_some_and(|state| state != "complete");
        let lagging = if report.pending > 0 {
            let since = *self.pending_since.get_or_insert(now);
            now.saturating_duration_since(since) >= PENDING_GRACE
        } else {
            self.pending_since = None;
            false
        };
        self.stale = broken || lagging;
    }

    pub fn is_stale(&self) -> bool {
        self.stale
    }
}

/// Whether a command line runs the CodeGraph MCP server: an argument naming
/// `codegraph` or `codegraph.js` (it runs under a bundled `node`), followed
/// by `serve` and `--mcp`.
pub fn is_codegraph_server(args: &[String]) -> bool {
    let Some(program) = args.iter().position(|arg| {
        let name = arg.rsplit('/').next().unwrap_or(arg);
        name == "codegraph" || name == "codegraph.js"
    }) else {
        return false;
    };
    let rest = &args[program + 1..];
    rest.iter().any(|arg| arg == "serve") && rest.iter().any(|arg| arg == "--mcp")
}

/// Parent links and command lines of the processes under a `/proc` root,
/// read once per worker pass.
#[derive(Debug, Default)]
pub struct ProcTable {
    children: std::collections::HashMap<u32, Vec<u32>>,
    cmdlines: std::collections::HashMap<u32, Vec<String>>,
}

impl ProcTable {
    /// Read `<proc_root>/<pid>/{stat,cmdline}`; `None` if the root cannot be
    /// listed. Processes that vanish while reading are skipped.
    pub fn read(proc_root: &Path) -> Option<ProcTable> {
        let mut table = ProcTable::default();
        for entry in std::fs::read_dir(proc_root).ok()?.flatten() {
            let Some(pid) = entry.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
                continue;
            };
            // The command name may hold spaces and parentheses: the ppid is
            // the second field after the last ')'.
            let Some(ppid) = stat
                .rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(1))
                .and_then(|s| s.parse::<u32>().ok())
            else {
                continue;
            };
            table.children.entry(ppid).or_default().push(pid);
            if let Ok(raw) = std::fs::read(entry.path().join("cmdline")) {
                let args = raw
                    .split(|b| *b == 0)
                    .filter(|part| !part.is_empty())
                    .map(|part| String::from_utf8_lossy(part).into_owned())
                    .collect();
                table.cmdlines.insert(pid, args);
            }
        }
        Some(table)
    }

    /// Whether a CodeGraph server runs among the descendants of `pane_pid`.
    pub fn connection(&self, pane_pid: Option<u32>) -> Connection {
        let Some(root) = pane_pid else {
            return Connection::Unknown;
        };
        let mut seen = std::collections::HashSet::from([root]);
        let mut queue = std::collections::VecDeque::from([root]);
        while let Some(pid) = queue.pop_front() {
            for &child in self.children.get(&pid).into_iter().flatten() {
                if !seen.insert(child) {
                    continue;
                }
                if self
                    .cmdlines
                    .get(&child)
                    .is_some_and(|args| is_codegraph_server(args))
                {
                    return Connection::Yes;
                }
                queue.push_back(child);
            }
        }
        Connection::No
    }
}

/// One agent as the knowledge-graph worker sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KgAgent {
    pub pane_id: String,
    pub path: PathBuf,
    pub pane_pid: Option<u32>,
}

/// Source of index freshness reports.
pub trait Prober {
    fn probe(&mut self, root: &Path) -> Option<ProbeReport>;
}

/// Minimum time between two probes of the same index root.
pub const PROBE_INTERVAL: Duration = Duration::from_secs(60);
/// A probe that has not finished by then is killed and counts as failed.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Runs `codegraph status --json <root>` at the lowest CPU and I/O priority.
/// The only CodeGraph command the sidebar runs; it creates and removes
/// SQLite's `-wal`/`-shm` files but never changes the index.
#[derive(Debug, Default)]
pub struct CodegraphProber {
    has_ionice: Option<bool>,
}

impl Prober for CodegraphProber {
    fn probe(&mut self, root: &Path) -> Option<ProbeReport> {
        use std::process::{Command, Stdio};
        let has_ionice = *self
            .has_ionice
            .get_or_insert_with(|| which::which("ionice").is_ok());
        let mut command = Command::new("nice");
        command.args(["-n", "19"]);
        if has_ionice {
            command.args(["ionice", "-c3"]);
        }
        command
            .args(["codegraph", "status", "--json"])
            .arg(root)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().ok()?;
        let deadline = Instant::now() + PROBE_TIMEOUT;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(_)) | Err(_) => return None,
                Ok(None) if Instant::now() >= deadline => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(100)),
            }
        }
        let mut output = String::new();
        std::io::Read::read_to_string(&mut child.stdout.take()?, &mut output).ok()?;
        parse_status_json(&output)
    }
}

/// Per-root probe bookkeeping kept by the worker between passes.
#[derive(Debug, Default)]
pub struct KgWorkerState {
    last_probe: std::collections::HashMap<PathBuf, Instant>,
    freshness: std::collections::HashMap<PathBuf, Freshness>,
}

impl KgWorkerState {
    /// The state of every agent. Each index root is probed at most once per
    /// [`PROBE_INTERVAL`], however many agents share it; roots no agent uses
    /// any more are forgotten.
    pub fn pass(
        &mut self,
        agents: &[KgAgent],
        procs: Option<&ProcTable>,
        prober: &mut dyn Prober,
        now: Instant,
    ) -> std::collections::HashMap<String, KnowledgeGraphState> {
        let roots: Vec<Option<PathBuf>> = agents.iter().map(|a| index_root(&a.path)).collect();
        let in_use: std::collections::HashSet<&PathBuf> = roots.iter().flatten().collect();
        self.last_probe.retain(|root, _| in_use.contains(root));
        self.freshness.retain(|root, _| in_use.contains(root));
        for root in &in_use {
            let due = self
                .last_probe
                .get(*root)
                .is_none_or(|last| now.saturating_duration_since(*last) >= PROBE_INTERVAL);
            if due {
                let report = prober.probe(root);
                self.freshness
                    .entry((*root).clone())
                    .or_default()
                    .record(report.as_ref(), now);
                self.last_probe.insert((*root).clone(), now);
            }
        }
        agents
            .iter()
            .zip(&roots)
            .map(|(agent, root)| {
                let connection = procs.map_or(Connection::Unknown, |table| {
                    table.connection(agent.pane_pid)
                });
                let stale = root
                    .as_ref()
                    .and_then(|r| self.freshness.get(r))
                    .is_some_and(Freshness::is_stale);
                (
                    agent.pane_id.clone(),
                    decide(root.is_some(), connection, stale),
                )
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    #[test]
    fn decide_follows_the_table() {
        use Connection::*;
        use KnowledgeGraphState as S;
        assert_eq!(decide(false, Yes, false), S::Available);
        assert_eq!(decide(false, No, false), S::None);
        assert_eq!(decide(false, Unknown, false), S::None);
        assert_eq!(decide(true, Yes, true), S::Attention);
        assert_eq!(decide(true, No, true), S::Attention);
        assert_eq!(decide(true, Unknown, true), S::Attention);
        assert_eq!(decide(true, Yes, false), S::Active);
        assert_eq!(decide(true, No, false), S::Attention);
        assert_eq!(decide(true, Unknown, false), S::Available);
    }

    #[test]
    fn index_root_needs_the_database() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join(".codegraph")).unwrap();
        let nested = root.join("a/b");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(index_root(&nested), None);
        std::fs::write(root.join(".codegraph/codegraph.db"), b"").unwrap();
        assert_eq!(index_root(&root), Some(root.clone()));
        assert_eq!(index_root(&nested), Some(root));
    }

    #[test]
    fn deleted_directory_has_no_index() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::create_dir_all(root.join(".codegraph")).unwrap();
        std::fs::write(root.join(".codegraph/codegraph.db"), b"").unwrap();
        let deleted = PathBuf::from(format!("{}/gone (deleted)", root.display()));
        assert_eq!(index_root(&deleted), None);
        assert_eq!(index_root(&root.join("missing")), None);
        assert_eq!(index_root(Path::new("/x/y (deleted)")), None);
    }

    #[test]
    fn parse_status_json_reads_the_fields() {
        let sample = r#"{"initialized":true,"pendingChanges":{"added":89,"modified":107,"removed":0},"index":{"reindexRecommended":false,"state":"complete"}}"#;
        assert_eq!(
            parse_status_json(sample),
            Some(ProbeReport {
                pending: 196,
                reindex_recommended: false,
                state: Some("complete".to_string()),
            })
        );
        assert_eq!(parse_status_json(r#"{"initialized":false}"#), None);
        assert_eq!(parse_status_json("garbage"), None);
    }

    fn report(pending: u64, reindex: bool, state: Option<&str>) -> ProbeReport {
        ProbeReport {
            pending,
            reindex_recommended: reindex,
            state: state.map(str::to_string),
        }
    }

    #[test]
    fn pending_changes_turn_stale_after_120_seconds() {
        let t0 = Instant::now();
        let mut f = Freshness::default();
        let pending = report(3, false, Some("complete"));
        f.record(Some(&pending), t0);
        assert!(!f.is_stale());
        f.record(Some(&pending), t0 + Duration::from_secs(119));
        assert!(!f.is_stale());
        f.record(Some(&pending), t0 + Duration::from_secs(120));
        assert!(f.is_stale());
        f.record(
            Some(&report(0, false, Some("complete"))),
            t0 + Duration::from_secs(180),
        );
        assert!(!f.is_stale());
    }

    #[test]
    fn reindex_or_partial_is_stale_at_once() {
        let now = Instant::now();
        let mut f = Freshness::default();
        f.record(Some(&report(0, true, Some("complete"))), now);
        assert!(f.is_stale());
        let mut f = Freshness::default();
        f.record(Some(&report(0, false, Some("partial"))), now);
        assert!(f.is_stale());
        let mut f = Freshness::default();
        f.record(Some(&report(0, false, None)), now);
        assert!(!f.is_stale());
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn codegraph_server_args_match() {
        assert!(is_codegraph_server(&args(&[
            "/opt/x/node",
            "--liftoff-only",
            "/opt/x/lib/dist/bin/codegraph.js",
            "serve",
            "--mcp",
        ])));
        assert!(is_codegraph_server(&args(&[
            "/usr/local/bin/codegraph",
            "serve",
            "--mcp"
        ])));
        assert!(!is_codegraph_server(&args(&[
            "node",
            "-e",
            "const x = '/opt/x/codegraph.js';",
            "serve",
            "--mcp",
        ])));
        assert!(!is_codegraph_server(&args(&["codegraph", "status"])));
        assert!(!is_codegraph_server(&args(&["grep", "codegraph serve --mcp"])));
    }

    fn fake_proc(root: &Path, pid: u32, ppid: u32, argv: &[&str]) {
        let dir = root.join(pid.to_string());
        std::fs::create_dir_all(&dir).unwrap();
        let comm = argv[0].rsplit('/').next().unwrap();
        std::fs::write(
            dir.join("stat"),
            format!("{pid} ({comm}) S {ppid} {pid} {pid} 0 -1 4194560"),
        )
        .unwrap();
        let mut cmdline = argv.join("\0");
        cmdline.push('\0');
        std::fs::write(dir.join("cmdline"), cmdline).unwrap();
    }

    #[test]
    fn connection_finds_a_grandchild_server() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fake_proc(root, 10, 1, &["/bin/bash"]);
        fake_proc(root, 11, 10, &["claude", "--continue"]);
        fake_proc(root, 12, 11, &["/usr/bin/codegraph", "serve", "--mcp"]);
        fake_proc(root, 20, 1, &["/bin/bash"]);
        fake_proc(root, 21, 20, &["claude"]);
        std::fs::create_dir_all(root.join("self")).unwrap();
        let table = ProcTable::read(root).expect("fake proc is readable");
        assert_eq!(table.connection(Some(10)), Connection::Yes);
        assert_eq!(table.connection(Some(20)), Connection::No);
        assert_eq!(table.connection(None), Connection::Unknown);
    }

    #[test]
    fn unreadable_proc_is_unknown() {
        assert!(ProcTable::read(Path::new("/nonexistent")).is_none());
    }

    struct FakeProber {
        calls: usize,
        report: Option<ProbeReport>,
    }

    impl Prober for FakeProber {
        fn probe(&mut self, _root: &Path) -> Option<ProbeReport> {
            self.calls += 1;
            self.report.clone()
        }
    }

    fn fresh_prober() -> FakeProber {
        FakeProber {
            calls: 0,
            report: Some(report(0, false, Some("complete"))),
        }
    }

    fn indexed_dir() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join(".codegraph")).unwrap();
        std::fs::write(dir.path().join(".codegraph/codegraph.db"), b"").unwrap();
        dir
    }

    fn kg_agent(pane: &str, path: &Path, pid: Option<u32>) -> KgAgent {
        KgAgent {
            pane_id: pane.to_string(),
            path: path.to_path_buf(),
            pane_pid: pid,
        }
    }

    #[test]
    fn worker_probes_a_shared_root_once() {
        let index = indexed_dir();
        let agents = [
            kg_agent("%1", index.path(), None),
            kg_agent("%2", index.path(), None),
        ];
        let mut prober = fresh_prober();
        let mut worker = KgWorkerState::default();
        let t0 = Instant::now();
        worker.pass(&agents, None, &mut prober, t0);
        worker.pass(&agents, None, &mut prober, t0 + Duration::from_secs(10));
        assert_eq!(prober.calls, 1);
        worker.pass(&agents, None, &mut prober, t0 + Duration::from_secs(61));
        assert_eq!(prober.calls, 2);
    }

    #[test]
    fn worker_skips_agents_without_index() {
        let plain = tempfile::tempdir().unwrap();
        let procs = tempfile::tempdir().unwrap();
        fake_proc(procs.path(), 10, 1, &["/bin/bash"]);
        fake_proc(procs.path(), 11, 10, &["/usr/bin/codegraph", "serve", "--mcp"]);
        fake_proc(procs.path(), 20, 1, &["/bin/bash"]);
        let table = ProcTable::read(procs.path()).unwrap();
        let agents = [
            kg_agent("%1", plain.path(), Some(10)),
            kg_agent("%2", plain.path(), Some(20)),
        ];
        let mut prober = fresh_prober();
        let states =
            KgWorkerState::default().pass(&agents, Some(&table), &mut prober, Instant::now());
        assert_eq!(prober.calls, 0);
        assert_eq!(states["%1"], KnowledgeGraphState::Available);
        assert_eq!(states["%2"], KnowledgeGraphState::None);
    }

    #[test]
    fn worker_reports_active_and_attention() {
        let index = indexed_dir();
        let procs = tempfile::tempdir().unwrap();
        fake_proc(procs.path(), 10, 1, &["/bin/bash"]);
        fake_proc(procs.path(), 11, 10, &["/usr/bin/codegraph", "serve", "--mcp"]);
        fake_proc(procs.path(), 20, 1, &["/bin/bash"]);
        let table = ProcTable::read(procs.path()).unwrap();
        let agents = [
            kg_agent("%A", index.path(), Some(10)),
            kg_agent("%B", index.path(), Some(20)),
        ];
        let mut prober = fresh_prober();
        let states =
            KgWorkerState::default().pass(&agents, Some(&table), &mut prober, Instant::now());
        assert_eq!(states["%A"], KnowledgeGraphState::Active);
        assert_eq!(states["%B"], KnowledgeGraphState::Attention);
    }

    #[test]
    fn failed_probe_keeps_previous_freshness() {
        let now = Instant::now();
        let mut f = Freshness::default();
        f.record(Some(&report(0, true, None)), now);
        f.record(None, now + Duration::from_secs(60));
        assert!(f.is_stale());
        let mut fresh = Freshness::default();
        fresh.record(None, now);
        assert!(!fresh.is_stale());
    }
}
