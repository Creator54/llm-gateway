# google-antigravity upstream wire spec (opencodex)

Provider id `google-antigravity`, alias `agy`, adapter `google`, `authKind: oauth`,
`googleMode: "cloud-code-assist"`.
Registry: `providers/registry/entries-core.ts:1182`.

## 1. Endpoint

| item | value | ref |
|---|---|---|
| base URL (default) | `https://daily-cloudcode-pa.googleapis.com` | entries-core.ts:1182 |
| override | allowed (`allowBaseUrlOverride: true`); must be non-empty or build throws | google.ts:1034-1035 |
| URL template | `${baseUrl}/v1internal:${method}${streamParam}` | google.ts:1036 |
| `method` | `streamGenerateContent` when `parsed.stream`, else `generateContent` | google.ts:1025 |
| `streamParam` | `?alt=sse` when streaming, `""` when not | google.ts:1026 |
| HTTP verb | `POST` | google.ts:1132 |
| other RPCs | `:fetchAvailableModels` (model discovery) | oauth/index.ts:1252; registry `modelDiscovery.path` |
| | `:loadCodeAssist`, `:onboardUser` (login/project discovery) | oauth/google-antigravity.ts:98,111 |
| | `:retrieveUserQuotaSummary`, `:fetchAvailableModels` (quota, always `daily-` host) | providers/quota/antigravity.ts:167-168 |

No region/location parameter exists on this path — no `locations/…` segment (contrast Vertex
at google.ts:1168). `project` is a **body** field, not a URL path element.

## 2. Request headers

Built at `adapters/google.ts:1027-1132`. Insertion order is the object literal order:

1. `Content-Type: application/json` (google.ts:1027)
2. any `provider.headers` entries (user config), overriding #1 if duplicated (google.ts:1028)
3. `User-Agent: <antigravityUserAgent()>` (google.ts:1130, constant at google.ts-antigravity-wire.ts:14)
4. `Authorization: Bearer <accessToken>` (google.ts:1131; token from `provider.apiKey`)

Nothing else. **No** `Accept`, no `anthropic-*`, no `x-goog-api-client`, no `x-goog-user-project`,
no client/Codex/Claude headers forwarded — inbound client headers are not forwarded at all on
this path. (`CLAUDE_CODE_HEADERS` in `adapters/client-fingerprint.ts` is for the Anthropic
adapter, not used here.) The only forwarded header is the bearer token; `provider.headers` is an
operator escape hatch.

User-Agent value (`adapters/client-fingerprint.ts:45-65`):

```
antigravity/ide/2.5.5 (os_type=windows; arch=amd64; aidev_client; auth_method=oauth)
```

- version `ANTIGRAVITY_IDE_VERSION = "2.5.5"` (hardcoded, client-fingerprint.ts:45)
- platform hardcoded `"windows/amd64"` (client-fingerprint.ts:47) — same on every host OS
- token order fixed: `os_type` → `arch` → `aidev_client` → `auth_method` (decompiled from LS 2.5.5)
- env override `GOOGLE_ANTIGRAVITY_USER_AGENT` wins verbatim (client-fingerprint.ts:61)
- Why it matters: CCA 404s CLI-shaped UAs for `gemini-3.7-*`+ (google-antigravity-wire.ts:7-12)

HTTP version: not pinned for this provider. Default global fetch transport unless the operator
sets `provider.upstreamHttpVersion` (types/provider.ts:480, lib/upstream-http-version.ts:4-56).
No ALPN/h2 pinning anywhere in the Antigravity path.

## 3. Request body

Flat Gemini body (`contents`, `systemInstruction`, `tools`, `toolConfig`, `generationConfig`),
then wrapped in the CCA envelope. Assembly order: google.ts:969-986 (body), 1043-1103 (CCA
rewrites), 1119-1128 (envelope).

### Envelope (exact key order)

