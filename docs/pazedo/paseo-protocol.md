# Paseo daemon protocol notes

What `crates/paseo_client` relies on, written against Paseo **0.15.4** (2026-11-14). Sources:
the Zod schemas in npm `@getpaseo/protocol@0.15.4` (`dist/messages.js`), `@getpaseo/client`
(`dist/daemon-client.js`), and the getpaseo/paseo repository at `42c7b7f`. "Verified" means
observed against the isolated test daemon (see [development.md](./development.md)) with
`paseo_debug` or the live tests; everything else is read from the sources.

## Why the Rust types are hand-written

The brief suggested generating Rust types from the Zod schemas (export JSON Schema, then
`typify`). That needs executing the npm package's code, which was not done. More importantly,
the protocol has about 220 inbound and 240 outbound message types, nearly all irrelevant to
pazedo, and it changes often. `crates/paseo_client/src/protocol.rs` models only what pazedo
uses, leniently: unknown fields are ignored, enums have an `Unknown` variant, unmodelled or
undecodable pushes arrive as `ServerMessage::Other` with the raw JSON, and `request_raw` sends
any request. On a Paseo upgrade: diff `dist/messages.js` between versions for the modelled
messages, bump `PASEO_PROTOCOL_PACKAGE_VERSION`, and run the live tests.

## Connection

- URL `ws://host:port/ws` (TCP) or the same path over a Unix socket. `daemon.listen` accepts
  `host:port`, `/path/to/socket` and `unix:///path/to/socket`; default `127.0.0.1:6767`.
- Envelopes. Client to daemon: `{"type":"hello",...}`, `{"type":"ping"}`,
  `{"type":"session","message":{...}}`. Daemon to client: `{"type":"pong"}`,
  `{"type":"hello.rejected","reason":...}`, `{"type":"session","message":{"type":...,"payload":{...}}}`.
- `hello`: `{clientId, clientType, protocolVersion: 1, auth?, appVersion?, capabilities}`.
  Paseo's own desktop, web and mobile apps send `clientType: "mobile"`; so does pazedo. The
  daemon closes a socket that sends no hello within 15 s, and rejects `protocolVersion < 1`.
- The connection is up when `{"type":"status","payload":{"status":"server_info",...}}` arrives:
  `serverId`, `hostname`, `version`, `permissions`, and ~100 boolean `features` (verified).
