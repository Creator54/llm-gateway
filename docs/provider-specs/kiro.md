# Kiro provider — upstream wire spec (as implemented by opencodex)

## 1. Endpoint

- Method `POST`, URL `https://runtime.{region}.kiro.dev/`. Path `/`, no query, no fragment (`src/adapters/kiro/wire.ts:68-77`).
- Region resolution (`src/oauth/kiro.ts:457-478`): account `apiRegion` → region inferred from profile ARN (segment 4) → account `ssoRegion` → `DEFAULT_REGION = "us-east-1"` (`kiro.ts:33`). Accountless legacy: `KIRO_API_REGION` → local kiro-cli import (`apiRegion` → ARN → `ssoRegion`) → `KIRO_REGION` → `us-east-1`. `normalizeKiroRegion` validates (`src/oauth/kiro-credentials.ts:115`).
- Base-URL override: `provider.baseUrl`. If host matches `^runtime\.[a-z]{2}(?:-[a-z]+)+-\d\.kiro\.dev$` **and** pathname is `/`, host is rebuilt with resolved region; else the configured URL is used verbatim including any path (`wire.ts:68-77`).
- Project identity is NOT in the URL: it is the body `profileArn` field plus `x-amzn-kiro-profile-arn` header.
- Timeout `ctx.timeoutMs ?? 200_000` (`src/adapters/kiro-retry.ts:320`).
- HTTP version: no explicit ALPN/HTTP2 config; plain `fetch` (HTTP/1.1, undici). Adapter only injects a pacing-aware executor (`kiro-retry.ts:170-175`).
- Gateway fallback: on connect failure or 502/503/504 the canonical runtime host is retried at `q.{region}.amazonaws.com`, same path, no query (`kiro-retry.ts:153-161, 295-308`).

## 2. Request headers

Constructed from scratch as a plain object — insertion order below is transmitted order. Auth always `Bearer <apiKey>`. **No inbound client headers are forwarded** (no `anthropic-version`, no `anthropic-beta`, no cookies). No manual `content-length`; `accept-encoding` left to the HTTP client default.

`wireClient === "cli"` — used when API key, Builder-ID fallback, or no profile ARN (`src/adapters/kiro/adapter.ts:117-129`):

| # | header | value |
|---|---|---|
| 1 | `authorization` | `Bearer ${token}` |
| 2 | `content-type` | `application/x-amz-json-1.0` |
| 3 | `accept` | `*/*` |
| 4 | `x-amz-target` | `AmazonCodeWhispererStreamingService.GenerateAssistantResponse` |
| 5 | `user-agent` | `aws-sdk-rust/1.3.15 ua/2.1 api/codewhispererstreaming/0.1.17975 os/{os} lang/rust/1.92.0 md/appVersion-2.14.2 m/F app/AmazonQ-For-CLI` |
| 6 | `x-amz-user-agent` | same, **without** `md/appVersion-2.14.2` |
| 7 | `x-amzn-codewhisperer-optout` | `true` |
| 8 | `amz-sdk-request` | `attempt=1; max=3` |
| 9 | `amz-sdk-invocation-id` | `invocationId()` |
| 10 | `tokentype` | `API_KEY` — only when token starts with `ksk_` |

`wireClient === "ide"` — enterprise profile ARN present (`adapter.ts:130-140`):

| # | header | value |
|---|---|---|
| 1 | `authorization` | `Bearer ${token}` |
| 2 | `content-type` | `application/x-amz-json-1.0` |
| 3 | `accept` | `application/vnd.amazon.eventstream` |
| 4 | `x-amz-target` | same target string |
| 5 | `user-agent` | `aws-sdk-js/1.0.27 ua/2.1 os/{osTag()} lang/js md/nodejs#22.21.1 api/codewhispererstreaming#1.0.27 m/E KiroIDE-1.0.0-{fp}` |
| 6 | `x-amz-user-agent` | `aws-sdk-js/1.0.27 KiroIDE-1.0.0-{fp}` |
| 7 | `x-amzn-codewhisperer-optout` | `true` |
| 8 | `x-amzn-kiro-agent-mode` | `vibe` |
| 9 | `amz-sdk-invocation-id` | UUID |