```json
{
  "model": "<wireModelId>",
  "userAgent": "antigravity",
  "requestType": "agent",
  "project": "<discovered CCA project id>",
  "requestId": "agent-<crypto.randomUUID()>",
  "request": { ...gemini body..., "sessionId": "<derived>" }
}
```
google.ts:1119-1128. Notes:
- `userAgent` body field is the literal `"antigravity"`, NOT the HTTP UA (google.ts:1121-1123).
- `requestId` is `agent-` + random UUIDv4 **per request** (google.ts:1125).
- `project` comes from `provider.project` (credential `projectId`); missing → throw
  "Antigravity requires a discovered Cloud Code Assist project id" (google.ts:1039-1040).
  Injected from credential at server/responses/adapter-dispatch.ts:766,
  request-transport.ts:291/722.
- `sessionId` is **nested only** inside `request` (camelCase, single canonical location;
  no top-level or snake_case copy) — google.ts:1075.

### sessionId derivation (google-antigravity-wire.ts:62-88)

```
anchor = "codex-thread:{parent}\0{own}" | "codex-thread:{parent}" | firstUserMessageText
if no anchor            -> "-" + random int (< 9e18)
else                    -> "-" + (be64(sha256(anchor)[0..8]) & 0x7FFFFFFFFFFFFFFF)
```
`parent` = `x-codex-parent-thread-id` (`_clientThreadId`), `own` = `_codexOwnThreadId`.
First-user-text fallback prefers a text or document part. Deterministic on purpose: replay
cache keys signatures by model+session, so the id must be stable across turns.

### Body construction / field mapping (google.ts:330-557)

- `systemInstruction = { parts: [{ text: systemText }] }` (google.ts:346)
- systemText = join("\n\n") of: inbound system prompt paragraphs, `buildNonOpenAIToolCatalogNudgeForTools(...)`,
  `GOOGLE_BREVITY_INSTRUCTION` (google.ts:44-60, 336-343) → then `identifyRoutedModel(...)`
  rewrites Codex "you are GPT-5" identity lines into a routed-model sentence
  (adapters/identity.ts:76-96).
- **CCA-only rewrites of systemText:**
  - `stripAntigravityBillingHeader` — deletes a leading `x-anthropic-billing-header:...` line
    (regex `^x-anthropic-billing-header:[^\n]*\n*`, no `/m`) (google.ts:87-89). CCA 429s if
    Anthropic billing metadata reaches systemInstruction.
  - `stripAntigravityRejectedClaudeSdkParagraph` — drops any paragraph equal to
    `"You are a Claude agent, built on Anthropic's Claude Agent SDK."` (google.ts:61-75).
    Exact-string match on `\n\n`-split paragraphs. CCA returns 429 RESOURCE_EXHAUSTED while
    this paragraph is present, for all CCA models.
- `contents[]`: user/assistant→`role:"user"` / `role:"model"`.
  - text → `{text}`; empty/malformed text dropped (`geminiTextPart`).
  - user turn with zero parts → `[{ text: GEMINI_EMPTY_PLACEHOLDER }]` (google.ts:418).
  - image: data URL → `{inline_data:{mime_type,data}}`; remote URL → `{text:"[image: <url>]"}`
    (google.ts:376).
  - video: data URL → `inline_data`; YouTube/Files-API URI → `file_data:{file_uri}`;
    other remote → `{text:"[video: <url>]"}`; optional `media_processing` rides on the part
    (google.ts:382-407).
  - document → `inline_data` (google.ts:414).
  - assistant tool call → `{functionCall:{name,args,id}}` + `thoughtSignature` when a real
    signature exists (google.ts:449-466).
  - `cache_control` / Anthropic prompt-cache markers: no handling at all in this adapter —
    dropped with the rest of the inbound part shape. No `cachedContent` field is ever sent.
