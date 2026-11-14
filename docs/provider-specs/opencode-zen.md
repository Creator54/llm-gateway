# opencode-zen upstream wire spec (as implemented by opencodex)

Repo: `~/opencodex`. All paths relative to repo root. Provider is **not** a bespoke transport: it is
the generic `openai-chat` adapter pointed at OpenCode Zen. Everything below is what that path emits.

---

## 0. Provider definition

`src/providers/registry/entries-extended.ts:1091`
```ts
id: "opencode-zen", label: "opencode zen",
baseUrl: "https://opencode.ai/zen/v1",
adapter: "openai-chat", authKind: "key",
dashboardUrl: "https://opencode.ai/auth",
```
- No `models:` list and no `liveModels` in the registry entry → models come from live `/models`
  discovery (`src/providers/free-directory.ts:64-75` `openAi()` helper sets
  `discovery: "live", liveModels: true`). `derive.ts:229` + `static-model-discovery.ts:36`
  (`registryEntrySupportsLiveModelDiscovery` = "not in STATIC_MODEL_CATALOG_PROVIDER_IDS").
- No `staticHeaders` on `opencode-zen` (contrast `opencode-free` at `:1202-1214`).
- No `upstreamHttpVersion` and no `chatCompletionsPath` on this entry.
- Zen-specific policy fields (`:1098-1123`): `modelReasoningEfforts` / `modelReasoningEffortMap`
  seeded from `DEEPSEEK_GATEWAY_THINKING_MODELS + OPENCODE_FREE_DEEPSEEK_MODELS`,
  `preserveReasoningContentModels` (same list), `modelContextWindows` for
  `DEEPSEEK_VISION_PREVIEW_MODEL: 1_048_576`, `modelInputModalities`, `noVisionModels`,
  `noJsonSchemaModels`, `modelResponsesTerminalRepair` for `muse-spark-1.{2,3}-contributor-free`
  (`graceMs: 5000`).

---

## 1. Endpoint

`src/adapters/openai-chat/wire.ts:38-41` + `src/adapters/openai-chat-url.ts:7-11`:
```
url = openaiChatCompletionsUrl(baseUrl)            // no chatCompletionsPath set
    = trim(baseUrl, /\/+$/).replace(/\/chat\/completions\/?$/, "") + "/chat/completions"
    => https://opencode.ai/zen/v1/chat/completions
method = "POST"                                     // openai-chat.ts:255
```
- No query string. No region/project selection — baseUrl is fixed in the registry row and can be
  overridden only by operator config (per-provider `baseUrl`). No `?alt=sse` (that is Google only).
- Body is `JSON.stringify(body)` with `Content-Type: application/json` (`openai-chat.ts:233`).
- Model list discovery (separate request): `providerModelsUrl()` `src/providers/model-discovery.ts:63-67`
  → `https://opencode.ai/zen/v1/models`.

## 2. Request headers

Built entirely in `src/adapters/openai-chat/wire.ts:28-33` — **in this insertion order** (plain object
literal; no explicit ordering is otherwise enforced):
```ts
headers = {
  "Content-Type": "application/json",
  ...agentRouterDefaultHeaders(provider.baseUrl, provider.headers), // {} for opencode.ai (src/adapters/agentrouter.ts: only host agentrouter.org)
};
if (hasCredential) headers.Authorization = `Bearer ${provider.apiKey}`;
if (provider.headers) Object.assign(headers, provider.headers);   // operator overrides win
```
- `Authorization: Bearer <apiKey>` — only when the key is non-empty. Key/oauth authMode with no key
  throws before I/O (`wire.ts:25-27`), except `keyOptional`.
- `provider.headers` is the merged registry `staticHeaders` (none for zen) + user config headers;
  merge is case-insensitive with user winning (`src/providers/registry.ts:46` `mergeRegistryStaticHeaders`,
  applied in `derive.ts:252`).
