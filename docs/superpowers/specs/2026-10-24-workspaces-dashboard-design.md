# Dashboard Workspaces and External Worktree Info — Design

Date: 2026-10-24
Fork: `Balestrino/workmux` (upstream `raine/workmux`), branch `feat/workspaces-dashboard`
Base: upstream `main` at `3cef2122` (workmux v0.1.277)

## Intent

The Octolabs team runs many coding agents in parallel across several
repositories laid out as `~/workspaces/<repo>/primary-clone` (the canonical
clone) with linked worktrees next to it (`~/workspaces/<repo>/<branch>`),
nested inside the clone, or elsewhere on disk (for example
`~/.vibe/worktrees/...`). Each worktree may own a runtime "environment"
managed by the Octolabs Worktree kit (state, backend/console URLs).

Today the kit's `overview --watch` command shows all of this, but as a
plain text screen redrawn every 5 s. The workmux dashboard already has the
interaction we want (preview, diff, jump to pane), but:

1. its Worktrees tab only shows one repository at a time, discovered from
   the cwd or from tracked agents;
2. its Project column is wrong for this layout: it shows `primary-clone`
   for the main checkout and the worktree directory name for linked
   worktrees;
3. it knows nothing about the kit's environments.

The goal is for `workmux dashboard` to replace `overview --watch` as the
daily view.

## Goals

1. **All worktrees of all workspace repositories in one Worktrees table.**
   A new config setting lists workspace directories. Every repository found
   there contributes all worktrees that `git worktree list` reports for it,
   wherever they live (sibling, nested, or outside the workspace).
2. **Correct project name** in both the Worktrees and Agents tabs: the
   directory directly under the workspace root that contains the repository
   (`orkestra-commons`), applied to every worktree of that repository.
3. **External worktree info provider.** A configured shell command returns
   JSON with, per worktree path, a status label with a tone, an ID and a
   list of label → value details. The dashboard shows it in a new **Env**
   column and in the Info panel.
4. **Orphan entries.** The provider can return entries whose worktree no
   longer exists. They appear as dimmed rows at the end of the Worktrees
   table, marked "(missing)", selectable, with their details and a hint in
   the Info panel (layout "variant A", chosen by the user; see mockup
   below).
5. **Kit side.** The Octolabs Worktree kit gains a command that emits this
   JSON from its registry, and its docs explain the configuration.

## Non-goals

- No actions on environments from the dashboard (start/stop/forget, open
  URL). Read-only in this round.
- No change to how the kit's setup installs workmux. The fork binary is
  installed by hand for now.
- No upstream PR in this round. Keep the changes generic so one is
  possible later (nothing Octolabs-specific in workmux).
- No change to the Agents tab beyond the Project name.
- No change to `workmux add/remove/merge` path logic (`__worktrees`
  layout stays the default for creation).

## Anti-goals (failure even if it "works")

- The dashboard becomes slow or janky with ~6 repositories and ~15
  worktrees, or blocks the UI thread on git or the provider command.
- A failing or missing provider command breaks or clutters the dashboard.
  Without the setting, behavior must be identical to upstream.
- Worktree actions (remove, close, jump, diff, merge, add) can be
  triggered on an orphan row, or act on the wrong repository when several
  repositories are listed together.
- Octolabs names, paths or kit concepts appear in workmux code or default
  config.

## Constraints

- Rust, ratatui; follow the existing dashboard structure
  (`src/command/dashboard/app/*`, `ui/*`), config in `src/config.rs`,
  background work on threads that send `AppEvent`s.
- Config changes are optional fields; absent fields keep upstream behavior.
- Unit tests in Rust for discovery, project naming, provider parsing and
  table rendering (the dashboard already has buffer-based render tests in
  `ui/worktree.rs`). Run `cargo fmt`, `cargo clippy`, `cargo test`.
  Python integration tests are optional.
- Development and testing happen on host `tore-dev-1` (`~/workmux`), which
  has no Octolabs repositories: use fixture repositories that reproduce the
  layout (`<ws>/<repo>/primary-clone` + sibling, nested and external
  worktrees).

