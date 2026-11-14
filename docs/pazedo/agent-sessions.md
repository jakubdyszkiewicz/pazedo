# Paseo agent sessions in Zed

Clicking an agent in the Paseo sidebar opens its session in a center-pane tab rendered by
Zed's own thread UI (`agent_ui::ConversationView`): streaming markdown, reasoning, tool call
cards, plans, permission prompts, the message editor, queueing and the stop button. The
**+** button on a workspace row starts a new agent there. Paseo owns the agents; Zed only
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
    workspace's directory (provider from `PAZEDO_AGENT_PROVIDER`, default `claude`, model from
    `PAZEDO_AGENT_MODEL`; a picker comes later).
  - Releasing the view only releases the timeline subscription; agents keep running.
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

## Known gaps

- Only the last 200 timeline entries are loaded; no "load older" yet.
- Edit diffs are shown as text, not as Zed's diff view (Paseo sends a unified diff; ACP's diff
  content expects file changes plus a git patch).
- Questions (`kind: question` permission requests) are shown as allow/deny.
- No provider/model/mode picker, no images or file attachments in prompts (mentions are sent as
  plain text).
- A timeline gap is only logged; it should reload the session.
- `agent_ui`'s `ThreadMetadataStore` records every `ConversationView`, so Paseo sessions also
  land in Zed's own thread history database. Not filtered yet (would need a small upstream
  edit).
