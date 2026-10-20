set shell := ["bash", "-euo", "pipefail", "-c"]
set dotenv-load

default:
    @just --list

# Fetch both gateway branches and build images.
build:
    #!/usr/bin/env bash
    set -euo pipefail
    mkdir -p .sources
    for repo in opencodex headroom; do
      dir=".sources/$repo"
      if [[ ! -d "$dir/.git" ]]; then
        git clone --single-branch --branch gateway "https://github.com/Creator54/$repo.git" "$dir"
      fi
      [[ -z "$(git -C "$dir" status --porcelain)" ]] || { echo "Uncommitted changes in $dir" >&2; exit 1; }
      git -C "$dir" fetch origin gateway
      git -C "$dir" checkout --detach FETCH_HEAD
    done
    ocx=$(git -C .sources/opencodex rev-parse HEAD)
    headroom=$(git -C .sources/headroom rev-parse HEAD)
    ocx_image="creator54/llm-gateway:ocx-$ocx"
    headroom_image="creator54/llm-gateway:headroom-$headroom"
    docker build --target runtime --label "org.opencontainers.image.revision=$ocx" -t "$ocx_image" .sources/opencodex
    docker build --target runtime-slim --build-arg HEADROOM_EXTRAS=proxy,code \
      --build-arg "HEADROOM_BUILD_VERSION=source-build+g$headroom" \
      --label "org.opencontainers.image.revision=$headroom" -t "$headroom_image" .sources/headroom
    printf 'OCX_IMAGE=%s\nHEADROOM_IMAGE=%s\n' "$ocx_image" "$headroom_image" > .env

# Test with a fake provider, never existing credentials.
test:
    bash tests/smoke.sh

# Build, then test.
check:
    just build
    just test
