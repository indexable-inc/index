#!/usr/bin/env bash

# builtins.path on a directory served from a jj object store is answered by
# the id of the tree the filter leaves: nothing is read, the result is mounted
# lazily like a flake input, and an edit outside the kept files leaves the
# store path alone. The oracle is a second repository holding exactly the kept
# files: same content, same id.

source common.sh
source ../rust-eval-lib.sh

repo=$TEST_ROOT/jj
jjInit "$repo"
url=file://$repo

echo utrecht > "$repo"/hello
mkdir "$repo"/keep "$repo"/drop
echo world > "$repo"/keep/foo
echo aside > "$repo"/keep/skip
echo noise > "$repo"/drop/bar

# The mounted root as a PATH value (what a flake's `./.` is), not a string
# carrying context: with context, builtins.path inherits the store object's
# references and takes the NAR road, as it does for any store path string.
root='(/. + builtins.unsafeDiscardStringContext (builtins.fetchTree { type = "jj"; url = "'$url'"; }).outPath)'
filteredExpr="builtins.path { name = \"filtered\"; path = $root; filter = p: t: baseNameOf p != \"drop\"; }"
keptExpr="builtins.path { name = \"kept\"; path = $root + \"/keep\"; }"
# A filtered SUBDIRECTORY whose predicate checks the coordinates it is asked
# in: the full path under the mounted root, as `dumpPath` would ask, never a
# path relative to the subdirectory. A wrong coordinate rejects everything.
nestedExpr="builtins.path { name = \"nested\"; path = $root + \"/keep\"; filter = p: t: builtins.substring 0 (builtins.stringLength (toString $root + \"/keep/\")) p == toString $root + \"/keep/\" && baseNameOf p != \"skip\"; }"
# filterSource: the same road, named after the mounted root.
sourceExpr="builtins.filterSource (p: t: baseNameOf p != \"drop\") $root"
# A predicate that keeps every directory and no file: every kept directory
# is emptied, and a jj tree has no empty directories, so the result is the
# empty tree (the NAR road would have kept the skeleton).
emptiedExpr="builtins.path { name = \"emptied\"; path = $root; filter = p: t: t == \"directory\"; }"

# As fetchAttr: the context is discarded so that printing the path is not,
# by itself, a copy (`ensureLazyPathsCopied` runs on nix eval's result).
evalPath() {
    nix eval --extra-experimental-features fetch-tree --impure --raw --expr \
        "builtins.unsafeDiscardStringContext (toString ($1))"
}

