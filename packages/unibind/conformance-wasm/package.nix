{
  id = "unibind-conformance-wasm";
  workspaceIfdRoots = true;
  inRustWorkspace = true;
  # Linux-only like the ts conformance package: CI runs the Node end-to-end
  # suite on the linux builders, and the wasm32 unit graph + pinned
  # wasm-bindgen-cli that assemble the browser package are exercised there.
  # The artifact itself is portable (no cpu/libc stamping).
  flake.systems = [
    "x86_64-linux"
    "aarch64-linux"
  ];
  packageSet.systems = [
    "x86_64-linux"
    "aarch64-linux"
  ];
  # Gate the Node end-to-end suite (passthru.tests.node-conformance) in CI
  # as `checks.<system>.unibind-conformance-wasm-node-conformance`.
  passthruTests = {
    prefix = "unibind-conformance-wasm";
  };
}
