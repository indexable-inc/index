{
  description = "ix example: wgpu-compute";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    index = {
      url = "github:indexable-inc/index";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = {index, ...}: let
    fleet = import ./ix.nix {inherit index;};
  in {
    ix.fleets.default = fleet;
    inherit (fleet) nixosConfigurations;
  };
}
