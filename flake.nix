{
  description = "Personal LLM gateway";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
    in {
      nixosModules.default = import ./module.nix;
      packages = eachSystem (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.writeShellApplication {
            name = "llm-gateway";
            runtimeInputs = [ pkgs.podman pkgs.docker-compose pkgs.coreutils ];
            runtimeEnv = {
              GATEWAY_COMPOSE = "${self}/compose.yaml";
              PODMAN_COMPOSE_PROVIDER = "${pkgs.docker-compose}/bin/docker-compose";
            };
            text = builtins.readFile ./scripts/gateway.sh;
          };
        });
      apps = eachSystem (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.default}/bin/llm-gateway";
        };
      });
      devShells = eachSystem (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShellNoCC {
            packages = [ pkgs.just pkgs.git pkgs.podman pkgs.docker-compose pkgs.curl pkgs.wl-clipboard pkgs.xclip ];
            PODMAN_COMPOSE_PROVIDER = "${pkgs.docker-compose}/bin/docker-compose";
          };
        });
    };
}
