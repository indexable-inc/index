# wgpu compute

A single-node fleet that runs an ordinary wgpu v30 compute shader on the VM:
upload sixteen `u32`s, square them on the GPU, map the result back, print it.
The point is the boundary, not the math -- arbitrary standard `wgpu` code
running inside a fleet VM against the host's GPU, through a validated
protocol instead of device passthrough.

## How it binds to the host GPU

The ix side is [indexable-inc/ix#6537](https://github.com/indexable-inc/ix/pull/6537)
(draft): a host GPU service on the VMM plus an `ix-wgpu` guest crate that
implements wgpu's custom-backend dispatch traits and forwards every call as
length-prefixed postcard frames over AF_VSOCK guest port **5010** (headless
compute only; no surfaces).

The demo's only platform seam is one function, `create_instance` in
[`src/main.rs`](src/main.rs). Everything after the instance -- adapter,
device, buffers, pipeline, dispatch, readback -- is byte-for-byte standard
wgpu. Once `ix-wgpu` is published, that one function swaps to the custom
backend and the rest of the program is untouched.

## Current status

- ix#6537 is a draft; the `ix-wgpu` crate is not yet published, so this
  example instantiates the stock wgpu backends (Vulkan/GL) today.
- Fleet VMs and CI runners have no GPU, so the demo detects "no adapter",
  prints a skip line, and exits 0. The health check therefore reads "wired
  up, awaiting a GPU" rather than "broken". On a workstation with a GPU the
  same binary runs the real dispatch.

## Run

```sh
# From the index repo root.
nix run .#wgpu-compute-up
```

## Shape

- [`ix.nix`](ix.nix) defines the fleet: one `compute` node.
- [`compute.nix`](compute.nix) installs the demo binary and runs it as the
  node's health check.
- [`package.nix`](package.nix) builds the demo as a standalone crate with its
  own committed `Cargo.lock`, keeping the wgpu tree out of the repo's root
  workspace lockfile.
- [`src/main.rs`](src/main.rs) is the demo; [`src/square.wgsl`](src/square.wgsl)
  is the shader.

## Verify

```sh
ix shell compute -- wgpu-compute-demo
```

Without an adapter it prints the skip line; with one it prints the adapter
info, the sixteen squares, and `OK` (and exits nonzero if the GPU result ever
disagrees with the CPU).