## Decided behavior

### Configuration (global config only, `dashboard` section)

```yaml
dashboard:
  # Directories scanned for repositories. ~ is expanded.
  workspaces:
    - ~/workspaces
  # Command run through `sh -c`; stdout must be the JSON below.
  worktree_info_command: node "$OCTOLABS_WORKTREE_CLI" overview --workmux-info
  # Seconds between provider runs. Default 15, minimum 2.
  # worktree_info_interval: 15
  # Optional; Env column added automatically when the command is set.
  # worktree_columns: [number, project, worktree, git, env, mux, age]
```

*Builder's call:* `workspaces`, `worktree_info_command` and
`worktree_info_interval` are **global-only**. In a project `.workmux.yaml`
they are ignored with a warning, following upstream's existing rule for
command settings (`auto_name.command`, `host_commands`, … in
`src/config.rs` `merge`): a cloned repository must not be able to make
the dashboard run arbitrary commands.

### Repository discovery

- For each workspace directory: a repository is any directory at depth 1
  or 2 below it whose `.git` is a directory (a main checkout). Depth 2
  covers `<repo>/primary-clone`. Do not descend into a repository once
  found. Skip hidden directories.
- Deduplicate repositories by their common Git directory (`repo_key`,
  already in `WorktreeInfo`).
- Repositories discovered the existing way (from tracked agents) are
  added too, so nothing currently visible disappears.
- When `workspaces` is set, the Worktrees tab defaults to **all
  repositories**. The existing project picker (`P`-style modal) gains an
  "All projects" entry at the top and still lets the user narrow to one.
  When `workspaces` is not set, the tab behaves exactly as upstream.

### Project name

- If a repository's main checkout is inside a workspace directory, the
  project name is the first path component below that workspace directory
  (`~/workspaces/orkestra-commons/primary-clone` → `orkestra-commons`;
  `~/workspaces/foo` → `foo`).
- When several repositories share that first component, each uses its full
  relative path instead. On the Octolabs VM,
  `~/workspaces/cross-repo-sync/{commons,hermes,gaterei,octolabs}` are four
  separate clones. **The user wants them shown**, as
  `cross-repo-sync/commons`, `cross-repo-sync/hermes`, and so on. No
  exclude setting.
- The project picker identifies repositories by `repo_key`, not by name.
  Today it deduplicates by name, which would merge distinct repositories.
- Every worktree of that repository gets that name, including worktrees
  outside the workspace. Look up by `repo_key` rather than by path.
- Agents tab: an agent's project is the project of the repository its pane
  path belongs to (resolve the common Git dir). Fall back to the current
  `extract_project_name` behavior when the path is not in a known
  repository.

### Provider JSON contract (schema version 1)

```json
{
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
}
```

- `tone` ∈ `success | warning | error | info | muted`, mapped to the
  existing palette colors (`success`, `accent`/warning, `danger`, `info`,
  `dimmed`). Unknown tone → `muted`.
- A worktree may have at most one entry; if the provider sends several for
  the same path, show the first. Paths are compared after
  canonicalization.
- Unknown fields are ignored. `schemaVersion` other than 1 → treat as
  provider error.

### Running the provider

- Runs in a background thread on its own timer (`worktree_info_interval`,
  default 15 s) while the dashboard is open, plus once immediately when
  the dashboard opens or the Worktrees tab is shown. Never more than one
  run at a time. Timeout 10 s, after which the child is killed.
  *Builder's call:* a timer separate from the 5 s worktree refresh,
  because the provider may be slower than git. `overview --json` measured
  about 1 s on the Octolabs VM with 3 environments, and the kit command
  below is lighter.
- *Builder's call:* if an orphan's canonical path matches a row that is
  listed, the entry is attached to that row instead of becoming a
  "(missing)" row. This protects against the provider and workmux seeing
  different sets of repositories.
- On failure (missing command, non-zero exit, timeout, invalid JSON): keep
  the last good result; if there has never been one, the Env column and
  orphan rows simply do not appear. A short error indicator in the footer
  or table header is welcome but not required.
