# Command Code ("command-code") upstream wire spec — as implemented by opencodex

Two distinct provider ids exist. This spec covers the OAuth one (`command-code`, `/alpha/generate`, NDJSON).
The other, `commandcode`, is a plain OpenAI-chat API-key preset (`baseUrl https://api.commandcode.ai/provider/v1`,
`adapter: openai-chat`) — not covered here except where noted.

## 1. Endpoint

- `POST {baseUrl}/alpha/generate` — `src/adapters/command-code.ts:646`
  - Built as `` `${provider.baseUrl.replace(/\/$/, "")}/alpha/generate` `` (single trailing slash stripped).
- `baseUrl` comes from provider config; registry default `https://api.commandcode.ai`
  (`src/providers/registry/entries-core.ts:436-439`).
- No query string ever. No region selection: there is no region/zone concept for this provider.
- HTTP version: no pin. `provider.upstreamHttpVersion` (default `"auto"`) is the only knob
  (`src/lib/upstream-http-version.ts:1-56`); command-code never sets it, so the runtime default applies
  (Bun fetch negotiation). No `connection`/`keepalive` header is added except on a connection-reset retry,
  which sets `connection: close` (`src/lib/upstream-retry.ts:587-597`).
- fetch init: `redirect: "manual"`, timeout `ctx.timeoutMs ?? 200_000`, `AbortSignal.any([caller, timeout])`
  (`src/adapters/command-code.ts:507-520`).
- Send path: `createAdapterPhysicalSend(...)` → `fetchCommandCode(...)`; admission/pacing wrapper at
  `src/adapters/physical-send.ts:18-57`.

## 2. Request headers

Set explicitly, in this insertion order, as a plain `Record<string,string>`
(`src/adapters/command-code.ts:633-643`):

```
Authorization: Bearer <provider.apiKey>
Content-Type: application/json
User-Agent: cli
x-command-code-version: <provider.commandCodeVersion ?? "0.52.1">
x-cli-environment: production
x-taste-learning: false
x-co-flag: false
x-session-id: <commandCodeSessionId(parsed)>
x-project-slug: <projectSlug(cwd)>        // only when process.cwd() resolves
```

- `x-command-code-version` override field: `provider.commandCodeVersion` (`src/types/provider.ts:340`),
  editable from the dashboard (`src/server/auth-cors.ts:987`).
- Client (Codex/Chat/Messages) headers are **not forwarded at all**. The adapter builds the header
  record from scratch; there is no `forwardHeaders`/client-header merge for this adapter
  (verified: no `forwardHeaders` symbol in `src/adapters/base.ts`; `provider.headers` is merged by
  other adapters only — anthropic.ts:1136, google.ts:1028).
- Everything inbound is dropped: `accept`, `accept-encoding`, `originator`, `session_id`, `conversation_id`,
  `chatgpt-account-id`, `OpenAI-Beta`, `anthropic-version`, `x-api-key`, etc.

### `x-session-id` computation (`src/adapters/command-code.ts:228-247`)
1. Pick identity, first match wins: `parsed._clientThreadId` → `["thread", id]`; else
   `parsed._reasoningReplayScope.clientThreadId` → `["replay", id]`; else
   `parsed.options.promptCacheKey` **only if** `parsed._promptCacheKeyIsSharedCohort === false` → `["cache", key]`.
2. If no identity: return `randomUUID()` (fresh per request).
3. Else: `hex = sha256("command-code:" + kind + "\0" + id).hex`, then
   `hex[0:8]-hex[8:12]-4+hex[13:16]-8+hex[17:20]-hex[20:32]` (UUIDv4-ish shape, digest nibbles at the
   version/variant positions are skipped, not rewritten).

### `x-project-slug` (`src/adapters/command-code.ts:223-225`)
`cwd.replace(/[^a-zA-Z0-9]+/g,"-").replace(/^-|-$/g,"").toLowerCase().slice(0,64) || "workspace"`.
Omitted entirely when `process.cwd()` throws.

## 3. Request body

`JSON.stringify` of a literal object; key order is the source order below
(`src/adapters/command-code.ts:617-631`):

