#!/usr/bin/env bash

# A relative `path:./sub` input inside a jj-backed flake is the subtree
# object: it evaluates at the store path its content has anywhere (the same
# id the same directory has committed at the root of another repository), is
# locked by its parent with no hash of its own, and follows the parent, so a
# stale child lock cannot exist.

source common.sh

root=$TEST_ROOT/root
jjInit "$root"
mkdir -p "$root/sub/nested"
# Every path this flake reports is a NAME, with the string context stripped:
# an outPath carries its store path as context and `nix eval` forces every
# such path into the store before it exits, so reporting a path would be
# materializing it. The materializations below are explicit (the `force*`
# derivations), and the assertions between them are about paths that are
# NOT yet in the store. `rootPath` is an output because `nix eval` resolves
# `<flake>#<attr>` against the outputs, and `outPath` is not one of them.
cat > "$root/flake.nix" <<EOF
{
  inputs.sub.url = "path:./sub";
  outputs = { self, sub }: {
    x = 2;
    y = self.x * sub.x;
    subPath = builtins.unsafeDiscardStringContext sub.outPath;
    # The context-carrying twin: force.nix interpolates it, and the context
    # is what makes the subtree an input of the derivation, i.e. the force.
    subPathForced = sub.outPath;
    rootPath = builtins.unsafeDiscardStringContext self.outPath;
    nestedPath = builtins.unsafeDiscardStringContext sub.nestedPath;
  };
}
EOF
cat > "$root/sub/flake.nix" <<EOF
{ outputs = { self }: { x = 7; }; }
EOF
echo alpha > "$root/sub/data"
echo beta > "$root/sub/nested/deep"

[[ $(nix eval "$root#y") == 14 ]]
lock=$root/flake.lock

# Locked by the parent: the literal path plus `parent`, no hash of any kind.
[[ $(jq -c .nodes.sub.locked "$lock") == '{"path":"./sub","type":"path"}' ]]
[[ $(jq -c .nodes.sub.parent "$lock") == '[]' ]]
(! grep -E 'narHash|treeHash' "$lock")

