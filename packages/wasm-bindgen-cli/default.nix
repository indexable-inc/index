# Pinned `wasm-bindgen-cli` matching the `wasm-bindgen` Cargo dep.
#
# Schemas are not stable across patch bumps, so the CLI and the Rust crate MUST
# match exactly. Built via nixpkgs's `buildWasmBindgenCli` factory. Consumers
# that build wasm artifacts pin their `wasm-bindgen` crate to this version.
{
  buildWasmBindgenCli,
  fetchCrate,
  ix,
  lib,
  rustPlatform,
  # Writer for `passthru.updateScript` (flake-package path only); null on the
  # overlay path.
  updateScriptWriter ? null,
}: let
  # Version + crate URL and SRI hash live in the sibling pins.json, never inline
  # (repo policy). Keep in sync with the `wasm-bindgen` Cargo dep; bump the
  # version/url in pins.json, then `nix run .#update` re-pins the hash.
  pin = ix.pins.loadPin ./pins.json "wasm-bindgen-cli";
  inherit (pin) version;
  updateScript = ix.pins.mkOptionalUpdater {
    writeNushellApplication = updateScriptWriter;
    nix = ix.nixPackageFor "packages/wasm-bindgen-cli: updateScript";
    pname = "wasm-bindgen-cli";
    relPath = "packages/wasm-bindgen-cli/pins.json";
  };
  src = fetchCrate {
    pname = "wasm-bindgen-cli";
    inherit (pin) version url hash;
  };
  # `rustPlatform.importCargoLock` materializes the complete cargoDeps shape
  # (per-crate `<name>-<version>` symlinks, `.cargo/config.toml`, and
  # `Cargo.lock`) and vendors registry crates through `static.crates.io`, so the
  # older `crates.io/api/v1/crates/.../download` endpoint (which rejects
  # cargo-vendor's curl User-Agent) is never used. See nixpkgs
  # `pkgs/build-support/rust/import-cargo-lock.nix`.
  cargoDeps = rustPlatform.importCargoLock {
    lockFile = src + "/Cargo.lock";
  };
in
  (buildWasmBindgenCli {
    inherit version src cargoDeps;
  }).overrideAttrs
  (old: {
    passthru =
      (old.passthru or {}) // lib.optionalAttrs (updateScript != null) {inherit updateScript;};
  })
