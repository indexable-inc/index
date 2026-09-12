# Two builds compile libfetchers out of this tree and only one of them calls
# this file. The fork's own flake does, through `packaging/components.nix`. The
# ix package scope (`index/packages/nix/default.nix`) does not: nixpkgs' modular
# nix packaging vendors its own copy of this file
# (`pkgs/tools/package-management/nix/modular/src/libfetchers/package.nix`) and
# `overrideSource` swaps `src` alone, so on that path this lambda is never
# called and the fileset below is ignored -- nixpkgs' own `overrideSource`
# doc string says as much.
#
# That is why the jj tree ABI is absent below. `libjj_tree.a` comes from the ix
# crate `jj-tree-abi`, which lives outside this tree, so neither caller can
# supply it from here: as a required formal it threw at eval on the fork's own
# path, and it was unreachable on the ix one. Giving it a default instead would
# make this file a second author for `-Djj-tree-prefix`, next to the scope that
# holds the only real value. So this tree owns the OPTION and its fail-closed
# default (`meson.options`; `meson.build` errors when the prefix is empty) and
# nothing more; the single author of the VALUE is that scope. A build from the
# fork's own flake therefore stops at meson configure with that error, which is
# the intended outcome: a nix without the archive has no jj fetcher.
{
  lib,
  mkMesonLibrary,

  nix-util,
  nix-store,
  nlohmann_json,
  libgit2,

  # Configuration Options

  version,
}:

let
  inherit (lib) fileset;
in

mkMesonLibrary (finalAttrs: {
  pname = "nix-fetchers";
  inherit version;

  workDir = ./.;
  fileset = fileset.unions [
    ../../nix-meson-build-support
    ./nix-meson-build-support
    ../../.version
    ./.version
    ./meson.build
    ./meson.options
    ./include/nix/fetchers/meson.build
    (fileset.fileFilter (file: file.hasExt "cc") ./.)
    (fileset.fileFilter (file: file.hasExt "hh") ./.)
  ];

  buildInputs = [
    libgit2
  ];

  propagatedBuildInputs = [
    nix-store
    nix-util
    nlohmann_json
  ];

  meta = {
    platforms = lib.platforms.unix ++ lib.platforms.windows;
  };

})
