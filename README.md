# LLM gateway

Clients → Headroom → opencodex → providers.

## Local

Docker Compose or Podman Compose: `compose.yaml`, image settings in `.env`.

```sh
direnv allow       # load tools
just check         # build and test
just up
just status
just logs
just info          # addresses
just down          # keeps data
```

Port: `GATEWAY_PORT` in `.env` (default **10204**). Tokens:

```sh
just copy-token admin llm-gateway-hub-1
just copy-token api llm-gateway-hub-1
```

## Nix run

Set `OCX_IMAGE` and `HEADROOM_IMAGE` in
`~/.local/share/llm-gateway/images.env`. Requires the Podman API running.

```bash
read -rsp 'New API key: ' key; printf '\n'
printf '%s' "$key" | nix run github:Creator54/llm-gateway -- init
unset key
```

```sh
nix run github:Creator54/llm-gateway -- up
nix run github:Creator54/llm-gateway -- status
nix run github:Creator54/llm-gateway -- down
```

## NixOS

Input: `llm-gateway.url = "github:Creator54/llm-gateway";`

```nix
imports = [ inputs.llm-gateway.nixosModules.default ];
services.llm-gateway = {
  enable = true;
  apiKeyFile = "/run/secrets/llm-gateway-key";
  port = 10204;
};
```

Nix builds headless images from the sources pinned in `flake.lock`. Update the input lock to upgrade; switching loads the images and restarts the services. No Docker build or image archive is needed.

Key file must be readable by UID 1000 and outside the Nix store.
