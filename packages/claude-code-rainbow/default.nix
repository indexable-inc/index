# claude-code-rainbow: proof that we can produce a *visibly customized* Claude
# Code binary through a chain of independently-cacheable Nix derivations, one
# per "mapping" (patch), without recompiling Bun.
#
# WHY this works at all (the load-bearing fact): Claude Code ships as a prebuilt
# Bun single-file executable with the minified app JS embedded as text inside
# the Mach-O/ELF, plus a trailer Bun appends. Bun does NOT fatally integrity-
# check that bundle, so an EQUAL-LENGTH, in-place byte swap leaves every offset
# (and the trailer) intact and the CLI still runs. We never rebuild Bun; we just
# rewrite bytes. See ./patch-binary.py for the two gates every rule must pass
# (equal length + a pinned occurrence count that fails loudly on version drift).
#
# Each mapping is its OWN `runCommand` derivation, folded so mapping N+1 consumes
# mapping N's output. That way every layer caches independently: editing the
# color map does not invalidate the banner layer.
{
  lib,
  ix,
  stdenv,
  runCommand,
  python3,
  autoPatchelfHook,
  darwin,
  makeBinaryWrapper,
  repoPackages ? {},
}: let
  claude-code =
    repoPackages.claude-code
      or (throw "claude-code-rainbow: needs the claude-code sibling (flake package set only)");
  inherit (claude-code) version;

  # The single fetched upstream binary, shared with the stock package: one
  # `fetchurl` derivation, one download, one store path (claude-code exposes it
  # as `passthru.nativeBinary`). Depending on this FOD rather than the wrapped
  # claude-code closure keeps each mapping derivation's input tiny -- just the
  # binary, nothing else from the wrapper.
  inherit (claude-code) nativeBinary;

  # Shared equal-length byte-patch layer primitive, owned by the canonical
  # claude-code package (whose own dev-channels gate patch is currently
  # commented out, so this is the only live consumer); reached via the threaded
  # packages root rather than a `../` climb. One cacheable layer per mapping, so
  # the fold below is a DAG.
  applyBytePatch =
    import (ix.paths.packagesRoot + "/claude-code/byte-patch.nix")
    {inherit runCommand python3;};

  # The rainbow chain. Order is irrelevant to correctness (finds and replaces
  # are disjoint), but the fold makes the caching layers explicit:
  #   nativeBinary -> banner -> colors -> final
  mappings = [
    {
      name = "banner";
      rules = [
        {
          find = "Welcome to Claude Code";
          replace = "Rainbow!!! Claude Code";
          # 2.1.259 bundle: 7 (2.1.228 had 10); the nine colour rules below
          # counted unchanged against the same bundle.
          expect = 7;
        }
      ];
    }
    {
      name = "colors";
      rules = [
        {
          find = "rgb(215,119,87)";
          replace = "rgb(255,20,255)";
          expect = 9;
        }
        {
          find = "rgb(245,149,117)";
          replace = "rgb(255,120,255)";
          expect = 2;
        }
        {
          find = "rgb(87,105,247)";
          replace = "rgb(20,255,120)";
          expect = 5;
        }
        {
          find = "rgb(0,102,102)";
          replace = "rgb(0,255,255)";
          expect = 2;
        }
        {
          find = "rgb(71,130,200)";
          replace = "rgb(255,120,20)";
          expect = 5;
        }
        {
          find = "rgb(255,0,135)";
          replace = "rgb(255,255,0)";
          expect = 2;
        }
        {
          find = "rgb(153,153,153)";
          replace = "rgb(255,100,255)";
          expect = 5;
        }
        {
          find = "rgb(135,0,255)";
          replace = "rgb(255,0,100)";
          expect = 9;
        }
      ];
    }
  ];

  patched =
    lib.foldl'
    (input: m:
      applyBytePatch {
        inherit (m) name rules;
        inherit input;
      })
    nativeBinary
    mappings;
