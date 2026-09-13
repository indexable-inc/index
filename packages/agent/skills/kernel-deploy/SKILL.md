---
name: kernel-deploy
description: "Rolling out kernel changes across cluster nodes: build once on one host, push the kbuild-unit cachePushRoot once, every other node substitutes. Use when a change touches the kernel (patch, config, src bump) and more than one node needs it."
---

## Deploying kernel changes

A kernel change is the most expensive rebuild in the tree. One host builds
it; the cache carries it; every other node substitutes. Never let two nodes
build the same kernel.

## Landed changes

Merge to main. `cache-push.yml` realises the push roots on the CI pool and
publishes to cache.ix.dev; `advance-cache-ready` moves the consumer pin once
every root realised. Then roll nodes with the fleet verbs (`switch` for an
in-place system switch, `up` to push and replace on a new image; see
`doc/ix/fleet.md`). Nodes substitute the new system closure instead of
rebuilding it.

## Iterating before merge: the kbuild-unit shared cache

For kernel iteration, use the per-TU lanes under `legacyPackages`
(x86_64-linux only, #3413): `kernel-unit` (tinyconfig),
`kernel-unit-defconfig`, `kernel-unit-ccache` (static fallback plan
strategy).

On one trusted builder:

```sh
# 1. Mass unit build with the per-derivation cache hook OFF: one synchronous
#    enqueue per unit serialized 3.6k-unit builds under queue backpressure.
nix build --option post-build-hook '' .#kernel-unit-defconfig.allUnits

# 2. Push once, under normal settings. cachePushRoot is one linkFarm whose
#    runtime closure spans every unit output plus the IFD artifacts (plan,
#    rendered units.nix, snapshot, skeleton tree). Building it enqueues ONE
#    obligation; the drainer publishes the NARs and the CA realisations
#    other hosts need to substitute instead of rebuild.
nix build .#kernel-unit-defconfig.cachePushRoot
```

On every other node, build normally; units substitute from the cache:

```sh
nix build .#kernel-unit-defconfig.vmlinux
```

## Why iteration stays cheap

- Plan reruns reproduce the snapshot bit-identically (#3667), so a header or
  Makefile edit re-executes only the units whose inputs changed; the rest
  substitute from the last push, including one made by another host.
- Dep sets are trimmed: a one-module body edit rebuilds that module's TU,
  its `.ko` link, and the modpost pass. No other module re-links.
- First eval of a fresh plan pays the IFD (a full monolithic kernel build at
  eval time). Push the root right after so no second host pays it again.

## Validate before rolling out

The byte-equivalence gates prove the unit-composed kernel matches the
monolithic reference build:

```sh
nix build .#kernel-unit-defconfig.vmlinuxEquivalence \
  .#kernel-unit-defconfig.modulesEquivalence
```
