{ix}: let
  # `ix.buildRustPackage` is curried on the full package set; read it from `ix`
  # rather than taking a `pkgs` callPackage formal (which `override` can't reach).
  inherit (ix) pkgs;
  inherit (pkgs) lib;
in
  # nix-cargo-unit bootstraps the unit graph, so it cannot consume ix.cargoUnit
  # and is built as a plain Rust package. It is a standalone Cargo workspace
  # (its own Cargo.toml + Cargo.lock, excluded from the root workspace), so the
  # build closure is exactly its own folder: a root-workspace lock bump no
  # longer invalidates it. `srcRoot = ./.` imports that folder whole, and
  # defaults `cargoLock` to the in-tree `Cargo.lock` and `meta.mainProgram` to
  # the pname.
  ix.buildRustPackage pkgs {
    pname = "nix-cargo-unit";
    # Single source of truth: read the version from the crate manifest rather
    # than re-pinning it here, so the two cannot drift.
    version = (lib.importTOML ./Cargo.toml).package.version;
    srcRoot = ./.;
  }
