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
    /// Whether a probe has ever reported on this root.
    probed: bool,
}

impl Freshness {
    /// Fold in one probe result; `None` (a failed probe) changes nothing.
    pub fn record(&mut self, report: Option<&ProbeReport>, now: Instant) {
        let Some(report) = report else {
            return;
        };
        self.probed = true;
        let broken = report.reindex_recommended
            || report
                .state
                .as_deref()
                .is_some_and(|state| state != "complete");
        let lagging = if report.pending > 0 {
            let since = *self.pending_since.get_or_insert(now);
            now.saturating_duration_since(since) >= PENDING_GRACE
        } else {
            self.pending_since = None;
            false
        };
        self.stale = broken || lagging;
    }

    /// Fold in a build seen in progress, when no probe runs. A rebuild
    /// leaves no usable index until it ends; another write keeps the last
    /// result (a server's auto-sync must not flash a warning), and with no
    /// result yet (a first `codegraph init`) the index counts as stale.
    pub fn record_build(&mut self, build: Build) {
        if build == Build::Rebuild || !self.probed {
            self.stale = true;
        }
    }

    pub fn is_stale(&self) -> bool {
        self.stale
    }
}

/// A CodeGraph build holding an index root, seen from the lock files
/// CodeGraph keeps in `.codegraph/` (CodeGraph 1.6.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Build {
    /// `rebuild.pid`: a `codegraph index`, from before it recreates the
    /// database until the new index is complete.
    Rebuild,
    /// `codegraph.lock`: any other write to the index, from the whole of a
    /// `codegraph init` to a `codegraph sync` or a server's auto-sync.
    Write,
}

/// The pid in a CodeGraph lock file: a bare number (`codegraph.lock`) or
/// JSON with a `pid` field (`rebuild.pid`).
fn lock_pid(raw: &str) -> Option<u32> {
    let raw = raw.trim();
    raw.parse::<u32>()
        .ok()
        .or_else(|| {
            let value: serde_json::Value = serde_json::from_str(raw).ok()?;
            u32::try_from(value.get("pid")?.as_u64()?).ok()
        })
        .filter(|pid| *pid > 0)
}

/// The build holding the index at `root`, if any: a lock file whose holder
/// `holder_alive` says is running. Never opens the database. A lock left
/// behind by a dead process holds nothing, so probes resume.
pub fn build_in_progress(root: &Path, holder_alive: fn(u32) -> bool) -> Option<Build> {
    let dir = root.join(".codegraph");
    let held = |name: &str| {
        std::fs::read_to_string(dir.join(name))
            .ok()
            .and_then(|raw| lock_pid(&raw))
            .is_some_and(holder_alive)
    };
    if held("rebuild.pid") {
        Some(Build::Rebuild)
    } else if held("codegraph.lock") {
        Some(Build::Write)
    } else {
        None
    }
}

