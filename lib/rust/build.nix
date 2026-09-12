{
  lib,
  # This is the repo Rust builder factory (`import`ed by lib/rust/tooling.nix
  # with an explicit `pkgs`, never `callPackage`d), not a package. It needs the
  # whole package set to build a `makeRustPlatform` and the policy-check
  # derivations, so there is no fixed dep list to enumerate and nothing for
  # `override` to reach past.
  # astlog-ignore: no-pkgs-in-callpackage
  pkgs,
  clippyPackage ? pkgs.clippy,
  rustToolchain,
  writePythonApplication,
  lists,
  # Shared pins reader, threaded through to policy.nix (see its arg doc).
  pins,
  # Flips `allowSubstitutes` back on for darwin cross-lane eval-time IFD nodes;
  # threaded through to vendor.nix. See its doc comment.
  evalTimeSubstitutable,
}: let
  inherit (builtins) removeAttrs;

  # The resolution boundary: caller args -> one resolved bundle (context, policy,
  # linker, effects, checks), defaults applied once. `buildPackage` consumes the
  # bundle; the rest of the surface is re-exposed below as the `rust` value the
  # cargo-unit side reads.
  resolve = import ./resolve.nix {
    inherit
      lib
      pkgs
      clippyPackage
      rustToolchain
      writePythonApplication
      lists
      pins
      evalTimeSubstitutable
      ;
  };
  inherit
    (resolve)
    resolveArgs
    toolchainId
    defaultRustToolchain
    clippyLintFlagsFromManifest
    policyPresets
    ;

  buildPackage = expandedArgs: let
    # Every policy check and build derivation needs a crate name for its
    # derivation name (and `meta.mainProgram`). Require `pname` explicitly rather
    # than papering a missing name over with a sentinel that surfaces far downstream.
    crateName = a: a.pname or (throw "rust.buildPackage: set `pname`.");
    # Shortcut: pass `srcRoot = ./.` for a repo-owned crate whose tracked tree
    # is the build closure. Imports the whole `srcRoot` tree, defaults
    # `meta.mainProgram` to `pname`, and keeps the resolver's `cargoLock` default
    # (`src + "/Cargo.lock"`) intact.
    rawArgs =
      if expandedArgs ? srcRoot
      then let
        inherit (expandedArgs) srcRoot;
        pname = crateName expandedArgs;
      in
        (removeAttrs expandedArgs ["srcRoot"])
        // {
          src = lib.fileset.toSource {
            root = srcRoot;
            fileset = srcRoot;
          };
          meta =
            (expandedArgs.meta or {})
            // {
              mainProgram = expandedArgs.meta.mainProgram or pname;
            };
        }
      else expandedArgs;

    resolved = resolveArgs rawArgs;
    inherit
      (resolved)
      context
      policy
      effects
      checks
      ;

    rustPlatform =
      rawArgs.rustPlatform or (pkgs.makeRustPlatform {
        cargo = context.rustToolchain;
        rustc = context.rustToolchain;
      });

    testEnabled = policy.tests.enable && (rawArgs.doCheck or true);

    cargoTestFlags =
      (rawArgs.cargoTestFlags or [])
      ++ lib.optional (testEnabled && policy.tests.useNextest) "--no-tests=pass";
    # Vendor through our own fetcher (`vendorDir` -> `static.crates.io`)
    # instead of letting nixpkgs's `importCargoLock` re-fetch each crate via
    # the legacy `crates.io/api/v1/crates/.../download` URL. The legacy
    # endpoint is now gated on User-Agent (no `curl/...`) and is a redirect
    # to the same CDN anyway, so going direct is both unblocked and faster.
    # Surface the vendor dir as `cargoDeps` (absolute store path); the
    # cargo-setup hook expects `cargoVendorDir` to be in-source, not a
    # `/nix/store` path. User-supplied `cargoHash`, `cargoDeps`, or
    # `cargoVendorDir` still wins. The resolver already resolved `vendorDir`
    # (honoring `sourceOverrides`), so reuse it.
    #
    # nixpkgs's `cargoSetupPostPatchHook` diffs `$cargoDeps/Cargo.lock`
    # against the lockfile in the source tree. The vendor dir only emits the
    # per-crate symlinks, so re-attach the lockfile here.
    defaultCargoDeps = pkgs.runCommand "cargo-deps" {__structuredAttrs = true;} ''
      mkdir -p "$out"
      cp -RL ${context.vendorDir}/. "$out/"
      cp ${context.cargoLockPath} "$out/Cargo.lock"
    '';

    hasCargoMeta = rawArgs ? cargoHash || rawArgs ? cargoDeps || rawArgs ? cargoVendorDir;

    buildArgs =
      removeAttrs rawArgs [
        "cargoArgs"
        "cargoExtraConfig"
        "cargoLock"
        "cargoTestFlags"
        "outputHashes"
        "policy"
        "rustPlatform"
        "rustToolchain"
        "sourceOverrides"
        "vendorDir"
      ]
      // {
        nativeBuildInputs = (rawArgs.nativeBuildInputs or []) ++ effects.linkerNativeInputs;
        inherit cargoTestFlags;
        useNextest = testEnabled && policy.tests.useNextest;
      }
      // lib.optionalAttrs (!hasCargoMeta) {
        cargoDeps = defaultCargoDeps;
      }
      // lib.optionalAttrs (effects.rustcArgsForHost != []) {
        RUSTFLAGS = (lib.toList (rawArgs.RUSTFLAGS or [])) ++ effects.rustcArgsForHost;
      };

    uncheckedPackage = rustPlatform.buildRustPackage buildArgs;

    policyChecks = checks.crate {
      pname = crateName rawArgs;
      # The policy merge flattens away whether the caller set `clippy.cargoArgs`;
      # the clippy check needs it, so the resolver surfaced it.
      clippyCargoArgsSet = resolved.clippyCargoArgsExplicit;
    };
  in
    # The policy-checked wrapper: the same Rust package with the policy checks
    # attached as `passthru.tests` and symlinked under `$out/rust-policy`. Still
    # the same package identity for eval-time callers that inspect it.
    pkgs.symlinkJoin {
      name = "${uncheckedPackage.name}-policy-checked";
      paths = [uncheckedPackage];
      inherit (uncheckedPackage) meta pname version;
      passthru =
        (uncheckedPackage.passthru or {})
        // {
          inherit policy;
          unchecked = uncheckedPackage;
          inherit policyChecks;
          tests =
            (uncheckedPackage.passthru.tests or {})
            // policyChecks
            // lib.optionalAttrs testEnabled {package = uncheckedPackage;};
        };
      postBuild = let
        linkPolicyCheck = name: check: ''ln -s ${check} "$out/rust-policy/${name}"'';

        linkPolicyChecks = lib.concatMapAttrsStringSep "\n" linkPolicyCheck policyChecks;
      in
        lib.optionalString (policyChecks != {}) ''
          mkdir -p "$out/rust-policy"
          ${linkPolicyChecks}
        '';
    };
in {
  inherit
    buildPackage
    resolveArgs
    toolchainId
    defaultRustToolchain
    clippyLintFlagsFromManifest
    policyPresets
    ;
}
