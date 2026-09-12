# Keep Cargo's compilation units independently cacheable. Only the final C ABI
# library and its installation metadata belong to the shared runtime package.
{
  ix,
  lib,
  componentPkgs,
}: let
  inherit
    (import ./rust-toolchain.nix {inherit ix lib componentPkgs;})
    buildPkgs
    hostPlatform
    buildPlatform
    cargoUnit
    nativeToolchain
    target
    targetToolchain
    appleToolchain
    nativeEnv
    targetEnv
    ;

  # Fingerprinting and embedded builtins use the original nix/rust -> nix/src
  # relative layout. The sibling parser view stays a path dependency.
  nixSource =
    (import ./runtime-sources.nix {
      inherit lib;
      inherit (ix) nixSrc;
    }).evaluator;
  parserSource = builtins.path {
    path = ix.rnix-0-12Src;
    name = "nix-eval-rnix-source";
  };
  sourceManifest = lib.importTOML (nixSource + "/rust/Cargo.toml");
  crateManifest = lib.importTOML (nixSource + "/rust/nix-eval-rs/Cargo.toml");
  toml = buildPkgs.formats.toml {};
  workspaceManifest = toml.generate "nix-eval-workspace.toml" (sourceManifest
    // {
      workspace =
        sourceManifest.workspace
        // {
          members = map (member: "nix/rust/${member}") sourceManifest.workspace.members;
          exclude = (sourceManifest.workspace.exclude or []) ++ ["rnix-0-12"];
          dependencies =
            sourceManifest.workspace.dependencies
            // {
              rnix = sourceManifest.workspace.dependencies.rnix // {path = "rnix-0-12";};
            };
        };
    });
  project = crateType: let
    manifest = toml.generate "nix-eval-${crateType}.toml" (crateManifest
      // {
        lib = crateManifest.lib // {crate-type = [crateType];};
      });
  in
    buildPkgs.runCommand "nix-eval-${crateType}-source" {} ''
      # shell
      mkdir -p "$out/nix"
      cp -R ${nixSource}/. "$out/nix/"
      cp -R ${parserSource} "$out/rnix-0-12"
      chmod -R u+w "$out"
      cp ${workspaceManifest} "$out/Cargo.toml"
      rm "$out/nix/rust/Cargo.toml"
      mv "$out/nix/rust/Cargo.lock" "$out/Cargo.lock"
      cp ${manifest} "$out/nix/rust/nix-eval-rs/Cargo.toml"
      # Declare the lockfile to the per-unit source closure scanner as well
      # as to the build script that locates it by walking parent directories.
      cp "$out/Cargo.lock" "$out/nix/rust/nix-eval-rs/Cargo.lock"
    '';
  releaseSource = project "cdylib";
  checkSource = project "rlib";
  clippyConfig = buildPkgs.writeTextDir "clippy.toml" (builtins.readFile (nixSource + "/rust/clippy.toml"));

  purePolicy = cargoUnit.policyPresets.pureBuild // {compiler.embedMetadata = true;};
  releasePlan = cargoUnit.planWorkspace {
    pname = "nix-eval-runtime";
    src = releaseSource;
    workspaceRoot = releaseSource;
    cargoLock = nixSource + "/rust/Cargo.lock";
    rustToolchain = targetToolchain;
    inherit target;
    profile = "release";
    cargoTargets = [["-p" "nix-eval-rs"]];
    policy = purePolicy;
    # Our binary cache serves narinfos, not floating-CA realisation records.
    contentAddressed = false;
    env = nativeEnv // targetEnv;
    nativeBuildInputs = lib.optionals (appleToolchain != null) appleToolchain.runtimeInputs;
    # Installation replaces the short Mach-O ID with the final store path.
    # Reserve load-command space at the target library's actual link step.
    extraLinkRustcArgsForPlatform = platform:
      lib.optional (hostPlatform.isDarwin && platform == target)
      "-Clink-arg=-Wl,-headerpad_max_install_names";
    extraRustcArgsForPlatform =
      if appleToolchain != null
      then appleToolchain.rustcArgsForPlatform
      else (_platform: []);
  };

  release = releasePlan.workspace;

  # Checks always target the build platform, including when the shipped
  # library targets Darwin from Linux. A test graph cannot share the cdylib
  # projection: its rlib dependencies and release ThinLTO are incompatible.
  checkPlan = kind: noDefaultFeatures:
    cargoUnit.planWorkspace {
      pname = "nix-eval-${kind}";
      src = checkSource;
      workspaceRoot = checkSource;
      cargoLock = nixSource + "/rust/Cargo.lock";
      rustToolchain = nativeToolchain;
      target = buildPlatform.rust.rustcTarget;
      profile = "test";
      contentAddressed = false;
      env = nativeEnv;
      # Isolated units run outside the Cargo workspace, so Clippy cannot
      # discover its test-specific lint configuration by walking parents.
      packageBuildEnv = lib.optionalAttrs (kind == "clippy") (
        lib.genAttrs ["nix-eval-rs" "ix-kernel" "nix-eval-driver"] (_name: {
          CLIPPY_CONF_DIR = clippyConfig;
        })
      );
      cargoTargets = [
        (
          (
            if noDefaultFeatures
            then ["-p" "nix-eval-rs" "--no-default-features"]
            else ["--workspace"]
          )
          ++ [
            {
              tests = "--tests";
              docs = "--lib";
              clippy = "--all-targets";
            }.${
              kind
            }
          ]
        )
      ];
      policy =
        purePolicy
        // {
          tests = {
            enable = kind != "clippy";
            useNextest = false;
          };
          clippy = {
            enable = kind == "clippy";
            packages =
              if noDefaultFeatures
              then ["nix-eval-rs"]
              else ["nix-eval-rs" "ix-kernel" "nix-eval-driver"];
            package = buildPkgs.clippy // {toolchain = nativeToolchain;};
            deniedLints = ["warnings"];
          };
        };
    };
  suite = kind: noDefaultFeatures: let
    inherit (checkPlan kind noDefaultFeatures) workspace;
    checks =
      {
        tests = workspace.testChecksByTarget;
        docs = lib.mapAttrs (_name: value: value.all) workspace.doctests;
        clippy = workspace.clippyByPackage;
      }.${
        kind
      };
    names = builtins.attrNames checks;
  in
    buildPkgs.runCommand "nix-eval-${kind}-${
      if noDefaultFeatures
      then "no-default"
      else "default"
    }" {
      deps = assert lib.assertMsg (names != []) "nix-eval ${kind} projection produced no checks";
        builtins.attrValues checks;
      __structuredAttrs = true;
    } ''
      # shell
      mkdir -p "$out"
      printf '%s\n' ${lib.escapeShellArgs names} > "$out/targets"
    '';
  aggregate = kind:
    buildPkgs.runCommand "nix-eval-${kind}" {
      deps = map (suite kind) [false true];
      __structuredAttrs = true;
    } ''
      # shell
      mkdir -p "$out"
      printf '%s\n' default no-default > "$out/features"
    '';
  checks = lib.genAttrs ["tests" "docs" "clippy"] aggregate;
  library = release.libraries.nix_eval_rs;
  runtimeLibrary = "libnix_eval_rs${hostPlatform.extensions.sharedLibrary}";
