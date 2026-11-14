# opencode (direct HTTP, replaces the `opencode run` subprocess)

Captured from the installed CLI (opencode 1.19.5) by pointing its `opencode`
provider at the parity recorder:

```json
{ "provider": { "opencode": { "options": { "baseURL": "http://127.0.0.1:10303/zen/v1" } } } }
```

    OPENCODE_CONFIG=/tmp/oc-probe/opencode.json opencode run -m opencode/deepseek-v4.1-flash-free "say hi"

## Request

    POST https://opencode.ai/inference/openai/v1/chat/completions

This is the live URL seen through a TLS proxy (the CLI honors `HTTPS_PROXY`
and `NODE_EXTRA_CA_CERTS`). `/zen/v1/chat/completions` answers the same way.

Headers in wire order:

| Header | Value |
|---|---|
| authorization | `Bearer public` (no login), else `Bearer <OPENCODE_API_KEY>` |
| content-type | `application/json` |
| user-agent | `opencode/1.19.5 ai-sdk/provider-utils/4.0.23 runtime/bun/1.4.4` |
| x-opencode-client | `cli` |
| x-opencode-project | `global` (project id; `global` outside a git repo) |
| x-opencode-request | user message id, `msg_<id>`, new per user turn |
| x-opencode-session | session id, `ses_<id>` |
| x-opencode-session-id | same session id |
| connection | `keep-alive` |
| accept | `*/*` |
| host | `opencode.ai` |
| accept-encoding | `gzip, deflate, br, zstd` |
| content-length | body length |

`x-opencode-parent-session-id` is added for subagent sessions
(`packages/opencode/src/session/llm/request.ts:186-202`).

Body keys in order: `model, max_tokens, temperature, reasoning_effort,
messages, stream, stream_options` (`{"include_usage": true}`). With tools:
`model, max_tokens, tool_choice: "auto", messages, tools, stream,
stream_options`. Model id is sent without the `opencode/` prefix.

## Auth

No signature, device id or attestation. Without a key the CLI sends
`Bearer public`, and the server treats it as anonymous; only models with
`allowAnonymous` work, rate-limited per IP. 403 means region or data-policy
restriction (`packages/console/app/src/routes/zen/util/handler.ts:141,150,169`),
not a missing secret.

## Models

The CLI reads `https://models.opencode.ai/api.json` (provider `opencode`, all
models use `@ai-sdk/openai-compatible`, so every model is chat/completions).
Free models end in `-free`. The server also has `/zen/v1/models`.

## Free tier gate (identified 2026-11-16)

The real rule is a **prompt-cache hit**, not a session id. Once this IP's
anonymous budget is spent, a request passes only when its prompt prefix is
already in opencode's server-side cache; a novel prefix is refused with
`429 FreeUsageLimitError`. The session id only feeds that cache key, which is
why earlier tests looked like an id rule.

Ids follow `~/opencode/packages/schema/src/identifier.ts`: 26 chars, the first
12 hex encode `~(((ms << 12) | counter) & 0xffffffffffff)`, then 14 random
chars. A valid id is still required (counter 0 and ascending ids are refused),
but a valid id alone is not enough.

Measured on a spent IP, one request per row, direct to `/zen/v1/chat/completions`:

| Request | Result |
|---|---|
| opencode's title body, byte-exact | 200 |
| same body, tools removed | 429 |
| opencode's build body (60 KB system prompt + 47 tools) | 200 |
| same body, only the first 5 tools | 429 |
| same body, one tool removed | 200 |
| same body, system prompt + one trailing space | 200 |
| same body, last user message replaced | 200 |
| title system prompt cached, brand new user message | 200 |
| novel system prompt, same model and shape | 429 |
| short message list, any id | 429 |

Edits that stay inside a prefix opencode has already sent are served from
cache; a prefix the server has never seen is charged to the exhausted budget
and refused. Header order, user-agent, IP family and HTTP version make no
difference.

### What this means for gw

A gateway cannot make a fresh client conversation free by imitating ids. Free
access depends on opencode having cached an identical prefix already, which
only opencode's own clients produce. `gw` therefore serves the free tier only
on cache-warm prefixes and reports refusals as a quota failure with the
server's `Retry-After`; paid Zen and Go (real key) are unaffected.

## Retry policy

The free tier answers 429 intermittently even for warm prefixes. `gw` retries
inline, before anything reaches the client:

| Case | Behaviour |
|---|---|
| 429, no `Retry-After` or one under 3 s | retry, 400 ms then 800 ms, at most 3 attempts |
| 429 with `Retry-After` over 3 s | stop at once; budget is spent |
| retries exhausted | `Failure::Quota` with the server's `Retry-After`, capped at 6 h |

A quota failure puts the account on cooldown, so a combo fails over to the next
target instead of hammering the free tier.

## gw mapping

- One `opencode` adapter covers free (no key) and paid (key) models; it
  replaces both `opencode-cli` and `opencode-zen` egress.
- Session id: stable per gw session key, formatted `ses_<id>`; request id
  `msg_<id>` per inbound request. Both feed Zen's caching and affinity.
- Same headers and order as above; user-agent pinned to the CLI version gw
  claims (config value, default `1.19.5`).
- Pass body fields through from the client. Do not add `temperature`,
  `max_tokens` or `reasoning_effort` defaults that the client did not send.

## Tools and prompts

gw forwards the client's `messages`, `system` and `tools` unchanged and never
adds the CLI's own agent tools or system prompt. The client runs the tools.
The CLI's 47 tools belong to its agent loop; its title request sends none,
so tool-free requests are normal traffic. The server side
(`packages/console/app/src/lib/inference-proxy.ts:17-48`) only reads the key
(`public` or empty means anonymous), the client IP and the request id. It does
not inspect tools or prompts.

Upstream (`~/opencode`, `packages/core/src/session/model-request.ts:281-291`)
also sends `x-session-affinity`, `X-Session-Id` and `promptCacheKey` (the
session id without `ses_`); 1.19.5 did not. Match the version gw claims.

## Live test

- Direct calls with the exact CLI headers reach the model API. No 403;
  the request format is accepted.
- Anonymous quota is per IP and shared with the CLI: after it ran out, both
  the CLI and direct calls got `429 FreeUsageLimitError` with
  `Retry-After: 24061`. Unavailable models return `400 Model is unavailable`.
- One CLI run succeeded after a direct call had already been refused.
  Unexplained; retest when the quota resets.

## Unknowns

- Whether the server checks the `user-agent` or `x-opencode-*` headers at all.
- Per-model anonymous rate limits.
