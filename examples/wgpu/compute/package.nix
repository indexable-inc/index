/**
The demo binary: a standalone Rust crate built straight from this directory
with its own committed Cargo.lock, so the wgpu dependency tree stays out of
the repo's root workspace lockfile (the example is meant to read like any
external wgpu project, not like repo plumbing).

The compute path is ordinary wgpu v30; the only platform seam is
`create_instance` in src/main.rs, where the unpublished `ix-wgpu` guest crate
(indexable-inc/ix#6537) will slot in its custom backend once it ships.
*/
{
  ix,
  lib,
  rustPlatform ? ix.pkgs.rustPlatform,
}: let
  fs = lib.fileset;
  src = fs.toSource {
    root = ./.;
    fileset = fs.unions [
      ./Cargo.toml
      ./Cargo.lock
      ./src
    ];
  };
in
  rustPlatform.buildRustPackage {
    pname = "wgpu-compute-demo";
    inherit ((lib.importTOML ./Cargo.toml).package) version;
    inherit src;

    # Pure-crates.io lockfile committed next to the sources; no cargoHash to
    # refresh by hand when the dependency set moves.
    cargoLock.lockFile = ./Cargo.lock;

    strictDeps = true;

    meta = {
      description = "Standard wgpu compute shader demo for ix fleet VMs";
      mainProgram = "wgpu-compute-demo";
    };
  }
