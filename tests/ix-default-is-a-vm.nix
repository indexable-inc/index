# Guard: a flake's `ix.default` output must be a NixOS configuration, never a
# fleet result.
#
# `ix apply` with a bare local target picks between two paths by looking for an
# `ix.default` binding in the flake source (`flake_exposes_ix_default` in ix's
# crates/ix/cli/src/commands/up.rs). Where it finds one it takes the single-VM
# path and builds `ix.default.config.system.build.toplevel`; where it does not
# it converges every `nixosConfigurations` entry that sets `ix.networking`.
#
# The detector reads whether a binding EXISTS, never what it is bound to.
# `mkVm`, `mkDev` and `mkFleet` all return the same fleet result, and a fleet
# result has no `config` -- its attributes are `nodes`, `planValue`,
# `nixosConfigurations`, the lifecycle wrappers. So a flake binding one of those
# to `ix.default` takes the single-VM path and dies with
#
#   flake '...' does not provide attribute 'packages.x86_64-linux.ix.default.config',
#   'legacyPackages.x86_64-linux.ix.default.config' or 'ix.default.config'
#
# naming an attribute path the user never typed. Every in-tree flake that bound
# `ix.default` bound a fleet result, all 15 of them, and no bare `ix apply` in
# any of those directories could work. The right binding is the one `ix init`
# scaffolds: `(mkFleet { ... }).nixosConfigurations.<name>`, which does have
# `config`.
#
# This calls each flake's real `outputs` function rather than matching its text,
# so a binding computed behind a `let` or a helper is classified by what it
# actually is.
{
  lib,
  nixpkgs,
  pkgs,
  ix,
  paths,
}: let
  hostSystem = pkgs.stdenv.hostPlatform.system;

  # The index surface as a project flake sees it: `importIxWasm` plus the
  # builders. The `*For` swap matches `exampleFleetsFor` so any wrapper
  # derivation would target this system rather than the default one.
  indexShim = {
    lib =
      ix
      // {
        mkFleet = ix.mkFleetFor hostSystem;
        mkVm = ix.mkVmFor hostSystem;
        mkDev = ix.mkDevFor hostSystem;
      };
  };

  # The `ix` flake input as a project flake sees it, which is how every
  # in-tree example reaches index since the guest-Nix injection: their inputs
  # are `ix.url = ...` and their outputs bind `index = ix.index` (flake.nix
  # `withNixPackage` is the WHY). `index` here is the same shim as the legacy
  # direct input above: inside this evaluation `ix` (the lib) already carries
  # whatever guest Nix this instantiation was given, so the shim IS the
  # injected surface when the gate runs from ix, and the standalone refusals
  # stay lazy exactly as they do for `exampleFleetsFor` (nothing on the
  # ix.default path forces an image build). `inputs.nixpkgs` carries the REAL
  # nixpkgs this check evaluates under, for the one example that reaches
  # through the flake record for it (templates/workers' host-side eval
  # check): this gate runs inside an evaluation where the real value exists,
  # so the stub does not invent one.
  ixShim = {
    index = indexShim;
    inputs = {inherit nixpkgs;};
  };

  # Flake inputs this gate can stand in for. Anything else required makes the
  # flake unevaluable here, and it falls to the source check below. `ix` is
  # the migrated examples' one input; `index` stays suppliable for
  # switch-multi (converter-only, deliberately on the bare index flake) and
  # any pre-migration out-of-tree shape that lands in the walk.
  suppliable = [
    "self"
    "index"
    "ix"
    "nixpkgs"
  ];

  # Directories the walk refuses to enter at all, with everything under them.
  # `views/` holds checked-in upstream trees: `viewSource` in lib/default.nix
  # resolves every view to `views/<name>`, so what is inside one is another
  # project's repository, and another project's test fixtures are never our
  # examples. Excluding the root rather than a path inside it is the point,
  # because the fixtures are not enumerable: there are 17 `flake.nix` files
  # under `views/` today and the next view somebody adds brings its own.
  #
  # ENG-12220 turned these fixtures from a fetched flake input, invisible here,
  # into files in this tree, and the walk started classifying them: 30 flakes
  # with this exclusion, 47 without. ENG-12505 is what that cost, though the
  # throw itself came from `classify` passing arguments the flake never declared
  # and is fixed there. Both changes stay. Either one alone makes the gate
  # evaluate again, and only this one keeps a foreign test suite out of it,
  # which is what matters the next time a fixture throws for reasons of its own.
  excludedRoots = ["views"];

  # A directory the walk descends into. `_`- and `.`-prefixed names are skipped
  # with their subtree, matching `discoverTree` in lib/discovery.nix.
  walkable = rel: entries: name:
    entries.${name}
    == "directory"
    && !(lib.hasPrefix "_" name)
    && !(lib.hasPrefix "." name)
    && !(builtins.elem (childOf rel name) excludedRoots);

  childOf = rel: name:
    if rel == ""
    then name
    else "${rel}/${name}";

  # The repo root is `paths.root` itself, not `paths.root + "/"`.
  dirOf = rel:
    if rel == ""
    then paths.root
    else paths.root + "/${rel}";

  fileOf = rel:
    if rel == ""
    then "flake.nix"
    else "${rel}/flake.nix";

  # Every directory in the repo holding a `flake.nix`, the repo root included.
  # Enumerated by walking rather than listed, so a project flake added anywhere
  # is covered on the next eval with no registry edit. Only `readDir` and the
  # 30-odd `flake.nix` files are read (0.4s on the tree this was written
  # against), and nothing derived from `paths.root` reaches the derivation, so
  # this does not couple the check to the whole tree.
  flakeDirs = let
    walk = path: rel: let
      entries = builtins.readDir path;
    in
      lib.optional ((entries."flake.nix" or null) == "regular") rel
      ++ lib.concatMap (name: walk (path + "/${name}") (childOf rel name)) (
        builtins.filter (walkable rel entries) (builtins.attrNames entries)
      );
  in
    walk paths.root "";

  shapeOf = binding:
    if binding == null
    then "no-binding"
    else if binding ? config
    then "vm"
    else "fleet";

  classify = rel: let
    dir = dirOf rel;
    flake = import (dir + "/flake.nix");
    args = builtins.functionArgs flake.outputs;
    # `functionArgs` marks an argument false when it has no default, so this is
    # exactly the set of inputs that would throw if we called `outputs`.
    unsuppliable =
      builtins.filter (name: !(builtins.elem name suppliable) && !args.${name})
      (builtins.attrNames args);
    # Only what the flake declares. Passing all three unconditionally throws
    # `called with unexpected argument` on any closed argument pattern that
    # wants fewer, which `unsuppliable` above cannot predict: `functionArgs`
    # reports the names an argument pattern binds and never whether it ends in
    # an ellipsis. A pattern that binds fewer names can also use no more, so
    # intersecting loses the gate nothing.
    #
    # This is the throw in ENG-12505, and it was never specific to a view: any
    # flake in this tree written `outputs = { self, nixpkgs }:` would have
    # stopped the gate the same way, with an error naming neither this file nor
    # the argument the gate added.
    supplied = builtins.intersectAttrs args {
      self = dir;
      index = indexShim;
      ix = ixShim;
      inherit nixpkgs;
    };
    outputs = builtins.addErrorContext "while evaluating the flake outputs of ${fileOf rel} for tests/ix-default-is-a-vm.nix" (
      flake.outputs supplied
    );
  in {
    file = fileOf rel;
    # Lazy on purpose: `outputs` throws for a flake we cannot supply, so the
    # shape is only asked for once `unsuppliable` says it is safe to ask.
    kind =
      if unsuppliable != []
      then "unevaluable"
      else shapeOf ((outputs.ix or {}).default or null);
  };

  classified = map classify flakeDirs;
  kind = name: builtins.filter (entry: entry.kind == name) classified;
  files = entries: map (entry: entry.file) entries;

  # A flake this gate cannot call gets the source check instead. Weaker, and
  # said out loud rather than skipped: a silent skip would make a new broken
  # binding in such a flake indistinguishable from no binding at all.
  unevaluableMentioningIxDefault =
    builtins.filter (
      entry:
        lib.hasInfix "ix.default"
        (builtins.readFile (paths.root + "/${entry.file}"))
    )
    (kind "unevaluable");

  # The passing state of the main assertion is an ABSENCE: with every in-tree
  # binding removed, `kind "fleet"` is empty whether the check works or the walk
  # silently found nothing. These two keep it honest.
  #
  # The predicate still discriminates. Both shapes come out of one `mkVm` call:
  # the fleet result it returns, and the NixOS configuration inside it, which is
  # the binding `ix init` scaffolds.
  fleetShaped = (ix.mkVmFor hostSystem) {modules = [];};
  vmShaped = fleetShaped.nixosConfigurations.default;

  # And the walk still found flakes to classify. Floors with slack, not exact
  # counts: they exist to fail when the walk returns nothing, not to be
  # maintained as flakes come and go.
  discoveredFloor = 20;
  evaluableFloor = 15;
  evaluable = builtins.length classified - builtins.length (kind "unevaluable");

  # And `excludedRoots` still names something. An exclusion that stops matching
  # is indistinguishable from no exclusion until the day it lets a foreign tree
  # back in, which is the failure it was written for.
  rootEntries = builtins.readDir paths.root;
  missingExcludedRoots =
    builtins.filter (rel: (rootEntries.${rel} or null) != "directory") excludedRoots;
