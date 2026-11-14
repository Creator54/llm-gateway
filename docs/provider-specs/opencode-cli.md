# opencode-cli — egress spec (as implemented in ~/opencodex)

## 0. Bottom line
opencodex makes **zero HTTP requests** for this provider. The adapter's `buildRequest()` returns an
empty body and `parseStream()` yields a hard error; all transport is a **subprocess**:
`opencode run` (official OpenCode CLI), one process per turn, prompt on stdin, NDJSON on stdout.
Upstream LLM traffic to opencode.ai is performed *by the CLI itself*, not by the gateway, and the
source states a hand-rolled HTTP request is refused with 403 regardless of headers.
Therefore "byte-for-byte egress" = exact argv / env / stdin bytes / staged files / local MCP server.
- `adapters/opencode-cli/adapter.ts:277-283` — `buildRequest()` → `{url: provider.baseUrl, method:"POST", headers:{}, body:""}` (stub, never used for egress).
- `adapters/opencode-cli/adapter.ts:281-283` — `parseStream()` yields `{type:"error", message:"…runTurn; the fetch/parseStream path is disabled."}`.
- `adapters/opencode-cli/profiles.ts:12-15` — "OpenCodex never sends anything there itself"; plain HTTP to the free tier is refused even with correct headers.
- `adapters/registry.ts:158-167` — registry entry `opencode-cli`, `contractParent: "codebuddy"` (shares only lifecycle posture; NDJSON mapping is its own).

## 1. Endpoint / destination selection
- Canonical destination is a **constant**, not configurable: `canonicalBaseUrl: "https://opencode.ai"`,
  `region: "global"`, `binaryCandidates: ["opencode"]` — `profiles.ts:39-52`.
- Provider's configured `baseUrl` must equal the canonical URL after `strip trailing "/" + lowercase`;
  otherwise the turn fails closed **before** staging/spawn with `400 invalid_request_error`,
  `code:"non_canonical_destination"`, `retryable:false` — `adapter.ts:310-322`, matcher at `coding-agent/profile.ts:96-107`.
- No query params, no path, no project/region switch. There is no HTTP method/URL emitted by opencodex.
- Registry row (`providers/registry/entries-extended.ts:1573-1615`): `baseUrl:"https://opencode.ai"`,
  `authKind:"key"`, `keyOptional:true`, `liveModels:false`, `noVisionModels:[...]`,
  `defaultModel:"glm-5.3-flash-free"`, static roster (see §3).

## 2. Subprocess invocation (the actual wire)
### argv — exact, positional order `adapter.ts:120-130`
```
opencode run --pure -m opencode/<modelId> --agent ocx-model --format json --dir <stagingDir>
```
- `<modelId>` = `parsed.modelId` verbatim; always prefixed `opencode/`.
- `--pure` = no external plugins. `--format json` = NDJSON. `--dir` pins cwd to the private staging dir.
- Prompt deliberately **not** in argv (world-readable via ps); it goes on stdin.
- Agent id constant: `OPENCODE_AGENT_ID = "ocx-model"` (`adapter.ts:41`).
- Binary path: `resolveCodingAgentBinary()` scans `PATH` for `opencode` (plus `.cmd/.exe/.bat/""` on win32),
  positive hits cached per `${providerId}:${candidate}` — `coding-agent/profile.ts:59-88`. Missing → `500 cli_not_found`, retryable false (`adapter.ts:326-338`).
- Windows `.cmd`/`.bat` shim: wrapped as `cmd.exe /d /s /c "<escaped line>"` with `windowsVerbatimArguments:true` — `lib/win-exec.ts:79-96`. POSIX: direct exec, no shell.

### spawn options `adapter.ts:381-386`
`stdio:["pipe","pipe","pipe"]`, `env` (see below), `windowsHide:true`. `cwd` is NOT set — `--dir` carries it.

### env — allow-list only `coding-agent/turn.ts:62-83`
Only these keys are copied from the parent (non-empty values); everything else is dropped, including all
vendor/region/auth vars and any proxy variable:
```
PATH HOME USERPROFILE LANG LC_ALL LC_CTYPE TMPDIR TEMP TMP SHELL SYSTEMROOT APPDATA
LOCALAPPDATA PROGRAMFILES PROGRAMFILES(X86) COMSPEC PATHEXT SYSTEMDRIVE USERNAME TZ
```
- No credential env var (`tokenEnv` absent for this profile: `coding-agent/profile.ts:34-39`).
- The provider's `apiKey`, if any, is **never injected**; it is used only to redact diagnostics (`adapter.ts:391`, `430`, `580+`).

