#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
root=$PWD
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT
export HOME="$tmp/home" XDG_DATA_HOME="$tmp/data" GATEWAY_COMPOSE="$root/compose.yaml"
mkdir -p "$tmp/bin" "$XDG_DATA_HOME/llm-gateway"
printf '%s\n' '#!/bin/sh' 'printf "%s\n" "$@" > "$CAPTURE"' > "$tmp/bin/podman"
chmod +x "$tmp/bin/podman"
export PATH="$tmp/bin:$PATH" CAPTURE="$tmp/args"
bash scripts/gateway.sh --help >/dev/null
test ! -e "$CAPTURE"
if bash scripts/gateway.sh up 2>/dev/null; then exit 1; fi
test ! -e "$CAPTURE"
touch "$XDG_DATA_HOME/llm-gateway/images.env"
bash scripts/gateway.sh down
grep -qx 'down' "$CAPTURE"
! grep -q -- '--volumes' "$CAPTURE"
bash scripts/gateway.sh up
grep -qx -- '--no-build' "$CAPTURE"
if bash scripts/gateway.sh refresh 2>/dev/null; then exit 1; fi
echo 'launcher=PASS'
