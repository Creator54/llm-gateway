{
  description = "Personal LLM gateway";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    headroom-src = {
      url = "github:Creator54/headroom/gateway";
      flake = false;
    };
    opencodex-src = {
      url = "github:Creator54/opencodex/gateway";
      flake = false;
    };
  };

  outputs = { self, nixpkgs, opencodex-src, headroom-src }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
    in {
      nixosModules.default = { pkgs, ... }: {
        imports = [ ./module.nix ];
        services.llm-gateway = {
          package         = nixpkgs.lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.opencodex;
          headroomPackage = nixpkgs.lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.headroom;
          ocxImageStream  = nixpkgs.lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.ocx-image;
          headroomImageStream = nixpkgs.lib.mkDefault self.packages.${pkgs.stdenv.hostPlatform.system}.headroom-image;
        };
      };
      packages = eachSystem (system:
        let
          pkgs = nixpkgs.legacyPackages.${system};
          opencodex = pkgs.callPackage ./nix/opencodex.nix {
            src = opencodex-src;
          };
          headroom = pkgs.callPackage ./nix/headroom.nix { src = headroom-src; };
        in (import ./nix/images.nix { inherit pkgs opencodex headroom; }) // {
          inherit opencodex headroom;
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
