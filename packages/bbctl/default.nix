{
  fetchurl,
  ix,
  lib,
  stdenvNoCC,
  # Writer for `passthru.updateScript`, bound only on the flake-package path
  # (lib/packages.nix); the overlay path leaves it null so `pkgs.*` carries no
  # updater. Same nullable-writer pattern as vector-bin / claude-code.
  updateScriptWriter ? null,
}: let
  # Prebuilt upstream release binary, aarch64-darwin only: the motivating
  # consumer is the vmkit macOS guest's iMessage bridge (ENG-7746), which needs
  # the exact darwin arm64 bytes to push into the guest. Version + URL + SRI
  # hash live in the sibling pins.json, never inline here (repo policy). Bump
  # the version/url in pins.json, then `nix run .#update` re-pins the hash.
  pin = ix.pins.loadPin ./pins.json "bbctl";
  updateScript = ix.pins.mkOptionalUpdater {
    writeNushellApplication = updateScriptWriter;
    nix = ix.nixPackageFor "packages/bbctl: updateScript";
    pname = "bbctl";
    relPath = "packages/bbctl/pins.json";
  };
in
  stdenvNoCC.mkDerivation {
    pname = "bbctl";
    inherit (pin) version;

    src = fetchurl {inherit (pin) url hash;};
    dontUnpack = true;

    # Upstream release binary; nothing to test at build time.
    doCheck = false;

    passthru = lib.optionalAttrs (updateScript != null) {inherit updateScript;};

    installPhase = ''
      # shell
      runHook preInstall

      install -Dm755 "$src" "$out/bin/bbctl"

      runHook postInstall
    '';

    meta = {
      description = "Beeper bridge-manager CLI: run self-hosted Matrix bridges on a Beeper account";
      homepage = "https://github.com/beeper/bridge-manager";
      license = lib.licenses.asl20;
      mainProgram = "bbctl";
      platforms = ["aarch64-darwin"];
      sourceProvenance = [lib.sourceTypes.binaryNativeCode];
    };
  }
