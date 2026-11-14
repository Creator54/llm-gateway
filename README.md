# LLM gateway

One local proxy for every LLM client and provider. Clients point at one base URL
with one key; `gw` routes each request to a provider, pools accounts, fails over,
and trims tokens without changing the client.

Rust, single binary, about 4 MB RAM idle.

## Install

```sh
nix run github:Creator54/llm-gateway -- --help
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
gw login openai --device
gw account import               # pull accounts from an existing opencodex setup
gw account list
gw serve                        # or the systemd unit
```

Connect a client. Each command writes only a base URL and a key:

```sh
gw connect pi | opencode | claude | codex
gw disconnect codex
```

`gw status` shows which agents point at the gateway.

## Models and routing

| Model string | Meaning |
|---|---|
| `provider/model` | that provider |
| `provider@account/model` | a named account |
| `combo/<name>` | try each entry in order |
| `auto[/profile]` | pick by profile |

Failover happens before the first byte reaches the client. A quota or 429 cools
that account for the server's `Retry-After`, then the next account or combo entry
is tried.

## Metrics

`GET /metrics` on the same port, behind the gateway key, Prometheus format.
`gw stats [--since 24h] [--json]` reads the local database and works while the
server is down.

## Token efficiency

On by default: stable prefixes, session stickiness, cache markers, stripping
changing sections, cache-break detection, local `/v1/models` and `count_tokens`.
Every transform is deterministic, fail-open, and switchable. `profile = "parity"`
turns all of them off so traffic matches a plain forwarder byte for byte.

## Development

```sh
just check          # fmt, test, clippy
bench/mem.sh        # RAM under streaming load
```

Provider wire contracts are in `docs/provider-specs/`. The design and phase plan
are in `docs/plan-rust-gateway.md`.
