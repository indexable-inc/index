#!/usr/bin/env bash

# A jj input is addressed by its tree id: the store path a lazy mount derives
# from the id alone (no file read) is the store path the forced copy lands on,
# and the store records the object under that id.

source common.sh

repo=$TEST_ROOT/jj
jjInit "$repo"
url=file://$repo

echo utrecht > "$repo"/hello
mkdir "$repo"/dir
echo world > "$repo"/dir/foo
ln -s hello "$repo"/link
printf '#!/bin/sh\necho hi\n' > "$repo"/run
chmod +x "$repo"/run

treeId=$(treeIdOf "$url")
[[ $treeId == blake3-* ]]

# Golden vector: the one oracle independent of the code under test. This
# exact fixture (hello "utrecht", dir/foo "world", symlink link -> hello,
# executable run with the two-line script above) has this tree id, measured
# from the deployed jj backend on x86_64-linux (gate nix3, dev-compute-5);
# the id is a pure function of the tree's bytes, kinds and names, so it is
# the same on every platform. jj's native ids are frozen (memory/
# jj-forge.md): a mismatch here means the tree encoding or the hash moved,
# which is a wire-format break, not a test to update casually.
[[ $treeId = blake3-A2hhtXGMq6IFH76H53la0yyN+h9QHZkhE9wPlK8hbDc= ]] \
    || fail "the frozen tree id moved: got $treeId"

# The mount: the path is derived, nothing is copied, so it is not valid yet.
outPath=$(fetchAttr "$url" "" outPath)
(! nix path-info "$outPath")

# Force the copy through a derivation that consumes the tree as a source.
# mkDerivation from the suite's config.nix: a raw 'derivation' has no PATH,
# so 'cat' is not found -- and worse, under a shell without '-e' the
# redirection still creates an EMPTY output and the builder exits 0.
cat > "$TEST_ROOT/force.nix" <<EOF
with import "\${builtins.getEnv "_NIX_TEST_BUILD_DIR"}/config.nix";
let tree = builtins.fetchTree { type = "jj"; url = "$url"; }; in
mkDerivation {
  name = "force";
  buildCommand = "cat \${tree}/hello \${tree}/dir/foo > \$out; test -x \${tree}/run; test -L \${tree}/link";
}
EOF
nix build --extra-experimental-features fetch-tree --impure -f "$TEST_ROOT/force.nix" --out-link "$TEST_ROOT/result"
[[ $(cat "$TEST_ROOT/result") == $'utrecht\nworld' ]]

# The forced object is the mounted path, registered under the tree id.
nix path-info "$outPath" >/dev/null
[[ $(caOf "$outPath") == "jj-tree $treeId" ]]
[[ -x $outPath/run ]]
[[ -L $outPath/link ]]
[[ ! -e $outPath/.jj ]]

# The store's own integrity record covers the bytes it wrote.
nix store verify --no-trust "$outPath"

# Nix never computes such an id: asking for one is refused, not approximated.
expectStderr 1 nix hash path --mode jj-tree "$repo" | grepQuiet "Jujutsu tree id"
expectStderr 1 nix store add --mode jj-tree "$repo"/dir | grepQuiet "Jujutsu tree id"

# Editing the tree moves the id, the mount and the object together.
echo amsterdam > "$repo"/hello
treeId2=$(treeIdOf "$url")
[[ $treeId2 != "$treeId" ]]
outPath2=$(fetchAttr "$url" "" outPath)
[[ $outPath2 != "$outPath" ]]
nix build --extra-experimental-features fetch-tree --impure -f "$TEST_ROOT/force.nix" --out-link "$TEST_ROOT/result"
[[ $(caOf "$outPath2") == "jj-tree $treeId2" ]]
# The old object is untouched: one id, one object.
[[ $(caOf "$outPath") == "jj-tree $treeId" ]]
