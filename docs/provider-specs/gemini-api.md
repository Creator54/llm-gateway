# Google Gemini API key ("gemini-api") upstream wire spec

Implemented in `gw/src/providers/gemini_api.rs`. This is the pay-per-token Gemini API at
`https://generativelanguage.googleapis.com/v1beta`, billed per token with an API key. It is
separate from `antigravity` (the Google Cloud Code Assist OAuth backend used by Gemini CLI), which
never accepts an API key: `gw login antigravity --key` is refused and points here.

## Credential

- `Credential::ApiKey` only; anything else fails as `Failure::Auth`.
- `x-goog-api-key: <key>`. Store it with `gw login gemini-api --key` (or `--env VAR`).
- Base URL override: `[endpoints] gemini-api` in `config.toml`.

## Endpoints by inbound API

The provider normalizes every inbound protocol onto the chat-completions wire via the shared
translators, maps it onto the Gemini `generateContent` envelope, and rewrites the reply back per
the client's protocol.

| Inbound | Upstream | Notes |
|---|---|---|
| Chat | `POST /v1beta/models/{model}:streamGenerateContent?alt=sse` (or `:generateContent`) | Model id rewritten; system prompt → `systemInstruction`; `max_tokens` → `maxOutputTokens`; `tools` → `function_declarations`. |
| Messages | `POST /v1beta/models/{model}:generateContent` | Translated by the shared Chat↔Messages translator before mapping. |
| Responses | `POST /v1beta/models/{model}:generateContent` | Translated by the shared translator before mapping. |

## Streaming

`stream: true` sends to the `:streamGenerateContent?alt=sse` method with a
`text/event-stream` content type, then terminates the client stream with a final SSE frame.
Each Gemini chunk is reduced to a chat-completions chunk (`delta.content`, `delta.tool_calls`,
`finish_reason`) plus a final usage chunk; the client stream ends with `data: [DONE]`.

## Failure mapping

| Upstream | gw |
|---|---|
| 401, 403 | `Auth` (account cools down 5 min) |
| 400 | `Fatal`: body passed to the client, account unaffected |
| 429 | `RateLimited` (`Retry-After`, else 60 s) |
| 5xx, network error | `Retryable` |

## Models

`gw models --refresh` lists `GET /v1beta/models` with the key (only ids the account may use
appear). The provider serves only paid pay-per-token models (`gemini-2.0-flash`, `gemini-2.0-pro`,
`gemini-1.5-flash`, `gemini-1.5-pro`); there is no free tier on the API-key backend.
