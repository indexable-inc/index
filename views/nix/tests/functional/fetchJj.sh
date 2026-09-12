#!/usr/bin/env bash

# The jj fetcher over jj's native object store: an input is identified by the
# blake3 id of its root tree, read in-process (no `jj` subprocess, no export,
# no NAR walk on the evaluation path). Every fixture here is created and
# edited with the jj CLI; the fetcher itself never runs it, and the
# no-jj-on-PATH block below is what pins that.

source common.sh

TODO_NixOS

requireJj

clearStoreIfPossible

# `jj`, `jjConfig` and `jjInit` come from common/functions.sh: one fixture
# for every jj test.
jjConfig

# $1: fetchTree URL
# $2: extra fields to splice into the fetchTree argument set (e.g. '; rev = "..."').
# $3: attribute to read from the result. `toString` makes this work for both
#     string attrs (outPath, rev, treeHash) and integer attrs (lastModified).
fetchAttr() {
    nix eval --extra-experimental-features fetch-tree --impure --raw --expr \
        "toString (builtins.fetchTree { type = \"jj\"; url = \"$1\"$2; }).$3"
}

# Whether the result set has attribute $3 at all.
hasAttr() {
    nix eval --extra-experimental-features fetch-tree --impure --json --expr \
        "(builtins.fetchTree { type = \"jj\"; url = \"$1\"$2; }) ? $3"
}

repo=$TEST_ROOT/jj
jjInit "$repo"
url=file://$repo

echo utrecht > "$repo"/hello
mkdir "$repo"/dir
echo world > "$repo"/dir/foo

# Untracked / ignored files that must NOT end up in the store.
cat > "$repo"/.gitignore <<EOF
result
*.tmp
build/
EOF
echo junk > "$repo"/scratch.tmp
mkdir "$repo"/build
echo artifact > "$repo"/build/out

fetchjj() { fetchAttr "$url" "$1" "$2"; }

# Basic fetch of the working copy. Only tracked files are present.
path=$(fetchjj "" outPath)
[[ $(cat "$path"/hello) = utrecht ]]
[[ $(cat "$path"/dir/foo) = world ]]
[[ -e "$path"/.gitignore ]]
[[ ! -e "$path"/scratch.tmp ]]
[[ ! -e "$path"/build ]]
[[ ! -e "$path"/.jj ]]

# The working copy is always identified by a revision (jj has no "dirty"
# state), 64 hex characters on the native backend.
rev=$(fetchjj "" rev)
[[ $rev =~ ^[0-9a-f]{64}$ ]]

# The identity of the input is its root tree id, never a NAR hash.
treeHash=$(fetchjj "" treeHash)
[[ $treeHash =~ ^blake3-[A-Za-z0-9+/=]{44}$ ]] || fail "treeHash is not an SRI blake3 hash: $treeHash"
[[ $(hasAttr "$url" "" narHash) = false ]] || fail "a jj fetch reports a narHash"

# lastModified is exposed; revCount is NOT. jj's index stores a generation
# number, not an ancestor count, so the fetcher emits no revCount rather than
# a number that means something else under the usual name.
[[ $(fetchjj "" lastModified) -gt 0 ]]
[[ $(hasAttr "$url" "" revCount) = false ]] || fail "a jj fetch reports a revCount"

# Fetching again without changes yields the same path.
path2=$(fetchjj "" outPath)
[[ $path = "$path2" ]]

# Editing a tracked file changes the revision, the tree and the store path
# (no commit needed).
echo amsterdam > "$repo"/hello
rev2=$(fetchjj "" rev)
[[ $rev != "$rev2" ]]
[[ $(fetchjj "" treeHash) != "$treeHash" ]]
path3=$(fetchjj "" outPath)
[[ $path != "$path3" ]]
[[ $(cat "$path3"/hello) = amsterdam ]]

