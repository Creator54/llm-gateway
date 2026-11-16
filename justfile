set shell := ["bash", "-euo", "pipefail", "-c"]
set dotenv-load

default:
    @just --list

# Start the gateway.
up:
    @env -u DOCKER_HOST -u DOCKER_CONTEXT podman compose -p llm-gateway -f compose.yaml up -d --no-build --wait

# Stop the gateway; keep its data.
down:
    @env -u DOCKER_HOST -u DOCKER_CONTEXT podman compose -p llm-gateway -f compose.yaml down

status:
    @env -u DOCKER_HOST -u DOCKER_CONTEXT podman compose -p llm-gateway -f compose.yaml ps

logs:
    @env -u DOCKER_HOST -u DOCKER_CONTEXT podman compose -p llm-gateway -f compose.yaml logs --tail 100

# Show running gateway endpoints.
info:
    #!/usr/bin/env bash
    set -euo pipefail
    containers=$(podman ps --filter label=com.docker.compose.project=llm-gateway --filter label=com.docker.compose.service=headroom --format '{{{{.Names}}')
    if [[ -z "$containers" ]]; then echo 'LLM gateway: stopped'; exit 0; fi
    while IFS= read -r container; do
      address=$(podman port "$container" 8787/tcp)
      printf '\n%s\n  API        http://%s/v1\n  Dashboard  http://%s/v1#dashboard\n' "LLM gateway" "$address" "$address"
    done <<< "$containers"
    printf '\nCopy token: just copy-token admin|api llm-gateway-hub-1\n'

# Copy a token without printing it.
copy-token kind container:
    #!/usr/bin/env bash
    set -euo pipefail
    case {{quote(kind)}} in
      admin) file=admin-api-token ;;
      api) file=service-api-token ;;
      *) echo 'Choose admin or api.' >&2; exit 2 ;;
    esac
    container={{quote(container)}}
    service=$(podman inspect --format '{{{{index .Config.Labels "com.docker.compose.service"}}' "$container")
    [[ "$service" == hub ]] || { echo 'Select a gateway hub container.' >&2; exit 1; }
    if [[ -n "${WAYLAND_DISPLAY:-}" ]]; then clipboard=(wl-copy);
    elif [[ -n "${DISPLAY:-}" ]]; then clipboard=(xclip -selection clipboard);
    else echo 'No graphical clipboard available.' >&2; exit 1; fi
    command -v "${clipboard[0]}" >/dev/null
    token=$(podman exec "$container" bun -e 'const s=(await Bun.file("/home/bun/.opencodex/"+process.argv[1]).text()).trim();if(!s)process.exit(1);process.stdout.write(s)' "$file")
    printf '%s' "$token" | "${clipboard[@]}"
    unset token
    echo 'Token copied. Clear your clipboard after use.'

# Build the gw release binary.
build:
    cd gw && nix shell nixpkgs#cargo nixpkgs#rustc -c cargo build --release

# Build, then link gw into ~/.local/bin.
install: build
    install -d ~/.local/bin
    ln -sf "{{justfile_directory()}}/gw/target/release/gw" ~/.local/bin/gw

# Test with a fake provider, never existing credentials.
test:
    bash tests/smoke.sh

# Unit tests for the parity harness and the gateway.
test-unit:
    bun test tests/parity
    nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#clippy nixpkgs#rustfmt -c sh -c 'cd gw && cargo fmt --check && cargo test && cargo clippy --all-targets -- -D warnings'

# Build, then test.
check:
    just build
    just test
    just test-unit
