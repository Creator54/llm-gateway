# Provider `openai` — upstream wire spec (as implemented by opencodex)

Source repo: `~/opencodex` (TypeScript/Bun). All refs are `path:line`.

## 0. Identity / where it is defined

- `src/providers/registry/entries-core.ts:121-132` — provider entry:
  ```
  id: "openai"
  label: "OpenAI (Codex login)"
  adapter: "openai-responses"
  baseUrl: "https://chatgpt.com/backend-api/codex"
  authKind: "forward"
  codexAccountMode: "pool"
  supportsServiceTier: true
  featured: true
  ```
  No `upstreamHttpVersion`, no `models` list (roster comes from the live catalog endpoint), no `responsesPath`.
- `src/providers/openai-tiers-destination.ts:9` — `CODEX_FORWARD_BASE_URL = "https://chatgpt.com/backend-api/codex"`.
- `src/providers/openai-tiers-destination.ts:22-26` — `isCanonicalOpenAiForwardProvider(provider)` = `adapter === "openai-responses" && authMode === "forward" && normalizedBaseUrl(baseUrl) === CODEX_FORWARD_BASE_URL`. Every "canonical-only" strip below is gated on this predicate.
- Sibling provider for API keys: `entries-core.ts:671-683` (`openai-apikey`, baseUrl `https://api.openai.com/v1`, `authKind: "key"`) — different wire contract, not this spec.

## 1. Endpoint(s)

| Purpose | Method | URL | Ref |
|---|---|---|---|
| Main turn | POST | `https://chatgpt.com/backend-api/codex/responses` | `src/adapters/openai-responses/passthrough.ts:207-213`; `src/server/responses/codex-ws-request.ts:9` |
| Main turn (WS) | WS upgrade | `wss://chatgpt.com/backend-api/codex/responses` | `codex-ws-request.ts:10` |
| Native compaction | POST | `https://chatgpt.com/backend-api/codex/responses/compact` | `src/server/responses/compact.ts:914` |
| Account roster | GET | `https://chatgpt.com/backend-api/codex/models?client_version=<v>` | `src/codex/model-entitlements.ts:35,44-50`; discovery version `"99.0.0"` at `:807` |
| Plan/quota probe | GET | `https://chatgpt.com/backend-api/wham/usage` | `src/codex/quota-query-backoff.ts:71,137`; `src/codex/reserve-availability.ts:10` |

URL construction (`passthrough.ts:205-213`):
```ts
const mayForwardCallerCredentials = isCanonicalOpenAiForwardProvider(provider);
const baseUrl = mayForwardCallerCredentials
  ? CODEX_FORWARD_BASE_URL
  : provider.baseUrl.replace(/\/+$/, "");
url = `${baseUrl}/responses`;   // no /v1
```
- No query string on `/responses`.
- No region / deployment / project selection exists. `grep region|deployment` in registry+tiers: no matches. The only "project/org" identity is the `chatgpt-account-id` header value (see §5).
- Method is always `POST` (`passthrough.ts:551-556`).

## 2. Request headers

Headers are built as a plain object in this order (`passthrough.ts:200-258`):

1. `Content-Type: application/json` (literal, first key).
2. `Object.assign(headers, provider.headers)` — static provider headers (the `openai` entry declares none).
3. Forwarded caller headers, only from this allowlist (`passthrough.ts:66-86`):
```ts
export const FORWARD_HEADERS = [
  "authorization",
  "chatgpt-account-id",
  "openai-beta",
  "originator",
  "session_id",
  "session-id",
  "thread-id",
  "x-client-request-id",
  "x-codex-beta-features",
  "x-codex-installation-id",
  "x-codex-parent-thread-id",
  "x-codex-turn-metadata",
  "x-codex-turn-state",
  "x-codex-window-id",
  "x-oai-attestation",
  "x-openai-subagent",
  "x-responsesapi-include-timing-metrics",
  CODEX_RESPONSES_LITE_HEADER,   // "x-openai-internal-codex-responses-lite"
];
```
   For the lite header, every existing casing of that key is deleted before the forwarded value is written (`passthrough.ts:228-234`).