# Adding a new file makes it tracked and visible (jj auto-tracks on snapshot).
echo new > "$repo"/dir/bar
path4=$(fetchjj "" outPath)
[[ $(cat "$path4"/dir/bar) = new ]]

# Filenames with special characters, including spaces and embedded newlines,
# come through the tree objects verbatim.
echo spaced > "$repo/a file with spaces"
weird=$(printf 'a\nb')   # a filename containing a newline
echo nl > "$repo/$weird"
path=$(fetchjj "" outPath)
[[ $(cat "$path/a file with spaces") = spaced ]]
[[ $(cat "$path/$weird") = nl ]]

# An executable and symlinks, then the same commit fetched by rev. By-rev and
# working-copy reach the tree by different calls (resolve versus snapshot)
# and must land on one store path: that is the whole content, executable bit
# and symlink targets included, agreeing byte for byte.
chmod +x "$repo"/dir/bar
ln -s hello "$repo"/symlink
ln -s '++/odd/target' "$repo"/pluslink
workdirPath=$(fetchjj "" outPath)
rev=$(fetchjj "" rev)
treeHash=$(fetchjj "" treeHash)
revPath=$(fetchAttr "$url" "; rev = \"$rev\"" outPath)
[[ $workdirPath = "$revPath" ]]
[[ $(cat "$revPath"/hello) = amsterdam ]]
[[ -x "$revPath"/dir/bar ]]
[[ -L "$revPath"/symlink && $(readlink "$revPath"/symlink) = hello ]]
[[ -L "$revPath"/pluslink && $(readlink "$revPath"/pluslink) = ++/odd/target ]]
[[ $(fetchAttr "$url" "; rev = \"$rev\"" rev) = "$rev" ]]
[[ $(fetchAttr "$url" "; rev = \"$rev\"" treeHash) = "$treeHash" ]]

# A metadata-only rewrite of the commit (`describe`) mints a new commit id
# over the same tree: the tree id and the store path stay, the rev moves.
# This is the property that makes the tree id, not the commit, the identity.
jj --repository "$repo" describe -m "described" >/dev/null
[[ $(fetchjj "" rev) != "$rev" ]] || fail "test setup: describe did not rewrite @"
[[ $(fetchjj "" treeHash) = "$treeHash" ]] || fail "a metadata-only rewrite changed the tree id"
[[ $(fetchjj "" outPath) = "$workdirPath" ]] || fail "a metadata-only rewrite changed the store path"

# A bookmark can be fetched via `ref` and resolves to the same tree.
jj --repository "$repo" bookmark create release -r @ >/dev/null
refPath=$(fetchAttr "$url" '; ref = "release"' outPath)
[[ $workdirPath = "$refPath" ]]

# A `rev` of 40 hex characters names a Git commit, which no native repository
# holds; it is refused at parse time, before any repository is opened.
expectStderr 1 nix eval --extra-experimental-features fetch-tree --impure --raw --expr \
    "(builtins.fetchTree { type = \"jj\"; url = \"$url\"; rev = \"0123456789abcdef0123456789abcdef01234567\"; }).rev" \
    | grepQuiet "not a Jujutsu native commit id"

# A flake in a Jujutsu workspace (which has a .jj but no .git) is routed to
# the jj fetcher. This is the case the feature was added for.
ws=$TEST_ROOT/jj-workspace
jj --repository "$repo" workspace add "$ws" >/dev/null
cat > "$ws"/flake.nix <<'EOF'
{
  outputs = { self, ... }: {
    answer = 42;
    hasFlake = builtins.pathExists (self + "/flake.nix");
    hasScratch = builtins.pathExists (self + "/scratch.tmp");
  };
}
EOF
printf '*.tmp\n' > "$ws"/.gitignore
echo junk > "$ws"/scratch.tmp

