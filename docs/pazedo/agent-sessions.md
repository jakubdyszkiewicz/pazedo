# Paseo agent sessions in Zed

Clicking an agent in the Paseo sidebar opens its session in a center-pane tab rendered by
Zed's own thread UI (`agent_ui::ConversationView`): streaming markdown, reasoning, tool call
cards (edits as real diffs), plans, permission prompts, the message editor, queueing and the
stop button. The composer's dropdowns change the agent's model, thinking level and mode. The **+** button on
a workspace row opens a menu of the daemon's providers and their models and starts a new agent
with the one picked. Paseo owns the agents; Zed only
observes and steers them, so agents started from the phone, the CLI or other agents show up
the same way.

Decided 2026-11-14 after comparing this with a native timeline view: reusing Zed's thread UI
needed no upstream edits, while a native view would have been thousands of lines to reach the
same features.

## How it fits together

- `paseo_ui::PaseoAgentItem` wraps a `ConversationView` in a workspace `Item`. It is built with
  public constructors only: its own `AgentConnectionStore`, `Agent::Custom { id: "paseo" }`, and
  `PaseoAgentServer`, an in-process `AgentServer` whose `connect` returns the shared
  `PaseoConnection`.
- `PaseoConnection` implements `acp_thread::AgentConnection`:
  - `load_session` (agent id = ACP session id) subscribes to the agent's timeline, fetches the
    last 200 projected entries, replays them into a new `AcpThread`, sets running/idle from the
    agent snapshot and raises pending permissions from it (a permission requested before the
    subscription is not replayed as an event).
  - It uses the **receipt** submission transport: `send_agent_message_request` with a fresh
    `messageId`; the response only means "accepted". Turn state is reported separately with
    `AcpThread::update_session_state` from `turn_started` / `turn_completed` / `turn_failed` /
    `turn_canceled`, which is what makes turns started elsewhere look the same as local ones.
  - `cancel` sends `cancel_agent_request`. `new_session` sends `agent.create.request` for the
    workspace's directory with the provider and model chosen in the sidebar's + menu, handed
    over through `PaseoConnection::set_next_new_agent` because `ConversationView` creates the
    session itself (fallback: `PAZEDO_AGENT_PROVIDER`, default `claude`, and
    `PAZEDO_AGENT_MODEL`).
  - `session_config_options` exposes the agent's model (choices from the daemon's provider
    snapshot), thinking level (the current model's `thinkingOptions`, ACP category
    `thought_level`) and mode (from the agent snapshot) as ACP config options, so Zed shows its
    own dropdowns; changes go out as `set_agent_model_request` / `set_agent_thinking_request` /
    `set_agent_mode_request`. Changing the model recomputes the thinking levels. The provider is
    fixed after creation and not offered.
  - History: the latest 2000 timeline *rows* (every streamed chunk is a row, so this is a
    handful of turns to a few hundred); when there is more, the tab shows "Load Full History",
    which reopens the session with all rows. `PAZEDO_AGENT_HISTORY_LIMIT` overrides the row
    count.
  - Releasing the view only releases the timeline subscription; agents keep running.