4. Pool override wins over the forwarded value (`passthrough.ts:242-246`):
```ts
headers["authorization"]     = `Bearer ${override.accessToken}`;
headers["chatgpt-account-id"] = override.chatgptAccountId;
```
   Only applied when `mayForwardCallerCredentials` and `_codexAccountOverride` is set.
5. `User-Agent` fallback (`passthrough.ts:88-96, 257`): if no provider-owned header matches `user-agent` case-insensitively, the caller's `User-Agent` is copied verbatim. Nothing else from the caller leaks.
6. `x-codex-routing-hint` (`passthrough.ts:525-536`): re-derived for canonical only, format `model=<model>` or `model=<model>;tier=<tier>`; all stale casings deleted first; omitted when model or tier fails validation (`src/codex/forward-transport-headers.ts:16-25`, limits model ≤256 B, tier ≤64 B, printable ASCII).

Guards:
- If `_codexAccountRequired` is set and `_codexAccountOverride` is missing, throws `"Codex pool account auth is required but unavailable"` before any I/O (`passthrough.ts:232-240`).
- Missing `Accept`, `Accept-Encoding`, `Content-Length`: none are set by the adapter. `Accept` is *not* in `FORWARD_HEADERS`, so a client's `Accept` is dropped. Only `content-type` is guaranteed.

WebSocket upgrade header set (`codex-ws-request.ts:74-86`):
- All HTTP headers copied except `content-type`, `content-length`, `accept`, `accept-encoding`.
- `openai-beta` = caller's value with `, responses_websockets=2026-02-06` appended if absent, else `responses_websockets=2026-02-06` (`WS_BETA`, `codex-ws-request.ts:12`).
- `xs-codex-routing-hint` re-derived from the JSON body (`codex-ws-request.ts:69-70`).
- `responses_multi_agent=v1` appended only for the injection path on `OPENAI_API_RESPONSES_URL` (not the canonical URL) — `src/server/responses/ws-upstream.ts:179-182`.

HTTP version: no pin for `openai`; `upstreamHttpVersion` undefined → Bun default negotiation (`src/lib/upstream-http-version.ts:1-40` only pins when explicitly set, and only for `https:` targets).

Header order: insertion order only (Content-Type → static → forwarded → override → routing hint). Not explicitly pinned; no code sorts.

## 3. Request body

Passthrough: the adapter forwards `parsed._rawBody` (already routed), never rebuilds from `modelId` (`passthrough.ts:538-541`).

Transform pipeline (`passthrough.ts:267-343`), then canonical-only stage:

a) `stripPreviousResponseId(body, forward || expanded)` — `canonical-forward.ts:17-22`. Drops `previous_response_id` on every forward route.

b) `normalizeRoutedAgentMessages` — skipped when `forward` (`passthrough.ts:271`).

c) `repairIdentityInResponsesBody(body, stripRoutedIdentity)` — deletes proxy-generated identity sentences on forward (`passthrough.ts:282-286`; regexes in `src/adapters/identity.ts:40-111`).

d) `mapRoutedResponsesReasoningEffort` — no-op for forward (`src/adapters/openai-responses/reasoning.ts:283`).

e) `applyTierDecisionToResponsesBody` — writes/deletes `service_tier` only on an explicit decision (`canonical-forward.ts:24-32`). `Kind "forward-caller"` (the Codex default) leaves the body untouched.

f) stateless/adjacency/paired repairs — `openai` sets none of `statelessResponses` / `requiresPairedResponsesToolResults`, so these are inert. `repairOrphanedInputItems` still runs for forward (`passthrough.ts:326-336`).

g) `stripUnsupportedForwardParams(body, canonical)` — `canonical-forward.ts:81-104`:
```ts
const drop = canonical ? ["metadata", "max_output_tokens"] : ["metadata"];
```
   → canonical: removes both `metadata` and `max_output_tokens`.

