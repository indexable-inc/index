# The planner discovers a build script's declared sibling source. Running the
# script must preserve that source's path relative to CARGO_MANIFEST_DIR too.
# Clippy must also receive package-scoped compile inputs used by env!().
{
  lib,
  pkgs,
  ix,
}: let
  fixture = lib.fileset.toSource {
    root = ./fixtures/cargo-unit-build-script-closure;
    fileset = lib.fileset.unions [
      ./fixtures/cargo-unit-build-script-closure/Cargo.toml
      ./fixtures/cargo-unit-build-script-closure/Cargo.lock
      ./fixtures/cargo-unit-build-script-closure/crates
      ./fixtures/cargo-unit-build-script-closure/shared
    ];
  };
  workspace = ix.cargoUnit.buildWorkspace {
    pname = "cargo-unit-build-script-closure";
    src = fixture;
    workspaceRoot = ./fixtures/cargo-unit-build-script-closure;
    cargoArgs = ["--workspace"];
    packageBuildEnv.cargo-unit-build-script-closure.CARGO_UNIT_COMPILE_ENV = "scoped compile input";
    policy =
      ix.cargoUnit.policyPresets.pureBuild
      // {
        clippy.enable = true;
      };
  };
  reader = workspace.binaries.cargo-unit-build-script-closure;
in
  pkgs.runCommand "cargo-unit-build-script-closure-check" {
    clippy = workspace.clippyByPackage.cargo-unit-build-script-closure;
  } ''
    set -euo pipefail
    mkdir -p "$out"
    ${reader}/bin/cargo-unit-build-script-closure > "$out/message.txt"
    cmp "$out/message.txt" ${./fixtures/cargo-unit-build-script-closure/shared/message.txt}
  ''