```jsonc
{
  "config": {                       // commandCodeConfig(cwd), :320-345
    "workingDir": "<abs cwd>",      // omitted when cwd unavailable
    "date": "YYYY-MM-DD",           // new Date().toISOString().slice(0,10)
    "environment": "<process.platform>",
    "structure": ["<entry>", ...],  // non-dot entries of cwd, max 64
    "isGitRepo": bool,
    "currentBranch": "",
    "mainBranch": "",
    "gitStatus": "",                // git status --porcelain, sliced to 2048
    "recentCommits": []             // git log --oneline -8, each sliced to 512
  },
  "memory": "",                     // projectContext (see below)
  "taste": null,
  "skills": null,
  "permissionMode": "standard",
  "mode": "agent",
  "params": {
    "model": "<canonical model id>",
    "messages": [ ...ModelMessage... ],
    "tools": [ ... ],
    "system": "<string>",
    "max_tokens": 64000,            // parsed.options.maxOutputTokens ?? provider.defaultMaxOutputTokens ?? 64000
    "stream": true,                 // always; the endpoint is NDJSON-only
    "temperature": 0.7,             // only if parsed.options.temperature !== undefined
    "reasoning_effort": "high"      // only if resolved
  }
}
```

Workspace/git metadata cached 30s, max 128 cwds (`:213-220`, `:258-318`); every git read has a 2s timeout
and fails to empty strings.

### Field mapping — messages (`wireMessages`, `:78-168`)
Wire is a ModelMessage array; roles are `user`, `assistant`, `tool`:

- assistant text → `{type:"text", text}`; thinking → `{type:"reasoning", text}`;
  tool call → `{type:"tool-call", toolCallId, toolName, input}` (input is the raw args object).
  Wire name = `namespace__name` when namespaced (`src/types/tools.ts:36-38`).
- toolResult → `{role:"tool", content:[{type:"tool-result", toolCallId, toolName, output:{type:"text"|"error-text", value}}]}`.
  `value` = text parts concatenated, images replaced by literal `[image]` (`toolResultText`, `:41-44`).
  `type` = `error-text` when `message.isError`, else `text`.
- Image parts inside a tool result **cannot** ride the tool-result; they are buffered and flushed as a
  following `{role:"user", content:[<image parts>]}` after all pending calls close (`:148-155`).
- Orphan toolResult (no matching pending call id) → converted to a **user** message:
  `"[tool result without adjacent tool call: <name> (<id>)]\n<text>"` (`:133-141`).
- Unanswered assistant tool calls are auto-closed before any non-tool message with
  `output:{type:"error-text", value:"[ocx] no tool result was recorded for this tool call; execution status unknown."}`
  (`:94-104`).
- user/developer: string content → `[{type:"text",text}]`; parts → text, image (`wireImagePart`),
  document → text, video → literal `"[video]"` (`:157-167`).
- Images: `{type:"image", image:<url-or-data-url>, mediaType?}` where mediaType comes from the data URL
  or the URL extension (`:56-61`).
- **Removed/rewritten:** every inbound provider extension is dropped — `cache_control`, message/item ids,
  `reasoning` signatures/redacted blocks, `previous_response_id`, `store`, `metadata`, `tool_choice`,
  `parallel_tool_calls`, `top_p`, `top_k`, `stop`, penalties, `service_tier`, `text.format`/response_format.
  Nothing inbound is echoed verbatim; the body is fully reconstructed.

### Tools (`wireTools`, `:198-204`)
```json
{ "name": "<namespace__name|name>", "description": "<desc>", "input_schema": <JSON schema> }
```
No `strict`, no `type:"function"` wrapper. `toolChoice:"none"` → `tools: []`; allowed/forced choices filter
the advertised set (`visibleTools`, `:170-196`).

### `system` (`:608-614`)
Join with `"\n\n"` of: `parsed.context.systemPrompt[]`, then an optional non-OpenAI tool-catalog nudge
(`buildNonOpenAIToolCatalogNudgeForTools`, `src/adapters/tool-catalog-nudge.ts:134`), then an optional
tool-choice instruction (`:181-196`), then `identifyRoutedModel(...)`.

