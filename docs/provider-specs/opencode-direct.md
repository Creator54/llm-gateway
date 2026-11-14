# opencode (direct HTTP, replaces the `opencode run` subprocess)

Captured from the installed CLI (opencode 1.19.5) by pointing its `opencode`
provider at the parity recorder:

```json
{ "provider": { "opencode": { "options": { "baseURL": "http://127.0.0.1:10303/zen/v1" } } } }
```

    OPENCODE_CONFIG=/tmp/oc-probe/opencode.json opencode run -m opencode/deepseek-v4.1-flash-free "say hi"

## Request

    POST https://opencode.ai/zen/v1/chat/completions

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

## gw mapping

- One `opencode` adapter covers free (no key) and paid (key) models; it
  replaces both `opencode-cli` and `opencode-zen` egress.
- Session id: stable per gw session key, formatted `ses_<id>`; request id
  `msg_<id>` per inbound request. Both feed Zen's caching and affinity.
- Same headers and order as above; user-agent pinned to the CLI version gw
  claims (config value, default `1.19.5`).
- Pass body fields through from the client. Do not add `temperature`,
  `max_tokens` or `reasoning_effort` defaults that the client did not send.

## Unknowns

- Whether the server checks the `user-agent` or `x-opencode-*` headers at all.
- Per-model anonymous rate limits.
