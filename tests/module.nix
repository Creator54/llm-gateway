{ nixpkgs, module, system ? "x86_64-linux" }:
let
  pkgs = nixpkgs.legacyPackages.${system};

  mkConfig = extra: (nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [ module ({ lib, ... }: {
      system.stateVersion = "25.11";
      services.llm-gateway = {
        enable = true;
        package = pkgs.hello;          # lightweight stand-in
        headroomPackage = pkgs.hello;
        apiKeyFile = "/run/secrets/gateway";
      } // extra;
    }) ];
  }).config;

  c = mkConfig {};
  invalid = mkConfig { apiKeyFile = "/nix/store/unsafe-key"; };
in
assert c.systemd.services.llm-gateway-hub.serviceConfig.User == "llm-gateway";
assert c.systemd.services.llm-gateway-headroom.serviceConfig.User == "llm-gateway";
assert builtins.elem "llm-gateway-hub.service" c.systemd.services.llm-gateway-headroom.requires;
assert builtins.elem "llm-gateway-hub.service" c.systemd.services.llm-gateway-headroom.after;
assert c.users.users.llm-gateway.isSystemUser;
assert builtins.any (a: !a.assertion && nixpkgs.lib.hasInfix "apiKeyFile" a.message) invalid.assertions;
true