- Tool call ids (`google.ts:159-170`, `createToolCallIdAllocator`): if the inbound id is usable
  it is reused; otherwise a stable id is allocated. A tool call with no usable id is downgraded
  to a text part `[tool_use without a usable id: <name>]` (google.ts:313-316) — Claude-on-CCA
  requires `functionCall.id` to map to `tool_use.id`.
- tool result → `{functionResponse:{name, response:{result}, id}}` (google.ts:288-305).
  Missing result → `response:{result: GEMINI_MISSING_TOOL_RESULT}`. Orphan result → text part
  `[tool_result without adjacent tool_use: ...]` (google.ts:318-325).
- contents[0] must not be a model turn opening with a `functionCall` — guarded at google.ts:519-528.
- `tools` = `[{functionDeclarations:[{name, description, parameters}]}]` (google.ts:546-557),
  filtered by allowed-tools tool_choice.
- `toolConfig.functionCallingConfig.mode`: `auto`→absent, `none`→`NONE`, `required`→`ANY`,
  allowed-tools→`ANY` + names (google.ts:586-592). Strict tools → `VALIDATED`.
- `generationConfig` (google.ts:985-1023), then recompiled/clamped by
  `compileGenerationConfig` (google-wire-compiler.ts:152-182):
  - `maxOutputTokens` = `clampGoogleMaxOutputTokens(identityModelId, requested)`
    — Gemini pro 65535, other Gemini 65536, claude 64000; unknown id passes through
    (google.ts:106-126).
  - `temperature` clamped to ≤2, `topP` ≤1, `stopSequences` deduped, max 5 entries.
  - `thinkingConfig.thinkingLevel` in {low, medium, high} (`xhigh|max|ultra`→`high`);
    `thinkingConfig.includeThoughts: true` only when `provider.showThinkingSummary` and the
    wire model matches `^gemini-` and is not image-capable (google.ts:1051-1084).
  - `responseMimeType:"application/json"` + `responseJsonSchema` for structured output.
    **Refused locally for non-Gemini CCA models** (google.ts:926-931).
  - image-capable models (explicit allowlist of 3) → `responseModalities:["TEXT","IMAGE"]`
    (google.ts:622-630, 1006-1008).
- **Claude-on-CCA forced `VALIDATED`** (google.ts:1080-1093): if wireModelId matches `/claude/i`,
  `toolConfig.functionCallingConfig.mode` is forced to `"VALIDATED"`; if the client asked
  `tool_choice:"none"`, `tools` and `toolConfig` are deleted instead.

### Wire compiler (google-wire-compiler.ts:217-246)

Rebuilds the body into a fixed key order: `contents`, `systemInstruction`, `tools`,
`generationConfig`, `toolConfig`, `sessionId` (only if non-empty). Tool names not matching
`GOOGLE_TOOL_NAME` are rewritten to `<clean>_<sha256(name)[0..8]>` (google-wire-compiler.ts:40-76).

### thoughtSignature handling

Two paths, both before send (google.ts:1095-1108):
- Gemini family → `applyAntigravityReplay(model, session, contents)` from the replay cache.
- Claude family → `sanitizeAntigravityClaudeSignatures(contents)`: strip signature fields from
  all non-`model` parts; drop `thought:true` parts with no valid signature
  (google-antigravity-wire.ts:196-215).
- Then `applyAntigravityThoughtSignatureFallback(...)` — the sentinel
  `"skip_thought_signature_validator"` may be placed on a first functionCall that replay could
  not sign.
- Only "real" signatures are ever forwarded (`isLikelyRealThoughtSignature`,
  google-antigravity-wire.ts:29-40): length ≥16, not the sentinel, not matching
  `^(fc|ctc|tsc|call|msg|rs|resp|reasoning|item|ws|toolu|tool|func|function)[-_]`, and
  matching `^[A-Za-z0-9+/_=-]+$`.
- Replay cache: `REPLAY_TTL_MS = 1h`, max 10,240 entries, 256 calls / 2MB per session, 64MB total
  (google-antigravity-replay.ts). Keyed by model+sessionId+functionCall identity (name+args).

