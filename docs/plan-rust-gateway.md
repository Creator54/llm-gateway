# Plan: single Rust gateway (`gw`)

Replace Headroom (Python) + opencodex (Bun) with one Rust binary that keeps the
`services.llm-gateway` module interface. Develop and test locally first; deploy
to phoenix later.

## Why

| Host | Headroom | opencodex hub | Total |
|---|---|---|---|
| laptop | ~1.37 GB (proxy + worker) | ~0.28 GB | ~1.65 GB |
| phoenix | 1.53 GB (peak 1.59) | 0.36 GB (peak 1.27, incl. opencode children) | ~1.9 GB |

Headroom runs `--mode cache`: no compression, bodies forwarded nearly byte for
byte. Its cost is memory; its benefit is unproven (see Phase 0).

Target: `gw` < 30 MB idle, < 100 MB at 50 concurrent streams.

## Scope

- Inbound: `/v1/responses` (HTTP + WS), `/v1/chat/completions`, `/v1/messages`,
  `/v1/messages/count_tokens`, `/v1/models`, health.
- Providers: openai (ChatGPT), anthropic (OAuth), google-antigravity, kiro,
  command-code, opencode-zen, opencode-cli.
- Agents: pi, codex, Claude Code, opencode.
- Out: GUI/dashboard, other opencodex providers, desktop integrations, ML
  compressors, telemetry.

## Architecture

```
agent -> inbound (3 protocols) -> router -> account pool -> adapter -> upstream
                                    |          |              |
                               session map  SQLite      cache layer
```

- New crate. Copy from joocode (MIT, keep notice): `protocol.rs` (Responses<->Chat),
  `antigravity.rs`, `upstream.rs` (cooldown, Retry-After), `combo.rs`,
  `integration_journal.rs`. Do not copy rtk-proxy code (BUSL-1.1).
- Stack: tokio, axum, reqwest + rustls, rusqlite (WAL).
- Passthrough when inbound protocol matches the provider; otherwise one
  streaming translator per pair. Responses are never buffered.
- `Provider` trait: build request, map stream, classify error
  (429 / auth / quota / fatal / retryable-before-first-byte).
- Builds for x86_64-linux and aarch64-linux (phoenix).

## Routing (selected by model name)

| Model | Meaning |
|---|---|
| `openai/gpt-6-luna` | provider, accounts rotate |
| `openai@work/gpt-6-luna` | pinned account |
| `combo/pi-subagents` | combo: failover / spread / weighted |
| `auto`, `auto/<profile>` | rules first, Panda optional (Phase 5) |

- Combo layer picks target; account layer picks account (sticky per session).
- Target exhausted only when all its accounts cool down.
- Failover only before the first output byte. Cooldown honors Retry-After /
  reset time, default 60 s. State machine mirrors opencodex
  `server/responses/core-combo.ts` and `oauth/generic-account-failover.ts`.
- Session key: client id (`session_id`, `x-session-affinity`,
  `x-claude-code-session-id`, `prompt_cache_key`), else hash(system + first
  user message). Session stays on provider + account until it fails, then stays
  on the fallback.

## Token efficiency (on by default; `parity` profile turns all off)

Rules for every transform: deterministic, fail-open (original bytes on any
failure or unknown shape), individually switchable server-side.

1. Never re-serialize or reorder bytes before the last cache breakpoint.
2. Session stickiness (provider caches are per account).
3. Cache markers: Anthropic `cache_control` when missing, TTL-order repair,
   1 h TTL for long-gap sessions; OpenAI `prompt_cache_key` / retention only
   where accepted (learned from 400s, stored in SQLite).
4. Strip per-turn churn from known prompts (pi session-overview; skill-list
   compaction except on Responses/Codex routes, where it raised task cost).
5. Cache-break detector: log which prefix part changed when cache-read share drops.
6. Cache warming for Anthropic only: refresh at min(0.9*TTL, TTL-10s) while the
   session is active, 60 min idle cap, skip budget-thinking requests. Agent-side
   warming must be off (no double billing).
