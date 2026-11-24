{
  description = "Personal LLM gateway";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = { self, nixpkgs }:
    let
      systems = [ "x86_64-linux" "aarch64-linux" ];
      eachSystem = nixpkgs.lib.genAttrs systems;
    in {
      nixosModules.default = ./module.nix;
      packages = eachSystem (system:
        let gw = nixpkgs.legacyPackages.${system}.callPackage ./nix/gw.nix { };
        in { inherit gw; default = gw; });
      apps = eachSystem (system: {
        default = {
          type = "app";
          program = "${self.packages.${system}.gw}/bin/gw";
        };
      });
      checks = eachSystem (system: {
        gw = self.packages.${system}.gw;
        module = import ./tests/module.nix {
          nixpkgs = nixpkgs;
          module = self.nixosModules.default;
          inherit system;
        };
      });
      devShells = eachSystem (system:
        let pkgs = nixpkgs.legacyPackages.${system};
        in {
          default = pkgs.mkShell {
            packages = [ pkgs.just pkgs.cargo pkgs.rustc pkgs.clippy pkgs.rustfmt pkgs.rust-analyzer pkgs.bun ];
          };
        });
    };
}
