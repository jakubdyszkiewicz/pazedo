# pazedo: a Zed fork that is a Paseo client

pazedo is a fork of [Zed](https://github.com/zed-industries/zed) that uses a
[Paseo](https://paseo.sh) daemon for workspaces, agent sessions and terminals, while Zed keeps
doing editing, files, language servers, git and search. The full brief (goal, research,
milestones) lives outside the repo in `~/dev/BRIEF.md` on the Mac mini.

Docs in this folder:

- [development.md](./development.md): building on the Mac mini, measurements, screenshots,
  the Paseo test daemon.

This folder is outside `docs/src/`, so Zed's mdBook and docs automation ignore it.

## Repository layout and remotes

- Local only for now: there is no GitHub fork yet. The clone's remote for Zed is named
  `upstream` (not `origin`) so that a fork can be added as `origin` later without renaming.
- Fork code goes into new crates (planned: `crates/paseo_client`, `crates/paseo_ui`) and fork
  tooling into `script/pazedo/`. Upstream files are changed only where there is no seam, and
  every such change is listed under [Upstream touch points](#upstream-touch-points).

## Syncing with upstream

Upstream moves fast (on the order of 100 commits a week, unverified), so sync often and keep
the diff against upstream small.

Procedure (merge, not rebase):

```sh
git fetch upstream
git switch main
git merge upstream/main          # resolve conflicts in the touch points listed below
cargo build -p zed               # then run it and check the fork's UI on screen
```

Why merge rather than rebase: several agents may work in worktrees branched off `main` at the
same time; rebasing `main` would rewrite the commits they are based on. Merging also resolves
each conflict once, instead of replaying every fork commit on every sync.

Do it at least weekly. When a merge conflicts in an upstream file, update the touch point
entry below if the seam moved.

## Upstream touch points

Every upstream file the fork changes, with the reason. Keep this list current; it is what
makes syncing tractable.

| File | Change | Why |
| --- | --- | --- |
| `README.md` | Two `> [!IMPORTANT]` lines at the top | Required by upstream's `.rules` (symlinked as `CLAUDE.md`) whenever an agent modifies source files: a reminder that a human must review the change. Only a human removes them. |

Planned (from the brief, not yet done):

- `crates/zed/src/zed.rs`: register the Paseo sidebar instead of `sidebar::Sidebar`
  (`multi_workspace.register_sidebar(...)`).
- `crates/terminal/src/terminal.rs`: a terminal kind backed by a Paseo terminal stream next to
  `TerminalType::{Pty, DisplayOnly}`.
