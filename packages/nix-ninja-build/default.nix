{
  ix,
  lib,
  repoPackages,
}:
# Incremental build lane for the patched nix fork via nix-ninja (#3655):
# `nix run .#nix-ninja-build-nix` materializes the patched source (the same
# Nix view packages/nix ships), configures it with meson
# inside upstream's own dev shell, and hands the ninja graph to nix-ninja,
# which turns every compilation unit into its own content-addressed
# derivation. A warm rerun after touching one .cc file recompiles only that
# unit's derivation and relinks its dependents, instead of the whole-package
# ~19 min packages/nix rebuild. Additive and non-gating: no fork check
# consumes this lane.
#
# Local/devshell consumption mode of nix-ninja: the client creates dynamic
# derivations and runs `nix build` on your behalf, so the daemon must enable
# the `dynamic-derivations` and `ca-derivations` experimental features (the
# Linux fleet builders already do); `recursive-nix` is only needed for the
# in-derivation mode this lane deliberately avoids.
let
  inherit (ix) pkgs;
  inherit (repoPackages) nix-ninja;

  # The ASSEMBLED fork client, not `repoPackages.nix-ix`: that recipe throws
  # un-overridden (its jjTree archive lives in ix), and this lane exists to
  # rebuild exactly the client the fleet runs, which is the assembled one.
  # Same consumer class as packages/nix-eval-jobs (the accessor's doc in
  # lib/default.nix has the story), so this package only evaluates with the
  # guest Nix injected and sits in `needsGuestNix` (lib/per-system.nix).
  forkClient = ix.nixPackageFor "packages/nix-ninja-build";

  # The identical view the fork package builds. There is no patch application
  # step, so this lane cannot drift from packages/nix.
  patchedSrc = ix.nixSrc;

  # Run under the fork client, not stock pkgs.nix: the generated derivations
  # are dynamic derivations, which need a >= 2.30 master-based client, and the
  # fork is the one client guaranteed protocol-compatible with the fleet
  # daemons this lane builds against.
  nixClient = lib.getExe forkClient;

  inner = ix.writeBashApplication pkgs {
    name = "nix-ninja-build-nix-inner";
    runtimeInputs = [nix-ninja];
    text = ''
      workdir=$1
      target=''${2:-src/nix/nix}
      cd "$workdir/src"

      # The upstream dev shell exports CC_LD/CXX_LD=mold, which meson bakes
      # into every link rule as -fuse-ld=mold. Inside a nix-ninja task
      # sandbox that linker is unreachable: nix-ninja discovers sandbox PATH
      # entries by which-ing the command's words, and `-fuse-ld=mold` is a
      # flag, not a word, so links die with "collect2: cannot find ld". The
      # wrapped bintools linker resolves through the cc wrapper's baked -B
      # path and needs no discovery.
      unset CC_LD CXX_LD

      # Meson accepts nix-ninja as its ninja via $NINJA (it reports the
      # minimum compatible version, 1.8.2). nix-ninja also re-execs
      # nix-ninja-task from PATH inside the derivations it emits.
      NINJA="$(command -v nix-ninja)"
      export NINJA

      if [ ! -d build ]; then
        meson setup build
      fi
      cd build

      log=$(mktemp)
      start=$(date +%s)
      nix-ninja "$target" 2>&1 | tee "$log"
      end=$(date +%s)

      compiled=$(grep -c 'Compiling C++ object' "$log" || true)
      linked=$(grep -c 'Linking target' "$log" || true)
      rm -f "$log"
      echo "nix-ninja-build-nix: built $target in $((end - start))s ($compiled compilation units compiled, $linked targets linked)"

      if [ -x "$target" ]; then
        "./$target" --version
      fi
    '';
    meta.description = "Inner nix-ninja driver for nix-ninja-build-nix (runs inside the patched tree's dev shell)";
  };
in
  ix.writeBashApplication pkgs {
    name = "nix-ninja-build-nix";
    text = ''
      workdir=''${NIX_NINJA_NIX_WORKDIR:-''${TMPDIR:-/tmp}/nix-ninja-build-nix-''${USER:-anon}}

      if [ "''${1:-}" = "--fresh" ]; then
        shift
        rm -rf "$workdir/src" "$workdir/.base"
      fi

      mkdir -p "$workdir"
      if [ ! -d "$workdir/src" ]; then
        # --no-preserve=mode: the store tree is read-only; the workdir is the
        # mutable checkout the incremental loop edits.
        cp -R --no-preserve=mode ${patchedSrc}/ "$workdir/src"
        # Same version marker the fork package compiles in (see
        # packages/nix/default.nix): meson reads .version, so the lane's
        # binary identifies the exact patch series it was built from.
        printf '%s\n' ${lib.escapeShellArg forkClient.version} > "$workdir/src/.version"
        printf '%s\n' ${patchedSrc} > "$workdir/.base"
      elif [ "$(cat "$workdir/.base" 2>/dev/null)" != ${patchedSrc} ]; then
        echo "nix-ninja-build-nix: warning: $workdir/src was materialized from an older patch series; rerun with --fresh to rebase it" >&2
      fi

      # Client-side feature gates for the dynamic derivations nix-ninja
      # emits; the daemon side must already allow them (fleet Linux builders
      # do).
      export NIX_CONFIG="extra-experimental-features = nix-command flakes dynamic-derivations ca-derivations"

      # Upstream's own dev shell (the patched tree keeps upstream's flake.nix
      # and flake.lock) supplies meson and every library dependency exactly as
      # nix upstream develops against; hand-rolling PKG_CONFIG_PATH and
      # friends here would just re-derive it, badly. Impure boundary: first
      # run evaluates that flake and fetches its locked inputs.
      exec ${nixClient} develop "path:${patchedSrc}" --command ${lib.getExe inner} "$workdir" "$@"
    '';
    meta.description = "Incremental per-compilation-unit build of the patched nix fork via nix-ninja (local mode)";
  }
