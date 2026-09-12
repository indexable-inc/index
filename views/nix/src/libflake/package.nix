{
  lib,
  mkMesonLibrary,
  nix-util,
  nix-store,
  nix-fetchers,
  nix-expr,
  nlohmann_json,
  cargo,
  rustc,
  rustPlatform,
  # Configuration Options
  version,
}: let
  inherit (lib) fileset;
in
  mkMesonLibrary (finalAttrs: {
    pname = "nix-flake";
    inherit version;

    workDir = ./.;
    fileset = fileset.unions [
      ../../nix-meson-build-support
      ./nix-meson-build-support
      ../../.version
      ./.version
      ./meson.build
      ./meson.options
      ./include/nix/flake/meson.build
      ./call-flake.nix
      # This component owns the standalone shared runtime. Include crate sources
      # explicitly so a developer's Cargo target directory never enters the source.
      ../../rust/Cargo.toml
      ../../rust/Cargo.lock
      ../../rust/nix-eval-rs
      ../../rust/ix-kernel
      ../../rust/nix-eval-driver
      ../libexpr/primops/derivation.nix
      ../libexpr/fetchurl.nix
      (fileset.fileFilter (file: file.hasExt "cc") ./.)
      (fileset.fileFilter (file: file.hasExt "hh") ./.)
    ];

    propagatedBuildInputs = [
      nix-store
      nix-util
      nix-fetchers
      nix-expr
      nlohmann_json
    ];

    nativeBuildInputs = [cargo rustc rustPlatform.cargoSetupHook];
    cargoRoot = "../../rust";
    cargoDeps = assert lib.assertMsg (builtins.pathExists ../../../rnix-0-12/Cargo.toml) ''
      the Rust runtime needs the sibling rnix-0-12 source view; build from the index monorepo or provide a prebuilt runtime
    '';
      rustPlatform.importCargoLock {lockFile = ../../rust/Cargo.lock;};

    postUnpack = ''
      ln -s ${builtins.path {
        path = ../../../rnix-0-12;
        name = "rnix-0-12";
        filter = path: _type: baseNameOf path != "target";
      }} "$NIX_BUILD_TOP/rnix-0-12"
    '';

    meta = {
      platforms = lib.platforms.linux ++ lib.platforms.darwin;
    };
  })