- **No `User-Agent` is set by the chat path.** Registry static UA exists only for `opencode-free`
  (`User-Agent: opencode`, `x-opencode-client: desktop`). The default `User-Agent: opencodex`
  (`src/lib/provider-outbound.ts:130,146-152`) is applied **only** to proxy-originated traffic
  (discovery/quota probes), never to inference. So a chat completion to Zen leaves the UA to the
  runtime (Bun default) unless the operator sets one — the entry at `:1203-1211` documents exactly
  that Zen's 429s in #2067 traced to a bare `Bun/x.y.z` UA.
- Client headers forwarded: **none.** The adapter generates the full header set; inbound client
  headers are not copied for the chat path (`FORWARD_HEADERS` / `applyCallerUserAgentFallback`
  exist only in `src/adapters/openai-responses/passthrough.ts:88,257`).
- Transport-level additions at send time (`src/server/responses/fetch-helpers.ts`):
  `Connection: close` (`:179`), `accept-encoding: identity` when identity is preferred (`:433-434`),
  and caller `accept-encoding` copied through if the built request did not claim one
  (`src/server/chat-native.ts:449-451`).
- HTTP version: Bun default negotiation. `upstreamHttpVersion` is unset for zen; when set it emits a
  Bun-only `protocol` init and requires an https target (`src/lib/upstream-http-version.ts:13-45`).

## 3. Request body

Two paths, same wire shape.

**Translated** (inbound Responses/Messages → chat), `src/adapters/openai-chat.ts:107-111`:
```ts
body = {
  model:  parsed._wireModelOverride ?? (provider.modelSuffixBracketStrip
            ? stripBracketedModelSuffix(parsed.modelId) : parsed.modelId),
  messages,
  stream: parsed.stream,
}
```
Then conditionally appended, in this order (`openai-chat.ts:112-231`):
`service_tier` (only if `canSerializeOpenAIChatServiceTier`), `reasoning_split` (only
`reasoningSplitModels`), `provider` (OpenRouter/Vercel routing only — never for zen), `tools`,
`tool_choice`, `max_tokens` (`resolveMaxTokens`), `temperature`, `top_p`, `stop`,
`reasoning_effort` | `thinking_budget` | `thinking: {type}` (effort mapping, `:148-192`),
`presence_penalty`, `frequency_penalty`, `prompt_cache_key` (only if `provider.promptCacheKey` —
not set for zen), `response_format` (`json_object`, or `json_schema`; downgraded to `json_object`
for `noJsonSchemaModels`), `parallel_tool_calls` (when tools present), and
**`stream_options: { include_usage: true }` whenever `stream` is true** (`:231`).

Message mapping — `messagesToChatFormat` (`src/adapters/openai-chat/messages.ts:88`):
- System prompt: joined `context.systemPrompt` + optional non-OpenAI tool-catalog nudge, then
  **rewritten by `identifyRoutedModel`** (`messages.ts:174`, `src/adapters/identity.ts:76-89`):
  any GPT-5/neutral identity sentence is replaced with
  `You are a coding agent powered by the <model>. If asked which model you are, identify as <model>. Do not claim to be a different model or to have a different creator.`
  (fallback: "configured model … Do not claim to be GPT-5 or made by OpenAI."). Emitted as one
  `{role:"system", content: <string>}` at the head of the array.
- `developer` messages from a translated ingress are folded to the wire role from
  `translatedChatDeveloperWireRole` → `"system"` unless an explicit role was recorded
  (`src/adapters/openai-chat/developer-role.ts:36-38`).
