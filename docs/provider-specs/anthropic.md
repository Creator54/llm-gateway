# Upstream wire spec — provider `anthropic` (Claude Pro/Max OAuth) as implemented by opencodex

Source roots: `~/opencodex/src`. Two egress lanes reach `api.anthropic.com`:
- **Adapter lane** (`adapter: "anthropic"`) — client spoke Responses or Chat, body is translated.
- **Native lane** (client already spoke Messages, `/v1/messages` inbound) — body forwarded under a field allowlist.
Both share the same URL/header/credential helpers so they cannot drift (`src/adapters/anthropic/passthrough.ts:1-20`).

---

## 1. Endpoint

- Method `POST`, body `application/json` (`src/adapters/anthropic.ts:1131-1152`).
- URL = `anthropicMessagesUrl(provider.baseUrl)`:
  trim trailing `/`, strip a trailing `/v1/messages` or `/v1`, append `/v1/messages` → `{origin}/v1/messages` (`src/adapters/anthropic.ts:397-407`). Base `https://api.anthropic.com` → `https://api.anthropic.com/v1/messages`.
- Unresolved `{placeholder}` in baseUrl is a hard error before I/O (`anthropic.ts:566-576`).
- Native lane builds `${base}${pathname}${search}` from the inbound path, so `/v1/messages`, `/v1/messages/count_tokens` both work, query string preserved (`src/server/claude-messages.ts:583-590`).
- Base URL/region choice: registry entry `id: "anthropic"`, `baseUrl: "https://api.anthropic.com"`, `authKind: "oauth"`, `allowBaseUrlOverride: true` (`src/providers/registry/entries-core.ts:487-498`). There is **no region/project selector** — Anthropic has no such concept here. Third-party Anthropic-compatible providers reuse the adapter with their own baseUrl (e.g. `api.xiaomimimo.com/anthropic`).
- `usesNativeAnthropicEndpoint()` = hostname exactly `api.anthropic.com` (`anthropic.ts:389-395`); it gates top-level `cache_control` support and OAuth credential legality.

## 2. Request headers

Base set, in this insertion order (`anthropic.ts:535-543`):

```
Content-Type: application/json
anthropic-version: 2023-06-01        // ANTHROPIC_API_VERSION
Accept: text/event-stream | application/json   // by stream flag
User-Agent: @anthropic-ai/sdk/0.74.0
```

Key auth (`anthropic.ts:546-550`): `x-api-key: <apiKey>`, or `Authorization: Bearer <apiKey>` when `provider.apiKeyTransport === "bearer"`.

OAuth auth (`anthropic.ts:555-564`), appended after the base set:

```
Authorization: Bearer <accessToken>
anthropic-beta: claude-code-20250219,oauth-2025-04-20      // ANTHROPIC_OAUTH_BETA
X-App: cli
X-Stainless-Retry-Count: 0
X-Stainless-Runtime: node
X-Stainless-Lang: js
X-Stainless-Timeout: 600
X-Stainless-Arch: <process.arch>          // e.g. x64
X-Stainless-OS: <process.platform>       // e.g. linux
X-Stainless-Package-Version: 0.74.0
X-Stainless-Runtime-Version: <process.version minus leading 'v'>   // e.g. 22.11.0
X-Claude-Code-Session-Id: <uuid>          // claudeCodeSessionId(token)
x-client-request-id: <crypto.randomUUID()>
```

Fingerprint constants: `src/adapters/client-fingerprint.ts:18-27` (mirrors Claude Code 2.1.63 / SDK 0.74.0). Session id derivation (`client-fingerprint.ts:36-44`): `sha256("claude-code-session:" + token)`, hex; then `h[0:8]-h[8:12]-4<h[13:16]>-<(h[16]&3|8) hex><h[17:20]>-h[20:32]`. Deterministic per credential; the token never leaves the function. Empty/absent token → seed `"opencodex-anon"`.