- `fp` = hex sha256 of `hostname + username + "-kiro"`, cached, sliced to 64 chars; `osTag()` = `macos#<ver>` / `win32#<ver>` / `linux#<ver>` (`src/adapters/kiro-wire.ts`). CLI `os/` value ∈ `linux|macos|windows` (`wire.ts:12-14`), and `kiroCliUserAgent(includeAppVersion)` is called with `true` for `user-agent`, `false` for `x-amz-user-agent` (`wire.ts:16-27`).
- `+ x-amzn-kiro-profile-arn: <arn>` appended last in either profile when an ARN resolves (`adapter.ts:141`).
- Constants: `AMZ_TARGET`, `SDK_VERSION="1.0.27"`, `NODE_VERSION="22.21.1"`, `KIRO_IDE_VERSION="1.0.0"` (`wire.ts:5-9`).

## 3. Request body

Top-level key order: `conversationState`, then optional `additionalModelRequestFields`, then optional `profileArn` (`src/adapters/kiro/payload.ts:447-465`). Nothing else — no top-level `modelId`, no stream flag, no temperature/max_tokens/top_p/metadata, no top-level `tools`, **no `cache_control` anywhere**.

```
conversationState
  chatTriggerType      : "MANUAL"                  (always)
  agentContinuationId  : <uuid v4>                 (cli only, payload.ts:450)
  agentTaskType        : "vibe"                    (cli only)
  conversationId       : <stableConversationId>    (deterministic, not per-turn UUID)
  currentMessage       : { userInputMessage }
  history              : [ entry... ]              (omitted when empty)
profileArn              : "<arn>"                  (omitted for ksk_ API keys — adapter.ts:110)
additionalModelRequestFields : { reasoning|output_config: { effort } }   (optional)
```

`userInputMessage` (`payload.ts:402-409`): `content` (string), `modelId` (normalized, present on every user message incl. history), `origin` = `"KIRO_CLI"` (cli) | `"AI_EDITOR"` (ide), `images[]` when present, `userInputMessageContext.toolResults[]` when present.
`assistantResponseMessage`: `{ content, toolUses?, reasoningContent? }`; `toolUses[].input` is a **JSON object, not stringified**; `toolUseId` must match `^[a-zA-Z0-9_-]{1,64}$` (`wire.ts:30-34`).

Rewrites / mapping:
- **System prompt scrub**: Kiro has no system field. `context.systemPrompt.join("\n\n")` → `identifyRoutedModel(...)` → prefixed onto the **first user turn's** `content` as `systemPrefix + content`; separator `"\n\n"` after the last part (`payload.ts:143-145, 180, 415-420`). Falls back to the current message when no user history exists.
- Injected system parts, appended in order: tool-catalog nudge, then `KIRO_COMPLETION_INSTRUCTIONS`; cumulatively bounded to 16 384 chars (`MAX_KIRO_INJECTED_INSTRUCTION_CHARS`, `kiro-constants.ts:77`; `payload.ts:146-178`).
- `developer` role treated as `user`. Consecutive same-role turns merged (`appendTurnText`). Empty user turn carrying only tool results → `"The requested tool result is attached."`; empty/failed tool results → `"The tool completed without textual output."` (`payload.ts:381-385`).
- Images: inline base64 only; remote refs / malformed data URLs replaced with a bounded URL-free marker (`payload.ts:229-235`).
- Tools live **only** in `currentMessage.userInputMessageContext.tools`, never in history. Shape `{ toolSpecification: { name, description, inputSchema } }`. Sanitization: ≤48 tools, ≤96 000 catalog bytes, description caps 1024/9216, JSON-Schema keywords stripped (`additionalProperties`, `pattern`, `format`, min/max, `$schema`, `allOf`/`anyOf`) (`src/adapters/kiro-tools.ts`).
- Completion tool `codex_kiro_final_answer` appended when tools exist and history does not already end in a delivered final answer (`payload.ts:129-136`).
- Reasoning replay: `reasoningContent` is `{ signature }` or `{ redactedContent }`, tagged by the wire field it arrived on (`src/adapters/kiro/reasoning.ts:103-125`). Assistant turns with only reasoning and no text/toolUses are dropped (`payload.ts:262-266`).
- `additionalModelRequestFields` only when `kiroNativeEffortField(model, effort)` verifies; effort ∈ `low|medium|high|xhigh|max`; unsupported effort throws locally (`payload.ts:453-464`).
- Rejected before send: tool_choice other than `auto`/`none`, `serviceTier`, Responses structured output (`src/adapters/kiro/conversation.ts:10-36`).
- Model id normalization (`src/providers/kiro-models.ts:76-86`): lowercase, strip `kiro/`/`kiro-` prefix, strip `-\d{8}` and `-(low|medium|high|xhigh|max)`, `(\d+)-(\d+)`→`$1.$2`, `claude-<ver>-<family>`→`claude-<family>-<ver>`.
- Serialized with `JSON.stringify` (compact), built once and reused across retries.