7. Tool-result cleanup (ANSI, redraws, duplicate lines) after the last breakpoint only.
8. Local `/v1/models` and `count_tokens`.

Not done: ML compression, summarizing or dropping history.

Success metric: cost per completed task and cache-read share, not tokens per request.

Replaces client-side: pi-cache-optimizer, the local
`extensions/opencodex-anthropic.ts` prompt rewrite (moves into the anthropic
OAuth adapter for all agents). Pauses are covered by the 1h cache TTL on
Anthropic breakpoints rather than keep-alive requests.

## Accounts

- Credentials `~/.config/gw/credentials.json` (0600); state in SQLite.
- `gw account add <provider>` ports opencodex flows: print URL + paste redirect
  URL/code (`oauth/login-cli.ts`), device code (`chatgpt-device.ts`,
  `kiro-device-login.ts`), API key prompt.
- `gw account list|pause|resume|remove|alias|clear-cooldown|import`.
- `gw` is the only refresher of an account's tokens (rotating refresh tokens).
  agy-profile accounts get their own logins.
- Non-loopback bind refused without an API key (Bearer or `x-gw-key`, stripped
  before forwarding).

## Parity

- Byte-identical method, URL, headers, body vs opencodex after masking volatile
  values (request ids, uuids, timestamps, derived session ids).
- Golden fixtures recorded against `tests/fake-upstream.ts` for each
  inbound protocol x provider x {text, tools, reasoning, images, stream,
  non-stream, 429, 401, mid-stream error}. Agent-side responses diffed the same way.
- A provider goes live only with a clean diff under the `parity` profile.
- TLS fingerprint differs from Bun; test per provider in Phase 1, switch an
  adapter to an impersonating client only if a provider rejects rustls.
- opencode-cli: same args, stdin, env as today; concurrency cap 1-2.

## Install / connect

| Command | Effect |
|---|---|
| `gw install` | binary, config, systemd unit; NixOS via `services.llm-gateway` |
| `gw connect <agent>` / `disconnect` | journaled base URL + key in pi/codex/claude/opencode configs; pi: provider `gw` beside existing ones, drop compat flags, remove pi-cache-optimizer once parity is shown |
| `gw uninstall [--purge]` | replay journal, stop unit; `--purge` removes credentials/state |
| `gw doctor` | ports, credentials, expiry, upstream reachability, client config drift |
| `gw status` | agents, accounts, quota, tokens, cache-read share and cost per provider/account |

## Phases

| Phase | Work | Size |
|---|---|---|
| 0 | Local: Rust dev shell, golden fixtures from local opencodex, joocode build + memory, Headroom-off cache measurement | 1 week |
| 1 | `gw` core: 3 inbound protocols, passthrough, auth, SQLite, install/connect/doctor, fallback route to opencodex for unported providers, memory bench, TLS check | 1.5-2 weeks |
| 2 | Pool, router, stickiness, cooldowns, combos; openai, opencode-zen, anthropic | 2-3 weeks |
| 3 | antigravity, command-code, kiro, opencode-cli; drop opencodex | 2-3 weeks |
| 4 | Token-efficiency items 3-7, each A/B'd on cost per task | 1-2 weeks |
| 5 | `auto` routing (rules, then Panda); agent-side rtk CLI / TokenEconomizer optional | 1 week |
| Deploy | phoenix: one `llm-gateway` unit on `port`, same nginx vhost, key, model ids; `gw account import` from `/var/lib/llm-gateway/ocx`; rollback = pin old flake rev | later |

## Risks

- Subscription accounts mimicking desktop clients, rotated from a server IP, can
  be flagged (kiro already shows 24 h suspensions).
- Reverse-engineered providers change; fixtures detect it, fixes are ongoing.
- All tokens live on the gateway host: tailnet-only, 0600, auth required.

## Open

- opencode-cli: keep subprocess (1:1) or switch to HTTP.
- phoenix `127.0.0.1:10200` is the live service's own opencodex (user `llm-gateway`); it goes when the service is replaced.
- omnix references the gateway; check before deployment.
- phoenix privileged steps need sudo.