- Auth: a daemon without a configured password admits every client as owner, without
  credentials, whatever address it listens on (`resolveSessionAdmission` in the 0.15.4 bundle;
  verified 2026-11-14 against the owner's real daemon on its Tailscale address, which has no
  password). With a password, `hello.auth` must be `{kind: "password", password}` or
  `{kind: "localCredential", token}`; the Paseo CLI reads the token from
  `<paseo home>/local-credential` and the password from the host URI or `PASEO_PASSWORD`.
  A refused hello gets `{"type":"hello.rejected","reason":"password_required" |
  "incorrect_password" | "incompatible_protocol"}` and the socket closes. pazedo sends a saved
  password or, to a daemon on its own machine, the local credential (development.md, "Daemon
  password"); verified 2026-11-15 against the test daemon with a password.
- Liveness: send `{"type":"ping"}` every 10 s; any inbound frame proves liveness. The daemon
  closes sockets that stop pinging for 45 s after their first ping. Paseo reconnects after
  `min(1.5 s × 2^attempt, 30 s)` and resets the attempt counter on `server_info`. pazedo uses the
  same values and treats 30 s without any inbound frame as a dead connection.

## Requests and responses

- A request is a session message with a client-chosen `requestId`; the response is a session
  message whose `payload.requestId` echoes it. Response types are listed per request in
  `protocol.rs` (`Request::RESPONSE_TYPE`); they are not always `<name>_response`, for example
  `subscribe_terminals_request` is answered by `terminals_changed`.
- Failures come back as `rpc_error {requestId, requestType?, error, code?}`. An unknown request
  type gives code `unknown_schema` and the connection stays open (verified).
- `agent_permission_resolved` also has a `requestId`, but it names the permission request.
  pazedo's request ids are prefixed `pazedo-`, so they never collide.
- Messages larger than the frame budget are split when both sides support `binary_chunking_v1`:
  binary frames `[0x31, 1, kind (0 text, 1 binary), flags (1 start, 2 end), payload...]`.
  Reassembly is unit-tested; not yet observed from the daemon.

## Subscriptions

With the `owned_subscriptions` capability the daemon pushes nothing unless asked:

- `fetch_workspaces_request` and `fetch_agents_request` with `subscribe: {}` return a snapshot
  plus a `subscriptionId`; later `workspace_update` and `agent_update` pushes carry that id.
  Never send a `subscriptionId`: the daemon assigns it and rejects client-chosen ones.
- Discrete events need `session.events.set_subscription.request {events: [...]}`:
  `project.update`, `agent_attention_required`, `agent_permission_request`,
  `agent_permission_resolved`, `terminal_attention_required`, `status.server_info`, ... (the
  allowed names are `SessionEventSubscriptionSchema`; an unknown name rejects the request).
- `subscription.release.request {subscriptionId}` ends one.
- Subscriptions belong to one socket. After a reconnect, request them again (new ids). With
  `features.directorySync`, sending the previous `sync: {generation, afterSeq}` makes the
  response contain only changes (not implemented in pazedo yet).

Verified live (`paseo_debug watch` while `script/pazedo/paseo-test run` started a mock agent):

1. `workspace_update` upsert for the new workspace (`status: done`).
2. `agent_update` upserts as the agent goes `initializing` → `idle` → `running`, each a full
   agent snapshot; `workspace_update` upserts move the workspace to `running`.
3. `agent_attention_required {reason: "finished", shouldNotify: false}` on the event
   subscription, then an `agent_update` with `status: idle, requiresAttention: true,
   attentionReason: finished`, and the workspace moves to `attention`.

Workspace `status` is a bucket the daemon derives from its agents: pending permission →
`needs_input`, unread error → `failed`, running → `running`, `requiresAttention` →
`attention`, otherwise `done`.

Read and unread (verified 2026-11-15, test daemon): `attention` is Paseo's unread and `done`
its read. Paseo's app clears an agent's attention when the agent's screen gains focus while
the app is visible, unless the reason is `permission` (that lasts until answered), with
`clear_agent_attention {agentId: string | string[], requestId?}` →
`clear_agent_attention_response {requestId, agentId, agents}` (the updated snapshots). The
workspace turns `done` once none of its agents needs attention. pazedo clears an agent when its
tab renders in the active window (`PaseoAgentItem::render`), and opening a workspace from the
sidebar shows the agent that most recently finished unseen. Not used yet:
`workspace.clear_attention.request {workspaceId}` (Paseo's "Mark as read") and
`workspace.mark_unread.request {workspaceId}`.

## Agent timeline

- `fetch_agent_timeline_request {agentId, direction: tail|before|after, cursor?: {epoch, seq},
  limit, projection: projected, wholeTurns}` returns merged history entries
  `{item, seqStart, seqEnd, timestamp, turnId}`, plus `epoch`, cursors, `hasOlder`/`hasNewer`,
  `reset`, `gap`. Verified: a mock agent's tail returned 23 entries of types `user_message`,
  `assistant_message`, `reasoning` and `tool_call`.
- `agent.timeline.set_subscription.request {agentIds}` starts live `agent_stream` pushes
  `{agentId, event, timestamp, seq?, epoch?}`. Verified: after `paseo-test send`, one
  `turn_started`, 176 token-level `timeline` events with consecutive `seq` (177 to 352), and
  `turn_completed`. Turn events carry no `seq`.
- Paseo's app subscribes, fetches the tail, applies live rows by `seq`, and fetches `after` the
  last cursor on a gap or after reconnect; a new `epoch`, `reset`, or
  `agent.timeline.replacement` means rebuilding from the tail.

### Timeline items and stream events

History entries and live `timeline` events carry an `AgentTimelineItemPayload`:
`user_message`, `assistant_message`, `reasoning`, `tool_call` (`callId`, `name`, `status`
running/completed/failed/canceled, and a `detail` of type shell, read, edit (with
`unifiedDiff`), write, search, fetch, sub_agent, plain_text, plan or unknown), `todo`, `error`,
`notification`, `compaction` and `plugin`. Live `assistant_message` events are deltas: each
holds the next chunk of text (verified; history entries are merged). Other stream events:
`thread_started`, `turn_started`, `turn_completed`, `turn_failed`, `turn_canceled`,
`permission_requested`, `permission_resolved`, `attention_required`. Typed in `protocol.rs` as
`TimelineItem`, `ToolCallDetail` and `AgentStreamEvent`, with an `Other` fallback.

### Creating agents and answering permissions (verified 2026-11-14)

- `agent.create.request {config: {provider, cwd, model?, modeId?, title?}, workspaceId?,
  initialPrompt?}` returns `agent.create.response {agent, error}`.
- A permission request raised before a client subscribed to the timeline is not replayed as a
  stream event; it is in the agent snapshot's `pendingPermissions`. A session view must read
  pending permissions from the snapshot when it opens.
- `agent_permission_response {agentId, requestId, response: {behavior: allow|deny,
  selectedActionId?}}` has no response message; `requestId` is the permission request's own id.
  The daemon then pushes `permission_resolved {requestId, resolution}` on the timeline and the
  turn continues (`turn_completed` for the mock plan approval).
- While a permission is pending the agent's `status` stays `running` with
  `requiresAttention: false`; the pending permission itself is the signal.

### Ordering and identity details that matter to a client (verified 2026-11-14)

- Request ids must be unique beyond one process. The daemon deduplicates `agent.create.request`
  by client id and request id; a client whose ids restart at zero after a restart gets
  `agent_request_key_conflict`. `paseo_client` prefixes ids with a random per-client value.
- A message sent with `send_agent_message_request {messageId}` comes back on the timeline as a
  `user_message` with `messageId` and `clientMessageId` equal to that id, both in history and
  in the live event. The live event can arrive before the send's response.
- `turn_completed` can arrive before `permission_resolved` for the same turn.

### Providers, models and modes (verified 2026-11-14)

- `get_providers_snapshot_request {cwd?}` returns `entries: [{provider, status
  (ready|loading|error|unavailable), enabled, label, models: [{id, label, isDefault,
  isSelectable?, ...}], modes: [{id, label}], defaultModeId}]`. Providers report `loading`
  until they have probed their models. A client that advertises `all_providers` also gets the
  dev-only `mock` provider on a dev daemon; one that does not (like a bare probe) does not.
- An agent snapshot carries `model`, `currentModeId` and `availableModes`.
- `set_agent_model_request {agentId, modelId}` and `set_agent_mode_request {agentId, modeId}`
  answer with `{agentId, accepted, error}`.
- `fetch_agent_timeline_request.limit` counts timeline rows, not projected entries: a limit of
  200 returned about 20 merged entries for a mock agent. Paseo's app pages 40 rows at a time.

### Messages between agents (verified 2026-11-14, real daemon)

When an agent starts another agent (`paseo run` from an agent's shell, or an orchestration tool), the new agent's
prompt is not a `user_message`: it is a `tool_call` named `agent_message`, `callId`
`paseo-agent-message:<uuid>`, with `agentMessage {event: message, sender: {id, serverId?,
title?}, text}` and a `plain_text` detail holding the same text. Other `event`s (`finished`,
`errored`, `permission-required`, `closed`) are news a parent receives about an agent it
started. pazedo shows `message` as a user message headed "From agent “<sender title>”" and the
others as cards titled after the event. Note also that `paseo run` called from inside an agent
defaults to that agent's workspace; pass `--cwd` and `--new-workspace local` to start elsewhere.

### Scripted mock prompts

The `mock` provider (test daemon only) reacts to phrases in the prompt, which makes session UI
testable without real accounts: "emit a synthetic plan approval" (a `plan` permission request
with actions `implement`/allow and `dismiss`/deny; verified), "emit synthetic questions", "emit a
synthetic turn failure", "replay a claude-shaped foreground shell tool call", "emit N agent
stream updates", "emit N byte diff agent stream payload". Any other prompt streams a synthetic
turn with text, reasoning and tool calls for the model's duration (`ten-second-stream`,
`one-minute-stream`, ...). Read from the 0.15.4 bundle; only the plan approval was tried.

## Terminals (verified 2026-11-14 unless noted)

- Terminals belong to a workspace. `create_terminal_request {cwd, workspaceId?, name?, size?}`
  returns `{terminal, error}`; without `workspaceId` the daemon picks a workspace of that
  directory. `subscribe_terminals_request {cwd, workspaceId}` lists only that workspace's
  terminals and answers with `terminals_changed` (with a `subscriptionId`); later
  `terminals_changed` pushes carry the **full** list again. `cwd` is required in 0.15.4.
- `subscribe_terminal_request {terminalId, restore: {mode: live|visible-snapshot|full-snapshot,
  scrollbackLines?, size?}}` returns `{terminalId, slot, subscriptionId, error}`. Slots are
  per connection and were not reused between attaches (0, then 1).
- Binary frames `[opcode, slot, payload]`: Output 0x01 (raw PTY bytes), Input 0x02 (client to
  daemon, raw bytes), Resize 0x03 (JSON `{rows, cols, intent?: claim|update}`), Snapshot 0x04
  (JSON grid state; not observed), Restore 0x05, ReplayBegin/Data/End 0x06-0x08 (only for
  clients advertising `terminal_chunked_replay`; ReplayBegin carries `{rows, cols,
  retainedHistoryLines}`). A client without that capability gets one Restore frame right
  after the subscribe response: ANSI from xterm's serializer that rebuilds screen and
  scrollback (observed: the prompt, earlier commands and their output, then cursor position).
- Size ownership: `intent: "claim"` makes the sender the size owner, `"update"` is applied
  only from the owner; a missing intent counts as `claim` (compat). Read from the 0.15.4
  bundle; that our resize reached the PTY was verified through `$COLUMNS`.
- JSON alternative for input: `terminal_input {terminalId, message: {type: input, data} |
  {type: resize, rows, cols, intent?}}`.
- `unsubscribe_terminal_request {terminalId}` detaches. `terminal_stream_exit {subscriptionId?,
  terminalId, error?}` ends a stream: without `error` the process exited (verified with
  `exit`); with `error` the daemon lost track of it and the process may still run.

## Projects, workspaces and closed agents (verified 2026-11-14, test daemon)

- `project.add.request {cwd}` adds a folder as a project (git or not) without a workspace:
  `project.add.response {project, error, errorCode?}`. pazedo's sidebar **+** uses it.
- `workspace.create.request {source, title?, ...}` always creates a new workspace (unlike
  `open_project_request`, which looks one up by directory). `source` is `{kind: directory,
  path, projectId?}` for the project's checkout, or `{kind: worktree, cwd, projectId?, action:
  branch-off|checkout, refName?, branchName?, ...}` for a new git worktree. With `branch-off`
  and no `branchName` the daemon picks a name (e.g. `black-piranha`) for both the worktree
  directory (`<paseo home>/worktrees/<hash>/<name>`) and its branch, off `refName`. Answered by
  `workspace.create.response {workspace, setupTerminalId, error, creation, ...}`. An optional
  `agent` / `firstAgentContext` starts an agent too (Paseo's app does; pazedo does not yet).
- `archive_workspace_request {workspaceId}` → `archive_workspace_response {workspaceId,
  archivedAt, error}` (the client also reads `canceled`, set when an undo from another client
  stopped it). Archives the workspace's agents, kills its terminals and, for a Paseo worktree no
  other workspace uses, deletes the worktree directory. The daemon sets `archivingAt` on the
  workspace while it runs, then removes it from the list. pazedo asks for confirmation (naming
  the worktree it deletes) and hides the row at once (verified 2026-11-15, test daemon).
- A workspace's `githubRuntime.pullRequest` carries, besides number, title, URL, state, merged
  and draft: `checksStatus` (none|pending|success|failure), `checks` [{name, status:
  success|failure|pending|skipped|cancelled, url, workflow, duration}], `reviewDecision`
  (approved|changes_requested|pending|null), `mergeable` (MERGEABLE|CONFLICTING|UNKNOWN),
  `baseRefName`/`headRefName`, `repoOwner`/`repoName`, and `forgeSpecific` (merge state, allowed
  merge methods, auto-merge and merge queue). pazedo shows the number and CI state in the row
  and the rest on hover.
- `branch_suggestions_request {cwd, query?, limit?}` lists branches, most recently committed
  first: `{branches, branchDetails?, error}`.
- `directory_suggestions_request {query, cwd?, includeFiles?, includeDirectories?, matchMode?:
  fuzzy|suffix, limit? (1-100)}` lists paths on the daemon's machine: `{directories, entries:
  [{path, kind}], error}`. An absolute prefix (`/root/dev/`, `~` expanded) lists what is under
  it; a bare name matches fuzzily under the home directory; an empty query returns nothing
  (verified 2026-11-15, dev box). pazedo's Add Project uses it for a daemon on another machine.
- `workspace.title.set.request {workspaceId, title}` renames a workspace; `null` or `""`
  reverts to the derived name: `workspace.title.set.response {requestId, workspaceId,
  accepted, title, error}`. Afterwards the workspace's `name` is the title and `title` mirrors
  it (verified 2026-11-15). pazedo renames inline from the sidebar (the row's pencil or its
  right-click menu); like Paseo's app, an empty name changes nothing.
- Agents whose provider session ended (status `closed`, e.g. after a daemon restart) are
  resumed by the daemon when a client opens them (they turned `idle` as soon as pazedo
  subscribed to their timeline). `refresh_agent_request {agentId}` resumes one explicitly; it
  is answered by a `status` message `{status: agent_refreshed, agentId, timelineSize}`. Paseo's
  app offers it as "Reload agent". Mock agents that never received a prompt have no history
  either way.

## Images in agent messages (verified 2026-11-16)

Providers' images (e.g. a tool result's screenshot) arrive as an `assistant_message` of their
own: `\n\n![Image](file:///<tmp>/paseo-attachments-<random>/<sha256>.<ext>)\n\n`, a file the
daemon wrote on its machine (path segments percent-encoded, `)` escaped). The Paseo app reads it
with `file_explorer_request {cwd, path, mode: "file"}`; `path` is relative to `cwd`, so the
request names the file's folder and file name. Without `acceptBinary` the response carries
`file: {kind: "image", encoding: "base64", content, mimeType, size}`. Verified against the real
daemon with a 347 KB PNG. The mock provider emits such a message for the prompt "emit settled
assistant image markdown: ![alt](url)".

## Usage (verified 2026-11-16, both daemons, Paseo 0.15.4)

`usage.list_reports.request {requestId, forceRefresh?, reportIds?, agentId?}` (feature
`usageSources`) is answered by one `usage.list_reports.update {requestId, report}` per account,
then `usage.list_reports.response {requestId, error}`. A report entry has `id`, `sourceId`
(`claude`, `codex`, `opencode-go`, `copilot`, …), `sourceLabel`, `account.label` (the email),
`fetchedAt`, `icon` (SVG markup, not a name), `report` and, while it fails, `lastSuccessful`.
`report.status` is `available` (`planLabel`, `windows`, `balances`, `details`), `unavailable`
(`problem.kind`: `expired`, `rejected`, `no_quota`) or `error`. A window has `label` ("Session"),
`shortLabel` ("5h"; empty shows the percent alone), `summary` (shown in the compact summary),
`usedPct`, `remainingPct`, `resetsAt` and `tone` (`default`, `ok`, `warning`, `danger`). The daemon
caches each report for five minutes; the Paseo app reads every five minutes while visible and
shows the percent left. The older `provider.usage.list.request` (feature `providerUsageList`,
marked for removal after 2027-03-26) answers the same in one response. The daemon config's
`usage.enabled` turns the feature off. Both daemons report Claude (Max 5x) and Codex (plus); the
Mac mini adds OpenCode Go and the dev box GitHub Copilot, neither with summary windows. The
test daemon on the Mac mini reports the same accounts as the real one.

`Client::request_with_updates` sends a `StreamingRequest` and collects its updates: an update
with the request's `requestId` and the request's `UPDATE_TYPE` goes to the request rather than
resolving it, so messages pushed in between still arrive as events.

## Observations and open questions

- Each `paseo run` from the CLI created a **new** workspace for the same directory
  (`wks_...` per run, all `local_checkout` of `~/dev/pazedo-sandbox`). The sidebar must expect
  several workspaces per directory.
- A first `fetch_agents_request` right after creating mock agents returned no entries; later
  ones listed them (by then `closed` with attention `finished`). Not explained yet.
- Capabilities pazedo advertises: `hello_rejection`, `owned_subscriptions`,
  `explicit_event_subscriptions`, `project_updates`, `all_providers`, `binary_chunking_v1`,
  `selective_agent_timeline`. Paseo's app sends more (timeline formats, terminal snapshot
  reflow, subagents, ...). Add them only when pazedo handles the format they enable.
