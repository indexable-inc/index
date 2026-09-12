# Enumerate optional workspace helpers only for declared producers. Keep the
# flake/cross alias rules identical to packageSet; select the final package
# value so existing alias precedence remains unchanged.
{
  lib,
  system,
  isLinux,
  packageRegistry,
  packages,
}: let
  nativeDeclarations = lib.genAttrs' (packageRegistry.flakeEntriesFor system) (
    entry: lib.nameValuePair entry.flake.attrName entry.workspaceIfdRoots
  );
  crossDeclarations = lib.listToAttrs (
    lib.concatMap (
      entry:
        map (
          target: lib.nameValuePair "${entry.cross.attrName}-${target}" entry.workspaceIfdRoots
        )
        entry.cross.targets
    )
    (packageRegistry.crossEntriesFor system)
  );
  providers = lib.filterAttrs (_: declared: declared) (nativeDeclarations // crossDeclarations);
in
  lib.optionalAttrs isLinux (
    lib.concatMapAttrs (
      name: _: let
        roots = packages.${name}.passthru.workspaceIfdRoots
          or (throw "workspace IFD provider '${name}' does not expose passthru.workspaceIfdRoots");
      in
        assert lib.assertMsg (builtins.isAttrs roots && roots != {})
        "workspace IFD provider '${name}' must expose a nonempty root set";
          lib.mapAttrs' (
            rootName: drv: lib.nameValuePair "workspace-ifd-${name}-${rootName}" drv
          )
          roots
    )
    providers
  )
