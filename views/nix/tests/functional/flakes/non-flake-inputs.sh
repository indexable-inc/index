#!/usr/bin/env bash

source ./common.sh

TODO_NixOS

createFlake1
createFlake2

nonFlakeDir=$TEST_ROOT/nonFlake
createGitRepo "$nonFlakeDir" ""

cat > "$nonFlakeDir/README.md" <<EOF
FNORD
EOF

git -C "$nonFlakeDir" add README.md
git -C "$nonFlakeDir" commit -m 'Initial'

flake3Dir=$TEST_ROOT/flake3
createGitRepo "$flake3Dir" ""

# A non-flake input needs an identity just like a flake one, so none of these
# can name a bare path on disk. README.md is reachable two ways, and both are
# exercised: `file://` fetches the file itself, so the input's source is that
# file; `git+file://...?dir=` fetches the repository, so the input's source is
# the tree and the input points inside it. `file+file://` is the same fetch as
# `file://` written with its transport spelled out, which is what makes the
# two comparable inside the build below.
cat > "$flake3Dir/flake.nix" <<EOF
{
  inputs = {
    flake1 = {};
    flake2 = {};
    nonFlake = {
      url = "git+file://$nonFlakeDir";
      flake = false;
    };
    nonFlakeFile = {
      url = "file://$nonFlakeDir/README.md";
      flake = false;
    };
    nonFlakeFile2 = {
      url = "file+file://$nonFlakeDir/README.md";
      flake = false;
    };
    nonFlakeFile3 = {
      url = "git+file://$nonFlakeDir?dir=README.md";
      flake = false;
    };
    relativeNonFlakeFile = {
      url = ./config.nix;
      flake = false;
    };
  };

  description = "Fnord";

  outputs = inputs: rec {
    inherit inputs;
    packages.$system.xyzzy = inputs.flake2.packages.$system.bar;
    packages.$system.sth = inputs.flake1.packages.$system.foo;
    packages.$system.fnord =
      with import ./config.nix;
      mkDerivation {
        inherit system;
        name = "fnord";
        dummy = builtins.readFile (builtins.path { name = "source"; path = ./.; filter = path: type: baseNameOf path == "config.nix"; } + "/config.nix");
        dummy2 = builtins.readFile (builtins.path { name = "source"; path = inputs.flake1; filter = path: type: baseNameOf path == "simple.nix"; } + "/simple.nix");
        buildCommand = ''
          cat \${inputs.nonFlake}/README.md > \$out
          [[ \$(cat \${inputs.nonFlake}/README.md) = \$(cat \${inputs.nonFlakeFile}) ]]
          [[ \${inputs.nonFlakeFile} = \${inputs.nonFlakeFile2} ]]
        '';
      };
  };
}
EOF

cp "${config_nix}" "$flake3Dir"

git -C "$flake3Dir" add flake.nix config.nix
git -C "$flake3Dir" commit -m 'Add nonFlakeInputs'

# Check whether `nix build` works with a lockfile which is missing a
# nonFlakeInputs.
nix build -o "$TEST_ROOT/result" "$flake3Dir#sth" --commit-lock-file

nix registry add --registry "$registry" flake3 "git+file://$flake3Dir"

_NIX_TEST_BARF_ON_UNCACHEABLE='' nix build -o "$TEST_ROOT/result" flake3#fnord
[[ $(cat "$TEST_ROOT/result") = FNORD ]]

