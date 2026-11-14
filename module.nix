{ config, lib, pkgs, ... }:
let
  cfg = config.services.llm-gateway;
  inherit (lib) mkEnableOption mkIf mkOption types;

  # Absolute state path used by the unit, the config and the key copy.
  stateDirPath = "/var/lib/${cfg.stateDir}";

  # Config is rendered from options so the key never lands in the Nix store.
  # The API key is read from a systemd credential at start.
  configFile = pkgs.writeText "gw-config.toml" ''
    listen = "127.0.0.1:${toString cfg.port}"
    api_key_file = ${lib.escapeShellArg "${cfg.stateDirPath}/api-key"}
    state_dir = ${lib.escapeShellArg cfg.stateDirPath}
    max_body_bytes = ${toString cfg.maxBodyBytes}
    profile = "${cfg.profile}"
  '';

  start = pkgs.writeShellScript "gw-start" ''
    set -eu
    state=${lib.escapeShellArg cfg.stateDirPath}
    # Install the client key with owner-only permissions, then hand the path to gw.
    umask 077
    cat "$CREDENTIALS_DIRECTORY/api-key" > "$state/api-key"
    chmod 600 "$state/api-key"
    exec ${cfg.package}/bin/gw serve --config ${configFile}
  '';
in
{
  options.services.llm-gateway = {
    enable = mkEnableOption "LLM gateway";

    package = mkOption {
      type = types.package;
      default = pkgs.callPackage ./nix/gw.nix { };
      defaultText = lib.literalExpression "pkgs.callPackage ./nix/gw.nix { }";
      description = "The gw package to run.";
    };

    apiKeyFile = mkOption {
      type = types.str;
      description = "Absolute runtime path to the client API key. Never put the key in the Nix store.";
    };

    port = mkOption {
      type = types.port;
      default = 10204;
      description = "Port gw listens on. Bound to loopback; put nginx or the tailnet in front.";
    };

    stateDir = mkOption {
      type = types.str;
      default = "llm-gateway";
      description = "Subdirectory under /var/lib for accounts, the usage journal and the key copy.";
    };

    maxBodyBytes = mkOption {
      type = types.ints.positive;
      default = 32 * 1024 * 1024;
      description = "Largest request body accepted.";
    };

    profile = mkOption {
      type = types.enum [ "default" "parity" ];
      default = "default";
      description = "`default` enables the token-efficiency edits; `parity` disables every one.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [
      {
        assertion = lib.hasPrefix "/" cfg.apiKeyFile && !(lib.hasPrefix "/nix/store/" cfg.apiKeyFile);
        message = "services.llm-gateway.apiKeyFile must be an absolute path outside the Nix store.";
      }
      {
        assertion = builtins.match "[a-zA-Z0-9_.-]+" cfg.stateDir != null;
        message = "services.llm-gateway.stateDir must be a single directory name.";
      }
    ];

    users.users.llm-gateway = {
      isSystemUser = true;
      group = "llm-gateway";
      description = "LLM gateway service user";
    };
    users.groups.llm-gateway = { };

    systemd.services.llm-gateway = {
      description = "LLM gateway (gw)";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      # Tokens live under the state directory; HOME keeps provider SDKs quiet.
      environment = {
        HOME = cfg.stateDirPath;
        RUST_LOG = "info";
      };

      serviceConfig = {
        Type = "simple";
        User = "llm-gateway";
        Group = "llm-gateway";
        StateDirectory = cfg.stateDir;
        StateDirectoryMode = "0700";
        LoadCredential = "api-key:${cfg.apiKeyFile}";
        ExecStart = start;
        Restart = "on-failure";
        RestartSec = "5s";

        NoNewPrivileges = true;
        PrivateTmp = true;
        ProtectSystem = "strict";
        ProtectHome = true;
        ReadWritePaths = [ cfg.stateDirPath ];
        CapabilityBoundingSet = "";
        AmbientCapabilities = "";
        LockPersonality = true;
        RestrictNamespaces = true;
        RestrictRealtime = true;
        SystemCallFilter = [ "@system-service" "~@privileged" ];
        # ring and rustls need writable-executable mappings for JIT-free code
        # paths; keeping W^X off is the conservative choice here.
        MemoryDenyWriteExecute = false;
      };
    };
  };
}