### stdin — exact bytes `adapter.ts:86-113` + `514-521`
UTF-8, then `stdin.end()`. Plain text, sections joined by `"\n\n"`:
1. If system prompt exists: `<system_instructions>\n{system}\n</system_instructions>`
2. Per message, in order: if assistant tool-call parts exist → `Assistant tool calls: ` + each part `JSON.stringify`ed, one per line.
3. Then `User: {text}` / `Assistant: {text}` / `Tool result: {text}` (role `developer` contributes only to step 1; anything else is skipped).
4. Always terminated by literal `Assistant:` as the last section.
- `buildSystemPrompt()` = `parsed.context.systemPrompt` lines + all `developer` messages joined `"\n\n"` — `coding-agent/protocol.ts:695-708`.
- Text extraction: strings as-is; arrays → only `type:"text"` parts, joined `"\n"` (`adapter.ts:106-113`). Images never reach here (refused earlier).
- Truncation: `historyCharLimit = clamp(ctxWindowTokens*3, 200_000, 4_000_000)` where ctxWindow is
  `provider.modelContextWindows[modelId] ?? provider.contextWindow`; if prompt longer, keep the first
  `historyCharLimit` chars and append `"\n[earlier conversation trimmed]"` — `adapter.ts:516-521`, `protocol.ts:20-50`.
  Note: truncation is a head-cut of the **whole** prompt (system block included).

### staged files (inside `mkdtemp($TMPDIR/ocx-opencode-cli-)`, mode 0600, flag `wx`) `adapter.ts:350-372`
- `{dir}/.opencode/agent/ocx-model.md` — always written. Frontmatter (`adapter.ts:56-77`):
  `description`, `mode: primary`, `temperature: 0.7`, `permission: {"*": deny, "opencodex_*": allow, read: {"*": deny, "/nonexistent/ocx-opencode": allow}, edit/bash/webfetch/task/todowrite/question/skill/lsp/external_directory: deny}`
  plus body instructing "Stateless bridge… only opencodex MCP tools… never use built-in tools."
- `{dir}/opencode.json` — **only if** the inbound request carried a usable tool catalog:
  `{"mcp":{"opencodex":{"type":"remote","url":"http://127.0.0.1:<port>/<uuid>","enabled":true}}}`
- Whole staging dir is `rm -rf`'d in `finally` (`adapter.ts:552`) — zero state survives a turn.

### lifecycle / timeouts `adapter.ts:36-38, 445-500, 559-575`
- Turn timeout **300000 ms** → SIGTERM, then SIGKILL after **2000 ms** grace; emits `504 timeout`, `retryable:true`.
- win32: `killWindowsProcessTree(pid)` if injected, else SIGTERM/SIGKILL ladder.
- Reap: race `close` against `killGraceMs*2+250 ms`; a second `kill()` fires at `killGraceMs`.
- stderr buffered to **8192 bytes** max, redacted (`[redacted]` for authorization lines, `sk-…`), surfaced in error messages.
- Abort signal → kill + `stdout.destroy()`, error `${label} turn was aborted.`, retryable false.
- Concurrency: registry calls it "per-turn spawn, scoped env, kill ladder" (`registry.ts:161-166`); no in-adapter semaphore, no connection reuse. Per-request pacing/queueing is the router's (`server/responses/run-turn-execution.ts:199,235`).
- Read limits: JSON line ≤ 8 MiB, total ≤ 64 MiB (`protocol.ts:20,22`); malformed/non-object line → `CodingAgentProtocolError` → `502 protocol_error`.

## 3. Request body (inbound → prompt) — field mapping
There is **no JSON body**. Mapping is `parsed` (already-normalized OcxParsedRequest) → text prompt:
- `context.systemPrompt` + `developer` messages → `<system_instructions>` block. No scrubbing beyond dropping non-text parts.
- `context.messages[].content` text parts → role-labelled lines. `toolCall` parts on assistant messages → `Assistant tool calls: <JSON>` lines.
- `role:"toolResult"` → `Tool result: <text>`; tool call/result ids are **not** transmitted except inside the JSON `toolCall` dump.
- Dropped entirely: `max_tokens`/`maxTokens`, `temperature`, `top_p`, `stop`, `response_format`, `tool_choice` (only used to compute `requireToolCall`), `thinking`/reasoning budgets, `metadata`, `stream`, cache_control, any ids other than tool-call JSON, and every image part (request is rejected instead).
- Inbound `tools[]` are **not** sent as prompt text; they are advertised over a local MCP server (§2 staged `opencode.json`, §4).

## 4. Client tools — local MCP capture bridge (only HTTP opencodex serves here)
`adapters/opencode-cli/tool-bridge.ts:1-41` + `adapters/codebuddy/tool-bridge.ts:595-597,478-483`
- Server: `Bun.serve` on `127.0.0.1:0` (ephemeral port), random path `/crypto.randomUUID()`, `maxRequestBodySize: 1 MiB`,
  `WebStandardStreamableHTTPServerTransport({sessionIdGenerator: undefined, enableJsonResponse:true})`, server name `opencodex-capture` v1.0.0.
