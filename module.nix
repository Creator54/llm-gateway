{ config, lib, pkgs, ... }:
let
  cfg = config.services.llm-gateway;
  inherit (lib) mkEnableOption mkIf mkOption types;

  # Bootstrap: copy default config if absent, then load the client API token.
  hubStart = pkgs.writeShellScript "llm-gateway-hub-start" ''
    set -eu
    state="$OPENCODEX_HOME"
    mkdir -p "$state"
    if [ ! -f "$state/config.json" ]; then
      cp ${cfg.package}/share/opencodex/docker/config.json "$state/config.json"
    fi
    # Load the client API token from the credential file into the environment.
    export OPENCODEX_API_AUTH_TOKEN="$(cat "$CREDENTIALS_DIRECTORY/client-key")"
    exec ${cfg.package}/bin/ocx start --port ${toString cfg.hubPort}
  '';
in {
  options.services.llm-gateway = {
    enable = mkEnableOption "LLM gateway";

    package = mkOption {
      type = types.package;
      description = "The opencodex package to use.";
    };

    headroomPackage = mkOption {
      type = types.package;
      description = "The headroom package to use.";
    };

    # Container image outputs — kept as optional extras for Compose users.
    ocxImageStream = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = "Nix-built opencodex image stream (optional, for Docker/Compose users).";
    };
    headroomImageStream = mkOption {
      type = types.nullOr types.package;
      default = null;
      description = "Nix-built Headroom image stream (optional, for Docker/Compose users).";
    };

    apiKeyFile = mkOption {
      type = types.str;
      description = "Absolute runtime path to the client API key. Never store the key in the Nix store.";
    };

    port = mkOption {
      type = types.port;
      default = 10204;
      description = "Port Headroom listens on (exposed to nginx).";
    };

    hubPort = mkOption {
      type = types.port;
      default = 10100;
      description = "Internal port the OCX hub listens on.";
    };

    stateDir = mkOption {
      type = types.str;
      default = "llm-gateway";
      description = "Subdirectory under /var/lib for gateway state.";
    };
  };

  config = mkIf cfg.enable {
    assertions = [{
      assertion = lib.hasPrefix "/" cfg.apiKeyFile && !(lib.hasPrefix "/nix/store/" cfg.apiKeyFile);
      message = "services.llm-gateway.apiKeyFile must be an absolute path outside the Nix store.";
    }];

    users.users.llm-gateway = {
      isSystemUser = true;
      group = "llm-gateway";
      description = "LLM gateway service user";
    };
    users.groups.llm-gateway = {};

    systemd.services.llm-gateway-hub = {
      description = "LLM gateway hub (opencodex)";
      after = [ "network-online.target" ];
      wants = [ "network-online.target" ];
      wantedBy = [ "multi-user.target" ];

      environment = {
        OPENCODEX_HOME = "/var/lib/${cfg.stateDir}/ocx";
        CODEX_HOME     = "/var/lib/${cfg.stateDir}/codex";
        HOME           = "/var/lib/${cfg.stateDir}";
        NODE_ENV       = "production";
      };

      serviceConfig = {
        Type = "simple";
        User  = "llm-gateway";
        Group = "llm-gateway";
        StateDirectory = [ cfg.stateDir "${cfg.stateDir}/ocx" "${cfg.stateDir}/codex" ];
        StateDirectoryMode = "0700";
        LoadCredential = "client-key:${cfg.apiKeyFile}";
        ExecStart = hubStart;
        Restart = "on-failure";
        RestartSec = "5s";

        # Hardening
        NoNewPrivileges    = true;
        PrivateTmp         = true;
        ProtectSystem      = "strict";
        ProtectHome        = true;
        ReadWritePaths     = [ "/var/lib/${cfg.stateDir}" ];
        CapabilityBoundingSet = "";
        AmbientCapabilities   = "";
        LockPersonality    = true;
        MemoryDenyWriteExecute = false; # bun JIT needs this
        RestrictNamespaces = true;
        RestrictRealtime   = true;
        SystemCallFilter   = [ "@system-service" "~@privileged" ];
      };
    };

    systemd.services.llm-gateway-headroom = {
      description = "LLM gateway context proxy (headroom)";
      after = [ "network-online.target" "llm-gateway-hub.service" ];
      wants = [ "network-online.target" ];
      requires = [ "llm-gateway-hub.service" ];
      wantedBy = [ "multi-user.target" ];

      environment = {
        HOME                            = "/tmp";
        HEADROOM_BEACON                 = "off";
        DO_NOT_TRACK                    = "1";
        HEADROOM_ALLOW_UNAUTHENTICATED_BIND = "1";
      };

      serviceConfig = {
        Type = "simple";
        User  = "llm-gateway";
        Group = "llm-gateway";
        ExecStart = "${cfg.headroomPackage}/bin/headroom proxy --host 127.0.0.1 --port ${toString cfg.port} --openai-api-url http://127.0.0.1:${toString cfg.hubPort}";
        Restart = "on-failure";
        RestartSec = "5s";

        # Hardening
        NoNewPrivileges    = true;
        PrivateTmp         = true;
        ProtectSystem      = "strict";
        ProtectHome        = true;
        CapabilityBoundingSet = "";
        AmbientCapabilities   = "";
        LockPersonality    = true;
        MemoryDenyWriteExecute = false; # Python extensions need this
        RestrictNamespaces = true;
        RestrictRealtime   = true;
        SystemCallFilter   = [ "@system-service" "~@privileged" ];
      };
    };
  };
}