## 4. Response

Streaming = SSE (`?alt=sse`), `data:` lines, one JSON object per frame, no `[DONE]` sentinel
(termination is `finishReason` or a usage-only frame). Parsed by
`parseStream` at google.ts:1186-1470.

CCA envelope unwrap (google.ts:1249-1262): each frame is `{"response": {<gemini payload>}}`.
Missing/non-object `response` → terminal error
`"google-antigravity response missing response wrapper"`.

Field handling per frame:
- `response.usageMetadata` read **before** the candidates guard so usage-only frames are not lost
  (google.ts:1266-1275). Accumulated; single terminal `done` emitted post-loop.
- `candidates` undefined/null/`[]` → continue; non-array or non-object candidate → terminal
  `invalidGoogleShapeEvent` (google.ts:1278-1299).
- `candidate.finishReason` → last finish reason + terminal signal.
- `content.parts[]` → for each part: `thought:true` + real signature captured as pending sig;
  text → `text_delta` (only for `^gemini-` non-image when `returnsThoughtSummaries`); `inlineData`
  → image written to disk + `\n![image](<url>)\n` text_delta (per-image encoded cap, 100MiB total
  body cap); `functionCall` → `tool_call_start{id:"call_"+uuid8, name:restoredName}` +
  `tool_call_delta{arguments:JSON.stringify(args ?? {})}` + `tool_call_end` (google.ts:1345-1385).
- Inline `{"error":{...}}` inside a 200 stream → terminal error, and
  `handleSignatureRejection(message)` (google.ts:1234-1241) — clears the session replay cache on
  any invalid-argument/signature error, and evicts durable-store entries when the text mentions
  `signature`.
- Buffer/frame safety: 100 MiB per data frame (`MAX_SSE_FRAME_BYTES`), unterminated frame →
  error; stream ending without a terminal signal → `"upstream stream ended without a terminal
  signal — possible truncation"`.
- Truncated turn (`MAX_TOKENS`/`MALFORMED_FUNCTION_CALL` mid tool call) → error, not `done`.

Non-streaming (`generateContent`): `parseResponse` at google.ts:1475-1681, full body buffered
with a 100 MiB cap, `raw.response` unwrapped (google.ts:1560-1575), identical part mapping, then
`done` with `usage` and `stopReason`.

`stopReason` mapping: `MAX_TOKENS`→`max_tokens`; `SAFETY|RECITATION|BLOCKLIST|PROHIBITED_CONTENT|SPII`
→`content_filter` (google.ts:1459-1464, 1673-1680).

Usage mapping (`usageFromGemini`, google.ts:599-607):
```
inputTokens         = usageMetadata.promptTokenCount ?? 0
outputTokens        = usageMetadata.candidatesTokenCount ?? 0
cachedInputTokens   = usageMetadata.cachedContentTokenCount   (only if present)
reasoningOutputTokens = usageMetadata.thoughtsTokenCount      (only if present)
```
No `totalTokenCount`, no `toolUsePromptTokenCount`, no `promptTokensDetails` — nothing else is read.

Mapping to each client protocol is done downstream by the generic bridge from these
`AdapterEvent`s (`text_delta` / `tool_call_start|delta|end` / `done{usage,stopReason}` / `error`).
No protocol-specific fields are invented inside the Google adapter.

## 5. Auth

Credential type: OAuth2 refresh token, Google account. Stored: `refresh`, `access`,
`expires = now + expires_in*1000 - 5min`, `email`, `projectId`, `accountId`
(oauth/google-antigravity.ts:24-38, 138-160).

Pinned public client (env-overridable, not user secrets) — google-antigravity.ts:18-21:
`client_id` / `client_secret` are the Antigravity public OAuth client.

