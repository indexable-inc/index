{
  # The Nix view, built through nixpkgs' modular nix packaging so it is
  # a drop-in for the daemon version the fleet runs (2.34.7). Surfaced as
  # `pkgs.nix-ix` in the repo package set and as the `nix-ix` flake output.
  #
  # Deliberately NOT in the nixpkgs overlay: the derivation reads
  # `pkgs.nixVersions.nixComponents_2_34` as its base, so injecting this package
  # under the bare `nix` name would make it its own base (infinite recursion),
  # exactly as nix-eval-jobs / nix-output-monitor document for their overrides.
  #
  # View updates move the daemon source deliberately. The updater only resolves the bootstrap action's
  # explicitly requested source ref into its generated lock; it does not move
  # the daemon version.
  id = "nix-ix";
  packageSet = true;
  # Not a flake output, not a tested package, not a cross entry: index can
  # no longer BUILD nix-ix. The fork's libfetchers links `libjj_tree.a`
  # (crate `jj-tree-abi`), which lives in the ix repository outside this
  # flake's source root, so default.nix takes it as an argument that throws
  # when forced. Every one of the three flags below forces it: a flake
  # output is evaluated by `nix flake check` and the flake-schema gate, a
  # passthru test set is collected into `checks`, and a cross entry is a
  # second flake output. Leaving any of them on turns index's own CI red
  # for a package index cannot complete. The recipe stays in the registry
  # package set (`packageSetFor`), which is how ix reaches it:
  # `(indexLib.packageSetFor pkgs).nix-ix.override { jjTree = ...; }`
  # (ix: nix/flake/outputs/workspace.nix). Tests and the Darwin cross build
  # (RFC 0009, #3585) go with the override, so they are ix's to expose.
  flake = false;
  overlay = false;
  passthruTests = false;
  updateScript = true;
  cross = false;
}
