# LLM gateway

One local proxy for every LLM client and provider. Clients point at one base URL
with one key; `gw` routes each request to a provider, pools accounts, fails over,
and trims tokens without changing the client.

Rust, single binary, about 4 MB RAM idle.

## Install

```sh
nix run github:Creator54/llm-gateway/rust-gateway -- --help
```

Or add the flake and run the service:

```nix
imports = [ inputs.llm-gateway.nixosModules.default ];
services.llm-gateway = {
  enable = true;
  apiKeyFile = "/run/secrets/llm-gateway-key";
  port = 10204;
};
```

The key file must be absolute and outside the Nix store; the unit copies it with
owner-only permissions at start.

## Use

```sh
gw login anthropic              # browser or device flow, stores the account
gw login openai --device        # ChatGPT subscription (Codex backend)
gw login openai-api --key       # OpenAI platform API key, billed per token
gw login gemini-api --key        # Google Gemini API key, billed per token
gw account list
gw serve                        # or the systemd unit
```

Connect a client. Each command writes only a base URL and a key:

```sh
gw connect pi | opencode | claude | codex
gw disconnect codex
```

`gw status` shows which agents point at the gateway.

`openai` and `openai-api` are separate providers: `openai` rides a ChatGPT login, while
`openai-api` calls `api.openai.com/v1` with a platform key (`openai-api/gpt-5.5`).
`gemini-api` is the same shape for Google: a pay-per-token key against
`generativelanguage.googleapis.com/v1beta` (`gemini-api/gemini-2.0-flash`).
`gw models --refresh` lists the models a key can use.

## Models and routing

| Model string | Meaning |
|---|---|
| `provider/model` | that provider |
| `provider@account/model` | a named account |
| `combo/<name>` | a named chain of targets, see Combos below |
| `auto[/profile]` | pick by profile |

Failover happens before the first byte reaches the client. A quota or 429 cools
that account for the server's `Retry-After`, then the next account or combo entry
is tried.

## Metrics

`GET /metrics` on the same port, behind the gateway key, Prometheus format.
`gw status [--since 7d] [--refresh] [--json]` shows connected agents, accounts,
usage with its equivalent list-price cost, and subscription quota. Usage comes from the
local database, so it works while the server is down. Quota is cached for five minutes
and refetched once a window resets; `--refresh` forces a fetch.

## Token efficiency

On by default: stable prefixes, session stickiness, cache markers, stripping
changing sections, cache-break detection, local `/v1/models` and `count_tokens`.
Every transform is deterministic, fail-open, and switchable. `profile = "parity"`
turns all of them off; only the edits an upstream requires (such as the bare
model id) remain.

## Development

```sh
just check          # fmt, test, clippy
bench/mem.sh        # RAM under streaming load
```

Provider wire contracts are in `docs/provider-specs/`.

## Combos

A combo is a named list of `provider/model` targets that clients use as `combo/<name>`.
Strategies: `failover` (listed order), `round_robin` (`*N` weights new sessions),
`cheapest` (by list price) and `quota_aware` (most subscription quota left first).

```
gw combo set fast quota_aware antigravity/gemini-3.8-flash kiro/claude-haiku-4.5 openai/gpt-6-luna
gw combo list
gw combo rm fast
```

`set` creates the combo or replaces its targets, keeps the rest of `config.toml` as written, and
refuses edits the gateway could not load. Restart `gw serve` to apply a change.