Then, in order: `provider.headers` merged over everything (`Object.assign`) (`anthropic.ts:1136`, passthrough `:210`), then `mergeAnthropicBetaHeader(headers, extraBetas)` (`src/providers/anthropic-fast.ts:68-88`) which deletes every case-variant `anthropic-beta` key and re-sets one canonical `anthropic-beta` at the end with de-duplicated comma-joined tokens (existing tokens first, then extras).

**Which client headers are forwarded:**
- Native messages lane: forwards the caller's headers verbatim except the strip set — `connection, keep-alive, transfer-encoding, upgrade, te, trailer, proxy-authenticate, proxy-authorization, host, content-length, accept-encoding, x-opencodex-api-key, origin` (`src/server/claude-messages.ts:207-211, 255-261`). `authorization`/`x-api-key` are forwarded only if a single, non-admission-secret token (`sk-ant-` prefix, no comma). `content-type` is force-set to `application/json` (`:591`).
  Exception: the managed native lane (proxy's own credential) sees only the caller's `anthropic-beta`, re-emitted through the beta allowlist; **no other caller header is read** (`passthrough.ts:14-18, 214`).
- Adapter lane (Responses/Chat inbound): **no caller header is forwarded**; headers are constructed from scratch.

HTTP version: Bun `fetch`, default negotiated (h2 over TLS). Explicit pin only if `provider.upstreamHttpVersion` is set, via non-standard `protocol:` init requiring an https target (`src/lib/upstream-http-version.ts:1-40`). No header ordering guarantee beyond insertion order above.

## 3. Request body (adapter lane, Responses/Chat → Messages)

Constructed in `anthropic.ts:986-1160`. Field order as built:

```
{ model, messages, stream, max_tokens, [system], [tools], [temperature], [top_p],
  [stop_sequences], [thinking], [output_config], [tool_choice], [speed], [cache_control] }
```

- `model` = `parsed.modelId` (already wire-resolved upstream of the adapter).
- `stream` = parsed stream flag; `max_tokens` = caller `maxOutputTokens` ?? provider `modelMaxOutputTokens[model]` ?? `defaultMaxOutputTokens` ?? 8192 (`DEFAULT_MAX_TOKENS`).
- `system`: **always an array of text blocks**. OAuth: `[{type:"text",text:"You are a Claude agent, built on Anthropic's Claude Agent SDK."}, ...]` — the identity block MUST be first, or the upstream 400s (`anthropic.ts:994-1001`, constant at `src/oauth/anthropic.ts:16`). Key auth: `[{type:"text",text:<system>}]` only if non-empty. OAuth additionally runs `stripHarnessDocsBlock()` on the caller's system text, deleting every `<docs>…</docs>` block (`src/adapters/anthropic/harness-scrub.ts:19-23`) — otherwise Anthropic answers 400 "Third-party apps now draw from your extra usage".
- Reasoning (`anthropic.ts:1010-1060`), for effort != `none`:
  - Adaptive models (Opus 4.7+/Fable): `thinking: {type:"adaptive", display:"summarized"}` (omit `display` when caller hides summary), `output_config: {effort}` where effort ∈ low|medium|high|xhigh|max (`minimal`→`low`).
  - Budget models: `thinking: {type:"enabled", budget_tokens}` with `budget = clamp(wantBudget, 1024, max_tokens-4096)`, `max_tokens = min(32000, max(maxOut, wantBudget+8192))`. Effort→budget: minimal 1024, low 4096, medium 8192, high 16384, xhigh 24576, max 32000.
  - `temperature` and `top_p` are **deleted** whenever thinking is enabled.
  - `none` + model supports explicit disable → `thinking:{type:"disabled"}`; Sonnet 5.5 → `thinking:{type:"between_tools"}` and sampling params deleted.
- Model contract shims (`anthropic.ts:1064-1073`): models that reject sampling params drop `temperature`+`top_p`; 4.5/4.6 families take only one → `top_p` deleted when both present. Opus 5.5/Sonnet 5.5 reject forced tool choice → `tool_choice:{type:"auto"}` and `tools` narrowed to the named tool.
- `textFormat json_schema` → `output_config.format = {type:"json_schema", schema: normalizeAnthropicOutputSchema(schema)}` (hoists `properties`, forces `type:"object"`, unions `required`, drops `oneOf/anyOf/allOf`) (`anthropic.ts:1075-1084, 1500-1540`).
- `tool_choice` mapping (`anthropic.ts:1086-1100`): auto→`{type:"auto"}`, none→`{type:"none"}`, required→`{type:"any"}`, named→`{type:"tool",name:<wire name>}`. `parallelToolCalls:false` → adds `disable_parallel_tool_use:true` inside `tool_choice` (and materializes `{type:"auto"}` first if none existed) (`:1120-1126`).
- Fast mode: `speed` emitted only when tier decision is a settled `set` matching the provider's `anthropic-speed` wire; default beta `fast-mode-2026-02-01` (`src/providers/anthropic-fast.ts:21, 68`; `anthropic.ts:1132-1134`).
- Prompt caching (`anthropic.ts:73-80, 1138-1150, 150-290`):
  - `cacheRetention` none→no cache_control; short/default→`{type:"ephemeral"}`; long→`{type:"ephemeral", ttl:"1h"}`.
  - First-party host only: `body.cache_control = cc` (top-level automatic caching, follows the moving last block), and explicit breakpoints are capped at 3, skipping the last user message.
  - Explicit breakpoints placed most-stable-first: last tools block, last system block, penultimate user message, last user message. Hard cap 4 (`MAX_CACHE_BREAKPOINTS`); excess stripped from messages → system → tools.
  - TTL ordering normalized: any 1h breakpoint appearing after a 5m one is demoted (delete `ttl`).
- Message conversion `messagesToAnthropicFormat` (`anthropic.ts:719-780`): one tool-call-id allocator for the whole request; `tool_use` ids conform or are rewritten; orphan `tool_result`s are paired; tool names passed through `toWire` (OAuth: `custom_` prefix; `escapeBuiltinToolNames` providers: `cx_`).
- IDs: `tool_use.id` falls back to `toolu_<24 hex>` when missing/blank (`anthropic.ts:409-411, 419-421`).
- Images: normalized/re-encoded to fit Anthropic limits before send, `tierBias` for 413-tightened retries, then a deterministic guard (`anthropic.ts:967-975`).
- No `metadata`, no `service_tier` is ever set by the adapter.

**Native lane body** (`src/adapters/anthropic/passthrough.ts:44-60, 148-217`): caller body cut to exactly
`model, messages, system, max_tokens, metadata, stop_sequences, stream, temperature, top_p, top_k, tools, tool_choice, thinking, output_config, service_tier`; everything else dropped; `model` overwritten with the routed wire model. Then:
- Opaque state (`thinking.signature`, `redacted_thinking`) kept only for first-party Anthropic (`:190-196`).
- OAuth: identity system block prepended (scrub applied), and every caller-declared **client** tool (no `type` or `type:"custom"`) renamed `custom_<name>` in `tools`, named `tool_choice`, and history `tool_use` blocks; collision under the prefix is an error (`:110-160`).
- `anthropic-beta`: caller header only reaches the wire if listed in `beta-allowlist.ts` — first-party allows exactly `interleaved-thinking-2025-05-14`; compatible providers allow none; caller headers > 2048 chars are dropped wholesale (`beta-allowlist.ts:29-42, 60-77`). Proxy-owned betas (OAuth pair, fast mode) are set by the request builder, not the allowlist.
- OAuth credential is refused for any destination that is not first-party Anthropic (`passthrough.ts:196-199`).

## 4. Response

- Streaming: SSE. Parsed by `decodeServerSentEvents(response.body, {includeComments:true})` (`anthropic.ts:1215-1250`). Event dispatch keys on `record.event || data.type`.
  - SSE comment or `event: ping` → `heartbeat`. Data-only `{"type":"ping"}` → `heartbeat`.
  - `message_start` → merge `message.usage` (cumulative), observe `usage.speed`.
  - `content_block_start`: `thinking` → empty `thinking_delta`; `tool_use` → `tool_call_start{id,name}` (name mapped back through `stripClaudeToolPrefix`); `redacted_thinking` → `redacted_thinking{data}`.
  - `content_block_delta`: `text_delta`→`text_delta`; `thinking_delta`/`reasoning_delta`→`thinking_delta`; `signature_delta` (only inside thinking/reasoning block)→`thinking_signature`; `input_json_delta`→`tool_call_delta{arguments}` (fragments forwarded immediately, assembled copy validated at block close; malformed JSON = terminal error).
  - `content_block_stop`: closes a `tool_use` with `tool_call_end`.
  - `message_delta`: merges cumulative usage, later frames win per key; captures `delta.stop_reason`.
  - `message_stop` → terminal. `error` event → `{type:"error", message: error.message}`.
  - EOF without `message_stop`: done only if a `stop_reason` was seen (or, for `anthropicEofTolerance` providers, if visible text or parseable tool JSON was seen); otherwise "upstream stream ended before message_stop — possible truncation".
  - `stop_reason: "error"` → 502 `upstream_error` (never `done`). `stop_reason: refusal|content_filter` → `incomplete{reason:"content_filter", retryable:false}` with usage preserved (`anthropic.ts:313-336`). `max_tokens` stays `done`.
- Non-stream `parseResponse` (`anthropic.ts:1436-1553`): JSON object required (502 otherwise); `content` must be an array; blocks mapped to the same adapter events; `tool_use.input` serialized as JSON arguments.
- Client-protocol mapping: adapter events (`text_delta`, `thinking_delta`, `thinking_signature`, `redacted_thinking`, `tool_call_start/delta/end`, `heartbeat`, `done`, `incomplete`, `error`) are bridged to OpenAI Responses SSE / Chat SSE / Anthropic SSE by the bridge layer; the adapter itself is protocol-agnostic.
- Usage (`anthropic.ts:609-638`): reported `input_tokens` **excludes** cache read/write, so opencodex normalizes to inclusive: `inputTokens = input_tokens + cache_read_input_tokens + cache_creation_input_tokens`; `outputTokens = output_tokens`; when either cache field is present also emit `cachedInputTokens` (= reads only), `cacheReadInputTokens`, `cacheCreationInputTokens`. A non-numeric/negative token value makes the whole usage unreported (`undefined`), not zero.
- Fast-mode echo: `usage.speed` ("fast"|"standard") is observed into tier metadata.

## 5. Auth

- Credential type: OAuth 2.0 + PKCE (S256), Claude subscription. Client id `9d1c2510-e61b-44d9-88ed-5944d1962f5e` (stored base64 in source) (`src/oauth/anthropic.ts:7`).
- Wire credential: bearer access token in `Authorization`; refresh token and expiry persisted by the credential store. `expires` = now + `expires_in`(s)*1000 − 5 min; missing/invalid `expires_in`→3600 (`anthropic.ts:88-110`). Account uuid/email captured from token response `account.uuid` / `account.email_address`.
- Refresh: `POST https://api.anthropic.com/v1/oauth/token`, `Content-Type: application/json`, body `{grant_type:"refresh_token", client_id, refresh_token}`, 30s timeout (`oauth/anthropic.ts:44-56, 186-192`). Triggered lazily when `expires <= now + REFRESH_SKEW_MS`, or immediately after a 401 replay (`oauth/index.ts:549-550, 584`; `adapter-dispatch.ts:683-690`). Anthropic's `defaultRefreshPolicy: "disabled"` — **no background refresh traffic**, refresh only on user action or a 401 (`oauth/index.ts:261-269`).
- Terminal refresh failures: HTTP 400/401 with `oauth_error` in {`invalid_grant`,`refresh_token_reused`,`revoked`,`revoked_token`,`refresh_token_revoked`} → mark account `needsReauth` (`oauth/index.ts:699`).
- Multi-account selection: `oauth/anthropic-routing.ts` — session affinity (24h TTL, ≤2000 entries), quota-ranked new-session selection, `autoSwitchThreshold` via `effectiveAnthropicAccountThreshold`, paused/`needsReauth` accounts excluded, reactive 429 failover bounded to `ANTHROPIC_POOL_MAX_FAILOVERS_PER_REQUEST = 3` (`anthropic-routing.ts:64`).
- **No Anthropic account-id / organization-id header is ever sent.** The account identity lives only in the bearer token. `X-Claude-Code-Session-Id` is a per-credential hash, not an account id.
- Quota probe (same credential, different endpoint): `GET https://api.anthropic.com/api/oauth/usage` with `Authorization: Bearer`, `Accept: application/json, text/plain, */*`, `Content-Type: application/json`, `User-Agent: claude-cli/2.1.280 (external, cli)`, and beta `claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,context-management-2025-06-27,prompt-caching-scope-2026-01-05` (`src/providers/quota/vendor-probes-oauth.ts:269-280`; UA at `src/providers/claude-cli-identity.ts:9-12`).

## 6. Errors

- Classification happens in the dispatch layer, not the adapter (`src/server/responses/adapter-dispatch.ts`):
  - 401 → one OAuth replay after refreshing the credential (only for `isOAuth401ReplayProvider`; guarded by `!oauth401ReplayAttempted`); if recovery still fails → `401 authentication_error` (`adapter-dispatch.ts:683-700, 613`).
  - 403 → `permission_error` only when it is the local `OAuthAccountPausedError`; otherwise a passthrough of the upstream status with message `Provider error <status>: …` and `Retry-After` resolved by `resolveClientRetryAfter` (`:396-398, 1390-1440`).
  - 429 → handled in this order: same-target wait-and-retry (only for key auth with `retryOn429`), multi-key pool rotation, then Anthropic OAuth account pool rotation (max 3 per request), then generic OAuth failover (kiro only). After the failover cap, `recordAnthropicAccount429` still records the cooldown (`:893-1035`).
  - 5xx → `Provider error <status>: …` with `upstream_error`; transient retry only for opted-in key-auth openai-chat/responses adapters — **anthropic OAuth takes no transient 5xx retry** (`key-failover.ts:617-635`).
  - Connect failure → `502 upstream_error`.
- Cooldown (`src/oauth/anthropic-routing.ts:905-945`), recorded on 429 and on local cooldown refusals:
  1. `Retry-After` parsed first: integer/decimal seconds (`ceil(s*1000)`) or an HTTP-date; ≤0 or unparseable → ignored (`:159-168`).
  2. Else `anthropic-ratelimit-unified-5h-status` / `-7d-status` equal to `rejected` → use `anthropic-ratelimit-unified-<w>-reset` (epoch **seconds**) and take the LATEST reset among rejected windows (`:170-190`).
  3. Else `DEFAULT_COOLDOWN_MS = 60_000`.
  Cooldown source recorded as `retry-after` | `reset-derived` | `default`; session affinity for that account cleared; a monotonic cooldown generation fences in-flight quota probes.
- When every eligible account is cooling: `AnthropicAccountCooldownError` → client `429 rate_limit_error` with `Retry-After: <seconds>` when known (`adapter-dispatch.ts:399-401`). Paused/finished turns after a pause still allow one reactive successor.
- Provider-specific suspension: accounts can be operator-`paused` or `needsReauth`; both are excluded from routing, and a paused active account with no survivor throws `OAuthAccountPausedError` → 403.
- Error body extraction: `formatAnthropicErrorBody` parses JSON only, extracts the message, redacts secrets, caps at 400 chars; non-JSON → empty detail (`anthropic.ts:297-307`).

## 7. Login flow (CLI)

Browser OAuth with PKCE + local callback, with a paste fallback (no device code, no API key).

1. `ocx login anthropic` → `loginAnthropic` (`src/oauth/anthropic.ts:152-185`). By default `importLocal: "fallback"`: first tries to import a local Claude Code token (macOS Keychain or `~/.claude/.credentials.json`) via `detectClaudeCodeToken()`; a valid unexpired token short-circuits login, an expired one is refreshed; with `--force-login` import is off (`oauth/index.ts:262`).
2. Callback server on `localhost:54545`, path `/callback` (binds `127.0.0.1` and best-effort `::1` when advertising `localhost`); 300s timeout; CSRF `state`; manual paste races the server (`src/oauth/callback-server.ts:1-20, 134-160`).
3. Authorize: `GET https://claude.ai/oauth/authorize?code=true&client_id=…&response_type=code&redirect_uri=http://localhost:54545/callback&scope=org:create_api_key user:profile user:inference&code_challenge=…&code_challenge_method=S256&state=…` (`oauth/anthropic.ts:112-135`). Instructions tell the user they may paste the final redirect URL or authorization code instead.
4. Exchange: `POST https://api.anthropic.com/v1/oauth/token`, JSON `{grant_type:"authorization_code", client_id, code, state, redirect_uri, code_verifier}`. A pasted `code#state` is split on `#` (`:137-150`). Response fields: `access_token`, `refresh_token`, `expires_in`, `account.{uuid,email_address}`.
5. No `/v1/oauth/profile` or device-code endpoint is used by opencodex (the profile endpoint is mentioned only in a comment about the reset-grant claim).

---

## Quick reproduction checklist for a Rust gateway

1. `POST https://api.anthropic.com/v1/messages`.
2. Headers (this order): `Content-Type: application/json`, `anthropic-version: 2023-06-01`, `Accept: text/event-stream|application/json`, `User-Agent: @anthropic-ai/sdk/0.74.0`, `Authorization: Bearer <token>`, `anthropic-beta: claude-code-20250219,oauth-2025-04-20`, the nine Claude Code `X-App`/`X-Stainless-*` headers, `X-Claude-Code-Session-Id: <sha256-derived uuid>`, `x-client-request-id: <uuid>`.
3. Body: `model`, `messages`, `stream`, `max_tokens`, `system` (array, identity block first for OAuth), optional `tools`/`temperature`/`top_p`/`stop_sequences`/`thinking`/`output_config`/`tool_choice`/`speed`/`cache_control`.
4. Prefix every client tool name with `custom_` (builtins `web_search`, `code_execution`, `text_editor`, `computer` exempt) and strip on the way back.
5. Scrub `<docs>…</docs>` from the caller's system text.
6. Parse SSE by `event || data.type`; treat `ping`/comments as heartbeats; `message_delta.usage` is cumulative; normalize usage as inclusive input.
7. 429: `Retry-After` → `anthropic-ratelimit-unified-{5h,7d}-reset` (latest rejected) → 60s.


## Unknowns
- Exact HTTP version on the wire: Bun fetch negotiates (h2 over TLS in practice), but nothing in source pins or asserts it. Pinning is only available through provider.upstreamHttpVersion.
- Header ORDER is only reproducible as object insertion order in JS (`anthropic.ts:535-564` then provider.headers then re-set anthropic-beta). Bun's fetch/Headers may reorder or normalize on serialization (e.g. lowercase in h2); no source guarantees the byte order.
- Whether Anthropic requires `anthropic-version` to also be sent on the OAuth token endpoint — opencodex does not send it there.
- There is no `metadata.user_id` and no `service_tier` in the adapter lane; the native lane forwards `service_tier` only if the caller sent it. Not verified whether upstream treats their absence as a third-party signal.
- The exact `X-Stainless-Arch`/`-OS` values depend on the host process (`process.arch`/`process.platform`), so a Rust gateway must choose the same strings (e.g. `x64` / `linux`).
- Anthropic's 529/`overloaded_error` has no special-casing in the Anthropic adapter path — it falls into the generic `Provider error <status>` branch. Not verified whether any upstream retry helper treats 529 specially.
- `output_config.effort` accepted range is documented in a comment as low|medium|high|xhigh|max; upstream rejection of other values was not re-verified in this pass.
- No account-id header exists, so multi-account routing is invisible on the wire; if upstream binds an OAuth token to an organization, opencodex does not model it.
- The `claude-cli/2.1.280` UA is used only for the `/api/oauth/usage` probe, not for `/v1/messages` (which still uses `@anthropic-ai/sdk/0.74.0`); which one upstream actually trusts for Messages was not verified.
- Whether `cch` billing-header signing is required for sustained acceptance: explicitly declared out of scope by opencodex (`client-fingerprint.ts:5-10`).