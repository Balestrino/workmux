# Octolabs fork of workmux

This repository (`Balestrino/workmux`) is a product fork of
[raine/workmux](https://github.com/raine/workmux), customised for the Octolabs
multi-agent workflow (see
[octolabs-worktree](https://github.com/Octolabs-Livorno/octolabs-worktree)).
Changes are not proposed upstream.

## Branches and remotes

- `origin` is this fork, `upstream` is `raine/workmux`.
- `main` is the fork's product line: upstream plus our features. New work
  starts from `main` in a feature branch and is merged back with `--no-ff`.
- Upstream is brought in by **merging** `upstream/main` into `main`, never by
  rebasing, so the fork history is never rewritten. Merge when a fix or
  feature is needed, not necessarily at every upstream release:

  ```bash
  git fetch upstream
  git merge upstream/main
  cargo test --bin workmux && cargo clippy --all-targets
  git push origin main
  ```

## Keeping merges cheap

- Put fork features in their own modules (e.g. `src/workspaces.rs`,
  `src/command/dashboard/worktree_info.rs`) and keep the hooks into upstream
  code small.
- Do not edit upstream files only for fork documentation: fork-specific notes
  live here or in `docs/superpowers/`. Config keys read only by the fork are
  documented with the feature.
- Cutting the upstream link for good (GitHub fork detach, crate rename) is
  deferred until merging costs more than it brings.

## Fork features

- **Workspaces dashboard** (`dashboard.workspaces`): lists the worktrees of
  every Git repository under the configured workspace roots, with stable
  project names, env/agent/unpushed columns and orphan rows.
- **External worktree info provider** (`dashboard.worktree_info_command`,
  `dashboard.worktree_extra_columns`): per-worktree runtime data from an
  external command (the octolabs-worktree CLI) shown as an info panel or as
  table columns.

Design and plan: `docs/superpowers/`.

## Installing

A single binary is built from `main` and installed as `workmux`, so agent
hooks calling `~/.local/bin/workmux` use the fork build; `workmux-ws` is kept
as a symlink for existing sessions:

```bash
cargo build --release
install -m 755 target/release/workmux ~/.local/bin/workmux
ln -sfn workmux ~/.local/bin/workmux-ws
```