Identity scrub (`src/adapters/identity.ts:76-82`): replaces Codex/GPT-5 identity sentences
(`You are Codex, a coding agent based on GPT-5…`) and neutral/routed identity sentences with a generated
line naming the routed model. This is the only "system prompt scrub".

### Model id (`canonicalCommandCodeModelId`, `:33-39`)
Compat aliases rewritten to live ids: `deepseek-v4-flash→deepseek/deepseek-v4-flash`,
`kimi-k3→moonshotai/Kimi-K3`, `glm-5.3→zai-org/GLM-5.3`, `glm-5.2→zai-org/GLM-5.2`. Anything else passes
through unchanged (case preserved).

### `reasoning_effort` (`supportedCommandCodeEffort`, `:562-592`)
- `undefined`/`"none"` → field omitted.
- Ladder from operator config if `modelReasoningEffortsAuthoritative === true`, else the shipped table
  `commandCodeReasoningEfforts(canonicalId, baseUrl)` (`src/providers/command-code-efforts.ts:28+`,
  ladder values `low|medium|high|xhigh|max`).
- `xhigh` → `max` if ladder lacks xhigh but has max; `ultra` → `max` only for `deepseek/deepseek-v4-flash`
  and `zai-org/glm-5.2`.
- Not in the ladder → field omitted (never sent as best-effort).
- On HTTP 400/422 whose body matches `/reasoning[_ -]?effort|unsupported effort|invalid effort/i`
  (`:492-494`), unless the operator authored the ladder (`:527-533`), the adapter refreshes the ladder
  from `https://commandcode.ai/models/<slug>` and replays **once** without `reasoning_effort`
  (`:652-687`).

### `projectContext` (opt-in)
`provider.projectContext === "on"` → `loadCommandCodeProjectContext(cwd)`, else
`{memory:"", taste:null, skills:null}` (`src/adapters/command-code-project-context.ts:11-15`, `:508-530`).
Caps: memory 32KB, taste 8KB, skills XML 32KB, per-skill file 8KB; scan TTL 30s; fail-soft to empty.
Skill roots: `.commandcode/skills`, `.agents/skills`, `.pi/skills`.

## 4. Response

- Body is **NDJSON**: one JSON object per `\n`-terminated line. Parser:
  `ndjson()` `:413-450`, `decodeEventLine()` `:472-486`, `stripEventFrame()` `:488-490`.
  - A leading `data:` prefix is defensively stripped (so a switched-to-SSE gateway still parses).
  - Unparseable lines, `null`, arrays, and non-objects are silently dropped (debug-logged), not fatal.
  - Final residual buffer is parsed even without trailing newline.
  - A stream with no `finish`/`finish-step` still yields a terminal `done` with `usage: undefined`.
- Event → AdapterEvent mapping (`parseStream`, `:689-787`):
  | wire `type` | effect |
  |---|---|
  | `text-start` / `text-end` / `tool-input-start` | ignored unless the model is MiMo (`xiaomi/mimo-*`), where they drive markup→tool-call restoration |
  | `text-delta` | `{type:"text_delta", text}` |
  | `reasoning-delta` | `{type:"thinking_delta", thinking: text}` |
  | `tool-call` | `tool_call_start{id: toolCallId \|\| randomUUID(), name: toolName \|\| "tool"}` → `tool_call_delta{arguments: JSON.stringify(input ?? args ?? {})}` → `tool_call_end`; ids default when absent |
  | `finish-step` \| `finish` | once only (`sawFinish`); usage from `totalUsage ?? usage`; stopReason from `rawFinishReason ?? finishReason` → `{type:"done", usage, stopReason}` |
  | `error` | `{type:"error", message, status:502}`; same message containing `tool result is missing`/`tool_result is missing` → `{status:502, errorType:"upstream_error", code:"missing_tool_result"}` |
  | anything else | no-op (MiMo boundary flush) |
  - `stopReason === "error"` on a finish event → `{type:"error", status:502, errorType:"upstream_error"}` carrying the parsed usage (failed turns still bill).
