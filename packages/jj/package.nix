{
  id = "jj";
  packageSet = true;
  # NOT a flake output. The binary this builds is the vendored fork's plain
  # `jj`, and there is exactly one jj on a host now: `packages/jj-ix` in the
  # ix root flake, which is this same CLI plus the ix store factories and the
  # native verbs. Exposing a second one as `index#jj` is how a host ended up
  # with two, only one of which had `view`. Nothing in this repository
  # installs or runs it (`rg 'indexPkgs\.jj\b|index\.packages\.<sys>\.jj\b'`
  # is empty outside the vendored tree).
  #
  # The PACKAGE stays, and deleting it would be the mistake here: it is the
  # only derivation that instantiates the vendored fork's cargo unit graph,
  # and `passthruTests` below is what puts the fork's clippy gates into
  # `checks`/`ciChecks`. Those are harvested through `packageSet`
  # (index/lib/per-system.nix, `packageTestsFor` reads `repoPackages`), not
  # through the flake output, so dropping the output keeps every gate.
  flake = false;
  overlay = false;
  # Puts jj's `passthru.tests` in `checks`/`ciChecks`. That set is exactly the
  # two clippy gates for the crates we own in this vendored workspace (see
  # `policy.clippy.packages` in default.nix); without the flag they are
  # derivations nothing builds, which is a lint gate that passes by never
  # running.
  passthruTests = true;
}
