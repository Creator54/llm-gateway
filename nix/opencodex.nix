{ lib, stdenvNoCC, bun, gitMinimal, cacert, makeWrapper, opencode, src }:
let
  dependencies = stdenvNoCC.mkDerivation {
    pname = "opencodex-dependencies";
    version = "${src.shortRev or "source"}-${stdenvNoCC.hostPlatform.system}";
    inherit src;
    nativeBuildInputs = [ bun cacert ];
    dontConfigure = true;
    dontFixup = true;
    buildPhase = ''
      export HOME="$TMPDIR/home"
      mkdir -p "$HOME"
      bun install --frozen-lockfile --production --ignore-scripts --cpu ${if stdenvNoCC.hostPlatform.isAarch64 then "arm64" else "x64"}
    '';
    installPhase = ''
      mkdir -p "$out"
      cp -R node_modules "$out/node_modules"
    '';
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = {
      x86_64-linux = "sha256-c+KuBdGl5Yogqn8WYICNn8k5luwhKR8idzyCNIYL72M=";
      # Platform-specific optional npm dependencies require a separate hash.
      aarch64-linux = "sha256-IHUUDo072fDtLgslZiJ5GVAg43OP7wX0Cvq+53uyUnU=";
    }.${stdenvNoCC.hostPlatform.system};
  };
in stdenvNoCC.mkDerivation {
  pname = "opencodex";
  version = src.shortRev or "source";
  inherit src;
  nativeBuildInputs = [ bun gitMinimal makeWrapper ];
  dontConfigure = true;
  buildPhase = ''
    runHook preBuild
    export HOME="$TMPDIR/home"
    mkdir -p "$HOME"
    cp -R ${dependencies}/node_modules .
    chmod -R u+w node_modules
    git init -q
    git add -f . ':!node_modules' ':!gui/node_modules'
    bun scripts/generate-compatibility-version.ts
    runHook postBuild
  '';
  installPhase = ''
    runHook preInstall
    mkdir -p "$out/share/opencodex" "$out/bin"
    cp -R package.json bun.lock node_modules src scripts docker "$out/share/opencodex/"
    makeWrapper ${bun}/bin/bun "$out/bin/ocx" \
      --add-flags "run $out/share/opencodex/src/cli/index.ts" \
      --prefix PATH : ${lib.makeBinPath [ bun opencode ]}
    runHook postInstall
  '';
  passthru = { inherit dependencies; };
}
