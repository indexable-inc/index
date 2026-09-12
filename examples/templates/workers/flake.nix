{
  description = "ix example: templates-workers";

  inputs = {
    # index instantiated with its guest Nix (`ix.index`). The bare
    # `github:indexable-inc/index` flake has no guest Nix by construction (the
    # jj tree ABI the fork links lives in ix) and refuses every image, so the
    # VM below comes from ix's instantiation of index. No nixpkgs input: the
    # image and cache.ix.dev were built against the nixpkgs ix locks, and a
    # second pin here would rebuild that world inside the VM.
    ix.url = "github:indexable-inc/ix/warmed-main";
  };

  outputs = {ix, ...}: let
    inherit (ix) index;
    # The nixpkgs the guests were built against (index's pin, which ix
    # follows), for the host-side eval check below.
    nixpkgs = ix.inputs.nixpkgs;
    # `default.ix` is JavaScript-syntax Nix. `builtins.wasm` converts it during
    # evaluation, so evaluating this flake takes index's patched nix with
    # `wasm-builtin` in `extra-experimental-features` (`ix apply` and `ix eval`
    # pass the flag).
    config = index.lib.importIxWasm ./default.ix {inherit index;};

    # The seam this example exists for. `templates` and `instances` are exports
    # that the config's own `mkVm` calls know nothing about; this renders each
    # instance through its template and merges the result with the named VMs,
    # so `nixosConfigurations` carries `web`, `worker-1` and `worker-2` and
    # `ix apply` cannot tell which of them came from where. A config exporting
    # neither key comes back through here unchanged.
    rendered = index.lib.templates.renderConfig config;

    # Every system these commands can be typed from. The guests are always
    # x86_64-linux; this is the set of machines that can evaluate them.
    hostSystems = [
      "aarch64-darwin"
      "aarch64-linux"
      "x86_64-darwin"
      "x86_64-linux"
    ];
    # Force every rendered node's toplevel and record the derivation it resolved
    # to, WITHOUT building any of them. `unsafeDiscardStringContext` is what
    # makes that possible: the string still has to be computed, so the whole
    # module system for every node is evaluated and any option type error,
    # missing attribute or port collision throws here -- but with the context
    # stripped the check no longer depends on those closures, so it costs seconds
    # instead of a real build. Same shape as
    # `examples/minecraft/hyperion/flake.nix`, whose comments explain it at
    # length.
    #
    # This exists because nothing else in the repo evaluates this example.
    # `exampleFleetsFor` (index's `lib/discovery.nix`) classifies on the fleet
    # shape, `nodes` + `planValue`, and a config exporting `templates` and
    # `instances` returns `nixosConfigurations` instead, so it is dropped -- and
    # dropped silently, unlike hyperion and switch-multi which print why
    # (index#4454). index's own `vm-templates` eval group covers the LIBRARY that
    # renders this example, which is not the same thing as covering the example.
    fleetEvalFor = system: let
      pkgs = nixpkgs.legacyPackages."${system}";
      lines =
        nixpkgs.lib.mapAttrsToList
        (name: cfg: "${name} ${builtins.unsafeDiscardStringContext cfg.config.system.build.toplevel.drvPath}")
        rendered.nixosConfigurations;
    in
      pkgs.runCommand "templates-workers-eval" {
        __structuredAttrs = true;
        drvPaths = builtins.concatStringsSep "\n" lines;
        # Guard the guard, BY NAME rather than by count. An empty
        # `nixosConfigurations` would make the line above vacuously true and this
        # check would pass having evaluated nothing -- a green tick meaning "found
        # no nodes", indistinguishable from "every node is fine". A bare count
        # would catch that and would also break the moment somebody adds a line
        # to the `instances` block, which is the edit this example exists to
        # demonstrate. So require the named VM and the first instance, and stay
        # silent about how many workers there are.
        #
        # Space-joined rather than a list: `__structuredAttrs` turns a Nix list
        # into a bash ARRAY, so `"$nodeNames"` would silently be only its first
        # element and the loop would test one name while looking like it tested
        # all of them.
        nodeNames = builtins.concatStringsSep " " (builtins.attrNames rendered.nixosConfigurations);
      } ''
        for required in web worker-1; do
          case " $nodeNames " in
            *" $required "*) ;;
            *)
              echo "templates-workers-eval: $required is not among the evaluated nodes ($nodeNames)" >&2
              exit 1
              ;;
          esac
        done
        printf '%s\n' "$drvPaths" > "$out"
      '';
  in {
    inherit (rendered) nixosConfigurations;

    # `nix flake check` in this directory is the gate, and it is the same command
    # CI would run, so neither can drift from what a contributor can type.
    checks = nixpkgs.lib.genAttrs hostSystems (system: {
      fleet-eval = fleetEvalFor system;
    });

    # `nix build .#worker-2-system` before `ix apply .#worker-2`, for an
    # instance and a named VM alike. Exposed under every system because these
    # are x86_64-linux guests whatever machine builds them: the machine that
    # types `nix build` contributes a builder, not an identity.
    packages = nixpkgs.lib.genAttrs hostSystems (_: rendered.systemPackages);
  };
}
