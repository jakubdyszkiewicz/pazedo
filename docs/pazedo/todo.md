# pazedo to-do

Open work, roughly in priority order. Move an item to the relevant doc (or delete it) when it is
done; record decisions where the code or docs that implement them live.

## Next

- **Authentication** for daemons with a password: send `hello.auth`, reading the local
  credential from the Paseo home or a password from the keychain, never from settings.json.
  The owner's daemon has no password and needs none (verified 2026-11-14). The endpoint is a
  setting now (`"paseo": {"endpoint": ...}`); several daemons in one sidebar would extend it.
- **Daemon on a remote dev box, source over SSH.** Not tested. The sidebar, agents and
  terminals should work against a remote daemon (they only stream over Paseo), but opening a
  workspace calls `find_or_create_local_workspace` with the daemon's path, which exists only on
  the dev box. Zed's `MultiWorkspace::find_or_create_workspace` takes a `RemoteConnectionOptions`
  host: give each daemon an optional SSH host in the connection settings and open its
  workspaces as Zed remote projects (Zed's remote server then runs files, language servers, git
  and search on the box). Check that the daemon's paths match the SSH view, and that file links
  in agent output and diffs resolve in the remote project.
- **Practice upstream merge** while the fork is only days behind, to test the touch-point list
  and the merge procedure in README.md.

## Daily-use gaps

- New workspaces start without an agent; Paseo's app starts one right away with a first
  prompt (`workspace.create.request` takes `agent` / `firstAgentContext`).
- The new-workspace picker only branches off existing branches; no "check out a branch" or
  pull request, and no custom branch name.
- Adding a project uses the native folder dialog only; no GitHub clone
  (`project.github.clone.request`) or "create a folder" (`project.create_directory.request`).
- Archive workspaces and remove projects from the sidebar.

- `Pazedo.app` keeps settings and data in `~/Library/Application Support/Pazedo`, but on macOS
  Zed's log (`~/Library/Logs/Zed/Zed.log`), cache (`~/Library/Caches/Zed`) and state
  (`~/.local/state/Zed`) ignore the custom data directory, so the app and dev builds share them;
  their logs interleave in one file. Fix in `crates/paths` if it gets in the way.

- Opening an agent whose workspace is the pazedo repository itself switches the window to that
  repository; trust it only if rust-analyzer may build there.

- The sidebar lists every workspace, including ones without agents; consider hiding or
  archiving idle workspaces.
- Several daemons (hosts) in one sidebar; the brief groups by host.

## Paseo panel

- One pane only: no splits inside the panel yet (Zed's terminal panel has them).
- The panel's dock position is not persisted; moving it to the right lasts until restart.
- The panel's tab bar scrolls when a workspace has many agents; there is no overview of them.
- Rename terminals from the panel.

## Terminals (see terminals.md)

- Not verified: two clients resizing the same terminal; a reconnect with terminal tabs open.
- The same terminal open in two windows: closing one tab detaches the other
  (`unsubscribe_terminal_request` names the terminal, not the slot).
- Split and restore-on-restart are unsupported (both would start a local shell).

## Later (from the brief)

- Remote workspaces without SSH: source code through the Paseo daemon instead of SSH. Would
  need a new Zed remote transport (today SSH, WSL, Docker) carrying Zed's remote-server
  protocol over Paseo; unknown whether Paseo can run a process with a clean binary pipe rather
  than a PTY. Do the SSH variant above first.
- Show Paseo plugin views (such as a task board) inside pazedo.