[[ $(nix eval "$ws"#answer) = 42 ]]
nix flake metadata "$ws" | grepQuiet "jj+file"
[[ $(nix eval "$ws"#hasFlake) = true ]]
[[ $(nix eval "$ws"#hasScratch) = false ]]

# Subtree identity. A jj tree is Merkle: editing one file changes the ids of
# exactly the trees on its path. Observed through relative path inputs, whose
# store path follows their subtree: an edit under `dir` moves `dir` and the
# root and leaves `other` where it was, and vice versa.
subRepo=$TEST_ROOT/jj-subtree
jjInit "$subRepo"
mkdir "$subRepo"/dir "$subRepo"/other
echo one > "$subRepo"/dir/a
echo two > "$subRepo"/other/b
# `flake = false`: `dir` and `other` hold data, not a flake.nix, and the
# subject here is their identity as subtrees, which a non-flake input reports
# as its outPath just the same.
cat > "$subRepo"/flake.nix <<'EOF'
{
  inputs.dir = { url = "path:./dir"; flake = false; };
  inputs.other = { url = "path:./other"; flake = false; };
  outputs = { self, dir, other }: {
    root = toString self;
    dir = toString dir;
    other = toString other;
  };
}
EOF
subEval() { nix eval --no-write-lock-file --raw "$subRepo#$1"; }
root1=$(subEval root); dir1=$(subEval dir); other1=$(subEval other)
echo one-edited > "$subRepo"/dir/a
root2=$(subEval root); dir2=$(subEval dir); other2=$(subEval other)
[[ $root2 != "$root1" ]] || fail "an edit under dir/ left the root tree unchanged"
[[ $dir2 != "$dir1" ]] || fail "an edit under dir/ left the dir subtree unchanged"
[[ $other2 = "$other1" ]] || fail "an edit under dir/ moved the other/ subtree"
echo two-edited > "$subRepo"/other/b
root3=$(subEval root); dir3=$(subEval dir); other3=$(subEval other)
[[ $root3 != "$root2" ]]
[[ $dir3 = "$dir2" ]] || fail "an edit under other/ moved the dir/ subtree"
[[ $other3 != "$other2" ]]

# A fresh, empty repository (the '@' commit has no files) fetches to an empty
# tree without error, and still exposes a valid revision.
empty=$TEST_ROOT/jj-empty
jjInit "$empty"
emptyPath=$(fetchAttr "file://$empty" "" outPath)
[[ -d $emptyPath ]]
[[ -z $(ls -A "$emptyPath") ]]
fetchAttr "file://$empty" "" rev | grepQuiet -E '^[0-9a-f]{64}$'

# A conflicted revision has no single content, so it is refused, both by rev
# and as the working copy. Denominators first, because a refusal is an
# absence and a fixture that never conflicted satisfies it just as well.
conflicted=$TEST_ROOT/jj-conflicted
jjInit "$conflicted"
jjc() { jj -R "$conflicted" "$@"; }
echo base > "$conflicted/a.txt"
echo untouched > "$conflicted/b.txt"
jjc describe -m base >/dev/null
conflictBase=$(jjc log -r @ --no-graph -T commit_id)
jjc new >/dev/null
echo left > "$conflicted/a.txt"
jjc describe -m left >/dev/null
conflictLeft=$(jjc log -r @ --no-graph -T commit_id)
jjc new "$conflictBase" >/dev/null
echo right > "$conflicted/a.txt"
jjc describe -m right >/dev/null
conflictRight=$(jjc log -r @ --no-graph -T commit_id)
jjc new "$conflictLeft" "$conflictRight" >/dev/null
conflictRev=$(jjc log -r @ --no-graph -T commit_id)
[[ $(jjc log -r @ --no-graph -T 'if(conflict,"1","0")') = 1 ]] \
    || fail "test setup: the fixture revision is not conflicted, so nothing below is being tested"
[[ $conflictRev =~ ^[0-9a-f]{64}$ ]]
# The unconflicted parent still fetches: the refusal is about the conflict,
# not the repository.
[[ -e $(fetchAttr "file://$conflicted" "; rev = \"$conflictLeft\"" outPath)/a.txt ]]
expectStderr 1 nix eval --extra-experimental-features fetch-tree --impure --raw --expr \
    "(builtins.fetchTree { type = \"jj\"; url = \"file://$conflicted\"; rev = \"$conflictRev\"; }).outPath" \
    | grepQuiet "unresolved conflicts"
expectStderr 1 nix eval --extra-experimental-features fetch-tree --impure --raw --expr \
    "(builtins.fetchTree { type = \"jj\"; url = \"file://$conflicted\"; }).outPath" \
    | grepQuiet "unresolved conflicts"

# A git-backed jj repository is not on the native store; the fetcher refuses
# it and names the scheme that reads it.
gitBacked=$TEST_ROOT/jj-git-backed
jj git init "$gitBacked" >/dev/null
echo x > "$gitBacked"/f
[[ -e $gitBacked/.jj ]] || fail "test setup: jj git init made no .jj"
expectStderr 1 nix eval --extra-experimental-features fetch-tree --impure --raw --expr \
    "(builtins.fetchTree { type = \"jj\"; url = \"file://$gitBacked\"; }).outPath" \
    | grepQuiet "git+file"

# A bare directory has no identity a lock file can name: the `path` scheme
# serves store objects only and refuses it with the jj hint
# (libfetchers/path.cc). Control: the same directory inside a jj repository
# fetches.
bare=$TEST_ROOT/bare-dir
mkdir -p "$bare"
echo plain > "$bare"/f
expectStderr 1 nix eval --extra-experimental-features fetch-tree --impure --raw --expr \
    "(builtins.fetchTree { type = \"path\"; path = \"$bare\"; }).outPath" \
    | grepQuiet "mutable directory"
jjInit "$bare"
[[ $(cat "$(fetchAttr "file://$bare" "" outPath)"/f) = plain ]]

# A path INSIDE a store object is as immutable as the object and is served
# rooted there: its own (NAR-addressed) store object holding just that
# subtree, not a subpath of the parent's. A subpath that does not exist is an
# error naming it, at the input rather than on the first read.
parent=$TEST_ROOT/store-subpath
mkdir -p "$parent/inner"
echo '{ outputs = _: { probe = "inner"; }; }' > "$parent/inner/flake.nix"
echo outer > "$parent/outer"
storeCopy=$(nix store add-path "$parent")
[[ $(nix eval --raw "path:$storeCopy/inner#probe") = inner ]]
innerPath=$(nix flake prefetch --json "path:$storeCopy/inner" | jq -r '.storePath')
[[ $innerPath != "$storeCopy"/* ]]
[[ -e $innerPath/flake.nix && ! -e $innerPath/outer ]]
expectStderr 1 nix flake metadata "path:$storeCopy/absent" | grepQuiet "absent"

# The fetcher reads the repository in-process. With no `jj` anywhere on PATH
# a fetch still works. The control comes first: the reduced PATH really has
# no jj, and the full one really had it (requireJj above).
nixOnly=$TEST_ROOT/nix-only-bin
mkdir -p "$nixOnly"
ln -sf "$(type -p nix)" "$nixOnly"/nix
if PATH=$nixOnly type -p jj > /dev/null; then
    fail "the reduced PATH still finds jj, so the no-jj check below would test nothing"
fi
noJjPath=$(PATH=$nixOnly nix eval --extra-experimental-features fetch-tree --impure --raw --expr \
    "toString (builtins.fetchTree { type = \"jj\"; url = \"$url\"; rev = \"$rev\"; }).outPath")
[[ $noJjPath = "$revPath" ]] || fail "a fetch without jj on PATH produced $noJjPath, expected $revPath"

# Locking. A flake whose input is a jj repository locks it by `treeHash`
# and never by `narHash`; the lock is verified by comparing ids, and an
# edited lock is refused with both ids named.
lockRepo=$TEST_ROOT/jj-lock-consumer
jjInit "$lockRepo"
# `flake = false`: the dep repository holds data, not a flake.nix.
cat > "$lockRepo"/flake.nix <<EOF
{
  inputs.dep = { url = "jj+file://$repo"; flake = false; };
  outputs = { self, dep }: { greeting = builtins.readFile (dep + "/hello"); };
}
EOF
nix flake lock "$lockRepo"
lockJson=$lockRepo/flake.lock
[[ $(jq -r '.nodes.dep.locked.type' "$lockJson") = jj ]]
[[ $(jq -r '.nodes.dep.locked.treeHash' "$lockJson") = "$treeHash" ]] \
    || fail "the lock's treeHash is not the tree the repository reports"
[[ $(jq -r '.nodes.dep.locked | has("narHash")' "$lockJson") = false ]] \
    || fail "the lock carries a narHash for a jj input"
[[ $(jq -r '.nodes.dep.locked.rev' "$lockJson") =~ ^[0-9a-f]{64}$ ]]
[[ $(nix eval --raw "$lockRepo#greeting") = amsterdam ]]
# Tamper with the locked tree id: same rev, different tree, refused.
bogus=blake3-$(head -c 32 /dev/zero | base64)
jq --arg h "$bogus" '.nodes.dep.locked.treeHash = $h' "$lockJson" > "$lockJson".tmp
mv "$lockJson".tmp "$lockJson"
expectStderr 1 nix eval --raw "$lockRepo#greeting" | grepQuiet "is locked to tree"
# A narHash on a jj input is an attribute the scheme does not accept.
jq --arg h "$treeHash" '.nodes.dep.locked.treeHash = $h | .nodes.dep.locked.narHash = "sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="' "$lockJson" > "$lockJson".tmp
mv "$lockJson".tmp "$lockJson"
# The refusal is `allowedAttrs`, so grep the mechanism: a bare "narHash"
# would also match any message that merely mentions the attribute.
expectStderr 1 nix eval --raw "$lockRepo#greeting" | grepQuiet "not supported by scheme 'jj'"

# This block needs a garbage collector, and the sanitizer lane does not have
# one. It burns evaluator memory on purpose (see the calibration note below),
# and `ci/gha/tests/default.nix:57` builds that lane with
# `enableGC = !withSanitizers` because Boehm is incompatible with ASan, so none
# of those hundred million list elements is ever reclaimed. Observed as
# `fetchJj FAIL 290.26s (exit status 137 or signal 9 SIGKILL)` in run
# 30719557490, with the same test passing in 195s on a collector-having build.
#
# WHAT THIS GIVES UP, stated because a skip without it is indistinguishable
# from one added to make a red go away: on the sanitizer lane, and only there,
# nothing checks that a working-copy write during an evaluation stays out of
# that evaluation. The property is about when the fetcher snapshots, which is
# platform- and toolchain-independent, and `tests on ubuntu` plus every local
# run still cover it on the same code. So the loss is one lane's worth of
# redundancy, not the property.
#
# The number is NOT the thing to tune. Shrinking it to fit would recreate the
# exact bug the calibration note below records, where the evaluation finished
# before the writer and the test passed against the code it was written to
# catch. A green test that tests nothing is worse than a red one.
if ! evaluatorHasGC; then
    echo "fetchJj: evaluator built without a garbage collector, skipping the" \
         "working-copy mutation test; see the comment above this line" >&2
else

    # A write to the working copy during an evaluation must not reach that
    # evaluation. The fetcher snapshots the working copy into `@` and then
    # reads that commit's tree objects, never the files, so the content of the
    # input is decided at the snapshot.
    #
    # The ordering inside the evaluation is deterministic rather than raced: the
    # `probe` read forces the fetch, `slow` then burns a few seconds of pure
    # evaluation, and only then is `hello` read. The writer just has to land
    # somewhere inside that window, which is why it sleeps a fraction of a second
    # and the window is seconds long. Calibrated rather than guessed: measured on
    # an unpatched build, 30M list elements is about 1.5s of evaluation and 250M
    # about 24s, so 100M gives roughly 5s against a writer that lands at 0.5s. The
    # first version of this test used 4M and a 1s writer, so the evaluation was
    # over before the write and the test passed against the code it was written to
    # catch.
    mutation_repo=$TEST_ROOT/jj-mutation
    jjInit "$mutation_repo"
    echo utrecht > "$mutation_repo"/hello
    echo marker > "$mutation_repo"/marker

    expr='
      let
        src = builtins.fetchTree { type = "jj"; url = "file://'"$mutation_repo"'"; };
        probe = builtins.readFile (src + "/marker");
        slow = builtins.foldl'"'"' (a: b: a + b) 0 (builtins.genList (x: x) 100000000);
      in builtins.seq probe (builtins.seq slow (builtins.readFile (src + "/hello")))
    '

    # Wait for the evaluation to actually take its snapshot before mutating,
    # rather than guessing with a sleep. Snapshotting the working copy is a jj
    # operation, so it shows up in the operation log; polling for that turns a
    # wall-clock race (which flakes on a loaded machine, where process startup
    # alone can outlast a fixed delay) into a real happens-before. A poll that
    # collides with the snapshot in progress prints a jj error and counts 0,
    # which only makes the loop go round once more.
    opCount() { jj --repository "$mutation_repo" op log --no-graph -T '"x"' | wc -c; }
    baselineOps=$(opCount)
    (
        for _ in $(seq 1 600); do
            [[ "$(opCount)" != "$baselineOps" ]] && break
            sleep 0.05
        done
        echo mutated > "$mutation_repo"/hello
    ) &
    writer=$!
    observed=$(nix eval --impure --raw --expr "$expr")
    wait "$writer"

    [[ $observed = "utrecht" ]] || fail "an evaluation read a working copy write that happened after its snapshot: got '$observed'"

    # And the next evaluation does see it, so the first result is a snapshot rather
    # than a stale cache.
    observed=$(nix eval --impure --raw --expr "$expr")
    [[ $observed = "mutated" ]] || fail "a later evaluation did not see the write: got '$observed'"
fi

# Forcing the copy lands on the store path the mount announced. The mount
# derived its path from the tree id with no reads; instantiating a
# derivation over it materialises the tree with the same method, and the two
# must agree or ensureLazyPathCopied fails with a path mismatch. A tree with
# a nested directory, an executable and a symlink, so every entry kind is
# materialised.
forceRepo=$TEST_ROOT/jj-force
jjInit "$forceRepo"
echo pinned > "$forceRepo"/file
mkdir "$forceRepo"/d
echo nested > "$forceRepo"/d/inner
printf '#!/bin/sh\n' > "$forceRepo"/d/run
chmod +x "$forceRepo"/d/run
ln -s file "$forceRepo"/link
forcePath=$(fetchAttr "file://$forceRepo" "" outPath)
# `drvPath` instantiates the derivation, which forces every lazy path in its
# context into the store; `src` is then the path that copy landed on.
drvSrc=$(nix eval --extra-experimental-features fetch-tree --impure --raw --expr \
    "let src = builtins.fetchTree { type = \"jj\"; url = \"file://$forceRepo\"; };
         drv = derivation { name = \"uses-jj-src\"; system = builtins.currentSystem; builder = \"/bin/sh\"; args = [ \"-c\" \"true\" ]; inherit src; };
     in builtins.seq drv.drvPath (toString drv.src)")
[[ $drvSrc = "$forcePath" ]] || fail "instantiation materialised $drvSrc, the mount announced $forcePath"
nix path-info "$forcePath" > /dev/null
[[ $(cat "$forcePath"/d/inner) = nested ]]
[[ -x "$forcePath"/d/run ]]
[[ $(readlink "$forcePath"/link) = file ]]
