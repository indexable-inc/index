{
  description = "ix apply multi-VM switch: several NixOS VMs switched in one command";

  inputs = {
    # The one example that keeps the bare public index flake as its input.
    # Every other example reads index through ix (`ix.index`, index
    # instantiated with its guest Nix) because it applies an index image; this
    # one demonstrates raw NixOS attrs, not the ix VM wrapper, so it uses only
    # the `.ix` converter from index, which needs no guest Nix.
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    index = {
      url = "github:indexable-inc/index";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {
    nixpkgs,
    index,
    ...
  }: let
    # `default.ix` is JavaScript-syntax Nix. `builtins.wasm` converts it during
    # evaluation, so evaluating this flake takes index's patched nix with
    # `wasm-builtin` in `extra-experimental-features` (`ix apply` and `ix eval`
    # pass the flag).
    importIx = index.lib.importIxWasm;
    example = importIx ./default.ix {inherit nixpkgs;};
  in {
    inherit (example) nixosConfigurations;
  };
}