h) canonical-only, in order (`passthrough.ts:337-343`):
   - `stripCanonicalForwardSamplingParams` — deletes `temperature`, `top_p`, `stop`, `user` (`canonical-forward.ts:105-…`, list at `:100`).
   - `stripDeprecatedPromptCacheRetention(body, modelId)` — deletes `prompt_cache_retention` only for `gpt-5.6` / `gpt-5.6-*` (`src/adapters/openai-responses/prompt-cache.ts:16-29`).
   - `stripCanonicalForwardPromptCacheOptions` — deletes top-level `prompt_cache_options` (`prompt-cache.ts:31-40`).
   - `normalizeCanonicalForwardPromptEnvelope` — `canonical-forward.ts:147-190`: deletes `truncation`; collects `input[]` items that are system messages, removes them from `input`, and appends their text (joined by `\n\n`) to `instructions` (`instructions = existing === "" ? folded : existing + "\n\n" + folded`). Skips folding if any system item's text is non-extractable.
   - `normalizeCanonicalForwardContinuationEnvelope` — `canonical-forward.ts:191-…`: when `store === false`, removes every `input[]` row with `type === "item_reference"`; then `stripPromptCacheBreakpoints` recursively removes Posit/Anthropic cache markers (bounded: depth 64, 100k nodes, atomic abort).

i) model normalization: `modelSuffixBracketStrip` (off for `openai`) then, for canonical, force SSE (`passthrough.ts:518-523`):
```ts
if (isCanonicalOpenAiForwardProvider(provider) && finalBody.stream !== true)
  finalBody = { ...finalBody, stream: true };
```
   `store` is deliberately **not** rewritten on forward (`passthrough.ts:516-521` comment).

j) `JSON.stringify(...)` and returned as `body` (`passthrough.ts:553-557`).

Fields the canonical backend never sees: `previous_response_id`, `metadata`, `max_output_tokens`, `temperature`, `top_p`, `stop`, `user`, `prompt_cache_options`, `prompt_cache_retention` (gpt-5.6*), `truncation`, `item_reference` rows under `store:false`, system-role items inside `input` (folded into `instructions`).

Fields preserved verbatim: `model`, `input`, `instructions` (augmented), `tools`, `tool_choice`, `parallel_tool_calls`, `reasoning` (`effort`/`summary`), `text`, `include`, `store`, `service_tier`, `prompt_cache_key`, `client_metadata`, plus any unknown top-level keys.

