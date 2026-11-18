# OpenAI API key ("openai-api") upstream wire spec

Implemented in `gw/src/providers/openai_api.rs`. This is the platform API at
`https://api.openai.com/v1`, billed per token with an API key. It is separate from `openai`
(ChatGPT/Codex login, `chatgpt.com/backend-api`), which never accepts a key:
`gw login openai --key` is refused and points here.

## Credential

- `Credential::ApiKey` only; anything else fails as `Failure::Auth`.
- `Authorization: Bearer <key>`. Store it with `gw login openai-api --key` (or `--env VAR`).
- Base URL override: `[endpoints] openai-api` in `config.toml`.

## Endpoints by inbound API

| Inbound | Upstream | Notes |
|---|---|---|
| Chat | `POST /chat/completions` | Model id rewritten; `max_tokens` renamed to `max_completion_tokens` unless the latter is set; streaming adds `stream_options.include_usage`. |
| Chat with tools and no `reasoning_effort: "none"` | `POST /responses` | Reasoning models reject function tools on chat completions. gw translates the request to Responses and converts the reply (stream or JSON) back to Chat. |
| Responses | `POST /responses` | Passed through with the model rewritten. |
| Messages | `POST /chat/completions` | Translated by the shared Chat↔Messages translator. |

`fast` requests set `service_tier: "priority"`. `x-request-id` and `openai-request-id` are relayed.

## Failure mapping

| Upstream | gw |
|---|---|
| 401 | `Auth` (account cools down 5 min) |
| 429 with `insufficient_quota` | `Quota` (cools down, capped at 10 min) |
| other 429 | `RateLimited` (`Retry-After`, else 60 s) |
| 5xx, network error | `Retryable` |
| anything else, including 403 | `Fatal`: body passed to the client, account unaffected |

A `Fatal` refusal is logged at `warn` with the sent body's shape (keys and sizes, never content).
In a combo, a 404 or a "model not found/deprecated" 400 sits the target out for an hour.

## Models

`gw models --refresh` lists `GET /models` with the key (only ids the account may use appear).
