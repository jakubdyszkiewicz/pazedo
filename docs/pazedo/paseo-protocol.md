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
- Auth: none on a loopback daemon without a password (verified on the test daemon).
  `auth: {kind: "password", password}` or `{kind: "localCredential", token}` otherwise; the
  local credential is presumably `~/.paseo/local-credential` (unverified). How the real daemon
  on the Tailscale address authenticates a client is **not verified yet**.
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

### Scripted mock prompts

The `mock` provider (test daemon only) reacts to phrases in the prompt, which makes session UI
testable without real accounts: "emit a synthetic plan approval" (a `plan` permission request
with actions `implement`/allow and `dismiss`/deny; verified), "emit synthetic questions", "emit a
synthetic turn failure", "replay a claude-shaped foreground shell tool call", "emit N agent
stream updates", "emit N byte diff agent stream payload". Any other prompt streams a synthetic
turn with text, reasoning and tool calls for the model's duration (`ten-second-stream`,
`one-minute-stream`, ...). Read from the 0.15.4 bundle; only the plan approval was tried.

## Terminals (from sources; not verified live yet)

- `subscribe_terminals_request {cwd, workspaceId?}`: `cwd` is required in 0.15.4. The first
  `terminals_changed` answers it; later ones push the **full** list again.
- `subscribe_terminal_request {terminalId, restore: {mode: live|visible-snapshot|full-snapshot}}`
  returns a `slot` (0-255). Binary frames `[opcode, slot, payload]`: Output 0x01, Input 0x02,
  Resize 0x03 (JSON `{rows, cols}`), Snapshot 0x04 (JSON terminal state), Restore 0x05,
  ReplayBegin/Data/End 0x06-0x08. `terminal_stream_exit` ends a stream.
- JSON alternative for input: `terminal_input {terminalId, message: {type: input, data} |
  {type: resize, rows, cols}}`.

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
