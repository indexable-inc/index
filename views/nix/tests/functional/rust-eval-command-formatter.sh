#!/usr/bin/env bash

source common.sh
source rust-eval-lib.sh

clearStoreIfPossible
rm -rf "$TEST_HOME"/.cache "$TEST_HOME"/.config "$TEST_HOME"/.local

# The flake gets a directory of its own rather than being $TEST_HOME itself:
# `path:` serves store objects only (src/libfetchers/path.cc), so the source
# needs an identity, and a jj workspace snapshots everything under it on every
# fetch -- which $TEST_HOME, where nix writes its caches and profiles while
# this test runs, is a poor thing to be.
flakeDir=$TEST_HOME/flake
jjFlakeDir "$flakeDir"

cp ./simple.nix ./simple.builder.sh ./formatter.simple.sh "${config_nix}" "$flakeDir"

cd "$flakeDir"

nix formatter --help | grep "build or run the formatter"
nix fmt --help | grep "reformat your code"
nix fmt run --help | grep "reformat your code"
nix fmt build --help | grep "build"

# shellcheck disable=SC2154
cat << EOF > flake.nix
{
  outputs = _: {
    formatter.$system =
      with import ./config.nix;
      mkDerivation {
        name = "formatter";
        buildCommand = ''
          mkdir -p \$out/bin
          echo "#! ${bash}" > \$out/bin/formatter
          cat \${./formatter.simple.sh} >> \$out/bin/formatter
          chmod +x \$out/bin/formatter
        '';
      };
  };
}
EOF

# A flake in a subdirectory of the workspace, reached as `dir=subflake`. It
# does NOT get a jjFlakeDir of its own: it is part of this workspace's tree,
# and a jj workspace nested inside another is a bug rather than a fixture.
mkdir subflake
cp ./simple.nix ./simple.builder.sh ./formatter.simple.sh "${config_nix}" "$flakeDir/subflake"

# The extra line is a marker. Both flakes define the same formatter, and until
# the source was version controlled PRJ_ROOT differed between them and so said
# on its own which one had run. It no longer does (see below), so the formatter
# says who it is.
cat << EOF > subflake/flake.nix
{
  outputs = _: {
    formatter.$system =
      with import ./config.nix;
      mkDerivation {
        name = "formatter";
        buildCommand = ''
          mkdir -p \$out/bin
          echo "#! ${bash}" > \$out/bin/formatter
          echo 'echo SUBFLAKE' >> \$out/bin/formatter
          cat \${./formatter.simple.sh} >> \$out/bin/formatter
          chmod +x \$out/bin/formatter
        '';
      };
  };
}
EOF

# No arguments check
[[ "$(nix fmt)" = "PRJ_ROOT=$flakeDir Formatting(0):" ]]
formatterConfig=$(rustCachedArm formatter)
for attempt in cold warm; do
    NIX_CONFIG="$formatterConfig" NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$TEST_ROOT/formatter-$attempt.json" \
        nix formatter run > "$TEST_ROOT/formatter-$attempt.out"
    [[ "$(cat "$TEST_ROOT/formatter-$attempt.out")" = "PRJ_ROOT=$flakeDir Formatting(0):" ]]
    assertRustServed "$TEST_ROOT/formatter-$attempt.json"
done
# Locking and application selection can reuse a flake-document question even
# in one cold invocation. Derivation writes distinguish evaluating the formatter
# from replaying its cached application without treating nested hits as failures.
assertDrvWrites "$TEST_ROOT/formatter-cold.json"
assertMemoServed "$TEST_ROOT/formatter-warm.json"
assertNoDrvWrites "$TEST_ROOT/formatter-warm.json"

# Argument forwarding check
nix fmt ./file ./folder | grep "PRJ_ROOT=$flakeDir Formatting(2): ./file ./folder"
nix formatter run ./file ./folder | grep "PRJ_ROOT=$flakeDir Formatting(2): ./file ./folder"

# test subflake
cd subflake
# PRJ_ROOT is the flake input's source directory (src/nix/rust-formatter.cc), and
# for a flake inside a version controlled tree that is the tree's root: `dir=`
# selects which outputs to read, the input still names the workspace. So the
# subflake's own formatter is what the SUBFLAKE marker proves ran, which is
# the half of this assertion PRJ_ROOT used to carry by itself.
nix fmt ./file > "$TEST_ROOT/subflake-fmt.out"
grep "SUBFLAKE" "$TEST_ROOT/subflake-fmt.out"
grep "PRJ_ROOT=$flakeDir Formatting(1): ./file" "$TEST_ROOT/subflake-fmt.out"

# Build checks
## Defaults to a ./result.
formatterProgram=$(nix formatter build)
[[ "$formatterProgram" = */bin/formatter ]]
[[ -x "$formatterProgram" ]]
[[ -L ./result ]]
[[ "$(readlink ./result)/bin/formatter" = "$formatterProgram" ]]
PRJ_ROOT="$flakeDir" "$formatterProgram" ./file > "$TEST_ROOT/built-formatter.out"
grep -Fx "SUBFLAKE" "$TEST_ROOT/built-formatter.out"
grep -Fx "PRJ_ROOT=$flakeDir Formatting(1): ./file" "$TEST_ROOT/built-formatter.out"
rm result

## Can prevent the symlink.
nix formatter build --no-link
[[ ! -e ./result ]]

## Can change the symlink name.
nix formatter build --out-link my-result | grep ".\+/bin/formatter"
[[ -L ./my-result ]]
rm ./my-result

# The formatter output is independently visible to the Rust flake renderer.
nix flake show | grep -P "package 'formatter'"
