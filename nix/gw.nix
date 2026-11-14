# The Rust gateway. Source is filtered to gw/ so docs, nix and the old
# TypeScript/Bun trees never enter the build inputs.
{ lib, rustPlatform }:
let
  root = ../.;
  cargoManifest = builtins.fromTOML (builtins.readFile (root + "/gw/Cargo.toml"));
in
rustPlatform.buildRustPackage {
  pname = "gw";
  version = cargoManifest.package.version;

  src = lib.fileset.toSource {
    inherit root;
    fileset = lib.fileset.difference
      (root + "/gw")
      (lib.fileset.maybeMissing (root + "/gw/target"));
  };

  # cargoRoot locates Cargo.lock for the vendor check; buildAndTestSubdir is the
  # directory cargo runs in. Both are needed for this layout.
  cargoRoot = "gw";
  buildAndTestSubdir = "gw";
  cargoLock.lockFile = root + "/gw/Cargo.lock";

  # reqwest uses rustls, so no openssl and no runtime dependencies at all.
  meta = {
    description = "LLM gateway: one Rust binary routing Responses, Chat and Messages to upstream providers";
    license = lib.licenses.mit;
    mainProgram = "gw";
    platforms = lib.platforms.linux;
  };
}