- Usage mapping (`usage()`, `:348-362`): input `inputTokens`, output `outputTokens`,
  `totalTokens = in+out`, cache read `inputTokenDetails.cacheReadTokens` →
  `cachedInputTokens` **and** `cacheReadInputTokens`; cache write `inputTokenDetails.cacheWriteTokens` →
  `cacheCreationInputTokens`. Missing detail fields are omitted, not zeroed.
- Back to client protocols: AdapterEvents are re-encoded generically —
  Responses SSE via `src/bridge/sse.ts` (usage → `input_tokens_details.cached_tokens` etc.,
  `src/bridge/internal.ts:36-80`), Chat Completions via `encodeChatCompletionSse`
  (`src/protocols/encoders/chat.ts:206`), Anthropic Messages via `encodeAnthropicMessageSse`
  (`src/protocols/encoders/messages.ts:402`), all through `encodeAdapterEventStream`
  (`src/protocols/encoders/adapter-events.ts:165`). Non-streaming clients get the same stream folded
  (`parseResponse`, `:788-793`).

## 5. Auth

- Credential type: a **long-lived `cmd_…`-style API key** used as the bearer. Access = refresh = apiKey,
  `expires = Number.MAX_SAFE_INTEGER` (`src/oauth/command-code.ts` login return + `refreshCommandCodeToken`).
- **No refresh flow.** `refreshCommandCodeToken(apiKey)` is a pass-through (`src/oauth/command-code.ts`, end of file);
  `defaultRefreshPolicy: "disabled"` (`src/oauth/index.ts:225-232`).
- Storage: OAuth account store; `accountId = whoami.user.id` (multi-account key), `source: "oauth"` or
  `"local-cli"` for imported keys.
- Multi-account hooks:
  - `explicitAccountReader("command-code") === true` → per-account quota probing
    (`src/providers/quota/account-cache.ts:182-185`), mode `"probe"`.
  - Quota probe bearer: `getTokenForAccountQuotaProbe("command-code", accountId)`
    (`src/providers/quota/vendor-probes-key.ts:1168-1177`).
  - Generic OAuth failover applies (authMode `oauth`, not in `EXCLUDED_PROVIDERS = {openai, anthropic}`)
    (`src/oauth/generic-account-failover.ts:169-171`).
- **No account-id header is sent upstream.** `chatgpt-account-id` style headers do not exist for this provider
  (grep: no `x-command-code-account`, no account header in the adapter).

## 6. Errors

Detection is status-based in the shared dispatch layer; the adapter itself does **no** status classification
except the reasoning-effort repair.

- **429** — `src/server/responses/adapter-continuation.ts:305-330` (same-target wait/retry, opt-in
  `retryOn429`) and `:1170-1195` (generic OAuth rotation). Command-code is `authMode:"oauth"`, so
  `rateLimitRetryPolicyFor` **returns null** for it — no same-key replay (`src/providers/key-failover.ts:566-604`;
  the canonical-URL patient policy at `:600-604` applies only to `authMode:"key"`, i.e. the `commandcode` preset).
- **401** — not in `isOAuth401ReplayProvider` (`src/server/responses/request-transport.ts:119-127`), so no
  credential-replay on 401; it falls through to the generic failover arm (400–599).
- **403 / 5xx** — same generic arm: `isRetriableFailoverStatus(400..599)` for non-antigravity
  (`src/oauth/generic-account-failover.ts:53-57`).
- Rotation requires ≥2 stored accounts, ≤3 failovers/request, ≤6 accounts/request
  (`:59-60`, `:489-492`).
- Cooldown (`:496-515`):
  - `Retry-After` parsed with `parseRetryAfterMs(..., {preserveImmediate:true, preserveServerDelay:true})`,
    one-day ceiling; Retry-After always wins.
  - else `exhaustedCooldownMs(...)` reset-aligned cooldown when quota evidence proves the allowance spent
    (`src/oauth/account-quota-rank.ts:227`) — used for command-code via its probe.
  - else `DEFAULT_COOLDOWN_MS = 60_000` (`:69`).
  - `"auth"` cooldown also 60s (`:327-328`).
- No command-code-specific suspension classes (unlike kiro `"suspended"` 24h / `"monthly_quota"`).
- Stream-level errors: `error` frame → client-visible 502 `upstream_error` (see §4).
- `finishReason:"error"` → 502 `upstream_error` with usage preserved.

