# Patch the `rnix` crate inside a rust tool's vendored cargo dependencies so
# the tool lexes underscore digit separators in nix numeric literals
# (`1_000`, `1_000.000_1`, `2.5e1_0`), the dialect the checked Nix view
# accepts. alejandra, statix, and deadnix all parse nix
# through rnix, and none of them can format or lint a tree using separators
# until their tokenizer takes them; their package dirs under packages/nix/
# apply this to the nixpkgs builds.
#
# Mechanics: nixpkgs vendors each tool's locked dependencies into a
# fixed-output vendor dir. Older vendorer generations copied it to a writable
# `$cargoDepsCopy`; the current one points cargo's `.cargo/config.toml`
# straight at the read-only store dir and exports only `$cargoDeps`. Handle
# both from `preBuild` (after every cargo-setup variant, before cargo reads a
# line of the crate): patch the writable copy when one exists, otherwise make
# our own copy of the store vendor dir, patch that, and repoint the cargo
# config -- either way no new fixed-output hash.
#
# The tokenizer moved across rnix releases, so the overlay source is selected
# by the vendored version, and an unknown version fails the build with
# instructions (a nixpkgs bump onto a new rnix minor adds a view, not silence).
# Each flavor is a jj view (views.toml) of the matching nix-community/rnix-parser tag,
# so the flavor's `src/` tree is
# the patched tokenizer and this overlays it over the vendored crate. The
# `.cargo-checksum.json` files the nixpkgs vendorers write carry no per-file
# hashes (`"files": {}`), so the edit needs no checksum rewrite; the guard
# keeps that assumption loud instead of latent.
#
# Semantics mirror the flex patch exactly: one or more `_` between two
# digits of the integer part, the fraction, or the exponent; never leading
# (`_1_000` stays an identifier) or trailing (`1_` still lexes as `1` then
# `_`). Token text keeps the separators, so a formatter passes them through
# verbatim.
{
  # The two rnix-parser views replace the vendored crate's `src/` directory.
  rnix012Src,
  rnix014Src,
}: tool:
tool.overrideAttrs (old: {
  preBuild =
    (old.preBuild or "")
    + ''
      # The vendor tree cargo actually reads is whatever the cargo setup hook
      # wrote into .cargo/config.toml -- across vendorer generations that has
      # been a writable $cargoDepsCopy, the read-only store output, or an
      # unpacked tree elsewhere in the build dir. The config is the one
      # authoritative pointer, so read it from there; prefer $cargoDepsCopy
      # only when a generation still exports it.
      rnixVendor="''${cargoDepsCopy:-}"
      rnixConfig=""
      if [ -z "$rnixVendor" ] && [ -n "''${cargoDeps:-}" ]; then
        # The current generation still copies the vendor tree to
        # $NIX_BUILD_TOP/<stripped-cargoDeps-name> (the alejandra build log
        # shows /build/alejandra-4.0.0-vendor: cargoSetupPostUnpackHook ran,
        # and cargoSetupPostPatchHook validates that copy's Cargo.lock) but
        # keeps the variable local to the hook. Reconstruct the copy's path
        # the way the hook names it; cargo is already pointed at it, so
        # patching in place needs no config rewrite.
        rnixHookCopy="$NIX_BUILD_TOP/$(stripHash "$cargoDeps")"
        if [ -d "$rnixHookCopy" ]; then
          rnixVendor="$rnixHookCopy"
        fi
      fi
      if [ -z "$rnixVendor" ]; then
        # Generations differ on where the vendored-sources config lands:
        # $CARGO_HOME, the source root's .cargo, or the build top's .cargo
        # (one level above the source root -- cargo finds it by walking up
        # parent directories).
        for rnixConfigCandidate in \
          ''${CARGO_HOME:+"$CARGO_HOME/config.toml" "$CARGO_HOME/config"} \
          .cargo/config.toml .cargo/config \
          "$NIX_BUILD_TOP/.cargo/config.toml" "$NIX_BUILD_TOP/.cargo/config"; do
          [ -f "$rnixConfigCandidate" ] || continue
          rnixVendor=$(sed -n 's/^[[:space:]]*directory[[:space:]]*=[[:space:]]*"\(.*\)"/\1/p' "$rnixConfigCandidate" | head -n 1)
          if [ -n "$rnixVendor" ]; then
            rnixConfig="$rnixConfigCandidate"
            break
          fi
        done
      fi
      if [ -z "$rnixVendor" ]; then
        echo "rnix-digit-separators: no cargoDepsCopy and no vendored-sources directory in any cargo config" >&2
        echo "CARGO_HOME=''${CARGO_HOME:-unset} cwd=$PWD" >&2
        find "$NIX_BUILD_TOP" -maxdepth 4 -name "config*" -path "*cargo*" 2>/dev/null | head -5 >&2
        exit 1
      fi
      if [ ! -w "$rnixVendor" ]; then
        rnixWritable="$NIX_BUILD_TOP/rnix-digit-separators-vendor"
        rm -rf "$rnixWritable"
        cp -r --reflink=auto "$rnixVendor" "$rnixWritable"
        chmod -R u+w "$rnixWritable"
        if [ -n "$rnixConfig" ]; then
          sed -i "s|$rnixVendor|$rnixWritable|g" "$rnixConfig"
        fi
        rnixVendor="$rnixWritable"
      fi
      patchedRnix=0
      # fetchCargoVendor lays crates out one level down, under one directory
      # per source (source-registry-0, source-git-*); older vendorers kept
      # them flat at the vendor root. Glob both levels.
      for rnixDir in "$rnixVendor"/rnix-* "$rnixVendor"/*/rnix-*; do
        [ -d "$rnixDir" ] || continue
        version=$(basename "$rnixDir")
        case "$version" in
          rnix-0.11.* | rnix-0.12.*) rnixOverlay="${rnix012Src}" ;;
          rnix-0.13.* | rnix-0.14.*) rnixOverlay="${rnix014Src}" ;;
          *)
            echo "rnix-digit-separators: no overlay flavor for vendored $version;" >&2
            echo "add an rnix view based on the matching rnix-parser tag" >&2
            exit 1
            ;;
        esac
        if grep -F '"src/tokenizer.rs"' "$rnixDir/.cargo-checksum.json" >/dev/null 2>&1; then
          echo "rnix-digit-separators: vendored $version records per-file hashes;" >&2
          echo "teach lib/util/rnix-digit-separators to rewrite .cargo-checksum.json" >&2
          exit 1
        fi
        # Overlay the patched fork tree's src/ over the vendored crate's:
        # the fork base is the exact tag the crate was cut from, so the
        # only delta is the patch DAG (tokenizer separators).
        cp -r --no-preserve=mode "$rnixOverlay"/src/. "$rnixDir/src/"
        patchedRnix=1
      done
      if [ "$patchedRnix" = 0 ]; then
        echo "rnix-digit-separators: no vendored rnix-* crate found in $rnixVendor;" >&2
        echo "did ${old.pname or "the tool"} stop parsing nix with rnix?" >&2
        echo "vendor dir entries:" >&2
        ls "$rnixVendor" 2>/dev/null | head -6 >&2
        exit 1
      fi
    '';
})
