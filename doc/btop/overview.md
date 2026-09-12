# btop

`packages/btop` repackages [btop](https://github.com/aristocratos/btop), the
resource monitor (CPU, memory, disk, network, process TUI), rebuilt from a
repo-owned fork instead of the upstream source. It is the minimal repackage
shape: take the nixpkgs `btop` derivation and override only its source.

## What this repo changes

`default.nix` is the whole delta: `btop.overrideAttrs` swaps `src` to
`ix.btopSrc`. Everything else is inherited from nixpkgs `btop` unchanged.

- Source: `ix.btopSrc` resolves to `views/btop`. The view carries its upstream
  anchor and fork commits in the repository's jj history.
- `meta.homepage` points at `indexable-inc/btop` so the built package advertises
  the fork, not upstream (`packages/btop/default.nix:10`).

## Build and wiring

- Flake output: `nix run .#btop` / `nix build .#btop`. `package.nix` sets
  `packageSet = true` and `flake = true` (`packages/btop/package.nix:1-5`); no
  overlay, so `pkgs.btop` stays the plain nixpkgs monitor.
- Update: `jj view refresh btop` from a jj workspace (docs/jj-view.md in ix).
- Platforms: inherited from nixpkgs `btop` (unix); no extra `systems` gate.
- Darwin consumers substitute a Linux cross build: `package.nix` sets
  `cross = true` (#3584), so the RFC 0009 lane compiles a Mach-O arm64 btop
  with the apple-sdk cross toolchain (clang + macOS SDK) and aliases it into
  `packages.aarch64-darwin.btop`; the darwin cache-push lane no longer builds
  btop natively.

Because the derivation is `overrideAttrs` over the upstream package, a nixpkgs
bump to `btop` (new build inputs, phase changes) flows through automatically;
only a source incompatible with the upstream build recipe would need work here.
