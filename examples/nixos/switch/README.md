<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/hero-dark.svg">
    <img src="assets/hero.svg" width="720" alt="ix apply uploads configuration.nix, builds the closure server-side, and switches the running devbox VM in place">
  </picture>
</p>

# NixOS switch

Want `nixos-rebuild switch` for a VM in the cloud, without building on your
laptop or shipping closures over your uplink? `ix apply .#devbox` uploads this
source tree to ix, builds the NixOS closure server-side, and activates it on
the running VM in place. Re-running converges the VM to the current
configuration, the same contract as `nixos-rebuild switch`.

## Run

```sh
# From this directory.
ix apply .#devbox
```

The first run creates `devbox` from `ix/base:latest` and activates this
configuration on it. Get the repo with
`git clone https://github.com/indexable-inc/index`.

## The loop

1. Edit [`configuration.nix`](configuration.nix): add a package to
   `environment.systemPackages` (try `pkgs.ripgrep`).
2. Run `ix apply .#devbox` again. ix uploads the change, builds the new closure,
   and switches the running VM to it.
3. `ix shell devbox` and confirm the new package is on `PATH`.

The VM keeps running across switches: only its system generation changes,
nothing is recreated.

## Shape

- [`flake.nix`](flake.nix) is the native `ix apply` entrypoint. It exposes
  `nixosConfigurations.devbox`, which `ix apply .#devbox` resolves to the NixOS
  system closure.
- [`default.ix`](default.ix) declares the VM with `index.lib.mkVm`; the flake
  inherits `nixosConfigurations` from that result and binds no `ix.default`,
  which a fleet result cannot satisfy.
- [`configuration.nix`](configuration.nix) is the NixOS module you edit.

## Fork it

Copy this directory into your own repo and keep `flake.nix` as the
entrypoint; its `ix` input pulls `github:indexable-inc/ix` for you, and
`ix.index` is index instantiated with the guest Nix an image needs (the bare
`github:indexable-inc/index` flake cannot evaluate one). The switch path needs
no admin rights: it builds and activates your own system onto your own VM.

## Scope

This builds on the target VM itself, the `ix apply` default. Switching
several VMs in one command is what [`switch-multi`](../../multi-vm/switch-multi)
demonstrates.
