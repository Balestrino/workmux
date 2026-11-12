# Sidebar knowledge-graph line — design

Date: 2026-11-12. Status: proposed (fork feature).

## Intent

Agents on a host can be helped by a code knowledge graph (today CodeGraph:
an index in `.codegraph/` plus an MCP server, `codegraph serve --mcp`, that
the agent starts). The person watching the sidebar wants to see, for every
agent, two things at a glance:

1. whether the agent's worktree has an index ready to use, and
2. whether the agent has the graph's MCP server connected,

and to notice the cases that need them to act (an index that exists but is
not used, or an index that has fallen behind the code).

## Goals

- One extra line in every agent tile of the sidebar, below the existing
  lines, showing the knowledge-graph state of that agent.
- Four states, decided by the person:

  | State | Shown | Meaning |
  |---|---|---|
  | none | `No knowledge graph` (dimmed) | no server connected to the agent and no index for its worktree |
  | available | `○ codegraph` (dimmed, neutral) | server connected, but the worktree has no index. This is the normal case today: the server is configured for every agent, indexes exist only in a few checkouts |
  | attention | `⚠ codegraph` (warning colour) | needs action: an index exists but the agent has no server connected, **or** the index is stale (see below) |
  | active | `● codegraph`, green, blinking | index present and fresh, and the server connected to this agent |

- "Connected" means the graph's MCP server process is running as a
  descendant of the agent's pane process, whether or not the agent is
  querying it right now (decided by the person).
- "Stale" (decided by the person: an old index must warn) means any of, as
  reported by `codegraph status --json` for the index root:
  `pendingChanges.added + modified + removed > 0`,
  `index.reindexRecommended == true`, or `index.state` not `complete`
  (`partial`, `indexing`, `failed`).
