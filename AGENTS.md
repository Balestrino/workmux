# Agent instructions

This repository is the Octolabs product fork of
[raine/workmux](https://github.com/raine/workmux). Everything about the fork is
in [FORK.md](FORK.md): branches and remotes, how upstream is merged, the fork's
features, the checks and how the binary is installed. Read it before working
here. Upstream's guide for contributors is [CONTRIBUTING.md](CONTRIBUTING.md).

This file belongs to the fork, like `FORK.md`: upstream has no `AGENTS.md`. If
upstream adds one, keep the fork's section apart when merging (FORK.md,
"Keeping merges cheap").

## Sessions and worktrees

Common procedure: the `octolabs-worktree-session` skill and `docs/sessions.md`
of the Octolabs Worktree kit. Only what is specific to this repository is here.

- **Base:** `origin/main`, the fork's product line. `upstream/main` is a base
  only for bringing upstream in (FORK.md, "Branches and remotes").
- **Worktree:** required for every change:
  `wt switch --create <branch> --base origin/main --no-cd --yes` from the
  primary clone. The repository has no Worktrunk hooks.
- **Environment:** none. Builds and tests run from the worktree, and the
  integration tests start private tmux servers and temporary homes of their own
  (FORK.md, "Checks"). Never try a build against the tmux server or the state
  files that the host's agents are using.
- **Checks:** those of FORK.md, "Checks": at least
  `cargo fmt --all -- --check`, `cargo clippy --all-targets` and
  `cargo test --bin workmux`; the integration suite for a change in behaviour.
  A build is heavy: one at a time, at low priority on a shared host.
- **Commits and PRs:** Conventional Commits. A feature branch from `main`,
  merged back with a merge commit (`--no-ff`), never squashed or rebased. The
  session merges its own branch once the checks are green and an independent
  review has passed, unless its brief says to ask first. Upstream comes in by merging
  `upstream/main`. Notes about the fork go in `FORK.md` or `docs/superpowers/`,
  not in upstream files. Every departure from upstream behaviour is recorded in
  FORK.md, "Upstream level and differences from upstream".
- **A person's yes is needed for:** nothing in this repository, unless the
  brief says otherwise. Installing a build is not the session's step: whoever
  coordinates the sessions installs it as the host's `workmux`, after trying it
  on an isolated tmux server and keeping a copy of the previous binary, and
  reports it to the person they work for.
- **Never:** rebase or rewrite `main`; open a pull request toward
  `raine/workmux` (changes are not proposed upstream); edit an upstream file
  only to document the fork; commit host names, paths or credentials (this fork
  is public).
- **After the merge:** the installed binary does not change until someone
  builds and installs it (FORK.md, "Installing"): say in the summary whether
  the change needs it. After an upstream merge, update the upstream level in
  FORK.md and tell whoever maintains the kit's pinned Workmux version. Then
  `wt remove <branch> --yes` from the primary clone.
