{
  lib,
  stdenvNoCC,
  fetchurl,
  ix,
  makeWrapper,
  nodejs_22,
  # Writer used to build `passthru.updateScript`. Bound to a real builder only on
  # the flake-package path (lib/packages.nix), which is where
  # `nix run .#humanlayer.updateScript` resolves; the overlay path leaves it
  # null, so `pkgs.humanlayer` carries no updateScript. Same pattern as
  # packages/yc and packages/claude-code.
  updateScriptWriter ? null,
}: let
  # Version and per-platform hashes live in manifest.json as the single owner;
  # this file only reads them back. Refresh with
  # `nix run .#humanlayer.updateScript` (see the updateScript below). Mirrors the
  # packages/yc layout.
  manifest = lib.importJSON ./manifest.json;
  inherit (manifest) version cliHash;

  # The `@humanlayer/cli` npm launcher is a thin JS shim that execs a
  # platform-specific, bun-compiled standalone binary shipped in a sibling
  # package (`@humanlayer/cli-<slug>`). Install both packages side by side,
  # matching npm's node_modules layout, because the standalone binary starts as
  # raw Bun when invoked outside the launcher.
  baseUrl = "https://registry.npmjs.org/@humanlayer";

  inherit (stdenvNoCC.hostPlatform) system;
  target =
    manifest.platforms.${system}
      or (throw "humanlayer: no prebuilt binary for ${system}; supported: ${lib.concatStringsSep ", " (builtins.attrNames manifest.platforms)}");

  cliSrc = fetchurl {
    url = "${baseUrl}/cli/-/cli-${version}.tgz";
    hash = cliHash;
  };
  platformSrc = fetchurl {
    url = "${baseUrl}/cli-${target.slug}/-/cli-${target.slug}-${version}.tgz";
    inherit (target) hash;
  };

  # Tracks the upstream npm `latest` dist-tag and refreshes manifest.json with
  # the per-platform SRI hashes. HumanLayer publishes no signed manifest, so
  # there is NO provenance check: the updater pins whatever bytes the npm
  # registry serves. The `nix build .#humanlayer` step in the update workflow
  # only proves the pinned bytes fetch and patch on x86_64-linux, not that they
  # are authentic, so the real gate is human review of the hash changes in the
  # auto-bump PR. Same posture as packages/yc.
  updateScriptArgs = {
    name = "humanlayer-update";
    runtimeInputs = [(ix.nixPackageFor "packages/humanlayer: updateScript")];
    meta.description = "Refresh packages/humanlayer/manifest.json to the latest HumanLayer CLI release";
    text = ''
      # nu
      const base = "https://registry.npmjs.org/@humanlayer"
      const slugs = {
        "x86_64-linux": "linux-x64",
        "aarch64-linux": "linux-arm64",
        "aarch64-darwin": "darwin-arm64",
        "x86_64-darwin": "darwin-x64"
      }

      # Run from the repo root: `nix run .#humanlayer.updateScript -- [version]`.
      # Without a version argument it tracks the upstream npm `latest` tag.
      def main [version?: string] {
        let v = ($version | default (http get $"($base)/cli/latest" | get version))
        let cli_hash = (^nix store prefetch-file --json $"($base)/cli/-/cli-($v).tgz" | from json | get hash)
        let platforms = (
          $slugs
          | transpose system slug
          | reduce --fold {} {|row acc|
              let url = $"($base)/cli-($row.slug)/-/cli-($row.slug)-($v).tgz"
              let sri = (^nix store prefetch-file --json $url | from json | get hash)
              $acc | insert $row.system { slug: $row.slug, hash: $sri }
            }
        )
        let out = "packages/humanlayer/manifest.json"
        { version: $v, cliHash: $cli_hash, platforms: $platforms } | to json --indent 2 | save --force $out
        print $"updated ($out) to ($v)"
      }
    '';
  };
in
  stdenvNoCC.mkDerivation {
    pname = "humanlayer";
    inherit version;
    dontUnpack = true;

    # Upstream npm tarball repack; nothing to test at build time.
    doCheck = false;

    nativeBuildInputs = [
      makeWrapper
      nodejs_22
    ];

    # Do not auto-patchelf the platform payload. The Linux package is a Bun
    # standalone executable with embedded application metadata; patchelfing it
    # makes it start as generic `bun`, so `humanlayer daemon ...` fails with
    # "Script not found". The npm JS launcher below executes the payload as
    # published, which is the form verified on NixOS.
    dontStrip = true;

    installPhase = ''
      # shell
      runHook preInstall

      mkdir -p \
        "$out/lib/node_modules/@humanlayer/cli" \
        "$out/lib/node_modules/@humanlayer/cli-${target.slug}" \
        "$out/bin"

      tar -xzf ${cliSrc} -C "$out/lib/node_modules/@humanlayer/cli" --strip-components=1
      tar -xzf ${platformSrc} -C "$out/lib/node_modules/@humanlayer/cli-${target.slug}" --strip-components=1

      patchShebangs "$out/lib/node_modules/@humanlayer/cli/bin/humanlayer"
      wrapProgram "$out/lib/node_modules/@humanlayer/cli/bin/humanlayer" \
        --prefix PATH : "${lib.makeBinPath [nodejs_22]}"
      ln -s "$out/lib/node_modules/@humanlayer/cli/bin/humanlayer" "$out/bin/humanlayer"

      runHook postInstall
    '';

    passthru = lib.optionalAttrs (updateScriptWriter != null) {
      updateScript = updateScriptWriter updateScriptArgs;
    };

    meta = {
      description = "HumanLayer CLI: manage the riptide remote daemon, agents, and HumanLayer API";
      homepage = "https://humanlayer.com";
      # License omitted rather than `licenses.unfree` so the per-system flake
      # package set (which evaluates nixpkgs without `allowUnfree`) can still
      # `nix run .#humanlayer`. Same posture as packages/yc. Distribution terms are
      # HumanLayer's; this flake only repackages the published binaries.
      mainProgram = "humanlayer";
      platforms = builtins.attrNames manifest.platforms;
      sourceProvenance = [lib.sourceTypes.binaryNativeCode];
    };
  }
