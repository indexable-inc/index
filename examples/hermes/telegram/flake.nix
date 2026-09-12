{
  description = "ix example: hermes-telegram";

  inputs = {
    # index instantiated with its guest Nix (`ix.index`). The bare
    # `github:indexable-inc/index` flake has no guest Nix by construction (the
    # jj tree ABI the fork links lives in ix) and refuses every image, so the
    # VM below comes from ix's instantiation of index. No nixpkgs input: the
    # image and cache.ix.dev were built against the nixpkgs ix locks, and a
    # second pin here would rebuild that world inside the VM.
    ix.url = "github:indexable-inc/ix/warmed-main";
  };

  outputs = {ix, ...}: let
    inherit (ix) index;
    # `default.ix` is JavaScript-syntax Nix. `builtins.wasm` converts it during
    # evaluation, so evaluating this flake takes index's patched nix with
    # `wasm-builtin` in `extra-experimental-features` (`ix apply` and `ix eval`
    # pass the flag).
    importIx = index.lib.importIxWasm;
    vm = importIx ./default.ix {inherit index;};
  in {
    # No `ix.default`. `mkVm` is a one-node `mkFleet`, so `vm` is a fleet result
    # and has no `config`. A bare `ix apply` prefers a flake's `ix.default` and
    # builds `ix.default.config.system.build.toplevel` from it, so binding the
    # fleet result here fails the apply on a missing attribute instead of
    # converging the node below.
    inherit (vm) nixosConfigurations;
  };
}