/// Whether `pid` is a running CodeGraph process: alive to `kill(pid, 0)`
/// and, where `/proc` shows its command line, running `codegraph` or
/// `codegraph.js`. A pid reused by another program, or one from another pid
/// namespace (an agent in a sandbox), is not a holder.
pub fn codegraph_process_alive(pid: u32) -> bool {
    let Ok(raw) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 sends nothing; it only checks that the pid exists.
    let alive = unsafe { libc::kill(raw, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    if !alive {
        return false;
    }
    match std::fs::read(format!("/proc/{pid}/cmdline")) {
        Ok(cmdline) => cmdline.split(|b| *b == 0).any(|arg| {
            let arg = String::from_utf8_lossy(arg);
            let name = arg.rsplit('/').next().unwrap_or(&arg);
            name == "codegraph" || name == "codegraph.js"
        }),
        // No `/proc` (macOS): liveness is all there is to go on.
        Err(_) => true,
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

/// Whether a command line is the `workmux sandbox run` supervisor.
fn is_sandbox_supervisor(args: &[String]) -> bool {
    args.iter()
        .position(|arg| arg.rsplit('/').next() == Some("workmux"))
        .is_some_and(|i| {
            args.get(i + 1).map(String::as_str) == Some("sandbox")
                && args.get(i + 2).map(String::as_str) == Some("run")
        })
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
            let Some(pid) = entry
                .file_name()
                .to_str()
                .and_then(|s| s.parse::<u32>().ok())
            else {
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
    /// A pane running a workmux sandbox (container or Lima) hosts its agent
    /// elsewhere, so without a local server its connection is unknown.
    pub fn connection(&self, pane_pid: Option<u32>) -> Connection {
        let Some(root) = pane_pid else {
            return Connection::Unknown;
        };
        let mut sandboxed = false;
        let mut seen = std::collections::HashSet::from([root]);
        let mut queue = std::collections::VecDeque::from([root]);
        while let Some(pid) = queue.pop_front() {
            for &child in self.children.get(&pid).into_iter().flatten() {
                if !seen.insert(child) {
                    continue;
                }
                if let Some(args) = self.cmdlines.get(&child) {
                    if is_codegraph_server(args) {
                        return Connection::Yes;
                    }
                    sandboxed |= is_sandbox_supervisor(args);
                }
                queue.push_back(child);
            }
        }
        if sandboxed {
            Connection::Unknown
        } else {
            Connection::No
        }
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
/// The only CodeGraph command the sidebar runs. It opens the database
/// read-write: SQLite creates and removes its `-wal`/`-shm` files, and
/// CodeGraph sets `journal_mode = WAL` on open, which makes a build running
/// at the same time fail with `database is locked`. The worker therefore
/// never runs it while [`build_in_progress`] sees a build.
#[derive(Debug)]
pub struct CodegraphProber {
    has_ionice: Option<bool>,
    program: PathBuf,
    timeout: Duration,
}

impl Default for CodegraphProber {
    fn default() -> Self {
        Self::with_program(Path::new("codegraph"), PROBE_TIMEOUT)
    }
}

impl CodegraphProber {
    pub fn with_program(program: &Path, timeout: Duration) -> Self {
        Self {
            has_ionice: None,
            program: program.to_path_buf(),
            timeout,
        }
    }
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
            .arg(&self.program)
            .args(["status", "--json"])
            .arg(root)
            .current_dir(root)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        let mut child = command.spawn().ok()?;
        let deadline = Instant::now() + self.timeout;
        loop {
            match child.try_wait() {
                Ok(Some(status)) if status.success() => break,
                Ok(Some(_)) | Err(_) => return None,
                Ok(None) if Instant::now() >= deadline => {
                    stop(&mut child);
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

/// How long a timed-out probe gets to exit after SIGTERM before SIGKILL.
const PROBE_STOP_GRACE: Duration = Duration::from_secs(2);

/// Stop a timed-out probe: SIGTERM first, so SQLite can close the database
/// and remove its `-wal`/`-shm` files, SIGKILL only if it does not exit.
fn stop(child: &mut std::process::Child) {
    if let Ok(pid) = i32::try_from(child.id()) {
        // SAFETY: plain kill(2) on our own child's pid, which we have not
        // reaped yet, so it cannot have been reused.
        unsafe {
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + PROBE_STOP_GRACE;
        while Instant::now() < deadline {
            if matches!(child.try_wait(), Ok(Some(_))) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    let _ = child.kill();
    let _ = child.wait();
}

/// Per-root probe bookkeeping kept by the worker between passes.
#[derive(Debug)]
pub struct KgWorkerState {
    last_probe: std::collections::HashMap<PathBuf, Instant>,
    freshness: std::collections::HashMap<PathBuf, Freshness>,
    /// Whether a lock holder runs; [`codegraph_process_alive`] but in tests.
    holder_alive: fn(u32) -> bool,
}

impl Default for KgWorkerState {
    fn default() -> Self {
        Self {
            last_probe: std::collections::HashMap::new(),
            freshness: std::collections::HashMap::new(),
            holder_alive: codegraph_process_alive,
        }
    }
}

impl KgWorkerState {
    /// The state of every agent. Each index root is probed at most once per
    /// [`PROBE_INTERVAL`], however many agents share it, and never while a
    /// build holds it: the first pass after the build ends probes it. Roots
    /// no agent uses any more are forgotten.
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
            if let Some(build) = build_in_progress(root, self.holder_alive) {
                self.freshness
                    .entry((*root).clone())
                    .or_default()
                    .record_build(build);
                continue;
            }
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
        assert!(!is_codegraph_server(&args(&[
            "grep",
            "codegraph serve --mcp"
        ])));
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
    fn sandboxed_pane_is_unknown() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        fake_proc(root, 30, 1, &["/bin/bash"]);
        fake_proc(
            root,
            31,
            30,
            &[
                "/usr/local/bin/workmux",
                "sandbox",
                "run",
                "/w",
                "--",
                "claude",
            ],
        );
        fake_proc(root, 32, 31, &["docker", "exec", "-it", "c1", "claude"]);
        let table = ProcTable::read(root).unwrap();
        assert_eq!(table.connection(Some(30)), Connection::Unknown);
    }

    #[test]
    fn timed_out_probe_is_asked_to_stop_first() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("terminated");
        let script = dir.path().join("slow-codegraph");
        std::fs::write(
            &script,
            format!(
                "#!/bin/bash\ntrap 'echo yes > {}; exit 0' TERM\nsleep 30 &\nwait\n",
                marker.display()
            ),
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut prober = CodegraphProber::with_program(&script, Duration::from_secs(1));
        assert_eq!(prober.probe(dir.path()), None);
        assert!(marker.exists(), "the probe got SIGTERM before any SIGKILL");
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
        fake_proc(
            procs.path(),
            11,
            10,
            &["/usr/bin/codegraph", "serve", "--mcp"],
        );
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
        fake_proc(
            procs.path(),
            11,
            10,
            &["/usr/bin/codegraph", "serve", "--mcp"],
        );
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
    fn lock_pid_reads_both_lock_formats() {
        assert_eq!(lock_pid("12345"), Some(12345));
        assert_eq!(lock_pid("12345\n"), Some(12345));
        assert_eq!(
            lock_pid("{\"pid\":42,\"mode\":\"rebuild\",\"startedAt\":1,\"ready\":false}\n"),
            Some(42)
        );
        assert_eq!(lock_pid("0"), None);
        assert_eq!(lock_pid("{\"mode\":\"rebuild\"}"), None);
        assert_eq!(lock_pid("garbage"), None);
        assert_eq!(lock_pid(""), None);
    }

    fn lock(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(".codegraph").join(name), content).unwrap();
    }

    #[test]
    fn build_in_progress_needs_a_live_holder() {
        let index = indexed_dir();
        let root = index.path();
        assert_eq!(build_in_progress(root, |_| true), None);
        lock(root, "codegraph.lock", "4242");
        assert_eq!(build_in_progress(root, |_| true), Some(Build::Write));
        assert_eq!(build_in_progress(root, |_| false), None);
        assert_eq!(build_in_progress(root, |pid| pid == 4243), None);
        lock(root, "rebuild.pid", r#"{"pid":4243,"mode":"rebuild"}"#);
        assert_eq!(build_in_progress(root, |_| true), Some(Build::Rebuild));
        assert_eq!(
            build_in_progress(root, |pid| pid == 4242),
            Some(Build::Write)
        );
        // writer.pid belongs to a long-lived server, not to a build.
        let index = indexed_dir();
        lock(
            index.path(),
            "writer.pid",
            r#"{"pid":4244,"mode":"daemon"}"#,
        );
        assert_eq!(build_in_progress(index.path(), |_| true), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn codegraph_holder_must_be_running_codegraph() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("codegraph");
        // No `exec`: the shell, whose cmdline is `/bin/sh <dir>/codegraph`,
        // stays the holder.
        std::fs::write(&script, "#!/bin/sh\nread line\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut child = std::process::Command::new(&script)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let pid = child.id();
        // Until the kernel has exec'd the script, the child's cmdline is
        // still this test binary's.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !codegraph_process_alive(pid) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(codegraph_process_alive(pid));
        // This test binary is alive but is not CodeGraph (a reused pid).
        assert!(!codegraph_process_alive(std::process::id()));
        drop(child.stdin.take());
        child.wait().unwrap();
        assert!(!codegraph_process_alive(pid));
        assert!(!codegraph_process_alive(u32::MAX));
    }

    #[test]
    fn build_marks_stale_only_when_unusable_or_unknown() {
        let now = Instant::now();
        let mut f = Freshness::default();
        f.record_build(Build::Write);
        assert!(f.is_stale(), "a first init has no usable index");
        let mut f = Freshness::default();
        f.record(Some(&report(0, false, Some("complete"))), now);
        f.record_build(Build::Write);
        assert!(!f.is_stale(), "an auto-sync keeps the last result");
        f.record_build(Build::Rebuild);
        assert!(f.is_stale(), "a rebuild recreates the database");
        f.record(Some(&report(0, false, Some("complete"))), now);
        assert!(!f.is_stale());
    }

    fn worker_with_live_holders() -> KgWorkerState {
        KgWorkerState {
            holder_alive: |_| true,
            ..KgWorkerState::default()
        }
    }

    #[test]
    fn worker_does_not_probe_during_a_build() {
        let index = indexed_dir();
        let agents = [kg_agent("%1", index.path(), None)];
        let mut prober = fresh_prober();
        let mut worker = worker_with_live_holders();
        let t0 = Instant::now();
        lock(index.path(), "codegraph.lock", "4242");
        let states = worker.pass(&agents, None, &mut prober, t0);
        assert_eq!(prober.calls, 0);
        assert_eq!(states["%1"], KnowledgeGraphState::Attention);
        let states = worker.pass(&agents, None, &mut prober, t0 + Duration::from_secs(120));
        assert_eq!(prober.calls, 0);
        assert_eq!(states["%1"], KnowledgeGraphState::Attention);
        // The first pass after the build probes at once.
        std::fs::remove_file(index.path().join(".codegraph/codegraph.lock")).unwrap();
        let states = worker.pass(&agents, None, &mut prober, t0 + Duration::from_secs(125));
        assert_eq!(prober.calls, 1);
        assert_eq!(states["%1"], KnowledgeGraphState::Available);
    }

    #[test]
    fn worker_keeps_a_fresh_index_through_a_sync_but_not_a_rebuild() {
        let index = indexed_dir();
        let agents = [kg_agent("%1", index.path(), None)];
        let mut prober = fresh_prober();
        let mut worker = worker_with_live_holders();
        let t0 = Instant::now();
        worker.pass(&agents, None, &mut prober, t0);
        assert_eq!(prober.calls, 1);
        lock(index.path(), "codegraph.lock", "4242");
        let states = worker.pass(&agents, None, &mut prober, t0 + Duration::from_secs(61));
        assert_eq!(prober.calls, 1);
        assert_eq!(states["%1"], KnowledgeGraphState::Available);
        lock(
            index.path(),
            "rebuild.pid",
            r#"{"pid":4243,"mode":"rebuild"}"#,
        );
        let states = worker.pass(&agents, None, &mut prober, t0 + Duration::from_secs(66));
        assert_eq!(prober.calls, 1);
        assert_eq!(states["%1"], KnowledgeGraphState::Attention);
    }

    #[test]
    fn worker_probes_past_a_dead_holders_lock() {
        let index = indexed_dir();
        let agents = [kg_agent("%1", index.path(), None)];
        lock(index.path(), "codegraph.lock", "4242");
        lock(
            index.path(),
            "rebuild.pid",
            r#"{"pid":4243,"mode":"rebuild"}"#,
        );
        let mut prober = fresh_prober();
        let mut worker = KgWorkerState {
            holder_alive: |_| false,
            ..KgWorkerState::default()
        };
        let states = worker.pass(&agents, None, &mut prober, Instant::now());
        assert_eq!(prober.calls, 1);
        assert_eq!(states["%1"], KnowledgeGraphState::Available);
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