in
  # Same unfree binary as the stock package, so wrap identically:
  # `allowVendoredUnfree` strips `meta.license` so the per-system flake package
  # set (evaluated without `allowUnfree`) can still build this.
  ix.allowVendoredUnfree (stdenv.mkDerivation {
    pname = "claude-code-rainbow";
    inherit version;

    dontUnpack = true;
    # Stripping corrupts the Bun trailer; keep the patched bytes verbatim.
    dontStrip = true;
    strictDeps = true;

    # Byte-patched upstream binary repack; nothing to test at build time.
    doCheck = false;

    # Any byte change invalidates the upstream Developer-ID signature, and
    # Apple Silicon SIGKILLs a Mach-O whose CodeDirectory hashes no longer
    # match. `autoSignDarwinBinariesHook` re-signs the patched binary ad-hoc
    # during fixup (dropping the now-meaningless hardened-runtime + Developer-ID
    # signature), which is what makes the patched CLI runnable again.
    nativeBuildInputs =
      [makeBinaryWrapper]
      ++ lib.optional stdenv.hostPlatform.isElf autoPatchelfHook
      ++ lib.optional stdenv.hostPlatform.isDarwin darwin.autoSignDarwinBinariesHook;

    # The stock `claude-code` package launches through a spec that sets
    # DISABLE_UPDATES=1; this POC used to ship the raw patched binary instead.
    # Running that raw binary let Claude Code's self-installer run: with
    # `installMethod: native` in ~/.claude.json it downloaded a stock native
    # build and wrote a `~/.local/bin/claude` launcher that shadowed the
    # wrapped `claude` on PATH, so new sessions silently lost the wrapper's
    # MCP config, system prompt, and flags (2026-07-19 incident). Bake the
    # same guard here: real binary in libexec, wrapper on PATH.
    installPhase = ''
      # shell
      runHook preInstall
      install -D -m755 ${patched} $out/libexec/claude
      makeWrapper $out/libexec/claude $out/bin/claude \
        --set DISABLE_UPDATES 1
      runHook postInstall
    '';

    # Proof gate: the patched binary must still run AND must actually contain
    # the rainbow bytes (new banner present, old banner gone, a rainbow hex
    # present). Runs natively; the prebuilt CLI only needs DISABLE_UPDATES=1 to
    # print its version offline.
    doInstallCheck = true;
    # Proof gate. The byte-level grep proof runs everywhere (in-sandbox): the
    # rainbow banner must be present, the stock banner gone, and a rainbow hex
    # present. The runtime `--version` smoke runs in-check on Linux, where the
    # raw patched binary needs no signature. On darwin we skip the in-sandbox
    # exec: AMFI refuses to launch a Mach-O that was ad-hoc re-signed inside
    # this same sandboxed build (SIGTRAP), even though the signature is valid
    # and the binary runs fine outside the sandbox. Runtime is proven
    # out-of-band there (see the report / `nix run .#claude-code-rainbow`).
    installCheckPhase =
      ''
        # shell
        runHook preInstallCheck

        echo "== rainbow byte proof =="
        new=$(grep -c "Rainbow!!! Claude Code" "$out/libexec/claude" || true)
        old=$(grep -c "Welcome to Claude Code" "$out/libexec/claude" || true)
        hex=$(grep -c "rgb(255,20,255)" "$out/libexec/claude" || true)
        echo "new banner count: $new"
        echo "old banner count: $old"
        echo "rainbow rgb count: $hex"
        [ "$new" -ge 1 ] || { echo "FAIL: new rainbow banner not present" >&2; exit 1; }
        [ "$old" -eq 0 ] || { echo "FAIL: original banner bytes still present ($old)" >&2; exit 1; }
        [ "$hex" -ge 1 ] || { echo "FAIL: rainbow rgb not present" >&2; exit 1; }
      ''
      + lib.optionalString (!stdenv.hostPlatform.isDarwin) ''
        echo "== version smoke =="
        $out/bin/claude --version
      ''
      + lib.optionalString stdenv.hostPlatform.isDarwin ''
        echo "== version smoke: skipped in darwin sandbox =="
        echo "AMFI blocks exec of a just-ad-hoc-signed Mach-O inside the build"
        echo "sandbox; run 'result/bin/claude --version' after build."
      ''
      + ''
        runHook postInstallCheck
      '';

    passthru = {
      # Each mapping layer, exposed so the derivation graph is inspectable.
      inherit nativeBinary;
      rainbowLayers = patched;
    };

    meta = {
      description = "Claude Code with a visibly rainbow'd banner and theme, patched via equal-length byte swaps";
      homepage = "https://www.anthropic.com/claude-code";
      # Stripped by `ix.allowVendoredUnfree` above; kept honest here.
      license = lib.licenses.unfree;
      mainProgram = "claude";
      inherit (claude-code.meta) platforms;
      sourceProvenance = [lib.sourceTypes.binaryNativeCode];
    };
  })
