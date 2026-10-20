{ nixpkgs, module, system ? "x86_64-linux" }:
let
  configured = key: (nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [ module {
      system.stateVersion = "25.11";
      services.llm-gateway = {
        enable = true;
        ocxImage = "example/ocx:test";
        headroomImage = "example/headroom:test";
        apiKeyFile = key;
      };
    } ];
  }).config;
  c = configured "/run/secrets/gateway";
  invalid = configured "/nix/store/unsafe-key";
in
assert c.virtualisation.oci-containers.backend == "podman";
assert c.virtualisation.oci-containers.containers.llm-gateway-headroom.ports == [ "127.0.0.1:10204:8787" ];
assert c.virtualisation.oci-containers.containers.llm-gateway-hub.ports == [];
assert builtins.elem "llm-gateway-network.service" c.systemd.services.podman-llm-gateway-hub.requires;
assert builtins.elem "podman-llm-gateway-hub.service" c.systemd.services.podman-llm-gateway-headroom.after;
assert builtins.any (a: !a.assertion && nixpkgs.lib.hasInfix "apiKeyFile" a.message) invalid.assertions;
true