Completion modes (`payload.ts:129-136`, `kiro-constants.ts:74`): `required` (tools present, no trailing delivered answer) | `text_fallback` (forced retry) | `disabled`. `text_fallback` appends `KIRO_COMPLETION_RETRY_MESSAGE` to current user content unless it is already the answer-delivered ack (`payload.ts:428-436`). Non-tool-result, non-retry turns get thinking tags injected (`payload.ts:437-444`).

## 4. Response

- Always AWS eventstream (`application/vnd.amazon.eventstream`), even for non-streaming client requests — the adapter drains the decoder into an array (`adapter.ts:342-368`).
- Decoder `decodeEventStream(response.body)` (`src/lib/eventstream-decoder.ts:177`): prelude + CRC, per-message CRC; truncation surfaces as `eventstream: truncated message at end of stream`.
- Frame headers read: `:message-type` (`event` | `exception` | `error`), `:event-type`, `:exception-type`, `:error-type`, `content-type` (`src/adapters/kiro/stream.ts:601-624`).
- Event → JSON field mapping (`src/adapters/kiro-events.ts:116-200+`):

| `:event-type` | payload fields | mapped to |
|---|---|---|
| `assistantResponseEvent` | `content`, `modelId` | text deltas via inline think-tag splitter; `modelId` updates context window |
| `reasoningContentEvent` | `text`, `signature`, `redactedContent` | `reasoning_raw_delta` (text) or `kiro_redacted_reasoning` (signature / redactedContent) |
| `toolUseEvent` | `name`, `toolUseId`, `input` (string), `stop` (bool) | tool-call start / delta / stop |
| `metadataEvent` | `tokenUsage{uncachedInputTokens, cacheReadInputTokens, cacheWriteInputTokens, outputTokens, totalTokens}`, `contextUsagePercentage`, `stopReason` | usage + stop reason |
| `meteringEvent` | `unit`, `unitPlural`, `usage` (or `amount`) | credits when unit ∈ {credit, credits} |
| `contextUsageEvent` | `contextUsagePercentage` | context pressure (primary source) |
| `messageMetadataEvent` | `conversationId` | returned conversation id (may differ from request) |
| `invalidStateEvent` | `message` | error |
| `error` | `reason`, `message` | error |
| `initial-response`, unknown | — | ignored (never logged raw) |

- Usage mapping (`kiro-events.ts:100-133`): `inputTokens = uncachedInputTokens + cacheReadInputTokens + cacheWriteInputTokens`; `cachedInputTokens = cacheReadInputTokens`; `cacheCreationInputTokens = cacheWriteInputTokens`; absent cache counters stay **undefined**, never coerced to 0. `totalTokens` passed through.
- Context total = `max(estimatedInput + output, ceil(contextWindow * contextUsagePercentage/100))` (`stream.ts:384-400`).
- Stop reasons: native `stopReason` from `metadataEvent` reaches the parser intact; `END_TURN` is the clean stop (`stream.ts:225`). `toolUseEvent.stop:true` closes the open call; a tool stop with no open call is a protocol error.
- Completion-mode stream handling: in `required` mode ordinary text is deferred and released only if the turn fails to call `codex_kiro_final_answer`; a valid completion call supersedes staged progress text (`stream.ts:240-300`). Failure triggers exactly one bounded second request with `forcedCompletionMode="text_fallback"` (`adapter.ts:180-267`).
- Stream-level throttle arrives after HTTP 200 and records a global cooldown for the next client retry (`kiro-retry.ts:112-115`).
- Per-protocol field naming is produced by the server layer from these `AdapterEvent`s, outside the Kiro adapter.

## 5. Auth

