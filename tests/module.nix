{ nixpkgs, module, system ? "x86_64-linux" }:
let
  pkgs = nixpkgs.legacyPackages.${system};

  mkConfig = extra: (nixpkgs.lib.nixosSystem {
    inherit system;
    modules = [ module ({ lib, ... }: {
      # Matches the nixpkgs pinned in this flake (its .version is 26.11).
      system.stateVersion = "26.11";
      services.llm-gateway = {
        enable = true;
        package = pkgs.hello; # lightweight stand-in for the real gw binary
        apiKeyFile = "/run/secrets/gateway";
      } // extra;
    }) ];
  }).config;

  c = mkConfig { };
  invalid = mkConfig { apiKeyFile = "/nix/store/unsafe-key"; };
  badDir = mkConfig { stateDir = "not/a/single/name"; };
  service = c.systemd.services.llm-gateway;

  checks = [
    (service.serviceConfig.User == "llm-gateway")
    (service.serviceConfig.StateDirectory == "llm-gateway")
    (service.serviceConfig.StateDirectoryMode == "0700")
    (service.serviceConfig.LoadCredential == "api-key:/run/secrets/gateway")
    c.users.users.llm-gateway.isSystemUser
    # Force the unit body so a broken reference fails here, not on deploy.
    (service.environment.HOME == "/var/lib/llm-gateway")
    (service.serviceConfig.ReadWritePaths == [ "/var/lib/llm-gateway" ])
    (nixpkgs.lib.hasInfix "gw-start" service.serviceConfig.ExecStart)
    # The rendered config must be valid TOML (bare paths broke gw on phoenix).
    (let
      script = builtins.readFile service.serviceConfig.ExecStart;
      path = builtins.head (builtins.match ".*--config ([^ \n]+).*" script);
      cfgToml = builtins.fromTOML (builtins.readFile path);
    in cfgToml.state_dir == "/var/lib/llm-gateway" && cfgToml.api_key_file == "/var/lib/llm-gateway/api-key")
    # A key inside the Nix store is rejected.
    (builtins.any (a: !a.assertion && nixpkgs.lib.hasInfix "apiKeyFile" a.message) invalid.assertions)
    # So is a state dir that is not a single path segment.
    (builtins.any (a: !a.assertion && nixpkgs.lib.hasInfix "stateDir" a.message) badDir.assertions)
  ];
  failures = builtins.filter (x: !x) checks;
in
if failures == [ ] then
  pkgs.writeText "llm-gateway-module-check" "module options and assertions behave\n"
else
  throw "module check failed: ${toString (builtins.length checks - builtins.length failures)}/${toString (builtins.length checks)} assertions passed"
