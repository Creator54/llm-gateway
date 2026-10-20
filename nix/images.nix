{ pkgs, opencodex, headroom }:
let
  hubStart = pkgs.writeShellScript "gateway-hub-start" ''
    set -eu
    state="$HOME/.opencodex"
    ${pkgs.coreutils}/bin/mkdir -p "$state"
    if [ ! -f "$state/config.json" ]; then
      ${pkgs.coreutils}/bin/cp ${opencodex}/share/opencodex/docker/config.json "$state/config.json"
    fi
    if [ -n "''${OCX_API_TOKEN_FILE:-}" ]; then
      ${pkgs.bun}/bin/bun ${opencodex}/share/opencodex/docker/bootstrap-token.ts < "$OCX_API_TOKEN_FILE"
    fi
    exec ${opencodex}/bin/ocx start --port 10100
  '';
  identity = pkgs.runCommand "gateway-container-identity" {} ''
    mkdir -p "$out/etc" "$out/home/bun" "$out/tmp"
    printf 'root:x:0:0:root:/root:/bin/sh\nbun:x:1000:1000:Gateway:/home/bun:/bin/sh\n' > "$out/etc/passwd"
    printf 'root:x:0:\nbun:x:1000:\n' > "$out/etc/group"
    chmod 1777 "$out/tmp"
  '';
in {
  ocx-image = pkgs.dockerTools.streamLayeredImage {
    name = "llm-gateway-ocx";
    tag = opencodex.version;
    contents = [ identity pkgs.cacert pkgs.dockerTools.binSh ];
    fakeRootCommands = ''
      chown 1000:1000 home/bun
    '';
    config = {
      User = "1000:1000";
      WorkingDir = "${opencodex}/share/opencodex";
      Env = [ "HOME=/home/bun" "NODE_ENV=production" "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
      Cmd = [ "${hubStart}" ];
    };
  };
  headroom-image = pkgs.dockerTools.streamLayeredImage {
    name = "llm-gateway-headroom";
    tag = headroom.version;
    contents = [ identity pkgs.cacert pkgs.dockerTools.binSh ];
    config = {
      Env = [ "HOME=/tmp" "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt" ];
      Entrypoint = [ "${headroom}/bin/headroom" "proxy" ];
    };
  };
}
