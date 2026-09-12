{
  lib,
  stdenv,
  fetchurl,
  ix,
  undmg,
  # Writer used to build `passthru.updateScript`. Only the flake package set
  # supplies it (lib/packages.nix); the overlay eval context leaves it null. The
  # updater is a maintainer-facing flake output, so the overlay build of
  # `pkgs.dia` simply omits `passthru.updateScript`.
  updateScriptWriter ? null,
}: let
  # Version and content hash are generated, never hand-edited. Bump with
  # `nix run .#dia.updateScript -- [version]`, which resolves the upstream
  # `Dia-latest.dmg` pointer (or a pinned version) and rewrites manifest.json.
  # We pin by raw bytes so the download is reproducible: unlike Homebrew, which
  # only lets you freeze an already-installed cask (`brew pin`) and has no
  # supported way to fetch an arbitrary older version, the hash pin makes any
  # exact version installable and verifiable.
  #
  # `version` is "<short>-<build>" (e.g. "1.34.1-81705"), matching the upstream
  # download filename `Dia-<version>.dmg`.
  manifest = lib.importJSON ./manifest.json;
  inherit (manifest) version hash;

  # Refreshes manifest.json to a Dia release. With no argument it tracks the
  # `Dia-latest.dmg` pointer, reading the resolved version out of the
  # Content-Disposition filename the CDN returns (there is no appcast/manifest);
  # pass a version to pin an exact one. `nix store prefetch-file` downloads the
  # .dmg once and emits the SRI hash the fetcher pins.
  updateScriptArgs = {
    name = "dia-update";
    runtimeInputs = [(ix.nixPackageFor "packages/dia: updateScript")];
    meta.description = "Refresh packages/dia/manifest.json to a Dia release";
    text = ''
      # nu
      const base = "https://releases.diabrowser.com/release"

      # Run from the repo root: `nix run .#dia.updateScript -- [version]`.
      def main [version?: string] {
        let v = ($version | default (
          http head $"($base)/Dia-latest.dmg"
          | where name == "content-disposition"
          | get value.0
          | parse --regex 'filename="?Dia-(?<v>[^"]+)\.dmg'
          | get v.0
        ))
        let url = $"($base)/Dia-($v).dmg"
        let hash = (^nix store prefetch-file --json --hash-type sha256 $url | from json | get hash)
        let out = "packages/dia/manifest.json"
        { version: $v, hash: $hash } | to json --indent 2 | save --force $out
        print $"updated ($out) to ($v)"
      }
    '';
  };
in
  stdenv.mkDerivation {
    pname = "dia";
    inherit version;

    src = fetchurl {
      url = "https://releases.diabrowser.com/release/Dia-${version}.dmg";
      inherit hash;
    };

    nativeBuildInputs = [undmg];
    # undmg's unpackCmd hook extracts the .app straight into the build dir.
    sourceRoot = ".";

    dontConfigure = true;
    dontBuild = true;
    # Signed upstream .app bundle, installed verbatim; nothing to test.
    doCheck = false;
    # The bundle is signed and notarized; any byte rewrite (strip, patch, the
    # default fixup) would void the code signature, so install it verbatim.
    dontFixup = true;

    installPhase = ''
      # shell
      runHook preInstall
      mkdir -p "$out/Applications" "$out/bin"
      cp -R Dia.app "$out/Applications/Dia.app"
      # `nix run .#dia` and a PATH `dia` launch the bundle executable directly.
      ln -s "$out/Applications/Dia.app/Contents/MacOS/Dia" "$out/bin/dia"
      runHook postInstall
    '';

    passthru = lib.optionalAttrs (updateScriptWriter != null) {
      updateScript = updateScriptWriter updateScriptArgs;
    };

    meta = {
      description = "Dia, The Browser Company's AI browser";
      homepage = "https://www.diabrowser.com";
      # License omitted rather than `licenses.unfree`: the per-system flake
      # package set evaluates nixpkgs without `allowUnfree`, so tagging this
      # proprietary bundle unfree would block `nix run .#dia`. Distribution terms
      # are The Browser Company's Dia license.
      mainProgram = "dia";
      platforms = ["aarch64-darwin"];
      sourceProvenance = [lib.sourceTypes.binaryNativeCode];
    };
  }