Token refresh — `refreshAntigravityToken` (google-antigravity.ts:224-236):
```
POST https://oauth2.googleapis.com/token
Content-Type: application/x-www-form-urlencoded
Accept: application/json
body: grant_type=refresh_token&client_id=…&client_secret=…&refresh_token=…
timeout 30s
```
After refresh it re-runs project discovery (`loadCodeAssist` → `onboardUser`) and merges
`projectId`. Refresh trigger: `expires` already includes a 5-minute skew
(`REFRESH_SKEW_MS`), plus the shared OAuth freshness gate (google-antigravity.ts:37-38 comment).

Per-request: `Authorization: Bearer <access>`; halo headers none. No account-id header is sent
upstream. Multi-account selection is entirely local: `oauth/generic-account-failover.ts`,
`oauth/account-quota-rank.ts`, `oauth/pool-kernel.ts`.

Model discovery request (oauth/index.ts:1246-1256): POST to
`${baseUrl}/v1internal:fetchAvailableModels`, headers
`Accept: application/json`, `Content-Type: application/json`, `User-Agent: <antigravity UA>`,
`Authorization: Bearer <token>`, body `{"project":"<projectId>"}`
(server/management/provider-routes.ts:1733-1751).

## 6. Errors

Detection/classification — `adapters/google-errors.ts`:

- Retryable statuses: `429, 500, 502, 503, 504` (google-errors.ts:137-140).
  HTTP-400 repair-replay path exists separately (google-http.ts:99-118).
- 429 split: body peeked; if `isQuotaExhaustedBody` → **do not retry**
  (google-http.ts:128-140). Quota-exhausted requires status RESOURCE_EXHAUSTED (or absent) and a
  needle from `GOOGLE_QUOTA_EXHAUSTED_NEEDLES` = `quotafailure, quota exceeded, exceeded your
  current quota, billing, individual quota reached, quota reached, enable overages, exhausted
  your capacity, daily limit reached, weekly limit reached` (google-errors.ts:37-47). Transient
  rate-limit patterns (`per minute, per-minute, per min, rpm, requests per minute, too many
  requests, rate limit, retry after, retry-after, concurrent request limit`) short-circuit to
  retryable *before* the needles (google-errors.ts:52-67).
- Classification strings (`classifyGoogle`, google-errors.ts:70-105), all prefixed `Antigravity `:
  `quota exhausted`, `rate limit exceeded` (429 / RESOURCE_EXHAUSTED), `authentication failed`
  (401 / UNAUTHENTICATED / "expired"), `access denied (PERMISSION_DENIED)`, `access denied` (403),
  `location not supported` (400 + FAILED_PRECONDITION + location text), `server overloaded`
  (503 / UNAVAILABLE), `invalid request` (400/404 / INVALID_ARGUMENT / NOT_FOUND), `upstream error`.
- Final error text = `<classified>: <sanitized upstream message, ≤500 chars>`; empty → `HTTP <status>`
  (google-errors.ts:112-121).
- **Antigravity 403 + `error.details[].reason == "VALIDATION_REQUIRED"`** → prefix becomes
  `Antigravity account validation required (VALIDATION_REQUIRED)`
  (google-errors.ts:6-20, 113-115). This is the provider-specific suspension path: the 403 body is
  read bounded (4096B) and content-encoding/content-length are stripped before
  re-materializing the response (google-http.ts:31-45); the dispatcher then rotates to a sibling
  account once (`isAntigravityValidationRefusal`, server/responses/antigravity-validation-refusal.ts:5-19;
  rotation at server/responses/adapter-dispatch.ts:800-808).

Retry/backoff (`fetchGoogleWithRetry`, google-http.ts:70-186):
- `TRANSIENT_RETRY_MAX_ATTEMPTS = 3` (lib/upstream-retry.ts:157).
- backoff = `min(250ms * 2^attempt, 2000ms)` with ±20% jitter
  (`GOOGLE_RETRY_BASE_MS=250`, `GOOGLE_RETRY_MAX_MS=2000`, google-http.ts:26-27;
  lib/upstream-retry.ts:398-414).
