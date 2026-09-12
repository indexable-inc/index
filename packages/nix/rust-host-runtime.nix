# Store policies have no evaluator source or dependencies in their build graph.
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
  source =
    (import ./runtime-sources.nix {
      inherit lib;
      inherit (ix) nixSrc;
    }).host;
  manifest = lib.importTOML (source + "/Cargo.toml");
  workspaceManifests =
    [
      manifest
    ]
    ++ map (member: lib.importTOML (source + "/${member}/Cargo.toml")) manifest.workspace.members;
  workspacePackages = map (member: member.package.name) workspaceManifests;
  workspaceTargets =
    map (
      member: member.lib.name or (lib.replaceStrings ["-"] ["_"] member.package.name)
    )
    workspaceManifests;
  toml = buildPkgs.formats.toml {};
  project = crateType: let
    projectedManifest = toml.generate "nix-host-${crateType}.toml" (
      manifest
      // {
        lib =
          manifest.lib
          // {
            crate-type = [crateType];
          };
      }
    );
  in
    buildPkgs.runCommand "nix-host-${crateType}-source" {} ''
      cp -R ${source} "$out"
      chmod u+w "$out" "$out/Cargo.toml"
      cp ${projectedManifest} "$out/Cargo.toml"
    '';
  releaseSource = project "cdylib";
  checkSource = project "rlib";
  clippyConfig = buildPkgs.writeTextDir "clippy.toml" (builtins.readFile (source + "/clippy.toml"));
  purePolicy =
    cargoUnit.policyPresets.pureBuild
    // {
      compiler.embedMetadata = true;
    };
  buildRelease = releaseSource:
    cargoUnit.buildWorkspace {
      pname = "nix-host-runtime";
      src = releaseSource;
      workspaceRoot = releaseSource;
      cargoLock = source + "/Cargo.lock";
      rustToolchain = targetToolchain;
      inherit target;
      profile = "release";
      cargoTargets = [
        [
          "--workspace"
          "--lib"
        ]
      ];
      policy = purePolicy;
      contentAddressed = false;
      env = nativeEnv // targetEnv;
      nativeBuildInputs = lib.optionals (appleToolchain != null) appleToolchain.runtimeInputs;
      extraLinkRustcArgsForPlatform = platform:
        lib.optional (
          hostPlatform.isDarwin && platform == target
        ) "-Clink-arg=-Wl,-headerpad_max_install_names";
      extraRustcArgsForPlatform =
        if appleToolchain != null
        then appleToolchain.rustcArgsForPlatform
        else (_platform: []);
    };
  release = buildRelease releaseSource;
  workspace = kind:
    cargoUnit.buildWorkspace {
      pname = "nix-host-${kind}";
      src = checkSource;
      workspaceRoot = checkSource;
      cargoLock = source + "/Cargo.lock";
      rustToolchain = nativeToolchain;
      target = buildPlatform.rust.rustcTarget;
      profile = "test";
      contentAddressed = false;
      env = nativeEnv;
      packageBuildEnv = lib.genAttrs workspacePackages (_package: {
        CLIPPY_CONF_DIR = clippyConfig;
      });
      cargoTargets = [
        [
          "--workspace"
          {
            tests = "--tests";
            docs = "--lib";
            clippy = "--all-targets";
          }
          .${
            kind
          }
        ]
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
            packages = workspacePackages;
            package =
              buildPkgs.clippy
              // {
                toolchain = nativeToolchain;
              };
            deniedLints = ["warnings"];
          };
        };
    };
  check = kind: let
    built = workspace kind;
    renderedTargets =
      {
        tests = built.testChecksByTarget;
        docs = lib.mapAttrs (_name: value: value.all) built.doctests;
        clippy = built.clippyByPackage;
      }
        .${
        kind
      };
    names = builtins.attrNames renderedTargets;
    expectedNames = lib.sort builtins.lessThan (
      if kind == "clippy"
      then workspacePackages
      else workspaceTargets
    );
    # Catalog names come from source manifests, never from the IFD renderer.
    targets = lib.genAttrs expectedNames (name: renderedTargets.${name});
  in
    buildPkgs.runCommand "nix-host-${kind}"
    {
      deps = assert lib.assertMsg (names == expectedNames)
      "nix-host ${kind} projection must cover every workspace package: expected ${toString expectedNames}, got ${toString names}";
        builtins.attrValues targets;
      __structuredAttrs = true;
    }
    ''
      mkdir -p "$out"
      printf '%s\n' ${lib.escapeShellArgs expectedNames} > "$out/targets"
    '';
  ownershipSource = builtins.path {
    path = ix.nixSrc + "/nix-meson-build-support/rust-runtime";
    name = "nix-runtime-ownership-source";
  };
  sourceChecks = buildPkgs.runCommand "nix-runtime-source-check-inputs" {} ''
    mkdir -p "$out"
    cp ${./runtime-sources.nix} "$out/runtime-sources.nix"
    cp ${./test_runtime_sources.py} "$out/test_runtime_sources.py"
  '';
  changedDomainSource = buildPkgs.runCommand "nix-host-store-stream-source-control" {} ''
    cp -R ${releaseSource} "$out"
    chmod u+w "$out/crates/store-stream/src/lib.rs"
    printf '\n// Store stream source isolation control.\n' >> "$out/crates/store-stream/src/lib.rs"
  '';
  changedDomain = buildRelease changedDomainSource;
  unitIdentityChanges = lib.genAttrs workspaceTargets (
    name: release.libraries.${name}.drvPath != changedDomain.libraries.${name}.drvPath
  );
  expectedUnitIdentityChanges = lib.genAttrs workspaceTargets (
    name:
      builtins.elem name [
        "nix_host_rs"
        "nix_store_stream"
      ]
  );
  checks =
    (lib.genAttrs ["tests" "docs" "clippy"] check)
    // {
      unitIsolation =
        buildPkgs.runCommand "nix-runtime-unit-isolation.json" {
          strictDeps = true;
          result = assert lib.assertMsg (unitIdentityChanges == expectedUnitIdentityChanges)
          "A store-stream source edit must invalidate only its unit and the host facade: ${builtins.toJSON unitIdentityChanges}";
            builtins.toJSON unitIdentityChanges;
        } ''
          printf '%s' "$result" > "$out"
        '';
      sourceIsolation =
        buildPkgs.runCommand "nix-runtime-source-isolation"
        {
          nativeBuildInputs = [
            buildPkgs.python3
            buildPkgs.nix
          ];
          strictDeps = true;
        }
        ''
          python ${sourceChecks}/test_runtime_sources.py
          mkdir -p "$out"
        '';
      ownership =
        buildPkgs.runCommand "nix-rust-runtime-ownership"
        {
          nativeBuildInputs = [
            buildPkgs.python3
            buildPkgs.meson
            buildPkgs.ninja
            buildPkgs.pkg-config
            buildPkgs.cargo
          ];
          strictDeps = true;
        }
        ''
          python ${ownershipSource}/test_ownership.py
          mkdir -p "$out"
        '';
    };
  library = release.libraries.nix_host_rs;
  runtimeLibrary = "libnix_host_rs${hostPlatform.extensions.sharedLibrary}";
in
  componentPkgs.stdenv.mkDerivation {
    pname = "nix-host-rs";
    version = "0.1.0";
    strictDeps = true;
    dontUnpack = true;
    dontConfigure = true;
    dontBuild = true;
    doCheck = true;
    checkPhase = ''
      # shell
      runHook preCheck
      cat ${checks.tests}/targets ${checks.docs}/targets
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
        *) echo "nix-host runtime did not produce a shared library: $artifact" >&2; exit 1 ;;
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
      cp ${source}/include/*.h "$out/include/"
      cat > "$out/lib/pkgconfig/nix-host-rs.pc" <<EOF_PC
      prefix=$out
      libdir=$out/lib
      includedir=$out/include

      Name: nix-host-rs
      Description: Evaluator-independent store and fetcher policies
      Version: 0.1.0
      Libs: -L$out/lib -lnix_host_rs
      Cflags: -I$out/include
      EOF_PC
      runHook postInstall
    '';
    passthru = {
      inherit checks;
      unitWorkspace = release;
    };
  }