- Edits and writes become ACP diff content: one file change with the absolute path (relative
  paths are joined with the agent's directory) and the hunks as a git patch. Zed matches the
  file headers of a git patch literally against the reported path, so the headers are dropped
  (a header-less patch is accepted for a single file). A provider's diff is only used when its
  hunk line counts add up; otherwise the diff is generated from the edit's old and new text, and
  only if neither works is it shown as text. Zed shows edit cards collapsed; the chevron opens
  the diff.
- Timeline items become ACP v2 values built from their wire JSON (the schema types are
  non-exhaustive builders; the wire format is the stable contract): user messages keyed by
  Paseo's `messageId`, assistant and reasoning deltas appended to one message until something
  else happens, tool calls as `ToolCallUpdate`s with a title, kind, status and a text body
  (command output, diffs as ```diff blocks), todos as a plan.
- Permission prompts map Paseo actions to ACP options; the chosen option goes back as
  `agent_permission_response`. A permission answered elsewhere withdraws the prompt.
- Losing the daemon connection or a replaced timeline puts the view into Zed's "connection
  lost" state; its reconnect button loads the agent again.

## Things that were not obvious

- **Echo ordering.** In receipt mode the thread recognises the echo of a submission only once
  it has applied the acceptance, but Paseo puts the user message on the timeline before it
  answers the send. The connection holds the agent's live events while a send is in flight and
  releases them when the thread acknowledges the acceptance (`ReceiptSubmission::ordered`).
  Without this the message showed up twice, once as a queued message.
- **One new entry per update cycle.** `ConversationView` handles `NewEntry` by syncing index
  `entries().len() - 1` when the cycle ends. Adding several entries in one cycle made it index
  past its own list and panic, so held events are applied one per cycle.
- **`turn_completed` before `permission_resolved`.** Resolving a permission only means
  "running again" while a turn is active; otherwise the thread kept showing a stop button.
- **Request ids** must be unique across processes (see paseo-protocol.md).

## Verified (2026-11-14, test daemon, off-screen runner)

- An existing mock agent opened in a tab with its history (markdown, an edit card, a command
  card) and Zed's message editor.
- A message typed into the editor and sent with Enter appeared once; the mock plan approval it
  triggered showed as Zed's permission card (Implement / Dismiss / Cancel). Clicking Implement
  answered Paseo: the agent went idle with no pending permission, the card disappeared, the
  thread returned to idle, and the sidebar's warning became a finished dot.
- A new agent created from Zed appeared in the sidebar, and its first message streamed live
  into the tab, which took the agent's title.

Also verified in the running app (2026-11-14, `script/pazedo/run-dev`, clicks and typing through
`script/pazedo/ui`): opening an agent from the sidebar, sending a message with Return, answering
the plan approval by clicking Implement, and a turn started with `paseo-test send` from the CLI
streaming into the already-open tab. When checking a screenshot, remember the view follows the
bottom of a running turn: a long mock turn scrolls earlier user messages out of view, and the
mock's own text runs one cycle into the next without a line break. The runner's
`PAZEDO_SCREENSHOT_DUMP_ENTRIES=1` prints the thread's entries next to the view's, which
settles whether something is missing or just scrolled away.

Also verified on 2026-11-14 in the running app: the + menu lists the test daemon's providers
(Claude, Codex, OpenCode and the dev-only Mock Load Test, which the daemon includes because
pazedo advertises `all_providers`) with their models; picking a mock model opened a New Agent
tab whose composer showed that model and created the agent with it; an edit card expanded to a
syntax-highlighted diff of the right file; "Load Full History" (with the limit set to 5 rows)
grew the thread from 20 to 82 entries.

Thinking level, verified 2026-11-14 off screen against the test daemon: the composer showed
Load Test · Low · Ten second stream; picking High in the dropdown set the agent's thinking to
`high` on the daemon (`paseo-test ls`).

Against the owner's real daemon with real Claude (2026-11-14, dev build via `run-dev` with
`PAZEDO_PASEO_ENDPOINT=100.64.0.1:6767`): an agent started by another agent showed its
prompt as "From agent “…”" (after adding that mapping); a Haiku agent asked to create a file
raised a Write permission with the new content as a diff, Allow in pazedo let the turn finish
and the file appeared in the project panel; the + menu listed Claude, Codex and OpenCode
models and created a Claude Haiku 5.5 agent whose composer showed mode, thinking level and
model; a six-hour Opus session with hundreds of tool calls opened and scrolled without
problems.

## Known gaps

- Questions (`kind: question` permission requests) are shown as allow/deny.
- No images or file attachments in prompts (mentions are sent as plain text).
- "Load Full History" reloads everything at once; there is no incremental paging.
- A timeline gap is only logged; it should reload the session.
- Diffs generated from an edit's old and new text show snippet line numbers (`@@ -1 +1 @@`),
  not the file's.