- Not run at all when `worktree_info_command` is unset.

### Display (variant A)

- **Env column**: `● <label>` colored by tone; `-` (dimmed) for worktrees
  without an entry. Present in the default column list but hidden when no
  provider data exists, the way the PR column hides itself today.
- **Orphan rows**: after all real rows, dimmed, with the project in the
  Project column, `<name> (missing)` in the Worktree column (the
  "(missing)" part in the error color), the status in Env, other cells
  empty. They have no jump number.
- **Info panel**: for a real worktree with an entry, add after the
  existing lines: `Env     <label> · <short id (first 8 chars)>` and one
  line per detail `<label padded> <value>`. For an orphan row: Project,
  Path, Env, details, and the hint on its own line.
- **Actions on orphan rows**: every worktree action is a no-op (optionally
  a brief footer message); the git log pane shows "(worktree missing)".
- *Builder's call:* the Worktree and Age columns keep upstream's rendering
  (`dev` for a main checkout, `handle →branch` when they differ, upstream
  age units). The mockup below shows the layout and the new elements, not
  exact cell text. One change: on a main checkout, the Info panel title
  shows `<project> · <branch>` instead of the handle, which would be
  `primary-clone` for every repository.
- With several repositories listed, every action on a real row is sent to
  that row's repository, using its `repo_key` and main root and never the
  cwd repository.

Mockup accepted by the user (variant A), as layout reference:

```
 #  Project            Worktree                  Git     Env        Mux Age
 1  orkestra           dev (primary-clone)       ↓173    -          -   3g
 2  orkestra-commons   dev (primary-clone)               -          -   5g
 3  orkestra-commons   vibe/security-audit-auth  +12 -3  -          ●   2h
 4  orkestra-gaterei   work/linkedin-connector   ↑2      ● ready    ●   1g
    orkestra-commons   dev-sandbox (missing)             ? unknown
┌ work/linkedin-connector ──────────┐┌ git log ─────────────
│ Branch  work/linkedin-connector   ││ a1b2c3d feat: ...
│ Base    dev (↑2)                  ││
│ Env     ready · dd4b4587          ││
│ backend http://127.0.0.1:18600    ││
│ console http://orkestra-…:18601   ││
```

### Kit side (repository `Octolabs-Livorno/octolabs-worktree`)

- `overview --workmux-info` prints the JSON above. It reads only the kit
  registry (`LifecycleService.list()`): no `git status`, and no call to
  `workmux status`, which would make the dashboard call itself back. There
  is one `worktrees` entry per environment whose worktree exists on disk
  (`worktreePresent`), whether or not it lies in a workspace. There is one
  `orphans` entry per environment whose worktree no longer exists:
  - `project` comes from the workspace layout and the same naming rule
    as workmux;
  - `name` is the missing directory's basename;
  - `hint` is the `forget` command. Status tone
  mapping: `ready` → success, `starting|stopping|preparing` → info,
  `failed|error` → error, everything else → warning.
- Docs (`docs/workmux.md`, runbook "dove guardare") describe the fork, the
  config above and that `overview --watch` is superseded.

## Left to the builder

- Exact struct/module names, where discovery lives (likely next to
  `discover_project_roots` in `src/command/list.rs`, moved somewhere
  shared), and whether `workmux list --all` also uses the new discovery
  (nice to have, not required).
- How multi-repository fetching is parallelized and how per-repository
  config (`Config::load_with_location_from`) is applied.
- Footer/error indicator wording; key handling details of the picker's
  "All projects" entry.
- Test fixture construction.

## Delivery

- Branch `feat/workspaces-dashboard` in `Balestrino/workmux`, pushed to
  the fork. `main` of the fork stays equal to upstream.
- A kit branch with `overview --workmux-info` and docs, as a PR on
  `octolabs-worktree`.
- Manual install on the Octolabs VM afterwards (`cargo install --path .`
  on tore-dev-1 and copy, or build on the VM); not part of this design.
