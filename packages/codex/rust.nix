# codex-rs built on the repo's per-unit Rust DAG (`ix.cargoUnit.buildWorkspace`),
# the same machinery that builds index's own crates. This replaces the old
# `rustPlatform.buildRustPackage` recipe so that:
#   - cross-compilation to Darwin falls out of `target = "aarch64-apple-darwin"`
#     (the RFC 0009 apple SDK toolchain), instead of needing a real Mac; and
#   - a Codex view update rebuilds only the crates whose sources changed, not the
#     whole vendored world (the fork bumps constantly).
#
# The one dependency whose build script would download prebuilts (the `v8`
# crate: a static archive and its bindgen output) gets them from ./prebuilt.nix
# through the two env vars the script reads, scoped to that crate via
# `packageBuildEnv.v8`; nothing is downloaded at build time. Other native code
# (aws-lc-sys, ...) is
# compiled from source with the clang/cmake inputs below. `target = null`
# builds for the host.
{
  lib,
  pkgs,
  ix,
  codexSrc,
  binName ? "codex",
  # Rust target triple for a cross build (e.g. "aarch64-apple-darwin"), or null
  # to build for the host. Only the Apple-Darwin triples are wired here (the one
  # lane that needs cross); other triples would need their own toolchain branch.
  target ? null,
}: let
  inherit (pkgs) stdenv;
  inherit (ix) cargoUnit;

  isCross = target != null;
  targetIsDarwin = isCross && lib.hasSuffix "-apple-darwin" target;

  # The system whose prebuilt archives this build needs: the cross target's
  # system, or the host system for a native build.
  targetSystem =
    if !isCross
    then stdenv.hostPlatform.system
    else if targetIsDarwin
    then
      (
        if lib.hasPrefix "aarch64-" target
        then "aarch64-darwin"
        else "x86_64-darwin"
      )
    else throw "codex rust.nix: unsupported cross target ${target}";

  prebuilt = import ./prebuilt.nix {inherit (pkgs) fetchurl runCommand;} targetSystem;

  # The Apple cross toolchain (zig cc + macOS SDK), or null for a native build.
  # Same wiring as lib/rust/workspace.nix `mkUnits`.
  appleToolchain =
    if targetIsDarwin
    then
      ix.appleSdkToolchain {
        appleSdk = ix.macosSdk {inherit pkgs;};
        inherit lib target pkgs;
        inherit (ix) writeBashApplication;
      }
    else null;

  # Git dependencies pinned in codex-rs/Cargo.lock, keyed by the exact Cargo.lock
  # source string (cargoUnit's vendorer keys by source, not name-version like
  # rustPlatform.importCargoLock). Refresh after a view update: list the
  # `source = "git+..."` lines of the new Cargo.lock, then
  # `nix flake prefetch --json "git+<url>?rev=<rev>"` for each new one (its
  # NAR hash is the fetchgit hash the vendorer checks; verified against an
  # unchanged pin as the control). vendor.nix fails the eval on any missing or
  # stale key, so a wrong list cannot build.
  outputHashes = {
    "git+https://github.com/dzbarsky/rules_rust?rev=b56cbaa8465e74127f1ea216f813cd377295ad81#b56cbaa8465e74127f1ea216f813cd377295ad81" = "sha256-uJpVLcQh8wWZA3GPv9D8Nt43EOirajfDJ7eq/FB+tek=";
    "git+https://github.com/helix-editor/nucleo.git?rev=4253de9faabb4e5c6d81d946a5e35a90f87347ee#4253de9faabb4e5c6d81d946a5e35a90f87347ee" = "sha256-Hm4SxtTSBrcWpXrtSqeO0TACbUxq3gizg1zD/6Yw/sI=";
    "git+https://github.com/microsoft/mxc?rev=6cd3d58f05d3447e67109cfb75e042803b843ca4#6cd3d58f05d3447e67109cfb75e042803b843ca4" = "sha256-XUkT2R+RYk9WIqgKnmIAagNW4xOTyp4bWHmQL1iznHw=";
    "git+https://github.com/openai-oss-forks/crossterm?rev=45fecb9508105988f42fe6ff0441783ed3717f92#45fecb9508105988f42fe6ff0441783ed3717f92" = "sha256-cQxQQuV+YEutuQiPurXVISq6F/99vCEk8qe5PU8BCSo=";
    "git+https://github.com/openai-oss-forks/tokio-tungstenite?rev=0e5b2d73aa18dd9f0a50ee9ff199d5aef7594186#0e5b2d73aa18dd9f0a50ee9ff199d5aef7594186" = "sha256-V1xmnrfRWOcZZogelZEA4vvyMj2awCfHVA5/glQ6KAI=";
    "git+https://github.com/openai-oss-forks/tungstenite-rs?rev=4fffad30fe373adbdcffab9545e9e9bf4f2fc19f#4fffad30fe373adbdcffab9545e9e9bf4f2fc19f" = "sha256-VVHhk7l9J/sEmG3q/UuV/sQ3f+fGsmq5vumSy8vbMvw=";
  };

  # `codex-rs` is a subtree of the (patched) codex source. Pass it as both the
  # build input and the workspace root, the shape cargoUnit expects for a
  # fetched/patched source (workspaceRoot = src).
  workspaceRoot = codexSrc + "/codex-rs";

  workspace = cargoUnit.buildWorkspace ({
      pname = "codex-rs${lib.optionalString isCross "-${target}"}";
      src = workspaceRoot;
      inherit workspaceRoot outputHashes;
      cargoLock.lockFile = workspaceRoot + "/Cargo.lock";
      # Match upstream's release build (scripts/codex_package/cargo.py): the
      # codex binary plus `codex-code-mode-host`, not the whole workspace of
      # test/support crates. codex spawns the host as a sibling of its own
      # executable (install-context `code_mode_host_program`) and
      # `features.code_mode_host` is default-enabled upstream (features/src/
      # lib.rs, Stage::Stable), so a codex shipped without the host fails every
      # session with "failed to spawn code-mode host".
      cargoArgs = ["--package" "codex-cli" "--package" "codex-code-mode-host"];
      cargoTargets = [["--package" "codex-cli" "--package" "codex-code-mode-host"]];
      cargoTargetNames = ["build"];
      # codex is an external vendored build, not our own linted workspace, so
      # skip clippy/audit/machete (also what the cross graph does).
      policy = cargoUnit.policyPresets.pureBuild;
      # Input-address the whole codex graph (cargo-unit defaults to
      # `contentAddressed = true`). Codex is built once and substituted from
      # cache.ix.dev by every other machine; a floating-CA output has no
      # eval-time path, so substituting it needs the cache's `/realisations`
      # build trace, which cache.ix.dev (atticd behind ncps) 404s -- it serves
      # narinfos only. Input-addressed drvs carry concrete out paths, so plain
      # narinfo substitution works. Same rationale the cross graph documents in
      # lib/rust/workspace.nix; it holds for the native codex graph too, and it
      # also keeps the input-addressed wrapper derivation from becoming a
      # deferred CA derivation that cannot resolve after this graph's IFD.
      contentAddressed = false;
      nativeBuildInputs =
        [
          pkgs.clang
          pkgs.cmake
          pkgs.pkg-config
          pkgs.gitMinimal
          pkgs.lld
        ]
        ++ lib.optionals (appleToolchain != null) appleToolchain.runtimeInputs;
      env =
        {
          # bindgen users dlopen libclang and need the header search paths the
          # Linux sandbox does not provide by default.
          LIBCLANG_PATH = "${lib.getLib pkgs.llvmPackages.libclang}/lib";
          # openssl-sys finds the system openssl through pkg-config. Host
          # (Linux) scope only, so it stays correct under cross: openssl only
          # enters codex's graph via native-tls (openssl on Linux,
          # Security.framework on macOS) and musl-only vendored pins, so no
          # *-apple-darwin unit ever compiles openssl-sys -- the only units
          # that read this are host-side Linux ones, where TARGET == HOST and
          # the pkg-config crate accepts the unqualified path.
          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";
          # Silence the warning-as-error false positives upstream documents
          # (GCC stringop-overflow in BoringSSL; Clang character-conversion).
          NIX_CFLAGS_COMPILE = toString (
            lib.optional stdenv.cc.isGNU "-Wno-error=stringop-overflow"
            ++ lib.optional stdenv.cc.isClang "-Wno-error=character-conversion"
          );
        }
        # appleToolchain.env carries the Darwin toolchain in target-suffixed
        # vars only, so host units (the cross graph builds proc-macro deps for
        # the host: sqlx-macros -> sqlx-sqlite -> libsqlite3-sys) fall through
        # to the ordinary host toolchain on PATH; no host-triple pins needed.
        // lib.optionalAttrs (appleToolchain != null) appleToolchain.env;
      # Build scripts emit `-l` flags that reach the final link, but their
      # `rustc-link-search` paths do not cross cargoUnit's per-unit boundary, so
      # the native libs the codex binary links (openssl, libcap on Linux) need
      # their lib dirs added to the final link search directly. The runtime
      # rpath rides along for free: the native final link runs through stdenv's
      # cc/ld-wrapper, which appends an `-rpath` entry for every store dir in
      # `-L`, embedding these paths in the ELF and pulling the libs into the
      # output's runtime closure.
      extraLinkRustcArgsForPlatform = _platform:
        ["-L" "native=${pkgs.openssl.out}/lib"]
        ++ lib.optionals stdenv.hostPlatform.isLinux ["-L" "native=${pkgs.libcap.lib}/lib"];
      # The v8 crate's build script consumes these prebuilts (./prebuilt.nix)
      # instead of downloading them: the static archive and the bindgen output
      # generated against it. Scoped to `v8` (its build-script-run unit and its
      # compile unit) so the store paths do not perturb the rest of the closure.
      packageBuildEnv.v8 = {
        RUSTY_V8_ARCHIVE = "${prebuilt.librustyV8}";
        RUSTY_V8_SRC_BINDING_PATH = "${prebuilt.srcBinding}";
      };
      # The v8 crate links rusty_v8 as a `+bundle` static lib, so rustc must
      # find librusty_v8.a at the *crate compile* to embed it into the v8 rlib
      # (this is where the build fails without it, not at the final link). The
      # build script's own copy lands under build_dir() and never crosses the
      # per-unit boundary; hand the compile the decompressed archive directly.
      packageRustcArgs.v8 = ["-L" "native=${prebuilt.librustyV8Lib}"];
    }
    // lib.optionalAttrs isCross {
      inherit target;
      # `embedMetadata = true` because this graph pins a stable toolchain
      # and `-Zembed-metadata=no` is nightly-only: leaving the default sends a
      # `-Z` flag to a rustc that exits 1 on it (ENG-12992). The cost is a
      # fatter rlib on a graph nothing links against twice.
      policy = cargoUnit.policyPresets.pureBuild // {compiler.embedMetadata = true;};
      rustToolchain = ix.languages.rust.toolchain pkgs {
        channel = "stable";
        version = "latest";
        targets = [target];
      };
      extraRustcArgsForPlatform =
        if appleToolchain != null
        then appleToolchain.rustcArgsForPlatform
        else (_platform: []);
    });
in {
  inherit workspace;
  binary = workspace.binaries.${binName};
  hostBinary = workspace.binaries.codex-code-mode-host;
}
