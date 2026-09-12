# Smoke checks for the Linux->macOS cross lanes (#3898): each proves its lane
# emits a real Mach-O arm64 artifact, which a green build alone does not.
# Imported by lib/per-system.nix into the per-system check catalog; only
# materialized on the CI system, where `crossPackages` exists.
{
  pkgs,
  mkCheck,
  crossPackages,
}: {
  # Proves the Linux->macOS cross toolchain actually emits a Darwin object,
  # which a successful build alone does not assert. `file` reads the Mach-O
  # header; a regression in the zig/SDK wiring fails here on x86_64-linux CI
  # rather than silently shipping a wrong-arch binary.
  cross-darwin-smoke = mkCheck "cross-darwin-smoke" {
    nativeBuildInputs = [pkgs.file];
    script = ''
      bin=${crossPackages.dag-runner-aarch64-apple-darwin}/bin/dag-runner
      info=$(file -b "$bin")
      echo "$info"
      case "$info" in
        *Mach-O*arm64*) ;;
        *)
          echo "expected Mach-O arm64, got: $info" >&2
          exit 1
          ;;
      esac
    '';
  };
  cross-darwin-web-monitor-smoke = mkCheck "cross-darwin-web-monitor-smoke" {
    nativeBuildInputs = [pkgs.file];
    script = ''
      pkg=${crossPackages.nix-web-monitor-aarch64-apple-darwin}
      bin=$pkg/bin/.nix-web-monitor-unwrapped
      info=$(file -b "$bin")
      echo "$info"
      case "$info" in
        *Mach-O*arm64*) ;;
        *)
          echo "expected Mach-O arm64, got: $info" >&2
          exit 1
          ;;
      esac
      read -r shebang < "$pkg/bin/nix-web-monitor"
      case "$shebang" in
        "#!/bin/sh") ;;
        *)
          echo "expected /bin/sh wrapper, got: $shebang" >&2
          exit 1
          ;;
      esac
      test -f "$pkg/share/nix-web-monitor/index.html"
    '';
  };
  # codex reaches Macs exclusively through the cross alias (#2690): the
  # required gate already builds `codex-aarch64-apple-darwin` as a
  # closure root, but a green build cannot assert architecture. Walk
  # the shipped entrypoint exactly as a Mac would: the shim must be
  # portable /bin/sh (a compiled wrapper would be a dead Linux ELF),
  # and both binaries it execs -- config-launch and the codex-rs
  # binary named by the launch spec -- must be Mach-O arm64 (#3583).
  cross-darwin-codex-smoke = mkCheck "cross-darwin-codex-smoke" {
    nativeBuildInputs = [pkgs.file pkgs.jq];
    script = ''
      pkg=${crossPackages.codex-aarch64-apple-darwin}
      read -r shebang < "$pkg/bin/codex"
      case "$shebang" in
        "#!/bin/sh") ;;
        *)
          echo "expected /bin/sh shim, got: $shebang" >&2
          exit 1
          ;;
      esac
      spec=$(sed -n 's/^export IX_LAUNCH_SPEC=//p' "$pkg/bin/codex")
      test -f "$spec"
      launcher=$(grep -o '/nix/store/[^ "]*/bin/config-launch' "$pkg/bin/codex")
      target=$(jq -r .target "$spec")
      for bin in "$launcher" "$target"; do
        info=$(file -b "$bin")
        echo "$bin: $info"
        case "$info" in
          *Mach-O*arm64*) ;;
          *)
            echo "expected Mach-O arm64 for $bin, got: $info" >&2
            exit 1
            ;;
        esac
      done
      # Every PATH dir the spec prepends must hold Mac executables too. The
      # cross lane hands codex the Linux build host's pkgs, and once (hydra HM
      # gen 738, 2026-09-03) the Darwin spec named an ELF ripgrep and bubblewrap
      # while this check stayed green (packages/codex/default.nix `pathPrepend`).
      for dir in $(jq -r '.path_prepend[]' "$spec"); do
        for bin in "$dir"/*; do
          info=$(file -bL "$bin")
          echo "$bin: $info"
          case "$info" in
            *Mach-O*arm64*) ;;
            *)
              echo "expected Mach-O arm64 on the spec's PATH, got: $info" >&2
              exit 1
              ;;
          esac
        done
      done
    '';
  };
  # btop is the first non-Rust cross package: a plain CMake/C++ build
  # driven by the cross toolchain's standalone clang + macOS SDK lane,
  # outside the cargo unit DAG the other smokes exercise. Assert the
  # C++ lane also emits a real Mach-O arm64 binary (#3584).
  cross-darwin-btop-smoke = mkCheck "cross-darwin-btop-smoke" {
    nativeBuildInputs = [pkgs.file];
    script = ''
      bin=${crossPackages.btop-aarch64-apple-darwin}/bin/btop
      info=$(file -b "$bin")
      echo "$info"
      case "$info" in
        *Mach-O*arm64*) ;;
        *)
          echo "expected Mach-O arm64, got: $info" >&2
          exit 1
          ;;
      esac
    '';
  };
  # Run the installed Linux-hosted compiler and read its configured target.
  # This stops a Git-source bootstrap regression at GHC instead of rebuilding
  # nom's Haskell closure before reporting the missing compiler.
  cross-darwin-ghc-smoke = let
    crossGhc = crossPackages.nix-output-monitor-aarch64-apple-darwin.crossGhc;
  in
    mkCheck "cross-darwin-ghc-smoke" {
      script = ''
        ghc=${crossGhc}
        info=$("$ghc/bin/${crossGhc.targetPrefix}ghc" --info)
        echo "$info"
        case "$info" in
          *"Target platform"*"aarch64-apple-darwin"*) ;;
          *)
            echo "expected aarch64-apple-darwin target" >&2
            exit 1
            ;;
        esac
      '';
    };
  # nom rides the same lane one level deeper: a Linux-hosted cross GHC
  # (ix.crossGhc) compiles the whole Haskell closure to Mach-O arm64
  # (#3606). Assert the Haskell lane emits a real Mach-O arm64 nom and
  # that the by-name alias symlinks survived the reimplementation.
  cross-darwin-nom-smoke = mkCheck "cross-darwin-nom-smoke" {
    nativeBuildInputs = [pkgs.file];
    script = ''
      pkg=${crossPackages.nix-output-monitor-aarch64-apple-darwin}
      info=$(file -b "$pkg/bin/nom")
      echo "$info"
      case "$info" in
        *Mach-O*arm64*) ;;
        *)
          echo "expected Mach-O arm64, got: $info" >&2
          exit 1
          ;;
      esac
      for alias in nom-build nom-shell; do
        if [ ! -e "$pkg/bin/$alias" ]; then
          echo "missing alias $alias" >&2
          exit 1
        fi
      done
    '';
  };
}