- Rejects (403) any request whose path ≠ the UUID path **or** which carries an `Origin` header.
- Tool names exposed as `mcp__opencodex__<alias>`; alias ≤40 chars + 16 hex hash if needed; caps: 128 tools, 256 KiB/tool, 2 MiB catalog, schema depth 32 / 4096 nodes, 8 KiB pattern (`codebuddy/tool-bridge.ts:12-46`).
- First `CallTool` is captured (single-shot, `called` latch) → emits `tool_call_start{id:"call_<uuid>",name}`, `tool_call_delta{arguments: JSON}`, `tool_call_end`, `done{stopReason:"tool_use",endTurn:false}`, then kills the child and destroys stdout. Handler reply text: "Call captured. Execution belongs to the external client."
- `requireToolCall` = inbound `tool_choice=="required"` or `{name:…}` or `{mode:"required"}` (`codebuddy/tool-bridge.ts:478-483`). If set and the turn ends with no capture → `502 opencode_required_tool_missing`.
- A **native** OpenCode tool call (`part.type` ∈ `tool`/`tool_use`, or `step_finish.reason` ∈ `tool-calls`/`tool_calls`) fails the turn closed: `502 opencode_native_tool_call`, retryable false (`adapter.ts:216-260, 505-517`).

## 5. Response parsing (stdout NDJSON) → AdapterEvent
`readJsonLines` (`protocol.ts:79-108`): UTF-8 stream decode, trim, skip blank lines, `JSON.parse` per line, must be an object.

Frame handling `mapOpenCodeFrame` (`adapter.ts:216-260`) — `part = frame.part ?? frame`:
| frame | mapping |
|---|---|
| `part.type == "text"` | cumulative snapshot diff → `text_delta{text: part.text.slice(lastLen)}`; only grows |
| `part.type == "reasoning" \| "thinking"` | cumulative diff → `thinking_delta` |
| `part.type == "tool" \| "tool_use"`, or `frame.type=="tool_use"` | fail closed (native tool) |
| `frame.type=="error"` | `mapOpenCodeError` (see §7) |
| `frame.type=="step_finish"` | `reason` from `part.reason ?? frame.reason ?? "stop"`; `tool-calls` → fail closed; else `FINISH_REASONS` |
| `step_start`, `message`, anything else | ignored (forward compatible) |
- `FINISH_REASONS` (`adapter.ts:172-176`): `stop`→`done/stop`, `length`→`incomplete`, `abort`→`incomplete`; unknown → `incomplete`.
- **Usage** (`mapUsage`, `adapter.ts:185-196`) from `part.tokens ?? frame.tokens`:
  `inputTokens = tokens.input`, `outputTokens = tokens.output`, `totalTokens = input+output`,
  and if `tokens.cache.read > 0` → `cachedInputTokens` **and** `cacheReadInputTokens` = that value.
  `tokens.total` and `tokens.cache.write` are **ignored**; no cache-write field is emitted.
- Terminal dedupe: once `done`/`error`/`incomplete` emitted, later terminals are dropped (`emitOnce`, `adapter.ts:419-427`).
- Client-protocol mapping (Responses / Chat Completions / Messages) happens **outside** the adapter from the
  AdapterEvent stream: `text_delta`, `thinking_delta`, `tool_call_start|delta|end`, `done{stopReason,endTurn,usage}`, `incomplete`, `error{message,status,errorType,code,retryable}`.

## 6. Auth
- **None.** Credentialless/keyless by design: the CLI owns the session, the free tier is anonymous.
  `profiles.ts:7-15` ("OpenCodex stores no token, reads none, and injects none"; no `tokenEnv`).
- Registry: `authKind:"key"` + `keyOptional:true` (`entries-extended.ts:1581-1583`) — exempt from key enforcement; a saved key never reaches the CLI.
- No refresh flow, no token endpoint, no multi-account pool, no account-id header, no OAuth snapshot for this provider (`adapter.ts:325` comment: no credential pre-flight on purpose).

## 7. Errors — detection, classification, retry
All statuses are **synthesized by opencodex**; nothing is parsed from HTTP headers.
- `mapOpenCodeError` (`adapter.ts:144-170`): reads `frame.error` (or the frame itself), `message`, `name`,
  `data.statusCode`; `statusCode` may be on `error` or `error.data`.
  - Free-tier gate: `name === "FreeTierError"` or message matches `/free tier can only be used from within/i`
    → `403`, `code:"opencode_free_tier_gate"`, `errorType:"upstream_error"`, **retryable:false**, with a fixed
    explanatory message. Documented wire form of the gate: `403 {"type":"FreeTierError","status":403}` (`profiles.ts:16-17`).
  - Otherwise: `{message, status (if present), errorType:"upstream_error"}`; `retryable` absent (defaults false).