- Only CodeGraph for now (decided by the person).
- **Stale needs to last (designer's call).** While its MCP server is
  connected, CodeGraph runs a file watcher that auto-syncs the index
  (observed: "graph will auto-sync on changes", syncs of 5-187 files in
  0.6-18 s). Pending changes are therefore normal for a few seconds after
  every edit. They count as stale only when they persist for more than
  2 minutes (two consecutive probes at least 2 minutes apart both report
  pending changes). `reindexRecommended` and a non-`complete` state count at
  once. Without a connected server nothing syncs, so an index that fell
  behind (e.g. after a `git pull`) shows the attention state.

## Non-goals

- Graphify, Serena and similar tools. **Future work, to be recorded in
  FORK.md as planned:** make the detection a list of tool adapters (index
  marker, server process signature, freshness probe) so a tool is added by
  configuration. The first implementation should keep the CodeGraph
  detection behind one small internal interface so that step stays cheap,
  but must not build a configuration surface for it yet.
- Showing whether the agent is querying the graph at this moment.
- The dashboard. Only the sidebar gets the line.
- Putting the information on an existing line (the person chose a full
  extra line, accepting that fewer agents fit).

## Anti-goals (a failure even if it works)

- The sidebar creating, syncing or repairing an index, starting a CodeGraph
  daemon or watcher, or writing anything inside `.codegraph/`. It only
  reads. (The kit that runs on this host has an accepted decision that
  indexes and daemons are created only by whoever works in a worktree.)
- A probe that makes the host noticeably slower: indexes can be hundreds of
  MB and several agents share the host.
- A line that flickers between states because a probe timed out.
- Host names, user paths or credentials in the fork (it is public).

## Constraints

- The sidebar daemon already polls per worktree (git status, PR, checks)
  and pushes snapshots to the `_sidebar-run` clients; the new data travels
  the same way (a new cache in the snapshot, `#[serde(default)]` so mixed
  daemon/client versions keep working).
- tmux already reports `pane_pid` (`LIVE_PANE_FORMAT` in
  `src/multiplexer/tmux.rs`). Process ancestry is read from `/proc`
  (Linux). On other platforms, or when ancestry cannot be read, the
  "connected" half is unknown.
- **Unknown connection (designer's call).** When the connection cannot be
  observed — Zellij panes (no pane pid), agents in a container or Lima
  sandbox (their processes are not descendants of the pane), or `/proc`
  unreadable — the line never warns for a missing server: with an index it
  shows `○ codegraph` (or `⚠` only if the index itself is stale), without
  one `No knowledge graph`. Remote agents (`AgentPane.remote` set) get no
  knowledge-graph line content at all (the line is left empty), because
  their directory is on another machine.
- Width: the sidebar can be ~25 columns; every variant must fit, and the
  existing truncation (grapheme-aware, see FORK.md "Emoji with the
  variation selector") applies. Use the plain `⚠` (U+26A0, no U+FE0F) and
  1-column glyphs `○` / `●` so the line has no wide-cell hazards.
- Agents whose directory no longer exists (worktree removed under a
  running agent; tmux reports the path with a ` (deleted)` suffix) have no
  index by definition.

## Decided how

- **Index lookup:** the nearest ancestor of the agent's directory (the
  agent-reported `cwd` when known, else the pane path) that contains
  `.codegraph/codegraph.db`. A bare `.codegraph/` without the database does
  not count — CodeGraph keeps a global `~/.codegraph/` (daemons, telemetry)
  that must never be taken for an index.
- **Server detection:** a process whose command line runs CodeGraph with
  `serve --mcp`, found among the descendants of the pane's process. Match on
  the arguments, not the executable name (the server runs under a bundled
  `node`).
- **Freshness probe:** `codegraph status --json <index root>`, run by the
  daemon as a child process at the lowest CPU and I/O priority, with a
  timeout, at most once per index root per minute, and only for index roots
  that some listed agent uses. Results are cached per index root.
- **Line placement:** a new template token, `{knowledge_graph}`, and a
  fourth line in the default tile templates of the flat sidebar. The
  grouped presentation also gets the token on a line of its own. The
  horizontal (top/bottom) bar keeps its three lines (designer's call: a
  taller bar costs every window a row); the token is available there for
  anyone who adds it to their template.
- **Blink:** the active state alternates between bright and dimmed green on
  the sidebar's own redraw tick (the one that animates the working
  spinner), not the terminal's SGR blink attribute, whose support in
  terminals (VS Code's included) is not reliable. Like the spinner, it
  animates only while the sidebar's window is the active one.

## Left to the builder

- **Settled 2026-11-12 (decided by the person):** `codegraph status --json`
  opens the database read-write and creates then removes SQLite's
  `codegraph.db-wal` / `codegraph.db-shm`; the index data is not changed
  (traced on CodeGraph 1.6.2: 1.5 s, 168 MB on a 197 MB index). The person
  accepted it as the freshness probe; no other `codegraph` subcommand runs.
- (Superseded by the line above.) Verify that `codegraph status --json` is read-only (no writes, no WAL
  checkpoint, no lock files, no daemon) on CodeGraph 1.6.2, and measure its
  time on a large index. If it is not read-only or is too slow (say, over
  2 s at idle), fall back to a cheaper staleness signal and document the
  choice; never accept a writing probe.
- What to show while the first probe for an index root has not finished
  (suggestion: treat the index as fresh until proven stale, so the line
  does not start on a warning), and what a failed or timed-out probe does
  (keep the last known result; with none, show the attention state only if
  the database is unreadable).
- How often ancestry is rescanned (the server starts a few seconds after
  the agent; rescanning on the existing pane poll is likely enough).
- Exact colours from the existing palette, and the wording of an optional
  config switch to turn the line off.
- Tests: unit tests for the state decision and the index lookup (including
  the global `~/.codegraph/` case and deleted directories), a rendering test
  at narrow width, and an isolated-tmux check with a fake server process.
- Documentation: the token in the sidebar customization guide, an entry in
  FORK.md "Fork features", and the Graphify/Serena follow-up in FORK.md.
