#!/usr/bin/env bash

# Lock identity of a jj input: `treeHash` (SRI blake3), never `narHash`.
# Round trip is byte-stable, a NAR promise is refused, and a tampered id is a
# mismatch, all without reading the tree.

source common.sh

dep=$TEST_ROOT/dep
jjInit "$dep"
cat > "$dep/flake.nix" <<EOF
{ outputs = { self }: { x = 7; }; }
EOF
echo payload > "$dep/data"

root=$TEST_ROOT/root
jjInit "$root"
# The sourceInfo probes are outputs: `nix eval <flake>#<attr>` resolves the
# attribute against the flake's OUTPUTS (packages.<system>.<attr>, then
# legacyPackages.<system>.<attr>, then <attr>), and `inputs` and `sourceInfo`
# are attributes of the flake value, not of its outputs.
cat > "$root/flake.nix" <<EOF
{
  inputs.dep.url = "jj+file://$dep";
  outputs = { self, dep }: {
    y = dep.x * 2;
    depHasTreeHash = dep.sourceInfo ? treeHash;
    depHasNarHash = dep.sourceInfo ? narHash;
    depTreeHash = dep.sourceInfo.treeHash;
  };
}
EOF

nix flake lock "$root"
lock=$root/flake.lock

depTreeId=$(treeIdOf "file://$dep")
[[ $depTreeId == blake3-* ]]
[[ $(jq -r .nodes.dep.locked.treeHash "$lock") == "$depTreeId" ]]
[[ $(jq -r .nodes.dep.locked.type "$lock") == jj ]]
[[ $(jq -r '.nodes.dep.locked | has("narHash")' "$lock") == false ]]
(! grep narHash "$lock")
[[ $(nix eval "$root#y") == 14 ]]

# Locking again from an unchanged tree reproduces the file byte for byte.
cp "$lock" "$TEST_ROOT/lock.before"
nix flake lock "$root"
cmp "$lock" "$TEST_ROOT/lock.before"

# sourceInfo carries the id, not a NAR hash.
[[ $(nix eval --json "$root#depHasTreeHash") == true ]]
[[ $(nix eval --json "$root#depHasNarHash") == false ]]
[[ $(nix eval --raw "$root#depTreeHash") == "$depTreeId" ]]

# A jj input cannot carry a NAR promise: the scheme rejects the attribute.
jq '.nodes.dep.locked.narHash = "sha256-FePFYIlMuycIXPZbWi7LGEiMmZSX9FMbaQenWBzm1Sc="' "$TEST_ROOT/lock.before" > "$lock"
expectStderr 1 nix eval "$root#y" | grepQuiet "'narHash' not supported by scheme 'jj'"

# A tampered id is a mismatch against the tree the rev names. The scheme
# refuses it (jj.cc) before any generic lock check runs: the tree is resolved
# from the rev and compared to the lock's id, with no tree read.
tampered=$(jq -r .nodes.dep.locked.treeHash "$TEST_ROOT/lock.before" | sed 's/^blake3-A/blake3-B/; t; s/^blake3-./blake3-A/')
[[ $tampered != "$depTreeId" ]]
jq --arg t "$tampered" '.nodes.dep.locked.treeHash = $t' "$TEST_ROOT/lock.before" > "$lock"
expectStderr 1 nix eval "$root#y" | grepQuiet "is locked to tree $tampered but revision"

# A non-blake3 treeHash is refused as such, before anything is fetched.
jq '.nodes.dep.locked.treeHash = "sha256-FePFYIlMuycIXPZbWi7LGEiMmZSX9FMbaQenWBzm1Sc="' "$TEST_ROOT/lock.before" > "$lock"
expectStderr 1 nix eval "$root#y" | grepQuiet "must be a BLAKE3"

cp "$TEST_ROOT/lock.before" "$lock"

# The dependency moving changes the lock, and only the id moves with it.
echo more > "$dep/data"
nix flake update dep --flake "$root"
depTreeId2=$(treeIdOf "file://$dep")
[[ $depTreeId2 != "$depTreeId" ]]
[[ $(jq -r .nodes.dep.locked.treeHash "$lock") == "$depTreeId2" ]]
(! grep narHash "$lock")

# `nix flake archive` copies the input to another store by its id. A jj-tree
# object is not self-certifying (Nix cannot check the id against the bytes),
# so the destination store's signature check counts zero signatures for it,
# as for an input-addressed path; the copy carries no signature, hence
# `--no-check-sigs`. The same copy without the flag is the refusal the trust
# model promises, and the message names the object kind.
expectStderr 1 nix flake archive "$root" --to "$TEST_ROOT/store-refused" | grepQuiet "lacks a signature"
json=$(nix flake archive --json "$root" --to "$TEST_ROOT/store2" --no-check-sigs)
depPath=$(echo "$json" | jq -r .inputs.dep.path)
[[ -e "$TEST_ROOT/store2/nix/store/$(basename "$depPath")/data" ]]
[[ $(nix path-info --store "$TEST_ROOT/store2" --json --json-format 2 "$depPath" | jq -r '.info[].ca.method') == jj-tree ]]

