# pazedo: a Zed fork that is a Paseo client

pazedo is a fork of [Zed](https://github.com/zed-industries/zed) that uses a
[Paseo](https://paseo.sh) daemon for workspaces, agent sessions and terminals, while Zed keeps
doing editing, files, language servers, git and search. The full brief (goal, research,
milestones) lives outside the repo in `~/dev/BRIEF.md` on the Mac mini.

Docs in this folder:

- [development.md](./development.md): building on the Mac mini, measurements, screenshots,
  the Paseo test daemon.
- [paseo-protocol.md](./paseo-protocol.md): the daemon protocol as `crates/paseo_client` uses
  it, with what was verified against a real daemon.
- [agent-sessions.md](./agent-sessions.md): how Paseo agents are shown with Zed's thread UI.

This folder is outside `docs/src/`, so Zed's mdBook and docs automation ignore it.

## Repository layout and remotes

- Local only for now: there is no GitHub fork yet. The clone's remote for Zed is named
  `upstream` (not `origin`) so that a fork can be added as `origin` later without renaming.
- Fork code goes into new crates and fork tooling into `script/pazedo/`. Crates so far:
  `crates/paseo_client` (daemon protocol client; no gpui dependency) and `crates/paseo_ui`
  (the Paseo workspace sidebar, the store behind it, and Paseo agent sessions shown with Zed's
  thread UI; see [agent-sessions.md](./agent-sessions.md)).
  Upstream files are changed only where there is no seam, and every such change is listed
  under [Upstream touch points](#upstream-touch-points).

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
| `Cargo.toml` | `crates/paseo_client`, `crates/paseo_ui` in `members` and `[workspace.dependencies]` | Every new crate. Conflicts here are trivial: keep both sides, sorted. |
| `crates/zed/Cargo.toml` | `paseo_ui` dependency | For the sidebar registration below. |
| `crates/zed/src/zed.rs` | In `initialize_workspace`, register `paseo_ui::PaseoSidebar` instead of `sidebar::Sidebar` and open it; drop the `use sidebar::Sidebar` import | The Paseo sidebar replaces the threads sidebar. The `sidebar` crate stays a dependency (`dump_workspace_info` is still used). |
| `README.md` | Two `> [!IMPORTANT]` lines at the top | Required by upstream's `.rules` (symlinked as `CLAUDE.md`) whenever an agent modifies source files: a reminder that a human must review the change. Only a human removes them. |

Planned (from the brief, not yet done):

- `crates/terminal/src/terminal.rs`: a terminal kind backed by a Paseo terminal stream next to
  `TerminalType::{Pty, DisplayOnly}`.
