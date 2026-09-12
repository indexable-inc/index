{
  lib,
  stdenv,
  mkMesonLibrary,

  unixtools,
  darwin,

  nix-util,
  boost,
  curl,
  aws-c-common,
  aws-crt-cpp,
  libseccomp,
  nlohmann_json,
  sqlite,
  cmake, # for resolving aws-crt-cpp dep
  cargo,
  rustc,
  rustPlatform,

  busybox-sandbox-shell ? null,

  # Configuration Options

  version,

  embeddedSandboxShell ? stdenv.hostPlatform.isStatic,

  withAWS ?
    # Default is this way because there have been issues building this dependency
    (lib.meta.availableOn stdenv.hostPlatform aws-c-common),
}:

let
  inherit (lib) fileset;
in

mkMesonLibrary (finalAttrs: {
  pname = "nix-store";
  inherit version;

  workDir = ./.;
  fileset = fileset.unions [
    ../../nix-meson-build-support
    ./nix-meson-build-support
    ../../.version
    ./.version
    ./meson.build
    ./meson.options
    ./include/nix/store/meson.build
    ./linux/meson.build
    ./linux/include/nix/store/meson.build
    ./unix/meson.build
    ./unix/include/nix/store/meson.build
    ./windows/meson.build
    ../../rust/nix-host-rs
    (fileset.fileFilter (file: file.hasExt "cc") ./.)
    (fileset.fileFilter (file: file.hasExt "hh") ./.)
    (fileset.fileFilter (file: file.hasExt "sb") ./.)
    (fileset.fileFilter (file: file.hasExt "md") ./.)
    (fileset.fileFilter (file: file.hasExt "sql") ./.)
  ];

  nativeBuildInputs =
    [cargo rustc rustPlatform.cargoSetupHook]
    ++ lib.optional withAWS cmake ++ lib.optional embeddedSandboxShell unixtools.hexdump;

  cargoRoot = "../../rust/nix-host-rs";
  cargoDeps = rustPlatform.importCargoLock {lockFile = ../../rust/nix-host-rs/Cargo.lock;};

  buildInputs = [
    boost
    curl
    sqlite
  ]
  ++ lib.optional stdenv.hostPlatform.isLinux libseccomp
  ++ lib.optional withAWS aws-crt-cpp;

  propagatedBuildInputs = [
    nix-util
    nlohmann_json
  ];

  mesonFlags = [
    (lib.mesonEnable "seccomp-sandboxing" stdenv.hostPlatform.isLinux)
    (lib.mesonBool "embedded-sandbox-shell" embeddedSandboxShell)
    (lib.mesonEnable "s3-aws-auth" withAWS)
  ]
  ++ lib.optionals stdenv.hostPlatform.isLinux [
    (lib.mesonOption "sandbox-shell" "${busybox-sandbox-shell}/bin/busybox")
  ];

  meta = {
    platforms = lib.platforms.linux ++ lib.platforms.darwin;
  };

})
