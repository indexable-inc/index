{
  description = "ix example: minecraft-blocks";

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
    vms = importIx ./default.ix {inherit index;};
  in {
    inherit (vms) nixosConfigurations;
  };
}