Quota probe (used for account gating/cooldown evidence), all Bearer-scoped, `redirect:"error"`, 8s timeout
(`src/providers/quota/vendor-probes-key.ts:28-32`, `1075-1230`):
- `GET https://api.commandcode.ai/alpha/whoami` → `user.id`, `org.id`
- `GET /alpha/billing/credits[?orgId=…]` → `data.windowLimits.fiveHour|weekly` (`{cap, used, resetTime|resetAt|reset_time|reset_at}`), `data.credits.{monthlyCredits,purchasedCredits,freeCredits}`
- `GET /alpha/billing/subscriptions[?orgId=…]` → `currentPeriodStart`, `currentPeriodEnd`
- `GET /alpha/usage/summary[?orgId=…][&since=<periodStart>]` → `totalCost` ?? `totalMonthlyCredits`
- 4xx except 408/429 → `TERMINAL_QUOTA_FAILURE`; other non-2xx → soft null.

## 7. CLI login flow

`ocx login command-code` (`src/oauth/command-code.ts`):

1. **Local import first** (skipped when `forceLogin`/add-account: `importLocal:"off"`):
   read `~/.commandcode/auth.json`, take `apiKey`, validate with `GET https://api.commandcode.ai/alpha/whoami`
   (`Authorization: Bearer`, `Accept: application/json`, 10s timeout); `accountId` from `user.id`
   (fallback `userId` in the file). No whoami 200 → import abandoned.
2. **Browser authorization-code-with-key flow** (no device code):
   - `state` = 32 random bytes base64url.
   - Local callback server on `127.0.0.1:5959` (falls back to an ephemeral port on EADDRINUSE),
     plus best-effort `::1` bind; path `/callback`.
   - Open `https://commandcode.ai/studio/auth/cli?callback=<urlencoded http://127.0.0.1:<port>/callback>&state=<state>`.
   - The studio page POSTs JSON `{apiKey, state, userId, userName, keyName}` to the callback
     (CORS allow-origin `https://commandcode.ai`). `state` mismatch rejects.
   - 120s timeout.
3. **Manual paste fallback** (when the client supports manual code input): accepts a bare API key, a
   pasted callback JSON, or a URL/query carrying `code` (+ `state`); any state-bearing input must match;
   the pasted key is validated against `/alpha/whoami` and its `user.id`/`userName` adopted.

## unknowns

- Whether Bun's fetch negotiates HTTP/2 to `api.commandcode.ai` (no pin, no `allowH2`/dispatcher config anywhere) — unverified.
- Byte-level header order on the wire: the record is insertion-ordered, but header serialization order is the fetch implementation's.
- Exact `x-session-id` semantics expected upstream (affinity vs. cache cohort) — only opencodex's derivation is known.
- Whether the real CLI sends additional headers (e.g. Accept, x-client-*): opencodex sends only the eight/nine listed.
- The upstream `error` frame's full shape (only `error` string-or-`{message}` is read).
- Whether `finish` and `finish-step` always both appear; code handles either.
- `/provider/v1/models` discovery is authenticated with the same Bearer and read as a `data[]` envelope (default envelope key `data`), no custom headers — parsed generically, so the exact row schema upstream returns was not inspected here.
- The `commandcode` (API-key) preset's `apiKeyValidation: "unknown"` — no live validation probe confirmed.

## Unknowns
- Bun fetch HTTP/2 vs HTTP/1.1 negotiation to api.commandcode.ai — no pin, no allowH2/dispatcher configured anywhere in the repo
- Wire-level header serialization order (record is insertion-ordered; transport order is implementation-defined)
- Semantics upstream attaches to x-session-id (affinity vs prompt-cache cohort)
- Whether the real Command Code CLI sends headers opencodex omits (Accept, other x-client-*)
- Full shape of the NDJSON error frame beyond `error: string | {message}`
- Exact /provider/v1/models row schema upstream (only generic data[] envelope parsing was read)
- Whether finish and finish-step always both appear in the stream
- commandcode API-key preset has apiKeyValidation: "unknown" — no live key validation probe confirmed