in
  componentPkgs.stdenv.mkDerivation {
    pname = "nix-eval-rs";
    version = "0.1.0";
    strictDeps = true;
    dontUnpack = true;
    dontConfigure = true;
    dontBuild = true;
    # Preserve the existing production test gate. Cross builds execute the
    # explicit native projection, never target-platform test binaries.
    doCheck = true;
    checkPhase = ''
      # shell
      runHook preCheck
      cat ${checks.tests}/features ${checks.docs}/features
      runHook postCheck
    '';
    nativeBuildInputs =
      lib.optional hostPlatform.isLinux buildPkgs.patchelf
      ++ lib.optional hostPlatform.isDarwin componentPkgs.stdenv.cc.bintools.bintools;
    installPhase = ''
      # shell
      runHook preInstall
      mkdir -p "$out/lib/pkgconfig" "$out/include"
      artifact=$(cat ${library}/nix-support/extern-path)
      case "$artifact" in
        *${hostPlatform.extensions.sharedLibrary}) ;;
        *) echo "nix-eval runtime did not produce a shared library: $artifact" >&2; exit 1 ;;
      esac
      cp "$artifact" "$out/lib/${runtimeLibrary}"
      chmod u+w "$out/lib/${runtimeLibrary}"
      ${
        if hostPlatform.isDarwin
        then ''
          install_name_tool -id "$out/lib/${runtimeLibrary}" "$out/lib/${runtimeLibrary}"
        ''
        else ''
          patchelf --set-soname ${runtimeLibrary} "$out/lib/${runtimeLibrary}"
        ''
      }
      cp ${nixSource}/rust/nix-eval-rs/include/*.h "$out/include/"
      cat > "$out/lib/pkgconfig/nix-eval-rs.pc" <<EOF
      prefix=$out
      libdir=$out/lib
      includedir=$out/include

      Name: nix-eval-rs
      Description: Shared Rust runtime for Nix evaluation and commands
      Version: 0.1.0
      Libs: -L$out/lib -lnix_eval_rs
      Cflags: -I$out/include
      EOF
      runHook postInstall
    '';
    passthru = {
      inherit checks;
      # Expose graph identities for rebuild controls without building tests
      # or modifying the source tree used by another worker.
      unitWorkspace = release;
      # Source projections must be resident before planning. Then all graph
      # and renderer derivations for the mandatory runtime gates can be built
      # together, without importing generated units or compiling Rust.
      metadataHelpers = {
        sources = {
          release = releaseSource;
          checks = checkSource;
        };
        release = releasePlan.helpers;
        checks = lib.genAttrs ["tests" "docs"] (kind: {
          default = (checkPlan kind false).helpers;
          noDefault = (checkPlan kind true).helpers;
        });
      };
    };
  }
