# LLM gateway

Clients → Headroom → opencodex → providers.

Only Headroom is available on the host, at `127.0.0.1:10204`.
State stays in Docker volumes. Existing services are untouched.

Keep this setup on localhost. Keep API keys private.

## Sources

Use the latest changes from these branches:

- [opencodex: gateway](https://github.com/Creator54/opencodex/tree/gateway)
- [Headroom: gateway](https://github.com/Creator54/headroom/tree/gateway)

Tag each image with its source commit. Deploy a specific image digest.

## Test

Requires Docker Compose and both images built locally.

```sh
bash tests/smoke.sh
```

The test creates a temporary key, checks access and responses, then restarts
opencodex to check that state survives. It never reads your existing credentials.
It resets only its own test volumes and leaves the test stack running.

## Stop the test

```sh
docker compose -p llm-gateway-smoke -f compose.yaml -f compose.test.yaml down --volumes
```

This deletes test state. Do not use `--volumes` with real provider state.
