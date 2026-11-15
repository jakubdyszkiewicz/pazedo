# pazedo to-do

Open work, roughly in priority order. Move an item to the relevant doc (or delete it) when it is
done; record decisions where the code or docs that implement them live.

## Next

- **Remote dev box, follow-ups.** Daemons on other machines work, several side by side, and
  Pazedo.app uploads its own remote server (Linux x86_64, macOS arm64) to SSH hosts (2026-11-15,
  see development.md "Remote server for SSH projects", "Daemon on another machine" and "Several
  daemons"). Still open: reaching a daemon that listens only on localhost through an SSH
  tunnel; checking that file links in agent output and diffs open in the remote project.

## Daily-use gaps

- The new-workspace picker only branches off existing branches; no "check out a branch" or
  pull request, and no custom branch name.
- Adding a project uses the native folder dialog only; no GitHub clone
  (`project.github.clone.request`) or "create a folder" (`project.create_directory.request`).
- Remove projects from the sidebar.

- Opening an agent whose workspace is the pazedo repository itself switches the window to that
  repository; trust it only if rust-analyzer may build there.

- The sidebar lists every workspace, including ones without agents; consider hiding or
  archiving idle workspaces.

## Paseo panel

- One pane only: no splits inside the panel yet (Zed's terminal panel has them).
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

## Decided against

- Starting a new workspace's first agent with a prompt from the new-workspace flow, as Paseo's
  app does (`workspace.create.request` takes `agent` / `firstAgentContext`): the owner decided
  against it on 2026-11-15.