- `Retry-After` parsed as delta-seconds or HTTP-date; when present the delay is
  `min(retryAfter, 2000ms)` — **the local 2s cap wins over the server's instruction**
  (lib/upstream-retry.ts:388-406). Immediately-following retries therefore never wait longer
  than 2s.
- `retryAfterIsLowerBound` is NOT set for Google (default behavior above).
- Per-attempt timeout: `ctx.timeoutMs ?? 200_000` (google-http.ts:74).
- Connection resets (`ECONNRESET`/`EPIPE`/"socket connection was closed unexpectedly") are retried
  with the same ladder (google-http.ts:162-176).
- Non-retryable or final attempt → response normalized through `normalizeFinalGoogleError`.
- 400 handling: `repairGoogleInvalidRequestBodyWithReport` re-compiles the body once and replays
  (attempt not counted); `reject-lossy` policy aborts the replay (google-http.ts:99-118).

Account cooldowns (`oauth/generic-account-failover.ts`):
- Antigravity failover trigger: **429 only** (`isRetriableFailoverStatus` returns
  `status === 429` for `google-antigravity`; 401/403 are handled by the separate auth-refusal path)
  — generic-account-failover.ts:53-57.
- `GENERIC_OAUTH_MAX_FAILOVERS_PER_REQUEST = 3`, `GENERIC_OAUTH_MAX_ACCOUNTS_PER_REQUEST = 6`
  (:59, :67).
- Cooldown = `parseRetryAfterMs(header)` if present, else `exhaustedCooldownMs(...)` (reset-aligned)
  if quota is provably spent, else `DEFAULT_COOLDOWN_MS = 60_000` (:69, 494-509).
- Auth-refusal (401/403) cooldown: 60s, source `"auth"`, recorded only when the credential
  generation is unchanged; rotation is one sibling, ring-ordered from the failed account, and only
  when the pool was already activated (:314-336).
- `kind === "suspended"` → flat `24h` cooldown (:507).

Quota probe (providers/quota/antigravity.ts): POST `{project}` to
`daily-cloudcode-pa.googleapis.com/v1internal:retrieveUserQuotaSummary`; on 403 retries once with
`User-Agent: antigravity/1.0`; 401/403 → `access_denied`; fallback
`:fetchAvailableModels` → Gem/Gem(Weekly)/Cla/Cla(Weekly) windows (168-293). Parsed from
`groups[].buckets[].resetTime` (98-160). Redirects are always a hard failure (`redirect_blocked`).

## 7. CLI login flow

`ocx login google-antigravity` → `loginAntigravity` (oauth/index.ts:310-315) →
`AntigravityOAuthFlow extends OAuthCallbackFlow` (google-antigravity.ts:162-215).

**Authorization-code + PKCE, local loopback callback, with paste fallback. Not a device code.**

1. `generatePKCE()` (S256). Redirect URI fixed to
   `http://127.0.0.1:51121/callback` (both `preferredPort` and `redirectUri`).
2. Redirect user to:
   `https://accounts.google.com/o/oauth2/v2/auth?response_type=code&client_id=…&redirect_uri=…&scope=<5 scopes space-joined>&code_challenge=…&code_challenge_method=S256&access_type=offline&prompt=<consent|consent select_account>&state=…`
   Scopes: `cloud-platform`, `userinfo.email`, `userinfo.profile`, `cclog`,
   `experimentsandconfigs`. `prompt="consent select_account"` when adding a second account.
   Instructions shown: "paste the redirect URL or code if prompted."
3. Callback server on `127.0.0.1:51121` (bounded 300s). Concurrent manual-input race accepts a
   pasted redirect URL, a `code#state` string, or a raw code (raw code without state is exempt
   from the state check — same PKCE session) (oauth/callback-server.ts:245-267).