- Assistant thinking replay: `reasoning_content` is written back **only** for models in
  `preserveReasoningContentModels` (`messages.ts:73-86` `openAIChatSerializesThinking`, applied
  `:283-291`). Zen's DeepSeek thinking models are on that list precisely because Zen returns
  HTTP 400 on a bare tool-call continuation (`entries-extended.ts:1092-1096`, #950/#994). On a
  cache miss an `orphanReasoning` placeholder is gated by the same preserve list (`messages.ts:342-357`).
  `reasoning_details` arrays are replayed when present (`:291`). No `signature` field exists on
  this wire (`:79`).
- Tool-call ids: duplicate/repeated wire ids are reminted `call_ocx_minted_<n>` (`messages.ts:103-110`).
  Tool-result images are flushed as a trailing synthetic `user` message
  `"[ocx] image output from the preceding tool result(s):"` (`:118-128`). `VIDEO_UNSUPPORTED_MARKER`
  text replaces video parts (`:1-37`). No `cache_control` anywhere on this wire.
- Field removals: nothing is stripped beyond the above; there is no system-prompt scrub of
  operator text, only the identity-sentence replacement.

**Passthrough** (inbound Chat → chat), `src/adapters/openai-chat/passthrough.ts:61-94`:
`{model, messages, stream}` + an allowlist copy of `CHAT_PASSTHROUGH_FIELDS` (`:18-40`):
`audio, frequency_penalty, logit_bias, logprobs, max_completion_tokens, max_tokens, metadata,
modalities, n, prediction, presence_penalty, reasoning_effort, response_format, seed, stop, store,
temperature, tool_choice, tools, top_logprobs, top_p, user, …`. Caller `messages` are forwarded
verbatim except an explicitly-recorded `developer`→`system` role rewrite (`:69-72`).
`include_usage` is merged into `stream_options` when streaming.

## 4. Response

- Dispatch is by `content-type` (`src/server/chat-native.ts:670-671`): `text/event-stream` → SSE
  parse; otherwise JSON.
- **Streaming** (`src/adapters/openai-chat.ts:274-400`): `response.body.getReader()` +
  `TextDecoder`. Per line, `sseFieldValue(line, "data")`; empty → skip; `[DONE]`
  (`:387-393`) flushes inline think text, flushes tool calls, then emits
  `{type:"done", usage, stopReason?}`. Non-JSON data frame → `{type:"error", message:"malformed upstream SSE data frame"}`
  (`:400`). A stream that ends with no terminal is an error (`:656`).
  Client-facing SSE headers when relaying: `Content-Type: text/event-stream; charset=utf-8`,
  `Cache-Control: no-cache`, `Connection: keep-alive` (`chat-native.ts:737-745`). Non-streaming
  clients get the buffered completion as JSON (`:757`).
- **Non-streaming** (`openai-chat.ts:690-725`): `response.json()`; non-object or unparseable →
  error; `unwrapChatCompletionPayload`; `json.success === false` with no error → error; `payload.error`
  → `upstreamErrorEvent`; `choices[0].message` + `finish_reason`.
- Stop reason mapping `src/adapters/openai-chat/response-events.ts:5-11`: `length`→`max_tokens`,
  `content_filter`→`content_filter`, else undefined.
- Usage mapping `response-events.ts:168-178`:
  ```ts
  inputTokens  = usage.prompt_tokens
  outputTokens = usage.completion_tokens
  cachedInputTokens      = usage.prompt_tokens_details.cached_tokens      (if present)
  reasoningOutputTokens  = usage.completion_tokens_details.reasoning_tokens (if present)
  ```
  Absent numbers default to 0 for input/output; cache/reasoning keys are omitted entirely when the
  upstream omits them.
- Unknown/unnamed tool call: `unnamedToolCallEvent` fails closed with
  `"upstream streamed a tool call without a function name — cannot dispatch"` (`:160-166`).
- `reasoningTextFrom` accepts `reasoning_content` or `reasoning` string (`:13-17`).

## 5. Auth

- Credential type: plain static API key (`authKind: "key"`), sent as `Authorization: Bearer <key>`.
  No OAuth, no refresh flow, no token endpoint, no device code. `opencode-zen` has no
  `refresh`/`oauth` fields in its registry entry.
- Multi-account: the generic key pool. `provider.apiKeyPool` (`src/types/provider.ts:523`) with
  selection strategy round-robin / fill-first / quota; current-selection checks in
  `src/providers/api-key-selection.ts` (`providerApiKeySelectionIsCurrent`,
  `resolveCurrentProviderApiKeyTransport`), used at `chat-native.ts:432-445`. A rotated key rebuilds
  the whole request (`activeAdapter = createOpenAIChatAdapter(activeProvider)`,
  `chat-native.ts:549-553`).
- Account-id headers: **none** for zen. (`x-opencode-session` exists in the codebase —
  `src/providers/opencode-go-transport.ts:5`, value `ocx_<32hex>` — but is applied only to the
  canonical `opencode-go` destination, and opencodex explicitly refuses to mint it for Zen's
  keyless tier, `entries-extended.ts:1200`.)

## 6. Errors

Classification is `classifyError(status, type, text)` in `src/lib/errors.ts`:
- `429` / text containing rate limit|throttl|too many requests|resource_exhausted →
  `{type:"rate_limit_error", code:"rate_limit_exceeded"}` (`errors.ts:445-456`).
- Quota wording (`insufficient_quota`, "usage limit reached", "exceeded your current quota",
  "quota exhausted", monthly/daily quota) → `{type:"insufficient_quota", code:"insufficient_quota"}`
  (`:426-436`). Checked **before** the 429 branch.
- `401` / `authentication_error` / auth wording → `{type:"authentication_error", code:"invalid_api_key"}`
  (`:467-473`). `chat-native.ts:622` also maps 401 → `authentication_error`.
- `403` → `permission_denied`, with `subscription_required` when a subscription-gate phrase is
  present and `location_not_supported` when the message says so (`:474-496`).
- `5xx` / "overloaded" / "server is busy" → server_error / `server_is_overloaded` (`:497-...`).
- `402` is treated as retryable evidence at `errors.ts:536` (`status 429 || 402 → true`), no
  distinct class — matches the docs note that Zen tiers can answer HTTP 402.
- **Retry-After / reset parsing** — `src/lib/retry-after.ts:33-55` `resolveClientRetryAfter`:
  order is (1) upstream `Retry-After` header (seconds or HTTP-date, `validateClientRetryAfterHeader`),
  (2) a `try again in Ns` hint parsed out of the message, (3) for 429 only, a default
  `DEFAULT_RETRYABLE_429_RETRY_AFTER_SEC` — suppressed when the classification is
  `insufficient_quota`, and suppressed entirely with `includeDefault:false` (cooldown metadata must
  not be shortened).
- **Zen-specific enrichment** — `src/providers/opencode-zen-rate-limit.ts:69-100`
  `enrichOpenCodeZenRateLimitMessage`: only for `status === 429`, only when authMode is key and a
  key exists (`:60-61`), only when the message matches `/rate\s*limit/i`, and only once. If the
  upstream sent no valid `Retry-After` (and no "try again in N" in the text), appends
  ` Try again in 15s.` — `OPENCODE_ZEN_SYNTHETIC_RETRY_AFTER_SEC = 15` (`:29`). Always appends
  `" Slow the request pace, or set providers.opencode-zen.retryOn429 for same-key backoff."`
  (`:97-99`). Upstream `Retry-After` always wins. Applied from
  `src/server/responses/adapter-dispatch.ts:~1410` and chat-native.
- **Same-key 429 backoff** — `rateLimitRetryPolicyFor` (`src/providers/key-failover.ts:574-599`):
  zen has no `retryOn429` knob, so it gets only `DEFAULT_RATE_LIMIT_RETRY`
  (`:275-281`: enabled, 3 attempts, 5 s interval, 60 s max, respects Retry-After) — actually that
  default is returned only when a `retryOn429` object is present; with **no knob** and not
  opencode-go/command-code the policy falls through to null. Net: **zen gets no same-key replay
  unless the operator sets `retryOn429`.** Loop: `chat-native.ts:516-530`
  (`prepareSameTarget429Wait` with `rateLimitRetryDelayMs(policy, retry-after header)`).
- **Key-pool failover** — `chat-native.ts:533-554`: on 429 with a pool, `rotateProviderTransportOn429`
  records a cooldown for the failed key and swaps in the next healthy one. Cooldown bounds:
  `DEFAULT_COOLDOWN_MS = 60_000`, `MAX_COOLDOWN_MS = 600_000` (`key-failover.ts:31-32`).
- Non-replayable responses (already-streaming bodies) are never retried (`isNonReplayableResponse`).
- No Zen-specific suspension/ban state is modeled anywhere; only the per-key cooldown map.

## 7. CLI login

Key providers log in by **pasting an API key** — no redirect URL, no device code, no OAuth.
`src/oauth/login-cli.ts:242-275` `handleKeyLogin(name)`:
1. namespacing/collision check against existing codex account namespaces (`:246-250`);
2. prints `🔑 <label> — opening <dashboardUrl> so you can create/copy an API key...` and opens
   `https://opencode.ai/auth` (`:251-253`, from `entries-extended.ts:1091`);
3. prompts `Paste your <label> API key: ` (`:258`) — no URL is fetched;
4. if the baseUrl template contains `{…}`, asks for a resolved endpoint URL (`:261-268`);
5. validates: `validateApiKey` (`src/oauth/key-providers.ts:~110-150`) issues `GET <models URL>`
   with `Authorization: Bearer <key>` (Gemini variant uses `x-goog-api-key`); 400/401/403 → invalid,
   anything else → `"unknown"` (accepted). For zen that is `GET https://opencode.ai/zen/v1/models`.
- Wiring: `KEY_LOGIN_PROVIDERS = deriveKeyLoginMap()` (`key-providers.ts:55`), populated from the
  registry, so `ocx login opencode-zen` exists without a zen-specific code path.
- `ocx opencode` (`src/cli/opencode.ts`) only injects `OPENCODE_CONFIG_CONTENT` into the OpenCode
  CLI with the key passed by env; it never touches Zen HTTP.

---

## Rust gateway checklist (byte-for-byte)

1. `POST https://opencode.ai/zen/v1/chat/completions`, no query, `Content-Type: application/json`.
2. Headers: exactly `Content-Type`, then `Authorization: Bearer <key>` — nothing else, unless the
   operator configured extra provider headers. Do **not** invent a UA; either omit (matching zen's
   default path) or let config supply it.
3. Body: `model`, `messages`, `stream` first; append optional fields in the order listed in §3;
   `stream_options: {include_usage: true}` on every streamed request.
4. Stream parse: SSE `data:` lines, `[DONE]` terminal; malformed frame = error, missing terminal = error.
5. Usage: `prompt_tokens` / `completion_tokens` / `prompt_tokens_details.cached_tokens` /
   `completion_tokens_details.reasoning_tokens`.
6. 429 with no `Retry-After` → synthesize 15 s; key-pool cooldown 60 s, cap 600 s; no same-key
   replay unless explicitly enabled (3 attempts × 5 s if enabled).


## Unknowns
- Whether Zen requires/expects a specific `User-Agent` on inference requests is not encoded: the registry only patches UA for `opencode-free` (`User-Agent: opencode`, `x-opencode-client: desktop`), so the keyed `opencode-zen` inference path emits no UA of its own and inherits the runtime default (Bun). I could not confirm from source whether opencodex intends the same UA pair for the keyed provider.
- I did not open any credential file, so I could not confirm the real on-wire header set of a live request (e.g. whether an operator-level `headers` config or `getOrAllocateRequestSessionLane` adds anything for zen at runtime).
- Exact Zen `/models` response schema / whether it needs the same headers is inferred only from the generic `providerModelsUrl` + `validateApiKey` Bearer probe; no zen-specific parser found.
- Whether `enrichOpenCodeZenUpstreamMessage` (`opencode-zen-rate-limit.ts:172`) does more than the 429 enrichment function was summarized but not read line-by-line.
- No code path was found that treats Zen HTTP 402 with a distinct classification — it is only folded into the generic retryable-evidence check at `errors.ts:536`; the real Zen 402 semantics (insufficient credits) are documented in `structure/` notes but not modeled.
- Header *order on the wire* is only implicitly defined (object literal insertion order in `wire.ts:28-33` followed by `Headers` re-lowering in `fetch-helpers.ts`); I cannot guarantee wire order from source for a Rust reimplementation.