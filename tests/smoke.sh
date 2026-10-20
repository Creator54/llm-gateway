#!/usr/bin/env bash
set -euo pipefail
unset DOCKER_HOST DOCKER_CONTEXT
cd "$(dirname "$0")/.."
export GATEWAY_PORT="${GATEWAY_PORT:-10204}"
# Override saved deployment volume names before any destructive cleanup.
export OCX_STATE_VOLUME=llm-gateway-smoke_ocx-state
export CODEX_STATE_VOLUME=llm-gateway-smoke_codex-state
export OPENCODE_STATE_VOLUME=llm-gateway-smoke_opencode-state
compose=(podman compose -p llm-gateway-smoke -f compose.yaml -f compose.test.yaml)
# Fresh test-only volumes. Never use this script with real-provider state.
"${compose[@]}" down --volumes --remove-orphans
"${compose[@]}" run --rm --no-deps --entrypoint bun hub -e '
import { randomBytes } from "node:crypto";
import { bootstrapToken } from "./docker/bootstrap-token";
const token=randomBytes(32).toString("hex");
await bootstrapToken(new Response(token).body!);
const p="/home/bun/.opencodex/config.json";
const c=await Bun.file(p).json();
c.providers={smoke:{adapter:"openai-responses",baseUrl:"http://fake:10302/v1",authMode:"local",allowPrivateNetwork:true,models:["gateway-test"],defaultModel:"gateway-test"}};
c.defaultProvider="smoke";
await Bun.write(p,JSON.stringify(c));
'
"${compose[@]}" up -d --wait --wait-timeout 120 hub fake headroom
status=$(curl --silent --output /dev/null --write-out '%{http_code}' "http://127.0.0.1:$GATEWAY_PORT/v1/models")
[[ "$status" == 401 ]] || { echo "Unauthenticated request returned $status, expected 401" >&2; exit 1; }
echo 'unauthenticated_catalog=PASS'
"${compose[@]}" run --rm --no-deps check
"${compose[@]}" restart hub
"${compose[@]}" up -d --wait --wait-timeout 120 hub headroom
echo 'after_restart:'
"${compose[@]}" run --rm --no-deps check
# Guard against accidentally publishing the hub on any host interface.
hub_id=$("${compose[@]}" ps -q hub)
[[ "$(podman inspect "$hub_id" --format '{{len .HostConfig.PortBindings}}')" == 0 ]] || { echo 'Hub port unexpectedly published' >&2; exit 1; }
echo 'hub_unpublished=PASS'
echo 'Smoke stack left running. Stop: podman compose -p llm-gateway-smoke -f compose.yaml -f compose.test.yaml down --volumes'
