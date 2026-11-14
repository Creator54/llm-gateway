# Memory bench

Measures a gateway's RAM under streaming load. Targets: **< 30 MB idle, < 100 MB at 50 streams**.

Measured 2026-11-16, x86_64, release build:

| Stack | Idle | 50 concurrent streams |
|---|---|---|
| gw | 4.0 MB | 9.4 MB peak (high-water) |
| headroom + opencodex | 2280 MB over 5 processes | not measured (compose stack) |

Load: 400 deltas per stream, 50 ms apart; first byte p50 38 ms, total p50 21 s.

```sh
bun bench/slow-upstream.ts --port 10399 &   # fake upstream: 400 deltas, 50 ms apart (~20 s)
bench/mem.sh -- ./gw                        # or: bench/mem.sh <PID>
bun bench/load.ts --requests 50 --url http://127.0.0.1:10204
```

Same recipe for headroom+opencodex: sample only the process under test, point `--url` at its port.

`--interval`/`--tokens` change the fake stream, `--chat` uses `/v1/chat/completions`;
env fallbacks: `GW_URL`, `GW_KEY`, `GW_REQUESTS`.