# The child's outPath is its own store object, named by the subtree id...
subPath=$(nix eval --raw "$root#subPath")
[[ $subPath == "$NIX_STORE_DIR"/*-source ]]
rootPath=$(nix eval --raw "$root#rootPath")
[[ $subPath != "$rootPath"/* ]]

# ...and that id is the one the same content has as a repository of its own:
# two fetch roads (subtree of one repo, root of another), one store path.
own=$TEST_ROOT/own
jjInit "$own"
cp -R "$root/sub/." "$own/"
ownId=$(treeIdOf "file://$own")
[[ $(fetchAttr "file://$own" "" outPath) == "$subPath" ]]

# Forcing the child materializes the subtree under its id, not the parent.
# mkDerivation: see identity.sh (a raw derivation has no PATH for `cat`).
# `subPathForced`, not `subPath`: the probe output is context-stripped so
# that reading the NAME cannot force; the force needs the context.
cat > "$TEST_ROOT/force.nix" <<EOF
with import "\${builtins.getEnv "_NIX_TEST_BUILD_DIR"}/config.nix";
mkDerivation {
  name = "force";
  buildCommand = "cat \${(builtins.getFlake "jj+file://$root").subPathForced}/data > \$out";
}
EOF
nix build --impure -f "$TEST_ROOT/force.nix" --out-link "$TEST_ROOT/result"
[[ $(cat "$TEST_ROOT/result") == alpha ]]
[[ $(caOf "$subPath") == "jj-tree $ownId" ]]
(! nix path-info "$rootPath")

# The child's identity does not depend on the store's state. Once the parent
# is materialized, its store path is a real directory with `sub/` inside it,
# and the impure root filesystem (real filesystem over the store mounts)
# answers for it from disk; a subtree looked up through that union would
# have quietly become parent/sub. The subtree is looked up through the
# evaluator's own mount table instead, so the child keeps its own object.
cat > "$TEST_ROOT/force-parent.nix" <<EOF
with import "\${builtins.getEnv "_NIX_TEST_BUILD_DIR"}/config.nix";
mkDerivation {
  name = "force-parent";
  buildCommand = "cat \${(builtins.getFlake "jj+file://$root").outPath}/flake.nix > \$out";
}
EOF
nix build --impure -f "$TEST_ROOT/force-parent.nix" --out-link "$TEST_ROOT/result-parent"
# The build read real bytes, not an empty file a lenient shell left behind.
[[ -s "$TEST_ROOT/result-parent" ]]
grepQuiet -F "outputs" "$TEST_ROOT/result-parent"
# The precondition of the trap holds: the parent is on disk, with the child's
# directory inside it.
nix path-info "$rootPath" >/dev/null
[[ -e $rootPath/sub/data ]]
[[ $(nix eval --raw "$root#subPath") == "$subPath" ]]
[[ $(nix eval --raw "$root#rootPath") == "$rootPath" ]]

# The parent moves, the child follows, and the lock does not change: it never
# held anything that could go stale.
cp "$lock" "$TEST_ROOT/lock.before"
echo gamma > "$root/sub/data"
[[ $(nix eval --raw "$root#subPath") != "$subPath" ]]
nix flake lock "$root"
cmp "$lock" "$TEST_ROOT/lock.before"

# An edit outside the subtree leaves the child's identity alone.
subPath2=$(nix eval --raw "$root#subPath")
echo unrelated > "$root/other"
[[ $(nix eval --raw "$root#subPath") == "$subPath2" ]]
[[ $(nix eval --raw "$root#rootPath") != "$rootPath" ]]

# Nested relative inputs compose through subtree objects too.
cat > "$root/sub/flake.nix" <<EOF
{
  inputs.nested.url = "path:./nested";
  outputs = { self, nested }: { x = 7; nestedPath = nested.outPath; };
}
EOF
cat > "$root/sub/nested/flake.nix" <<EOF
{ outputs = { self }: { z = 3; }; }
EOF
nestedPath=$(nix eval --raw "$root#nestedPath")
[[ $nestedPath == "$NIX_STORE_DIR"/*-source ]]
[[ $(jq -c .nodes.nested.locked "$lock") == '{"path":"./nested","type":"path"}' ]]
[[ $(jq -c .nodes.nested.parent "$lock") == '["sub"]' ]]
own2=$TEST_ROOT/own2
jjInit "$own2"
cp -R "$root/sub/nested/." "$own2/"
[[ $(fetchAttr "file://$own2" "" outPath) == "$nestedPath" ]]

# A relative path that names no directory is an error naming the path.
cat > "$root/flake.nix" <<EOF
{
  inputs.sub.url = "path:./sub";
  inputs.missing.url = "path:./absent";
  outputs = { self, sub, missing }: { y = 1; };
}
EOF
expectStderr 1 nix eval "$root#y" | grepQuiet absent

# A relative path is a flake-input notion: fetched on its own it has no parent.
expectStderr 1 nix eval --impure --expr 'builtins.fetchTree { type = "path"; path = "./sub"; }' \
    | grepQuiet "resolves only as a flake input"

# A NAR promise on a relative input cannot be checked: the subtree is
# addressed by its tree id and never NAR-ingested. The promise lives in
# flake.nix, where no lock update can remove it, so the refusal names the
# attribute and the file rather than `nix flake update`. This is the one
# reachable case of that refusal (the jj scheme rejects `narHash` at parse
# time; git is NAR-addressed), so it is pinned here.
cat > "$root/flake.nix" <<EOF
{
  inputs.sub.url = "path:./sub?narHash=sha256-FePFYIlMuycIXPZbWi7LGEiMmZSX9FMbaQenWBzm1Sc=";
  outputs = { self, sub }: { y = 1; };
}
EOF
expectStderr 1 nix eval "$root#y" | grepQuiet "Remove 'narHash' from the input in flake.nix"
cat > "$root/flake.nix" <<EOF
{
  inputs.sub.url = "path:./sub";
  outputs = { self, sub }: { y = 1; subPath = builtins.unsafeDiscardStringContext sub.outPath; };
}
EOF

# A NON-flake relative input has the same identity on every evaluation: the
# subtree object, whether or not a lock file existed when the evaluation
# started. Before the fix the first evaluation (no lock: fresh branch)
# mounted the subtree and the second (lock present: kept branch) recorded no
# tree, so call-flake.nix invented `<parent>/data-dir`; the two outPaths
# differed with rc=0 both times, and the assertion below fails on that
# version by construction.
mkdir -p "$root/data-dir"
echo payload > "$root/data-dir/file"
cat > "$root/flake.nix" <<EOF
{
  inputs.sub.url = "path:./sub";
  inputs.data = { url = "path:./data-dir"; flake = false; };
  outputs = { self, sub, data }: {
    y = 1;
    subPath = builtins.unsafeDiscardStringContext sub.outPath;
    dataPath = builtins.unsafeDiscardStringContext data.outPath;
    rootPath = builtins.unsafeDiscardStringContext self.outPath;
  };
}
EOF
rm -f "$lock"
dataPath1=$(nix eval --raw "$root#dataPath")
[[ -e $lock ]] || fail "test setup: the first evaluation wrote no lock file, so the second cannot take the kept branch"
[[ $(jq -c .nodes.data.locked "$lock") == '{"path":"./data-dir","type":"path"}' ]]
dataPath2=$(nix eval --raw "$root#dataPath")
[[ $dataPath2 == "$dataPath1" ]] || fail "non-flake relative input moved from $dataPath1 to $dataPath2 once the lock existed"
dataPath3=$(nix eval --raw --no-eval-cache "$root#dataPath")
[[ $dataPath3 == "$dataPath1" ]]
# It is the subtree object, not a subpath of the parent...
[[ $dataPath1 == "$NIX_STORE_DIR"/*-source ]]
[[ $dataPath1 != "$(nix eval --raw "$root#rootPath")"/* ]]
# ...at the id the same directory has as a repository of its own.
own3=$TEST_ROOT/own3
jjInit "$own3"
cp -R "$root/data-dir/." "$own3/"
[[ $(fetchAttr "file://$own3" "" outPath) == "$dataPath1" ]]
# The same holds for a relative input of a kept, non-relative flake input:
# resolving it needs that flake's tree, not its parent's, so the kept flake
# is refetched (pinned, cached) rather than handled from the parent's lock.
consumer=$TEST_ROOT/consumer
jjInit "$consumer"
cat > "$consumer/flake.nix" <<EOF
{
  inputs.dep.url = "jj+file://$root";
  outputs = { self, dep }: { dataPath = builtins.unsafeDiscardStringContext dep.inputs.data.outPath; };
}
EOF
[[ $(nix eval --raw "$consumer#dataPath") == "$dataPath1" ]]
[[ $(nix eval --raw --no-eval-cache "$consumer#dataPath") == "$dataPath1" ]]

# A relative child's identity does not depend on the store's state, on the
# road a LOCKED parent takes either. `dep` is locked in `consumer` by tree
# id and has a relative flake child; forcing dep's root registers dep's
# store object. Before the fix, the next evaluation served dep from that
# object (a flattened copy with no subtree objects) and composed the child
# as `<dep>/sub`; the assertion below fails on that version. Now the
# repository wins whenever it exists, so the child keeps its own object.
cat > "$consumer/flake.nix" <<EOF
{
  inputs.dep.url = "jj+file://$root";
  outputs = { self, dep }: {
    subPath = builtins.unsafeDiscardStringContext dep.inputs.sub.outPath;
    depPath = builtins.unsafeDiscardStringContext dep.outPath;
    depPathForced = dep.outPath;
  };
}
EOF
lockedSubPath=$(nix eval --raw "$consumer#subPath")
[[ $lockedSubPath == "$(nix eval --raw "$root#subPath")" ]]
depPath=$(nix eval --raw "$consumer#depPath")
cat > "$TEST_ROOT/force-dep.nix" <<EOF
with import "\${builtins.getEnv "_NIX_TEST_BUILD_DIR"}/config.nix";
mkDerivation {
  name = "force-dep";
  buildCommand = "cat \${(builtins.getFlake "jj+file://$consumer").depPathForced}/flake.nix > \$out";
}
EOF
nix build --impure -f "$TEST_ROOT/force-dep.nix" --out-link "$TEST_ROOT/result-dep"
# The build read real bytes, not an empty file a lenient shell left behind.
[[ -s "$TEST_ROOT/result-dep" ]]
grepQuiet -F "outputs" "$TEST_ROOT/result-dep"
# The precondition of the trap holds: dep's object is registered under its id.
[[ $(caOf "$depPath") == "jj-tree $(treeIdOf "file://$root")" ]]
[[ -e $depPath/sub/data ]]
[[ $(nix eval --raw --no-eval-cache "$consumer#subPath") == "$lockedSubPath" ]] \
    || fail "the relative child of a locked jj input changed identity once the parent's store object existed"
[[ $(nix eval --raw --no-eval-cache "$consumer#depPath") == "$depPath" ]]

# Without the repository, the store object serves dep's own tree (lock.sh
# pins that road), but it cannot name the subtree the child is, and the
# child is refused naming the repository rather than composed as a subpath.
mv "$root" "$root.away"
expectStderr 1 nix eval --no-eval-cache "$consumer#depPath" | grepQuiet "The repository the flake was locked from is required"
expectStderr 1 nix eval --no-eval-cache "$consumer#depPath" | grepQuiet "jj+file://$root"
mv "$root.away" "$root"
[[ $(nix eval --raw --no-eval-cache "$consumer#subPath") == "$lockedSubPath" ]]