4. Exchange at `https://oauth2.googleapis.com/token` (form-urlencoded):
   `grant_type=authorization_code, client_id, client_secret, code, redirect_uri, code_verifier`.
5. Project discovery, in order:
   - `POST https://cloudcode-pa.googleapis.com/v1internal:loadCodeAssist`
     headers `Authorization: Bearer`, `Accept: */*`, `Content-Type: application/json`,
     `User-Agent: <antigravity UA>`; body `{"metadata":{"ideType":"ANTIGRAVITY"}}` (google-antigravity.ts:97-105).
     Project extracted from `cloudaicompanionProject` | `projectId` | `project` (string or `.id`).
   - fallback `POST https://daily-cloudcode-pa.googleapis.com/v1internal:onboardUser`, same headers,
     body `{"tier_id":"free-tier","metadata":{"ide_type":"ANTIGRAVITY","ide_name":"antigravity","ide_version":"2.5.5"}}`
     (note snake_case body keys here vs camelCase `ideType` in loadCodeAssist).
     Polls up to 5 attempts, 2s apart; retries 429/5xx; hard 4xx aborts; success when `done === true`,
     project from `response`. (google-antigravity.ts:108-135)
6. If no project is discovered, login **fails** rather than persisting a credential that every
   request would reject (google-antigravity.ts:212-217).

Account import (`ocx` account import) reuses the same refresh + `GET
https://www.googleapis.com/oauth2/v2/userinfo` to derive `{email, id}`; the imported email must
match the record's email and `projectId` must be present, else `missing_project`
(oauth/google-antigravity.ts:243-261, oauth/account-import/google-antigravity-adapter.ts:47-57).

## Minimal byte-reproduction checklist

```
POST {baseUrl}/v1internal:streamGenerateContent?alt=sse
Content-Type: application/json
User-Agent: antigravity/ide/2.5.5 (os_type=windows; arch=amd64; aidev_client; auth_method=oauth)
Authorization: Bearer <access token>

{"model":"<wire id>","userAgent":"antigravity","requestType":"agent","project":"<project>","requestId":"agent-<uuid4>","request":{"contents":[…],"systemInstruction":{"parts":[{"text":"…"}]},"tools":[…],"generationConfig":{…},"toolConfig":{…},"sessionId":"-<int64>"}}
```
Plus: strip the Claude-SDK paragraph, strip a leading `x-anthropic-billing-header:` line, force
`VALIDATED` for Claude models, never forward foreign tool-call ids as `thoughtSignature`, and
`response`-unwrap every SSE frame.


## Unknowns
- Exact HTTP version actually negotiated: opencode uses the default fetch (Bun/undici) transport with no pin for this provider; whether Google terminates h2 vs h1.1 on this endpoint is not asserted anywhere in the source.
- Whether CCA expects/validates any header not sent here (e.g. x-goog-api-client, x-activity-request-id). Source sends none beyond the four listed; no test asserts their absence against a live upstream.
- Header byte order on the wire: object insertion order is Content-Type, provider.headers, User-Agent, Authorization, but whether the HTTP client preserves that order in the serialized request is not verified in source.
- Whether `requestId` format/composition (`agent-<uuid4>`) is required or merely fingerprint-mimicking — the source calls it a protocol constant but has no test against the real client's bytes.
- Whether CCA mirrors `mm`/`requestType` values other than "agent" (e.g. for background or non-agent turns); only "agent" appears.
- Exact upstream reset-time semantic of retrieveUserQuotaSummary buckets beyond `resetTime` parsing (normalizeResetAt) — not traced in detail.
- Whether the 2s Retry-After cap causes real-world repeated 429s on Antigravity; source comments acknowledge the tradeoff but there is no live measurement.
- Whether `sessionId` must be stable per conversation on the real client or is purely local replay bookkeeping — source explicitly says it only verifies the local association, not Google's semantics (google-antigravity-wire.ts:78-79).