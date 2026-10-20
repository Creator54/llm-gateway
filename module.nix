{ config, lib, ... }:
let
  cfg = config.services.llm-gateway;
  inherit (lib) mkEnableOption mkIf mkOption types;
in {
  options.services.llm-gateway = {
    enable = mkEnableOption "LLM gateway";
    ocxImage = mkOption { type = types.str; description = "opencodex image reference, preferably pinned by digest."; };
    headroomImage = mkOption { type = types.str; description = "Headroom image reference, preferably pinned by digest."; };
    apiKeyFile = mkOption {
      type = types.str;
      description = "Absolute runtime path to the client API key, readable by container UID 1000. Never put the key in the Nix store.";
    };
    port = mkOption { type = types.port; default = 10204; description = "Localhost API port."; };
  };

  config = mkIf cfg.enable {
    assertions = [{
      assertion = lib.hasPrefix "/" cfg.apiKeyFile && !(lib.hasPrefix "/nix/store/" cfg.apiKeyFile);
      message = "services.llm-gateway.apiKeyFile must be an absolute path outside the Nix store.";
    }];
    virtualisation.podman.enable = true;
    virtualisation.oci-containers = {
      backend = "podman";
      containers = {
        llm-gateway-hub = {
          image = cfg.ocxImage;
          environment.OCX_API_TOKEN_FILE = "/run/secrets/client-key";
          volumes = [
            "llm-gateway-ocx:/home/bun/.opencodex"
            "llm-gateway-codex:/home/bun/.codex"
            "llm-gateway-opencode:/home/bun/.local"
            "${cfg.apiKeyFile}:/run/secrets/client-key:ro"
          ];
          extraOptions = [
            "--network=llm-gateway" "--network-alias=hub" "--init" "--read-only"
            "--cap-drop=ALL" "--security-opt=no-new-privileges"
            "--tmpfs=/tmp:size=64m,mode=1777"
            "--tmpfs=/home/bun/.cache:mode=1777"
            "--tmpfs=/home/bun/.config:mode=1777"
          ];
        };
        llm-gateway-headroom = {
          image = cfg.headroomImage;
          dependsOn = [ "llm-gateway-hub" ];
          cmd = [ "--host" "0.0.0.0" "--port" "8787" "--openai-api-url" "http://hub:10100" ];
          ports = [ "127.0.0.1:${toString cfg.port}:8787" ];
          environment = {
            HEADROOM_BEACON = "off";
            DO_NOT_TRACK = "1";
            HEADROOM_ALLOW_UNAUTHENTICATED_BIND = "1";
          };
          extraOptions = [ "--network=llm-gateway" "--cap-drop=ALL" "--security-opt=no-new-privileges" ];
        };
      };
    };
    systemd.services.llm-gateway-network = {
      description = "LLM gateway container network";
      serviceConfig = { Type = "oneshot"; RemainAfterExit = true; };
      path = [ config.virtualisation.podman.package ];
      script = ''
        podman network exists llm-gateway || podman network create llm-gateway
      '';
    };
    systemd.services.podman-llm-gateway-hub = {
      requires = [ "llm-gateway-network.service" ];
      after = [ "llm-gateway-network.service" ];
    };
    systemd.services.podman-llm-gateway-headroom = {
      requires = [ "llm-gateway-network.service" ];
      after = [ "llm-gateway-network.service" ];
    };
  };
}