# Check whether flake input fetching is lazy: flake3#sth does not
# depend on flake2, so this shouldn't fail.
rm -rf "$TEST_HOME/.cache"
clearStore
mv "$flake2Dir" "$flake2Dir.tmp"
mv "$nonFlakeDir" "$nonFlakeDir.tmp"
nix build -o "$TEST_ROOT/result" flake3#sth
(! _NIX_TEST_BARF_ON_UNCACHEABLE='' nix build -o "$TEST_ROOT/result" flake3#xyzzy)
(! _NIX_TEST_BARF_ON_UNCACHEABLE='' nix build -o "$TEST_ROOT/result" flake3#fnord)
mv "$flake2Dir.tmp" "$flake2Dir"
mv "$nonFlakeDir.tmp" "$nonFlakeDir"
_NIX_TEST_BARF_ON_UNCACHEABLE='' nix build -o "$TEST_ROOT/result" flake3#xyzzy flake3#fnord

# Check non-flake inputs have a sourceInfo and an outPath
#
# This may look redundant, but the other checks below happen in a command
# substitution subshell, so failures there will not exit this shell
export _NIX_TEST_BARF_ON_UNCACHEABLE='' # FIXME
nix eval --raw flake3#inputs.nonFlake.outPath
nix eval --raw flake3#inputs.nonFlake.sourceInfo.outPath
nix eval --raw flake3#inputs.nonFlakeFile.outPath
nix eval --raw flake3#inputs.nonFlakeFile.sourceInfo.outPath
nix eval --raw flake3#inputs.nonFlakeFile2.outPath
nix eval --raw flake3#inputs.nonFlakeFile2.sourceInfo.outPath
nix eval --raw flake3#inputs.nonFlakeFile3.outPath
nix eval --raw flake3#inputs.nonFlakeFile3.sourceInfo.outPath
nix eval --raw flake3#inputs.relativeNonFlakeFile.outPath
nix eval --raw flake3#inputs.relativeNonFlakeFile.sourceInfo.outPath

# Check non-flake file inputs have the expected outPaths
[[
  $(nix eval --raw flake3#inputs.nonFlake.outPath) \
  = $(nix eval --raw flake3#inputs.nonFlake.sourceInfo.outPath)
]]
[[
  $(nix eval --raw flake3#inputs.nonFlakeFile.outPath) \
  = $(nix eval --raw flake3#inputs.nonFlakeFile.sourceInfo.outPath)
]]
[[
  $(nix eval --raw flake3#inputs.nonFlakeFile2.outPath) \
  = $(nix eval --raw flake3#inputs.nonFlakeFile2.sourceInfo.outPath)
]]
[[
  $(nix eval --raw flake3#inputs.nonFlakeFile3.outPath) \
  = $(nix eval --raw flake3#inputs.nonFlakeFile3.sourceInfo.outPath)/README.md
]]
[[
  $(nix eval --raw flake3#inputs.relativeNonFlakeFile.outPath) \
  = $(nix eval --raw flake3#inputs.relativeNonFlakeFile.sourceInfo.outPath)/config.nix
]]

# Make branch "removeXyzzy" where flake3 doesn't have xyzzy anymore
git -C "$flake3Dir" checkout -b removeXyzzy
rm "$flake3Dir/flake.nix"

cat > "$flake3Dir/flake.nix" <<EOF
{
  inputs = {
    nonFlake = {
      url = "git+file://$nonFlakeDir";
      flake = false;
    };
  };

  description = "Fnord";

  outputs = { self, flake1, flake2, nonFlake }: rec {
    packages.$system.sth = flake1.packages.$system.foo;
    packages.$system.fnord =
      with import ./config.nix;
      mkDerivation {
        inherit system;
        name = "fnord";
        buildCommand = ''
          cat \${nonFlake}/README.md > \$out
        '';
      };
  };
}
EOF
git -C "$flake3Dir" commit -a -m 'Remove packages.xyzzy'
# A git source takes its lock file only as a commit (flakes/lock-file-writes.sh).
nix flake lock "$flake3Dir" --commit-lock-file
[[ -z "$(git -C "$flake3Dir" status --porcelain)" ]]
git -C "$flake3Dir" checkout master

# Test whether fuzzy-matching works for registry entries.
nix registry add --registry "$registry" flake4 flake3
(! nix build -o "$TEST_ROOT/result" flake4/removeXyzzy#xyzzy)
nix build -o "$TEST_ROOT/result" flake4/removeXyzzy#sth
