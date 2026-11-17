# Pazedo

**Paseo 🤝 Zed**

Pazedo is a fork of the [Zed](https://zed.dev) editor that works as a client for
[Paseo](https://paseo.sh). Paseo runs your coding agents (Claude Code, Codex, OpenCode, …) and
their terminals in a daemon, on your machine or on a remote box, and lets you follow them from
its desktop, phone and CLI apps. Pazedo adds Zed to that list. Paseo's workspaces, agents and
terminals show up in Zed, while Zed still does what it is good at: editing, language servers,
git, search and remote projects over SSH.

Agents belong to the daemon, not to the editor. An agent started from your phone gets a tab in
Pazedo, an agent started in Pazedo keeps running when you quit it, and closing its tab archives
it, as in Paseo's own apps.

## Versions

| | |
| --- | --- |
| Paseo | 0.15.4 (protocol types follow `@getpaseo/protocol` 0.15.4; other versions may work but are untested) |
| Zed | based on Zed `main` as of 2026-11-15 (`596dfdcf32`, 1.30.0-dev), merged regularly |
| Platform | macOS on Apple silicon (the only one built and tested so far). Remote projects can be on Linux x86_64 or macOS arm64. |

## What Pazedo adds to Zed

### Paseo workspaces in the sidebar

- **All your Paseo workspaces** in Zed's sidebar, grouped by project or by status, as in
  Paseo's app (Needs input, Failed, Ready to review, Working, Done). Press cmd-g to switch.
- **Live status for each workspace:** its agents' state, branch, and its pull request with CI
  checks and review state.
- **Create workspaces** from a project's checkout or as a new git worktree. Rename and archive
  them.
- **Add projects** from a folder or by cloning from GitHub, and remove them.
- **Several daemons at once,** for example your laptop and a dev box. The header says "All
  online" or "2/3 online", and the same repository on several daemons is listed as one
  project.
- **Daemons on other machines** open as Zed remote projects over SSH. Pazedo uploads its own
  remote server.
- **Password-protected daemons:** the password is kept in the macOS Keychain. A daemon on the
  same machine needs no password.
- **Keyboard first:** arrow keys, type-to-find, cmd-n for a new workspace, cmd-shift-0 to reveal
  the open workspace, cmd-shift-minus and cmd-shift-= to collapse and expand all.

### Agents in Zed's own thread UI

- **The Paseo panel** has a tab for each agent and terminal of the open workspace. It replaces
  Zed's agent panel.
- **Zed's thread view** shows the agents: streaming markdown, reasoning, tool calls, diffs,
  plans, permission prompts, queued messages and the stop button.
- **New agents** start on any provider the daemon offers. You can switch the model, thinking
  level and mode from the message box.
- **Zed's context ring** shows the agent's context window and session cost.
- **Pasting images** into a message sends them to the agent. Images the agent produces are
  shown in the thread, fetched through the daemon when it runs on another machine.
- **Finished shell commands** collapse to one line that expands on click. With
  `"agent": { "expand_terminal_card": false }`, running commands collapse too.
- **Tab menus** on agent and terminal tabs have Rename and Copy ID. Each tab shows its
  provider's icon (Claude, Codex, OpenCode, …).

### Paseo terminals

- **The daemon's shells** shown in Zed's terminal view: Zed does the selection, search, links
  and themes, while the shell runs in the daemon and keeps running when you quit Pazedo.
  Closing the tab ends it, as in Paseo's apps.

### Smaller additions

- **AI subscription usage in the title bar:** Claude, Codex and others, with their limits and
  reset times, read from Paseo.
- **Type-to-find in the Project Panel:** start typing to jump to a file.
- **Opening files in the Project Panel:** a click selects a file, and a second click opens it
  (`project_panel.open_files_on_click`).
- **No Sign In button:** it is hidden by default (`title_bar.show_sign_in`).

Pazedo keeps its settings and data apart from an installed Zed, in `~/.config/pazedo` and
`~/Library/Application Support/Pazedo`, so both can be installed side by side.

## Getting started

You need a running Paseo daemon; see [paseo.sh](https://paseo.sh). There are no prebuilt
downloads yet, so build the app from source. That needs Rust and Xcode, as for
[building Zed on macOS](./docs/src/development/macos.md):

```sh
script/pazedo/bundle --install   # release build into ~/Applications/Pazedo.app (about 30 minutes)
```

Then point Pazedo at your daemon in `~/.config/pazedo/settings.json`:

```json
"paseo": { "endpoint": "127.0.0.1:6767" }
```

For a daemon on another machine, add the SSH host where its folders are. You can also list
several daemons:

```json
"paseo": {
  "daemons": [
    { "endpoint": "127.0.0.1:6767" },
    { "name": "dev box", "endpoint": "dev-box:6767", "ssh": "user@dev-box" }
  ]
}
```

Daemons can also be added and edited from the gear button in the sidebar header.

### Updating

Pazedo doesn't update itself: it builds Zed's dev channel, which has no update server, and the
app never checks for new versions. To update, pull this repository and build again:

```sh
git pull
script/pazedo/bundle --install
```

## Documentation

- [docs/pazedo/development.md](./docs/pazedo/development.md): building, settings and every
  feature in detail, with how each one was verified.
- [docs/pazedo/README.md](./docs/pazedo/README.md): how the fork is organized, syncing with
  upstream Zed, and every place where Zed's own code was changed.
- [docs/pazedo/paseo-protocol.md](./docs/pazedo/paseo-protocol.md): the parts of Paseo's daemon
  protocol that Pazedo uses.
- [docs/pazedo/agent-sessions.md](./docs/pazedo/agent-sessions.md) and
  [docs/pazedo/terminals.md](./docs/pazedo/terminals.md): how agents and terminals are shown.
- [docs/pazedo/todo.md](./docs/pazedo/todo.md): what is still missing.

## How the fork is built

Most of Pazedo is in its own crates: `paseo_client` (the daemon protocol, without UI) and
`paseo_ui` (the sidebar, panel, agent and terminal tabs, usage indicator). Zed's own code is
changed in only a few places, each listed in [docs/pazedo/README.md](./docs/pazedo/README.md),
so that upstream Zed can be merged in often.

## License

Like Zed, Pazedo is licensed under GPL-3.0-or-later, with Apache-2.0 components where marked
(see [LICENSE-GPL](./LICENSE-GPL) and [LICENSE-APACHE](./LICENSE-APACHE)).

Pazedo is an independent project. It is not affiliated with or endorsed by Zed Industries or
the Paseo project.