in
  assert lib.assertMsg (!(fleetShaped ? config) && vmShaped ? config) ''
    tests/ix-default-is-a-vm.nix cannot tell a fleet result from a VM any more:
    `mkVm {}` reports config=${lib.boolToString (fleetShaped ? config)} and its
    `nixosConfigurations.default` reports config=${lib.boolToString (vmShaped ? config)}.

    The first should be false and the second true. Until they are, the
    ix.default check below passes by testing nothing.
  '';
  assert lib.assertMsg (builtins.length classified >= discoveredFloor && evaluable >= evaluableFloor) ''
    tests/ix-default-is-a-vm.nix found ${toString (builtins.length classified)} flake.nix
    files (floor ${toString discoveredFloor}), ${toString evaluable} of them evaluable
    (floor ${toString evaluableFloor}).

    The walk over paths.root returned too little to be checking anything. Either
    the tree moved under it or `flakeDirs` is broken; fix that before lowering
    the floors.
  '';
  assert lib.assertMsg (missingExcludedRoots == []) ''
    tests/ix-default-is-a-vm.nix excludes these roots from the walk, and they
    are not directories in the repository root any more:

      ${lib.concatStringsSep "\n      " missingExcludedRoots}

    They hold checked-in upstream trees whose own `flake.nix` fixtures this
    gate must not call. If a root moved, point `excludedRoots` at where it went
    rather than dropping the entry, or the walk starts classifying another
    project's test suite.
  '';
  assert lib.assertMsg (kind "fleet" == []) ''
    These flakes bind `ix.default` to a fleet result:

      ${lib.concatStringsSep "\n      " (files (kind "fleet"))}

    `index.lib.mkVm`, `mkDev` and `mkFleet` all return a fleet result, which has
    no `config`. `ix apply` with a bare local target prefers a flake's
    `ix.default` and builds `ix.default.config.system.build.toplevel` from it, so
    this binding fails the apply on a missing attribute instead of converging the
    flake's nodes.

    Drop the `ix.default = ...;` line and keep `inherit (vm) nixosConfigurations;`.
    A bare `ix apply` then converges every node the flake declares. If you want
    the single-VM path instead, bind the configuration rather than the fleet:
    `ix.default = vm.nixosConfigurations.<name>;`.
  '';
  assert lib.assertMsg (unevaluableMentioningIxDefault == []) ''
    These flakes mention `ix.default` and take a flake input this gate cannot
    supply, so it could not evaluate the binding to check it:

      ${lib.concatStringsSep "\n      " (files unevaluableMentioningIxDefault)}

    Supply the input from tests/ix-default-is-a-vm.nix (`suppliable`), or check
    by hand that the bound value has `config` and say so here.
  '';
    pkgs.runCommand "ix-default-is-a-vm-guard" {__structuredAttrs = true;} "touch $out"