- Synthetic failures (`adapter.ts:291-620`):
  | condition | status/code | retryable |
  |---|---|---|
  | image input present | 400 `unsupported_input_modality` | false |
  | non-canonical baseUrl | 400 `non_canonical_destination` | false |
  | tool catalog invalid | 400 `invalid_tool_catalog` | false |
  | `opencode` not on PATH | 500 `cli_not_found` | false |
  | staging dir/file failure | 500 `staging_setup_failed` | false |
  | spawn threw | 500 `cli_spawn_failed` | false |
  | child emitted `error` event | 500 `cli_spawn_failed` | false |
  | timeout (300 s) | 504 `timeout` | **true** |
  | malformed stream / no stdout | 502 `protocol_error` | false |
  | non-zero exit | 502 `process_exit_error` (stderr appended) | false |
  | exit 0 but no terminal `step_finish` | 502 `protocol_error` | false |
  | native tool attempt | 502 `opencode_native_tool_call` | false |
  | required tool missing | 502 `opencode_required_tool_missing` | false |
- **No** `Retry-After` parsing, **no** reset-time parsing, **no** cooldown/quota arithmetic, **no** provider
  suspension state anywhere in this adapter. Any cooldown/failover is the router's generic reaction to
  `error.retryable` + `status` (`server/responses/run-turn-execution.ts`; health/quota in `routing/health.ts`,
  `routing/quota.ts`). Note the free-tier gate is explicitly non-retryable → no cooldown.

## 8. CLI login flow (belongs to the CLI, not to opencodex)
- opencodex implements **none** of it: no paste-URL redirect, no device code, no API-key endpoint for this row.
  It only (a) checks the binary exists and (b) tells the operator how to install it:
  `curl -fsSL https://opencode.ai/install | bash` (or `npm i -g opencode-ai`) — `profiles.ts:45`, `entries-extended.ts:1613`.
- The free tier authenticates on the **CLI's own** anonymous session; any real sign-in is the operator's
  `opencode` CLI session outside this proxy. (The only `opencode auth login` mention in the tree is for the
  unrelated Windsurf/Devin import path, `oauth/devin/types.ts:41-44`.)
- No endpoints are contacted by opencodex for login.

## 9. Minimal Rust reproduction checklist
1. Refuse provider `opencode-cli` unless `base_url` (trailing-slash/case-insensitive) == `https://opencode.ai`.
2. Refuse image parts: 400 `unsupported_input_modality`.
3. Resolve `opencode` on PATH (cache positive hits).
4. `mkdtemp` staging dir; write `.opencode/agent/ocx-model.md` (exact frontmatter above, mode 0600/exclusive).
5. If inbound tools present: start loopback capture MCP server → write `opencode.json` with its URL.
6. Spawn `opencode run --pure -m opencode/<model> --agent ocx-model --format json --dir <dir>` with the 22-key env allow-list, pipes, UI-hidden.
7. Write the prompt sections verbatim (system block, `Assistant tool calls:` lines, `User:`/`Assistant:`/`Tool result:` lines, trailing `Assistant:`), truncate as specified.
8. Parse NDJSON, diff cumulative `text`/`reasoning` snapshots, map `step_finish`/usage, fail closed on native tools.
9. Kill ladder 300 s / 2 s; redact stderr; 8192-byte stderr cap; `rm -rf` staging dir.

## Unknowns
- The CLI's actual upstream HTTP request (URL path under https://opencode.ai, method, headers such as x-opencode-client/user-agent, HTTP version, body shape) is inside the opencode binary, not in ~/opencodex/src — not confirmable from source. profiles.ts only asserts the destination host and that hand-rolled HTTP is refused 403.
- Exact NDJSON frame field names the CLI emits beyond what the mapper reads (part.id, part.type, part.text, part.reason, part.tokens.{input,output,total,count?}, frame.type, frame.error.{message,name,statusCode,data.statusCode}); frame examples are referenced by tests/docs but not fully enumerated here.
- Whether `tokens.total` from the CLI is ever trusted — the adapter recomputes input+output and ignores it.
- Whether any router-level cooldown/suspension is applied to opencode-cli after a 502/500 — the adapter sets retryable:false everywhere except timeout, but the generic router path (routing/health.ts, routing/quota.ts) was not read in depth.
- Whether the CLI needs any network/proxy env var (HTTPS_PROXY etc.); baseScopedEnv drops all of them, so a proxied deployment may behave differently — not addressed in source.
- Whether `--dir` also changes model/agent config resolution such that a user-level ~/.config/opencode config could still apply; adapter comment claims no user .opencode state is inherited but this is enforced only by `--dir` + `--pure`.
- Cost/quota accounting for this row (usage ledger keys) was not traced; usage is passed to the generic onUsage bridge in server/responses/run-turn-execution.ts.