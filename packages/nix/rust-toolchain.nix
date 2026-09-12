# One build/target toolchain contract for independent host and evaluator runtimes.
{
  ix,
  lib,
  componentPkgs,
}: let
  buildPkgs = componentPkgs.buildPackages;
  hostPlatform = componentPkgs.stdenv.hostPlatform;
  buildPlatform = buildPkgs.stdenv.hostPlatform;
  isCross = buildPlatform.rust.rustcTarget != hostPlatform.rust.rustcTarget;
  # packageSetFor and crossIxFor bind this factory to ix.pkgs, the build
  # platform. The component scope may independently target another platform.
  cargoUnit = assert lib.assertMsg (
    ix.pkgs.stdenv.hostPlatform.rust.rustcTarget == buildPlatform.rust.rustcTarget
  ) "nix-eval runtime requires a build-platform ix.cargoUnit factory";
    ix.cargoUnit;
  nativeToolchain = buildPkgs.symlinkJoin {
    name = "nix-eval-rust-toolchain";
    paths = [
      buildPkgs.rustc
      buildPkgs.cargo
    ];
    passthru = {
      inherit (buildPkgs.rustc) version targetPlatforms badTargetPlatforms;
    };
  };
  target = hostPlatform.rust.rustcTarget;
  targetToolchain =
    if isCross
    then
      ix.languages.rust.toolchain buildPkgs {
        channel = "stable";
        version = "latest";
        targets = [target];
      }
    else nativeToolchain;
  appleToolchain =
    if isCross && hostPlatform.isDarwin
    then
      ix.appleSdkToolchain {
        pkgs = buildPkgs;
        appleSdk = ix.macosSdk {pkgs = buildPkgs;};
        inherit lib target;
        inherit (ix) writeBashApplication;
      }
    else null;

  compilerEnv = platform: cc: let
    suffix = lib.replaceStrings ["-"] ["_"] platform.rust.rustcTarget;
    cargoSuffix = lib.toUpper suffix;
  in {
    "CC_${suffix}" = lib.getExe' cc "${cc.targetPrefix}cc";
    "CXX_${suffix}" = lib.getExe' cc "${cc.targetPrefix}c++";
    "AR_${suffix}" = lib.getExe' cc.bintools "${cc.bintools.targetPrefix}ar";
    "CARGO_TARGET_${cargoSuffix}_LINKER" = lib.getExe' cc "${cc.targetPrefix}cc";
  };
  nativeEnv = compilerEnv buildPlatform buildPkgs.stdenv.cc;
  targetEnv =
    if appleToolchain != null
    then appleToolchain.env
    else compilerEnv hostPlatform componentPkgs.stdenv.cc;
in {
  inherit
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
}