- Bearer token in `authorization`. Families: `aws_sso_oidc` (clientId + clientSecret, Builder ID) and `kiro_desktop` (Kiro desktop / kiro-cli token, prefix `aoa`); `ksk_` prefix ⇒ Kiro API key (`adapter.ts:109`, `src/oauth/kiro-credentials.ts:28,310`).
- **No account-id headers.** Identity = `x-amzn-kiro-profile-arn` + body `profileArn`.
- Builder-ID fallback ARN (fixed, request-scoped, never persisted, never seeds region inference): `arn:aws:codewhisperer:us-east-1:638616132270:profile/AAAACCCCXXXX` (`kiro-constants.ts:16`, `kiro.ts:520-534`). Applied only when the account has no own ARN **and** `authType === "aws_sso_oidc"`; the same verdict forces the `cli` wire client.
- Refresh A, AWS SSO OIDC (`kiro.ts:573-613`): `POST https://oidc.{region}.amazonaws.com/token`, `content-type: application/json`, body `{"grantType":"refresh_token","clientId":"…","clientSecret":"…","refreshToken":"…"}`, region from `resolveKiroRegion` (ssoRegion). Response `{accessToken, refreshToken?, expiresIn?}` → `expires = now + (expiresIn ?? 3600)*1000`; old refresh kept if absent. Signal = 30 s timeout.
- Refresh B, Kiro desktop (`kiro.ts:561-571`): `POST https://prod.{region}.auth.desktop.kiro.dev/refreshToken`, JSON `{"refreshToken":"…"}`, same response shape.
- Refresh C: on HTTP 400 with a rotated local kiro-cli credential matching the same account, retried once with the local refresh token + its routing metadata (`kiro.ts:657-678`).
- Terminal refresh errors: OAuth `error` values in `KIRO_TERMINAL_REFRESH_ERRORS` mark the credential dead (`kiro.ts:536-560`).
- Multi-account hooks: `genericFailoverAccountId` / `genericFailoverLimit` / `isGenericOAuthFailoverEnabled(config,"kiro")`, rotated reactively on classified 429/400/403 (`src/server/responses/adapter-dispatch.ts:1041-1090`, `adapter-continuation.ts:448`, `sidecar-execution.ts:190`).

## 6. Errors

HTTP classification `classifyKiroHttpError` (`src/adapters/kiro-errors.ts:100-180`):

| condition | status / errorType / code | retryable |
|---|---|---|
| quota text (`insufficient_quota`, "quota exhausted") | 429 / `insufficient_quota` / `insufficient_quota` | no |
| 429 rate (`requests per min|minute|second`, rpm, tpm) | 429 / `rate_limit_error` / `rate_limit_exceeded` | **yes** |
| 401 | 401 / `authentication_error` / `invalid_api_key` | no |
| 403 | 403 / `permission_error` / `permission_denied` | no |
| 400 validation | 400 / `invalid_request_error` / `invalid_request_error` | no |
| 503 | 503 / `server_error` / `server_is_overloaded` | yes |
| {500,502,503,504} | sanitized "Kiro upstream …" message | — |

- Detail keys: `__type`, `code`, `error`, `name`, `reason`, `message`, `Message`, `errorMessage` (`kiro-errors.ts:3`); `:exception-type` / `:error-type` headers also folded in. Messages sanitized and truncated to 500 chars.
- Account-scoped refusal `classifyKiroRefusal` (`src/adapters/kiro-refusal.ts:11-30`):
  - `400|429` + `reason === "MONTHLY_REQUEST_COUNT"` → `monthly_quota`
  - `403` + `reason === "TEMPORARILY_SUSPENDED"` or message contains `"temporarily suspended"`, `"temporarily is suspended"`, `"locked your account"`, `"locked it as a"` → `suspended`
  - `429` → `rate`; else `other` (never convicts an account). Bodies > `BOUNDED_BODY_MAX_BYTES` degrade to status-only.
- Cooldowns:
  - Suspension → **24 h** quarantine, `cooldownSource: "kiro-suspension"`, bound to credential generation so a re-login clears it (`src/oauth/generic-account-failover.ts:117-124`).
  - Monthly quota → until `resetAt` from cached account evidence, capped at `ACCOUNT_QUOTA_TTL_MS` (`src/providers/kiro-usage.ts:327-343`).
