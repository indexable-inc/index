/**
The fleet's single node: installs the wgpu compute demo and runs it as the
health check.

Today the node has no GPU adapter, so the demo prints a "skipping" line and
exits 0 -- the health check reads "wired up, awaiting a GPU" rather than
"broken". Once the ix-side wgpu-over-virtio service (indexable-inc/ix#6537)
lands and the `ix-wgpu` guest crate is published, the same check exercises a
real dispatch against the host GPU over AF_VSOCK port 5010.
*/
{
  ix,
  lib,
  pkgs,
  ...
}: let
  demo = import ./package.nix {
    inherit ix lib;
    inherit (pkgs) rustPlatform;
  };
in {
  # On the PATH for interactive runs: `ix shell compute -- wgpu-compute-demo`.
  environment.systemPackages = [demo];

  ix.healthChecks.wgpu-compute = {
    description = "wgpu compute demo dispatches (or skips cleanly without an adapter)";
    command = [(lib.getExe demo)];
  };
}
