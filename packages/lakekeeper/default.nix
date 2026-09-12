# Lakekeeper, the Apache Iceberg REST catalog (Rust), shipped as an upstream
# prebuilt binary like vector-bin.
{
  autoPatchelfHook,
  fetchzip,
  ix,
  lib,
  stdenv,
  # Writer for `passthru.updateScript` (flake-package path only); null on the
  # overlay path. Same nullable-writer pattern as claude-code / yc.
  updateScriptWriter ? null,
}: let
  # Add a target here, with its own release hash, before building on another
  # arch. The package-set/flake targets and meta.platforms below gate the arch.
  targets = {
    x86_64-linux = "x86_64-unknown-linux-gnu";
  };
  # Version + per-release URL and SRI hash live in the sibling pins.json, never
  # inline (repo policy). The pin is `prefetch = manual`: this fetchzip runs
  # with `stripRoot = false`, whose output tree no prefetch command reproduces
  # (verified: both flat and --unpack hashing differ), so after editing the
  # version/url refresh the hash by building and copying the `got:` hash from
  # the mismatch error. The updater deliberately skips it rather than write a
  # wrong hash.
  pin = ix.pins.loadPin ./pins.json "lakekeeper";
  updateScript = ix.pins.mkOptionalUpdater {
    writeNushellApplication = updateScriptWriter;
    nix = ix.nixPackageFor "packages/lakekeeper: updateScript";
    pname = "lakekeeper";
    relPath = "packages/lakekeeper/pins.json";
  };
in
  stdenv.mkDerivation {
    pname = "lakekeeper";
    inherit (pin) version;

    # Upstream ships a single bare `lakekeeper` binary in the tarball (no wrapping
    # directory), so stripRoot must stay off.
    src = fetchzip {
      inherit (pin) url hash;
      stripRoot = false;
    };

    passthru = lib.optionalAttrs (updateScript != null) {inherit updateScript;};

    # Upstream release binary; nothing to test at build time.
    doCheck = false;

    nativeBuildInputs = [autoPatchelfHook];
    buildInputs = [
      stdenv.cc.cc.lib
      stdenv.cc.libc
    ];

    installPhase = ''
      # shell
      runHook preInstall
      install -Dm755 "$src/lakekeeper" "$out/bin/lakekeeper"
      runHook postInstall
    '';

    meta = {
      description = "Apache Iceberg REST Catalog written in Rust";
      homepage = "https://lakekeeper.io";
      license = lib.licenses.asl20;
      mainProgram = "lakekeeper";
      platforms = builtins.attrNames targets;
      sourceProvenance = [lib.sourceTypes.binaryNativeCode];
    };
  }
