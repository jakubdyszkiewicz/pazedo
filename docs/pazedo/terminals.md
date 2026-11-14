# Paseo terminals in Zed

A Paseo terminal is a shell the daemon owns, the same one Paseo's desktop and phone apps show.
pazedo renders it with Zed's own terminal view (`terminal_view::TerminalView`): the grid,
selection, copy, search, links, scrolling and themes are Zed's; the process runs in the daemon
and keeps running when the tab closes.

- The sidebar lists a workspace's terminals under its agents once the workspace has been
  opened in the window (terminal lists are subscribed per workspace, see below). Clicking one
  attaches to it in a center-pane tab.
- The **+** menu on a workspace row starts with **New Terminal**: the daemon starts a shell in
  the workspace's directory and the tab opens.
- When the shell exits (`exit`, or killed from another client), the tab closes, like a local
  Zed terminal. Closing the tab only detaches.

## How it fits together

- `crates/terminal` gained one seam (an upstream touch point): `Terminal::forward_input` on a
  display-only terminal sends everything the terminal would write to a PTY (keystrokes,
  pastes, mouse reports, answers to terminal queries) and every grid resize to a channel, as
  `ForwardedInput::{Bytes, Resize}`. Output still goes in through `write_raw_output` and
  `replace_display_output`, which existed already. No new terminal kind was needed.
- `paseo_ui::PaseoTerminalItem` owns a display-only `Terminal` and a `TerminalView` inside a
  workspace `Item`. It is not serialized: restoring a Zed terminal tab would start a local
  shell. It does not split for the same reason (`TerminalView::clone_on_split` spawns a local
  terminal).
- `paseo_client::Client::subscribe_terminal` attaches to a terminal's stream and returns the
  daemon's response plus a receiver of that terminal's binary frames. The driver installs the
  slot route while it handles the response, so the restore frame the daemon sends right behind
  the response cannot be lost to a race. The receiver closes when the connection is lost.
- The item attaches once the view has laid out, asking for `restore: full-snapshot` at the
  tab's size. Attaching earlier wrote the restore into Zed's default-sized grid; the first
  layout then cut the alternate screen (vim, htop) instead of reflowing it, so a reopened vim
  showed a few `~` lines and no text. The daemon answers
  with one `Restore` frame (0x05) of ANSI produced by xterm's serializer, which rebuilds screen
  and scrollback; the item feeds it to `replace_display_output`. `Output` frames (0x01) go to
  `write_raw_output`. pazedo does not advertise `terminal_chunked_replay`, so the daemon never
  sends the chunked replay frames (0x06-0x08).
- Input goes out as binary `Input` frames (0x02) on the slot, resizes as `Resize` frames
  (0x03) with `{rows, cols, intent: "claim"}`. Paseo shares one size between all clients: the
  last client to claim decides. pazedo claims on attach and whenever the tab's grid changes size.
- After a reconnect the store emits `Connected` and every open tab attaches again (new slot,
  new restore). `terminal_stream_exit` without an error closes the tab; with an error (the
  daemon lost track of the process, which may still run) the tab prints a notice.
- `PaseoStore::watch_terminals` subscribes with `subscribe_terminals_request {cwd,
  workspaceId}`. The sidebar watches the workspace that is open in the window; watched
  workspaces stay watched for the session and are subscribed again after reconnects.

## Verified (2026-11-14, test daemon, Paseo 0.15.4)

With the off-screen runner (`PAZEDO_SCREENSHOT_TERMINAL`, see
[development.md](./development.md)):

- A new terminal opens with the shell prompt; typing `echo pazedo-$((6*7))` and Enter into
  Zed's terminal view prints `pazedo-42`. `paseo terminal capture` shows the same screen.
- Reattaching to that terminal restores the earlier commands and output; colours render;
  `$COLUMNS` in the shell matches the tab's width (the resize reached the daemon's PTY).
- Typing `exit` closes the tab and the daemon drops the terminal from its list.

In the real app (`script/pazedo/run-dev`, driven with `script/pazedo/ui`):

- The open workspace lists its terminals; clicking one attaches; typed commands run, and lines
  wrap at the column count the shell reports.
- vim started in the tab, tab closed, app restarted, terminal reopened: vim's screen comes back
  exactly (after the fix above), keeps taking input, redraws at the new width when the pane is
  zoomed, and after `:wq` the shell reports the new `$COLUMNS`.

Not verified yet: two clients fighting over the size, a reconnect with tabs open, and the same terminal open in two windows (the second tab's unsubscribe on close
would detach the first, since `unsubscribe_terminal_request` names the terminal, not a slot).