- 429 in transport (`kiro-retry.ts:248-274`): bounded body read, `content-encoding`/`content-length` stripped on the rebuilt response, `Retry-After` via `resolveClientRetryAfter` (message-derived reset honored), delay `parseRetryAfterMs(...) ?? 2000`; transient only when `retryable && code === "rate_limit_exceeded"`; `THROTTLE_ATTEMPTS = 3` behind a shared in-process throttle probe.
- Connection resets (`kiro-retry.ts:19-21,216-230`): `RESET_ATTEMPTS = 3`, backoff base 150 ms → max 1000 ms; retried only when no output emitted; codes/strings `ECONNRESET`, `EPIPE`, `UND_ERR_*`, `fetch failed`, `premature close`, `other side closed`, `unexpected EOF`, `network connection lost`, `terminated`, `truncated message at end of stream`, `eventstream: truncated`, plus DNS/connect codes `{ENOTFOUND, EAI_AGAIN, ECONNREFUSED, ENETUNREACH, EHOSTUNREACH}`.
- Endpoint errors: 400/403 whose lowercase body contains `unknownoperation` / `invalidsignature` / … mark the endpoint bad and permit gateway rotation (`kiro-retry.ts:24-31, 227-240`) — allowed only when the turn produced no reasoning and no text (`adapter.ts:241`).
- Never retried: any error after output was emitted; `invalid Kiro …` protocol errors.

## 7. Login flow used by the CLI

Import-first; no browser redirect-paste OAuth on this path.
1. `restoreStaleKiroCliSessionRecovery()` recovers an interrupted prior account switch (`kiro.ts:351`).
2. Local import: kiro-cli SQLite (`~/.local/share/kiro-cli/data.sqlite3`, `~/Library/Application Support/kiro-cli/data.sqlite3`, `%LOCALAPPDATA%\Kiro-Cli\data.sqlite3`; keys `kirocli:oidc:token`, `kirocli:oidc:device-registration`) → JSON credentials file (`kiro.ts:397-401`, `kiro-credentials.ts:18-24`).
3. `KIRO_ACCESS_TOKEN` env (with `KIRO_REFRESH_TOKEN`, `KIRO_PROFILE_ARN`, `KIRO_API_REGION`, `KIRO_REGION`) — `kiro.ts:395-410`.
4. Manual paste of an `aoa…` token through `onManualCodeInput` (`kiro.ts:411-440`). "Browser login is not supported for Kiro."
5. `--force-login`: snapshot current kiro-cli session → `kiro-cli logout` → `kiro-cli login` → re-import; exact rollback of the prior session on any failure (`kiro.ts:353-392`).

Device grants exist for the GUI (`src/oauth/kiro-device-login.ts`), methods `builder-id | google | github`:
- `POST https://oidc.us-east-1.amazonaws.com/client/register` (`:193`)
- `POST https://oidc.us-east-1.amazonaws.com/device_authorization` (`:201`)
- poll `POST https://oidc.us-east-1.amazonaws.com/token` (`:232`)
- Social IdP host `https://prod.us-east-1.auth.desktop.kiro.dev` (`:41`); all bodies `content-type: application/json`, 20 s timeout; max 4 concurrent flows, 15 min lifetime, 60 s terminal retention. Exposed as `POST /api/oauth/login`.

## Quota probe (separate from generation)
`POST https://management.{region}.kiro.dev/` with `x-amz-target: AmazonCodeWhispererService.GetUsageLimits` (`src/providers/kiro-usage.ts:32,103,204`). Region deliberately ignores the Builder-ID service ARN (`kiro-usage.ts:92-99`). Not on the generation path.

## Where to start in Rust
`src/adapters/kiro/adapter.ts:104-178` (headers + URL + body assembly) → `src/adapters/kiro/payload.ts:108-468` (body) → `src/adapters/kiro/stream.ts:601-780` + `src/adapters/kiro-events.ts:116+` (eventstream decode) → `src/adapters/kiro-retry.ts:248-320` + `src/adapters/kiro-refusal.ts:11-30` (errors/cooldowns).


## Unknowns
- Exact auto-added hop-by-hop headers (accept-encoding, host, connection) that undici emits — not specified in source
- stableConversationId hash inputs and digest algorithm unread, so byte-identical conversationId reproduction is unverified
- invocationId() generation details (UUID v4 vs v7) unconfirmed
- initial-response event semantics are accepted but unused; whether upstream always emits it is unverified
- Whether metadataEvent ever actually carries cacheWriteInputTokens on the wire (code supports it, no capture cited)
- osTag() version source library unconfirmed
- Server-layer mapping of AdapterEvents into Responses/Chat/Messages field names lives outside the Kiro adapter and was not read
- Exact KIRO_TERMINAL_REFRESH_ERRORS member list not read
- ACCOUNT_QUOTA_TTL_MS exact value not read