# The archived path is the lock's path: name plus id, nothing else.
[[ $(nix flake archive --json --dry-run "$root" | jq -r .inputs.dep.path) == "$depPath" ]]

# A tree-locked input whose store object is registered is served from the
# store when, and only when, the repository the lock names is absent: on such
# a host the object is the only source of the tree. store2 holds the object
# (copied above) and no jj repository at all.
mv "$dep" "$dep.away"
# `--no-eval-cache`: a cached `y` would answer without touching dep at all.
[[ $(nix eval --no-eval-cache --store "$TEST_ROOT/store2" "$root#y") == 14 ]]
# Same lock, a store without the object: the error names both roads, the
# absent repository first, so the success above came from the object, not
# from some other road to the repository.
expectStderr 1 nix eval --store "$TEST_ROOT/store3" "$root#y" | grepQuiet "is not a Jujutsu repository"
expectStderr 1 nix eval --store "$TEST_ROOT/store3" "$root#y" | grepQuiet "is not in the store either"
mv "$dep.away" "$dep"

# With the repository present it is the source, even though the object is
# registered in the store: the repository can name subtrees and the object
# cannot, so preferring the object would make a relative child's identity a
# function of the store's state (relative.sh pins the child's side of this;
# here, the object's presence changes nothing about what is served).
[[ $(nix eval --no-eval-cache --store "$TEST_ROOT/store2" "$root#y") == 14 ]]

# Locked means `rev` AND `treeHash`. The tree id is the identity; the rev is
# what lets the fetcher read that identity without snapshotting the working
# copy. A locked fetch is read-only: a file dropped into dep's working copy
# after locking is neither seen nor snapshotted.
opCount() { jj -R "$dep" --ignore-working-copy op log --no-graph -T '"x"' | wc -c; }
opsBefore=$(opCount)
echo stray > "$dep/stray"
depRev=$(jq -r .nodes.dep.locked.rev "$lock")
[[ $depRev =~ ^[0-9a-f]{64}$ ]]
# Pure evaluation (no --impure): a locked input is what pure mode fetches
# without a warning. The tree is read THROUGH the mount (`pathExists`), not
# from disk: the store path is lazy and may not exist as a directory at all.
lockedTree="builtins.fetchTree { type = \"jj\"; url = \"file://$dep\"; rev = \"$depRev\"; treeHash = \"$depTreeId2\"; }"
[[ $(nix eval --extra-experimental-features fetch-tree --json --expr "builtins.pathExists (($lockedTree).outPath + \"/stray\")" 2> "$TEST_ROOT/locked.err") == false ]]
[[ $(nix eval --extra-experimental-features fetch-tree --raw --expr "builtins.readFile (($lockedTree).outPath + \"/data\")") == more ]]
(! grep "is unlocked" "$TEST_ROOT/locked.err")
[[ $(opCount) == "$opsBefore" ]] || fail "a locked (rev + treeHash) fetch wrote an operation to the repository"
# The id alone is not a lock: pure evaluation says so (the content-hash
# warning every unlocked-but-hashed input gets), and the fetch it then makes
# is the unlocked one, a snapshot of whatever is on disk, which no longer
# matches the id.
expectStderr 1 nix eval --extra-experimental-features fetch-tree --raw --expr "(builtins.fetchTree { type = \"jj\"; url = \"file://$dep\"; treeHash = \"$depTreeId2\"; }).outPath" \
    > "$TEST_ROOT/unlocked.err"
grepQuiet "is unlocked" "$TEST_ROOT/unlocked.err"
grepQuiet "is locked to tree $depTreeId2 but revision" "$TEST_ROOT/unlocked.err"
[[ $(opCount) != "$opsBefore" ]] || fail "test setup: the unlocked fetch did not snapshot, so the read-only control above proved nothing"
rm "$dep/stray"

# The write side of `allow-dirty-locks`. An entry without a revision can only
# come from a hand-edited lock (every fetch writes rev and treeHash), but once
# present it is contagious: any change that forces the lock to be REWRITTEN
# would re-serialise it. Writing a lock containing an unlocked entry is
# refused by default; accepting one that still pins content by hash is
# exactly what the flag licenses. (Reading such a lock only warns; the
# refusal is about producing a new file with the defect baked in.)
cp "$lock" "$TEST_ROOT/lock.pristine"
jq 'del(.nodes.dep.locked.rev)' "$lock" > "$lock.tmp"
mv "$lock.tmp" "$lock"
cat > "$root/flake.nix" <<EOF
{
  inputs.dep.url = "jj+file://$dep";
  inputs.extra.url = "jj+file://$dep";
  outputs = { self, dep, extra }: { y = dep.x * 2; };
}
EOF
expectStderr 1 nix flake lock "$root" > "$TEST_ROOT/dirty-lock.err"
grepQuiet -F "unlocked input" "$TEST_ROOT/dirty-lock.err"
grepQuiet -F -- "--allow-dirty-locks" "$TEST_ROOT/dirty-lock.err"
nix flake lock "$root" --allow-dirty-locks
[[ $(jq -r '.nodes.extra.locked.rev' "$lock") =~ ^[0-9a-f]{64}$ ]]
[[ $(jq -r '.nodes.dep.locked | has("rev")' "$lock") = false ]]