Inbound protocol mapping (how a Chat or Anthropic client's body becomes this Responses body):
- Chat → Responses: `src/chat/inbound.ts:442-…, 589-640`. `system`/`developer` → `instructions` (`\n\n` joined); `store:false`; `stream` from the caller; `max_completion_tokens|max_tokens` → `max_output_tokens`; `temperature`, `top_p`, `presence_penalty`, `frequency_penalty`, `stop`, `user`, `parallel_tool_calls`, `service_tier`, `prompt_cache_key`, `metadata` copied when present; `reasoning` built from effort/summary with `summary ?? "auto"`; `response_format` → `text`. Internal hop is `POST http://localhost/v1/responses` with `inboundWire: "chat"` (`src/server/chat-completions.ts:419-444`).
- Anthropic Messages → Responses: `src/claude/inbound.ts:367-432`. `system` → `instructions`; `store:false`; `stream` from caller; user/assistant turns → `input` items; mid-conversation Anthropic `system` messages become `{type:"message", role:"developer", ...}` items in place (not re-hoisted).

Compaction request (`src/responses/compaction.ts:79`; `src/server/responses/compact.ts:914`): body carries the compaction prompt (`COMPACT_PROMPT`), no tools, and strips `compaction_trigger` / `additional_tools`; response is a unary JSON `{type:"compaction"}` item, decoded by `buildCompactV1Output`.

## 4. Response

Streaming format: verbatim upstream Server-Sent Events. The adapter is `passthrough: true` and normally relays bytes untouched (`passthrough.ts:568-573`).

- Non-streaming caller + canonical backend → the SSE terminal is folded back into a JSON Responses object (`src/server/responses/passthrough-delivery.ts:434-436`, `canonicalBufferedJson`).
- SSE relay with terminal repair: `src/server/responses-terminal-repair.ts:77-…`; terminal event types are exactly `response.completed`, `response.failed`, `response.incomplete` (`:32-34`). Repair synthesizes the missing terminal, tracks added items, `response.created`, and monotonic sequence numbers.
- Chat client → `src/chat/outbound.ts:257-300` `responsesSseToChatCompletionsSse`: emits chat chunks, stable per-tool-call indexes (`call_id`/`item id` maps), and stamps the response-level service tier on every chunk.
- Anthropic client → `src/claude/outbound.ts` (SSE framing `sseFrame` at `:107`; `message_start` snapshot at `:351`).

Usage extraction (`src/adapters/openai-responses/passthrough.ts:137-166`):
```
input_tokens, output_tokens, total_tokens
input_tokens_details.cached_tokens        -> cachedInputTokens
input_tokens_details.cache_write_tokens   -> cacheCreationInputTokens
output_tokens_details.reasoning_tokens    -> reasoningOutputTokens
```
Unknown usage keys are preserved (returned as `rawUsage`) even when token counts are zero/absent (openai/codex#41980). Returns `undefined` when input+output are 0 and there are no extras.

Cache token mapping to Anthropic (`src/claude/outbound.ts:90-101`):
```
input_tokens              = max(0, input_tokens - cached - cache_write)
cache_read_input_tokens   = cached
cache_creation_input_tokens = cache_write
output_tokens             = output_tokens
server_tool_use.web_search_requests (only when > 0)
```

Relayed response headers back to the client are allowlisted (`src/server/safe-response-headers.ts:1-14`): `retry-after`, `x-request-id`, `openai-request-id`, `x-codex-turn-state`, `openai-model`, `x-models-etag`, `x-reasoning-included`, `x-codex-credits-*`, `x-codex-safety-buffering-*`, all `x-ratelimit-*`, and `x-codex[-…]-(primary|secondary|tertiary)-(used-percent|window-minutes|reset-at)` / `x-codex[-…]-limit-name`.

## 5. Auth

Credential: ChatGPT OAuth access token (JWT), used as `Authorization: Bearer <access_token>`, plus `chatgpt-account-id: <account id>`.

- `src/oauth/chatgpt.ts:5-15`:
  ```
  CLIENT_ID  = app_EMoamEEZ73f0CkXaXp7hrann
  AUTH_URL   = https://auth.openai.com/oauth/authorize
  TOKEN_URL  = https://auth.openai.com/oauth/token
  SCOPE      = openid profile email offline_access api.connectors.read api.connectors.invoke
  CALLBACK_PORT = 1455, CALLBACK_PATH = /auth/callback
  ORIGINATOR = "opencodex"
  ```
- Account id extraction (`chatgpt.ts:27-41`), first hit wins over `[id_token, access_token]`:
  1. top-level `chatgpt_account_id`
  2. `payload["https://api.openai.com/auth"].chatgpt_account_id`
  3. `organizations[0].id`
  Conflict between (1) and (2) detected separately (`chatgpt.ts:116-…`).
- Credential shape (`chatgpt.ts:136-166`): requires non-empty `access_token`; `refresh_token` defaults to `""`; `expires = now + expires_in*1000` with `expires_in` defaulting to 3600 and finite-guarded.
- Token exchange (authorization code), `chatgpt.ts:203-221`:
  ```
  POST https://auth.openai.com/oauth/token
  Content-Type: application/x-www-form-urlencoded
  grant_type=authorization_code&client_id=...&code=...&redirect_uri=...&code_verifier=...
  ```
- Refresh, `chatgpt.ts:255-277`:
  ```
  POST https://auth.openai.com/oauth/token
  Content-Type: application/x-www-form-urlencoded
  grant_type=refresh_token&client_id=...&refresh_token=...
  ```
  (form-encoded; the comment notes Codex-rs uses JSON and both are accepted.)
- Refresh trigger: proactive when `expiresAt <= now + 60_000` (`REFRESH_SKEW_MS`, `src/codex/account-store.ts:33,857`); plus reactive single replay on a pre-stream 401 (`src/server/responses/passthrough-dispatch.ts:1114-1150`).
- Multi-account selection hooks:
  - Pool mode injection: `src/codex/auth-context.ts:1513-1527` `applyCodexAuthContextToProvider` sets `_codexAccountOverride {accessToken, chatgptAccountId}` and `_codexAccountRequired` when `mode === "pool"` and `authMode === "forward"`.
  - Applied in routing at `src/server/responses/request-prepare.ts:1305`.
  - Selection ranking lives in `src/codex/routing/selection.ts` (`pickLowestUsageCodexAccount:584`, `pickResetFirstCodexAccount:316`, `pickAlternateCodexAccount:604`, `pickUnboundStrategyAccount:421`, `pickPriorityPreemption:707`).
  - Account store file: `~/.config/opencodex/codex-accounts.json` (`join(getConfigDir(), "codex-accounts.json")`).
  - 429 → next account: `src/server/responses/passthrough-dispatch.ts:1368-1397`, passing the upstream `retry-after` into `rotateGenericOAuthAccountOn429`.
- Roster/quota probes use the same credential with title-cased header (`ChatGPT-Account-Id`) — `src/codex/model-entitlements.ts:723-729, 876-882`; `src/codex/auth-api/pool-quota-probe.ts:263-266, 409` (8 s timeout on the probe).

## 6. Errors, retries, cooldowns

Classification of a pre-stream Codex rejection (`src/codex/quota-rejection.ts:357-398`) returns kinds:
`reset-eligible-exhaustion` | `scoped-quota-exhaustion` | `generic-rate-limit` | `unverified-billing-or-quota` | `transient-server-error` | `authentication-error` | `permission-error` | `other`.

```
401                                   -> authentication-error
403 (+ denial code parsed from body)  -> permission-error
5xx (TRANSIENT_SERVER_STATUSES)       -> transient-server-error
429/402 with reset-eligible code      -> reset-eligible-exhaustion (alternateRetryEligible)
429/402 with scoped code              -> scoped-quota-exhaustion  (alternateRetryEligible)
429 otherwise                         -> generic-rate-limit
402 otherwise                         -> unverified-billing-or-quota
```
Codes:
- reset-eligible: `usage_limit_exceeded`, `insufficient_quota` (`quota-rejection.ts:3-6`)
- org/project scoped (rotation deliberately does NOT help): `credit_balance_exhausted`, `organization_spend_limit_exceeded`, `project_spend_limit_exceeded`, `organization_usage_limit_exceeded` (`:33-38`)
- 403 workspace denial: `codex_workspace_access_denied`; 403 entitlement: `codex_entitlement_missing` (`:81-92`)

Cooldown math (`src/codex/routing/cooldown-math.ts`):
- `CODEX_DEFAULT_QUOTA_COOLDOWN_MS = 60_000` (`:12`)
- `CODEX_MAX_QUOTA_COOLDOWN_MS = 24h` (`:13`)
- `CODEX_MAX_RESET_DERIVED_COOLDOWN_MS = 15 * 60_000` (`:20`) — reset timestamps are capped well below the Retry-After ceiling
- `CODEX_MAX_QUOTA_AVOID_MS = 6h` (`:26`), `CODEX_QUOTA_PROBE_INTERVAL_MS = 5 min` (`:28`), `CODEX_FAILURE_WINDOW_MS = 5 min` (`:29`)
- Transient soft-avoid: 30 s → 2 min → 10 min → 30 min escalation (`:31-37`)
- `computeQuotaCooldown` precedence (`:211-221`): `retry-after` → reset-derived (clamped to 15 min) → default 60 s.

Retry-After parsing: seconds (numeric) or HTTP-date; `src/lib/upstream-retry.ts:388-398` (`retryAfterDelayMs`), and per-provider `parseRetryAfterMs` in `src/oauth/generic-account-failover.ts`-adjacent modules (`anthropic-routing.ts:159`, `key-failover.ts:351`). Same-target 429 wait releases the body and sleeps with heartbeats: `src/lib/upstream-retry.ts:366-375` `prepareSameTarget429Wait`, used at `passthrough-dispatch.ts:1443-1455`.

Failover limits: `GENERIC_OAUTH_MAX_FAILOVERS_PER_REQUEST = 3` and a per-request credential-hop budget (`src/oauth/generic-account-failover.ts:59`; `reserveCredentialHop` at `passthrough-dispatch.ts:1382`). `isRetriableFailoverStatus` = all 4xx/5xx except antigravity (429 only) (`generic-account-failover.ts:53-57`).

Suspension: no ChatGPT-specific permanent suspension; a failed refresh quarantines by credential generation and marks `needsReauth` (`passthrough-dispatch.ts:1131-1145`; `shouldMarkAccountNeedsReauthForCodexAuthFailure` in `src/codex/auth-context.ts:1496-1500`).

Client-facing error shaping (`passthrough-delivery.ts:173-190`):
```
400 -> invalid_request_error / invalid_request_error
401 -> authentication_error / invalid_api_key
403 -> permission_error / permission_denied
429 -> rate_limit_error / rate_limit_exceeded
499 -> client_closed_request
503 -> server_error / server_is_overloaded
```
plus recognized bare codes: context `context_length_exceeded|context_window_exceeded|input_too_long`, quota `usage_limit_exceeded|usage_limit_reached|rate_limit_exceeded|insufficient_quota`, overload `server_is_overloaded`. Anthropic clients: `src/claude/outbound.ts:57-73` (402 billing, 429 rate_limit, 504 timeout, 529 overloaded).

Quota headers parsed off responses (`src/codex/quota.ts:509-…`): `x-codex-{primary,secondary,tertiary}-used-percent`, `-reset-at`, `-window-minutes`. Window classification: explicit ≥28-day duration → monthly; explicit sub-day duration → short/burst (weekly lands in the secondary slot). SHAM payload equivalent at `quota.ts:861-…` (`rate_limit.{primary,secondary,tertiary}_window`, `rate_limit_reset_credits.available_count`).

## 7. Login flow used by the CLI

`ocx account login openai|codex|chatgpt` (`CODEX_NAMES = {openai, codex, chatgpt}`, `src/cli/account-auth.ts:196`).

Flags (`account-auth.ts:37-46`): `--id <account-id>`, `--reauth`, `--device`, `--method builder-id|google|github`, `--code -`, `--no-wait`, `--json`.

Control plane (local gateway API):
1. `POST /api/codex-auth/login` body `{ id?, reauth?, device? }` → `{ url?, deviceCode?, instructions?, browserLaunch?, flowId }` (`account-auth.ts:197-217`).
2. Optional `POST /api/codex-auth/login/code` body `{ flowId, input }` for the paste path (`account-auth.ts:218-223`).
3. Poll every 2 000 ms: 150 attempts (browser, 5 min) / 480 attempts (device, 16 min ≈ the 15-min grant) (`account-auth.ts:235-245`).
4. Other routes in the same namespace: `/api/codex-auth/{accounts,active,login-status,accounts/refresh,accounts/pause,quota/history,main/reauth-device}`.

Browser flow (default):
- PKCE S256. `GET https://auth.openai.com/oauth/authorize?client_id=app_EMoamEEZ73f0CkXaXp7hrann&response_type=code&redirect_uri=http://localhost:1455/auth/callback&scope=openid+profile+email+offline_access+api.connectors.read+api.connectors.invoke&codex_cli_simplified_flow=true&originator=opencodex&id_token_add_organizations=true` + `code_challenge`/`code_challenge_method=S256`/`state` (`chatgpt.ts:167-198`).
- Local listener on `127.0.0.1:1455` path `/auth/callback`. If the port is in use, the flow falls back to paste of the final redirect URL or `code#state` (`src/oauth/callback-server.ts:140-160, 275`).

Device flow (`--device`, `src/oauth/chatgpt-device.ts`):
- `POST https://auth.openai.com/api/accounts/deviceauth/usercode`, JSON `{"client_id":"app_EMoamEEZ73f0CkXaXp7hrann"}` → `device_auth_id`, `user_code`, `interval` (`:17, 99-113`).
- User opens `https://auth.openai.com/codex/device` (`:22`) and enters `user_code`.
- `POST https://auth.openai.com/api/accounts/deviceauth/token`, JSON `{"device_auth_id","user_code"}` (`:130-150`); 403/404 = still pending (sleep `min(interval, remaining)`), other non-2xx = terminal; success returns `authorization_code` + `code_verifier`.
- Exchange: `POST https://auth.openai.com/oauth/token` form-encoded `grant_type=authorization_code&client_id=...&code=...&code_verifier=...&redirect_uri=https://auth.openai.com/deviceauth/callback` (`:168-183`).
- Device grant TTL 15 min; deadline re-checked after each poll.

No API-key path under this provider — that is `openai-apikey`.

## 8. WebSocket transport (optional side path)

Gated by `shouldUseCodexWsUpstream(url, init, runtime, upstreamWebsocketConfigured)` (`src/server/responses/ws-upstream.ts:119-149`): Bun capability, no `SOCKS5_PROXY`, URL must equal `CODEX_RESPONSES_HTTP_URL` (api.openai.com only with explicit opt-in), method POST, string body, root-level `stream === true`.

Frame: `JSON.stringify({...body, type: "response.create"})` with `stream` deleted (`codex-ws-request.ts:75-77`).
Per-turn headers carried inside `client_metadata`, not the upgrade (`codex-ws-request.ts:8, 37-53`): `x-codex-turn-state`, `x-codex-turn-metadata`, and `x-openai-internal-codex-responses-lite` under key `ws_request_header_x_openai_internal_codex_responses_lite`.
Metadata limits (`src/server/responses/codex-ws-metadata.ts:3-6`): 32 KiB total, 16 families, 128 headers, 4096 B per value. Oversized first frame refuses WS before dialing (`ws-upstream.ts:186-190`) and falls back to SSE.

## 9. Minimal Rust reproduction checklist

1. `POST https://chatgpt.com/backend-api/codex/responses`, body `application/json`.
2. Headers, in this order: `content-type: application/json`, then for each present in the caller request and in `FORWARD_HEADERS` (lowercase lookup, emit original case), then `authorization: Bearer <access_token>` and `chatgpt-account-id: <id>`, then `x-codex-routing-hint` derived from `model`/`service_tier`.
3. Body: take caller body; delete `previous_response_id`, `metadata`, `max_output_tokens`, `temperature`, `top_p`, `stop`, `user`, `prompt_cache_options`, `prompt_cache_retention` (gpt-5.6*), `truncation`; remove `input[]` system messages and append their text to `instructions` (`\n\n`); drop `input[]` rows with `type=="item_reference"` when `store==false`; set `stream=true`.
4. Relay SSE verbatim; synthesize `response.completed|failed|incomplete` if the upstream ends without one.
5. Auth: refresh at expiry-60 s via form-encoded `grant_type=refresh_token`. One replay on a pre-stream 401.
6. Errors: 401 auth, 403 permission, 5xx transient, 429 `usage_limit_exceeded|insufficient_quota` = reset-eligible (rotate), org/project-scoped codes = do not rotate, else generic rate limit; cooldown = retry-after → reset (≤15 min) → 60 s.

## Unknowns
- Whether an `Accept` header is ever sent on the canonical HTTP path. The adapter sets only `Content-Type`, and `accept` is not in FORWARD_HEADERS, so a caller's `Accept` is dropped and nothing is substituted. No explicit `Accept: text/event-stream` was found anywhere in the responses path.
- Concrete HTTP version used for chatgpt.com. `upstreamHttpVersion` is unset on the `openai` entry, so Bun negotiates via ALPN; the negotiated version was not measured.
- Wire header emission order beyond insertion order. Headers are a plain object mutated static-then-forwarded-then-override; no code pins an order and none was observed on the wire.
- Whether opencodex synthesizes `session_id` / `thread-id` for the plain HTTP path when the client omits them. `originator` is only synthesized in src/server/audio-transcriptions.ts:109; the `session_id`-from-`prompt_cache_key` derivation is referenced only in comments (request-prepare.ts:483) and its implementation was not located.
- Exact schema of the `x-codex-turn-metadata` envelope (subagent_kind and friends). It is consumed by effort-policy and copied into WS `client_metadata`, but the field set was not extracted.
- Body of `rateLimitRetryPolicyFor`. Known to return null for non-key auth (OAuth/forward) so 429s are never replayed on the same token, but the function body was not read.
- Whether `originator: codex_cli_rs` is ever injected on the canonical responses path. Only `audio-transcriptions.ts:109` sets it; `ws-upstream.ts` explicitly never fabricates it.
- Whether `store` is ever coerced to `false` for this provider. The comment at passthrough.ts:516-521 says explicit caller storage semantics are left untouched, and the code confirms no write, but no upstream behaviour test was run.
- Exact `service_tier` values accepted by the canonical backend and its response-side echo semantics; `responseTierAuthorityForProvider` returns false for this provider, so response-tier evidence is discarded (openai-tiers-destination.ts:33-35).