filtered=$(evalPath "$filteredExpr")
kept=$(evalPath "$keptExpr")
nested=$(evalPath "$nestedExpr")
filteredSource=$(evalPath "$sourceExpr")
emptied=$(evalPath "$emptiedExpr")
[[ $filtered == "$NIX_STORE_DIR"/*-filtered ]]
[[ $kept == "$NIX_STORE_DIR"/*-kept ]]
[[ $nested == "$NIX_STORE_DIR"/*-nested ]]
[[ $emptied == "$NIX_STORE_DIR"/*-emptied ]]

# The Rust evaluator's road into `EvalState::addPathToStore` (its host
# question carries the accepted set instead of the predicate) lands on the
# same store paths.
for pair in "$filtered=$filteredExpr" "$kept=$keptExpr" "$nested=$nestedExpr" "$filteredSource=$sourceExpr" "$emptied=$emptiedExpr"; do
    [[ $(NIX_CONFIG=$rustArm evalPath "${pair#*=}") == "${pair%%=*}" ]]
done

# All are mounts: derived, nothing copied, not valid yet.
(! nix path-info "$filtered")
(! nix path-info "$kept")
(! nix path-info "$nested")
(! nix path-info "$filteredSource")
(! nix path-info "$emptied")

# Force both through a derivation that reads them (mkDerivation from the
# suite's config.nix, for the reason identity.sh gives).
cat > "$TEST_ROOT/force.nix" <<EOF
with import "\${builtins.getEnv "_NIX_TEST_BUILD_DIR"}/config.nix";
let
  filtered = $filteredExpr;
  kept = $keptExpr;
  nested = $nestedExpr;
  filteredSource = $sourceExpr;
  emptied = $emptiedExpr;
in mkDerivation {
  name = "force";
  buildCommand = "cat \${filtered}/hello \${filtered}/keep/foo \${kept}/foo \${nested}/foo > \$out; test ! -e \${filtered}/drop; test ! -e \${nested}/skip; test ! -e \${filteredSource}/drop; test -e \${filteredSource}/keep/skip; test -z \$(ls -A \${emptied})";
}
EOF
nix build --extra-experimental-features fetch-tree --impure -f "$TEST_ROOT/force.nix" --out-link "$TEST_ROOT/result"
[[ $(cat "$TEST_ROOT/result") == $'utrecht\nworld\nworld\nworld' ]]
[[ -e $filtered/keep/foo ]]
[[ -e $filtered/keep/skip ]]
[[ ! -e $filtered/drop ]]
[[ -e $kept/foo ]]
[[ -e $kept/skip ]]
[[ -e $nested/foo ]]
[[ ! -e $nested/skip ]]
[[ -e $filteredSource/keep/skip && ! -e $filteredSource/drop ]]
[[ -d $emptied && -z $(ls -A "$emptied") ]]

# The oracle: the kept files as repositories of their own carry the ids the
# filtered objects were registered under. The id is a function of the kept
# content alone, not of the tree it was cut from.
oracle=$TEST_ROOT/oracle
jjInit "$oracle"
echo utrecht > "$oracle"/hello
mkdir "$oracle"/keep
echo world > "$oracle"/keep/foo
echo aside > "$oracle"/keep/skip
[[ $(caOf "$filtered") == "jj-tree $(treeIdOf "file://$oracle")" ]]
# filterSource: the same tree under the mounted root's name.
[[ $filteredSource != "$filtered" ]]
[[ $(caOf "$filteredSource") == $(caOf "$filtered") ]]
keepOracle=$TEST_ROOT/keep-oracle
jjInit "$keepOracle"
echo world > "$keepOracle"/foo
echo aside > "$keepOracle"/skip
[[ $(caOf "$kept") == "jj-tree $(treeIdOf "file://$keepOracle")" ]]
nestedOracle=$TEST_ROOT/nested-oracle
jjInit "$nestedOracle"
echo world > "$nestedOracle"/foo
[[ $(caOf "$nested") == "jj-tree $(treeIdOf "file://$nestedOracle")" ]]
# The empty tree: what an empty repository's working copy is.
emptyOracle=$TEST_ROOT/empty-oracle
jjInit "$emptyOracle"
[[ $(caOf "$emptied") == "jj-tree $(treeIdOf "file://$emptyOracle")" ]]
nix store verify --no-trust "$filtered" "$filteredSource" "$kept" "$nested" "$emptied"

# An edit outside the kept files moves the input's id and nothing else: both
# copies keep their store paths, and stay valid.
echo louder > "$repo"/drop/bar
[[ $(treeIdOf "$url") != $(caOf "$filtered" | cut -d' ' -f2) ]]
[[ $(evalPath "$filteredExpr") == "$filtered" ]]
[[ $(evalPath "$keptExpr") == "$kept" ]]
nix path-info "$filtered" "$kept" >/dev/null

# An edit inside them moves both.
echo amsterdam > "$repo"/keep/foo
[[ $(evalPath "$filteredExpr") != "$filtered" ]]
[[ $(evalPath "$keptExpr") != "$kept" ]]
echo world > "$repo"/keep/foo
[[ $(evalPath "$filteredExpr") == "$filtered" ]]

# A pinned sha256 names a NAR hash, which only the NAR road can check: that
# road is kept, and lands on the NAR-addressed path, a different one.
narHash=$(nix path-info --json --json-format 2 "$filtered" | jq -r '.info[].narHash')
pinned=$(evalPath "builtins.path { name = \"filtered\"; path = $root; filter = p: t: baseNameOf p != \"drop\"; sha256 = \"$narHash\"; }")
[[ $pinned != "$filtered" ]]
[[ $(caOf "$pinned") == "nar $narHash" ]]
