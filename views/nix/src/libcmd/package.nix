{
  lib,
  stdenv,
  mkMesonLibrary,
  nix-util,
  nix-store,
  nix-fetchers,
  nix-expr,
  nix-flake,
  nix-main,
  lowdown,
  nlohmann_json,
  # Configuration Options
  version,
  # Whether to enable Markdown rendering in the Nix binary.
  enableMarkdown ? !stdenv.hostPlatform.isWindows,
}: let
  inherit (lib) fileset;
in
  mkMesonLibrary (finalAttrs: {
    pname = "nix-cmd";
    inherit version;

    workDir = ./.;
    fileset = fileset.unions [
      ../../nix-meson-build-support
      ./nix-meson-build-support
      ../../.version
      ./.version
      ./meson.build
      ./meson.options
      ./include/nix/cmd/meson.build
      (fileset.fileFilter (file: file.hasExt "cc") ./.)
      (fileset.fileFilter (file: file.hasExt "hh") ./.)
    ];

    buildInputs = lib.optional enableMarkdown lowdown;

    propagatedBuildInputs = [
      nix-util
      nix-store
      nix-fetchers
      nix-expr
      nix-flake
      nix-main
      nlohmann_json
    ];

    # libflake installs the runtime and its pkg-config file. This consumer
    # links that same shared library and never builds a second Rust runtime.
    mesonFlags = [
      (lib.mesonEnable "markdown" enableMarkdown)
    ];

    meta = {
      platforms = lib.platforms.linux ++ lib.platforms.darwin;
    };
  })
