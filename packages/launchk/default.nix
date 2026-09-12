{
  lib,
  rustPlatform,
  ix,
}:
# Reference package for the external-Rust-tool house style: a standalone
# third-party binary built from a checked jj view (views.toml) with
# `rustPlatform.buildRustPackage`. See `skills/dependency-intake/SKILL.md`.
let
  src = ix.launchkSrc;
in
  rustPlatform.buildRustPackage {
    pname = "launchk";
    # No upstream release tag past the crate version; pin to the master rev with
    # the nixpkgs unstable-version spelling so a bump reads as a dated change.
    version = "0.3.1-unstable-2025-06-07";

    inherit src;

    # launchk commits a pure-crates.io Cargo.lock, so read it straight from the
    # source: a rev bump carries the dependency set with no checked-in lock to
    # drift and no coarse cargoHash to refresh by hand.
    cargoLock.lockFile = src + "/Cargo.lock";

    strictDeps = true;

    # xpc-sys generates the XPC framework bindings with bindgen, which needs
    # libclang on the build host.
    nativeBuildInputs = [rustPlatform.bindgenHook];

    cargoBuildFlags = [
      "-p"
      "launchk"
    ];
    cargoTestFlags = [
      "-p"
      "launchk"
    ];

    meta = {
      description = "Cursive TUI for observing launchd agents and daemons";
      homepage = "https://github.com/intellekthq/launchk";
      license = lib.licenses.mit;
      mainProgram = "launchk";
      platforms = lib.platforms.darwin;
    };
  }
