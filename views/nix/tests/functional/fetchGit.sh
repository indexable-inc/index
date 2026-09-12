#!/usr/bin/env bash

source common.sh

requireGit

clearStoreIfPossible

# Intentionally not in a canonical form
# See https://github.com/NixOS/nix/issues/6195
repo=$TEST_ROOT/./git

export _NIX_FORCE_HTTP=1

rm -rf "${repo}"-tmp "$TEST_HOME"/.cache/nix "$TEST_ROOT"/worktree "$TEST_ROOT"/minimal

createGitRepo "$repo"

echo utrecht > "$repo"/hello
touch "$repo"/.gitignore
git -C "$repo" add hello .gitignore
git -C "$repo" commit -m 'Bla1'
rev1=$(git -C "$repo" rev-parse HEAD)
git -C "$repo" tag -a tag1 -m tag1

echo world > "$repo"/hello
git -C "$repo" commit -m 'Bla2' -a
git -C "$repo" worktree add "$TEST_ROOT"/worktree
echo hello >> "$TEST_ROOT"/worktree/hello
git -C "$TEST_ROOT"/worktree commit -m 'Bla2-worktree' -a
rev2=$(git -C "$repo" rev-parse HEAD)
git -C "$repo" tag -a tag2 -m tag2

# Check whether fetching in read-only mode works.
nix-instantiate --eval -E "builtins.readFile ((builtins.fetchGit \"file://$TEST_ROOT/worktree\") + \"/hello\") == \"utrecht\\n\""

# Fetch a worktree. A worktree is a checkout like any other: the fetch is of
# the commit it has checked out, on its own branch.
unset _NIX_FORCE_HTTP
worktreeRev=$(git -C "$TEST_ROOT"/worktree rev-parse HEAD)
[[ $(nix eval --impure --raw --expr "(builtins.fetchGit \"file://$TEST_ROOT/worktree\").rev") = "$worktreeRev" ]]
path0=$(nix eval --impure --raw --expr "(builtins.fetchGit \"file://$TEST_ROOT/worktree\").outPath")
path0_=$(nix eval --impure --raw --expr "(builtins.fetchTree { type = \"git\"; url = \"file://$TEST_ROOT/worktree\"; }).outPath")
[[ $path0 = "$path0_" ]]
path0_=$(nix eval --impure --raw --expr "(builtins.fetchTree \"git+file://$TEST_ROOT/worktree\").outPath")
[[ $path0 = "$path0_" ]]
export _NIX_FORCE_HTTP=1
[[ $(tail -n 1 "$path0"/hello) = "hello" ]]

# Nuke the cache
rm -rf "$TEST_HOME"/.cache/nix

# Fetch the default branch.
path=$(nix eval --impure --raw --expr "(builtins.fetchGit \"file://$repo\").outPath")
[[ $(cat "$path"/hello) = world ]]

# Fetch again. This should be cached.
# NOTE: This has to be done before the test case below which tries to pack-refs
# the reason being that the lookup on the cache uses the ref-file `/refs/heads/master`
# which does not exist after packing.
mv "$repo" "${repo}"-tmp
path2=$(nix eval --impure --raw --expr "(builtins.fetchGit \"file://$repo\").outPath")
[[ $path = "$path2" ]]

[[ $(nix eval --impure --expr "(builtins.fetchGit \"file://$repo\").revCount") = 2 ]]
[[ $(nix eval --impure --raw --expr "(builtins.fetchGit \"file://$repo\").rev") = "$rev2" ]]
[[ $(nix eval --impure --raw --expr "(builtins.fetchGit \"file://$repo\").shortRev") = "${rev2:0:7}" ]]

# Fetching with a explicit hash should succeed.
path2=$(nix eval --refresh --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; rev = \"$rev2\"; }).outPath")
[[ $path = "$path2" ]]

path2=$(nix eval --refresh --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; rev = \"$rev1\"; }).outPath")
[[ $(cat "$path2"/hello) = utrecht ]]

mv "${repo}"-tmp "$repo"

# Fetch when the cache has packed-refs
# Regression test of #8822
git -C "$TEST_HOME"/.cache/nix/gitv3/*/ pack-refs --all
path=$(nix eval --impure --raw --expr "(builtins.fetchGit \"file://$repo\").outPath")

# Fetch a rev from another branch
git -C "$repo" checkout -b devtest
echo "different file" >> "$TEST_ROOT"/git/differentbranch
git -C "$repo" add differentbranch
git -C "$repo" commit -m 'Test2'
git -C "$repo" checkout master
devrev=$(git -C "$repo" rev-parse devtest)
nix eval --raw --expr "builtins.fetchGit { url = \"file://$repo\"; rev = \"$devrev\"; }"

[[ $(nix eval --raw --expr "builtins.readFile (builtins.fetchGit { url = \"file://$repo\"; rev = \"$devrev\"; allRefs = true; } + \"/differentbranch\")") = 'different file' ]]

# In pure eval mode, fetchGit without a revision should fail.
[[ $(nix eval --impure --raw --expr "builtins.readFile (fetchGit \"file://$repo\" + \"/hello\")") = world ]]
(! nix eval --raw --expr "builtins.readFile (fetchGit \"file://$repo\" + \"/hello\")")

# Fetch using an explicit revision hash.
path2=$(nix eval --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; rev = \"$rev2\"; }).outPath")
[[ $path = "$path2" ]]

# In pure eval mode, fetchGit with a revision should succeed.
[[ $(nix eval --raw --expr "builtins.readFile (fetchGit { url = \"file://$repo\"; rev = \"$rev2\"; } + \"/hello\")") = world ]]

# But without a hash, it fails.
expectStderr 1 nix eval --expr 'builtins.fetchGit "file:///foo"' | grepQuiet "'fetchGit' doesn't fetch unlocked input"

# Using a clean working tree should produce the same result.
path2=$(nix eval --impure --raw --expr "(builtins.fetchGit $repo).outPath")
[[ $path = "$path2" ]]

# An unclean tree has no commit to lock to, so it is refused, and the error
# names the files that differ. Untracked files are not changes: `bar` and
# `dir2/bar` below never appear in it.
mkdir "$repo"/dir1 "$repo"/dir2
echo foo > "$repo"/dir1/foo
echo bar > "$repo"/bar
echo bar > "$repo"/dir2/bar
git -C "$repo" add dir1/foo
git -C "$repo" rm hello

unset _NIX_FORCE_HTTP
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchGit $repo).outPath" \
    | grepQuiet "has uncommitted changes to 2 tracked file(s)"
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchGit $repo).outPath" | grepQuiet "dir1/foo"
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchGit $repo).outPath" | grepQuiet "hello (deleted)"
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchGit $repo).outPath" | grepQuietInverse "dir2/bar"

# ... unless we're using an explicit ref or rev: those name a commit, and the
# working tree is not consulted at all.
path3=$(nix eval --impure --raw --expr "(builtins.fetchGit { url = $repo; ref = \"master\"; }).outPath")
[[ $path = "$path3" ]]

path3=$(nix eval --raw --expr "(builtins.fetchGit { url = $repo; rev = \"$rev2\"; }).outPath")
[[ $path = "$path3" ]]

# Committing makes it fetchable again, at the new commit.
git -C "$repo" commit -m 'Bla3' -a
rev3=$(git -C "$repo" rev-parse HEAD)

path2=$(nix eval --impure --refresh --raw --expr "(builtins.fetchGit \"file://$repo\").outPath")
[ ! -e "$path2"/hello ]
[ ! -e "$path2"/bar ]
[ ! -e "$path2"/dir2/bar ]
[ ! -e "$path2"/.git ]
[[ $(cat "$path2"/dir1/foo) = foo ]]

# A bare checkout and an explicit `?rev=<HEAD>` are the same fetch, so they
# must land on one store path.
path4=$(nix eval --impure --refresh --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; rev = \"$rev3\"; }).outPath")
[[ $path2 = "$path4" ]]

[[ $(nix eval --impure --expr "builtins.hasAttr \"rev\" (builtins.fetchGit $repo)") == "true" ]]
[[ $(nix eval --impure --raw --expr "(builtins.fetchGit $repo).rev") = "$rev3" ]]

expect 102 nix eval --raw --expr "(builtins.fetchGit { url = $repo; rev = \"$rev2\"; narHash = \"sha256-B5yIPHhEm0eysJKEsO7nqxprh9vcblFxpJG11gXJus1=\"; }).outPath"

path5=$(nix eval --raw --expr "(builtins.fetchGit { url = $repo; rev = \"$rev2\"; narHash = \"sha256-Hr8g6AqANb3xqX28eu1XnjK/3ab8Gv6TJSnkb1LezG9=\"; }).outPath")
[[ $path = "$path5" ]]

# Ensure that NAR hashes are checked.
expectStderr 102 nix eval --raw --expr "(builtins.fetchGit { url = $repo; rev = \"$rev2\"; narHash = \"sha256-Hr8g6AqANb4xqX28eu1XnjK/3ab8Gv6TJSnkb1LezG9=\"; }).outPath" | grepQuiet "error: NAR hash mismatch"

# It's allowed to use only a narHash, but you should get a warning.
expectStderr 0 nix eval --raw --expr "(builtins.fetchGit { url = $repo; ref = \"tag2\"; narHash = \"sha256-Hr8g6AqANb3xqX28eu1XnjK/3ab8Gv6TJSnkb1LezG9=\"; }).outPath" | grepQuiet "warning: Input .* is unlocked"

# tarball-ttl should be ignored if we specify a rev
echo delft > "$repo"/hello
git -C "$repo" add hello
git -C "$repo" commit -m 'Bla4'
rev3=$(git -C "$repo" rev-parse HEAD)
nix eval --tarball-ttl 3600 --expr "builtins.fetchGit { url = $repo; rev = \"$rev3\"; }" >/dev/null

# Update 'path' to reflect latest master
path=$(nix eval --impure --raw --expr "(builtins.fetchGit \"file://$repo\").outPath")

# Check behavior when non-master branch is used
git -C "$repo" checkout "$rev2" -b dev
echo dev > "$repo"/hello

# A dirty tree is refused however the repository is spelled.
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchGit \"file://$repo\").outPath" \
    | grepQuiet "has uncommitted changes"
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchGit $repo).outPath" \
    | grepQuiet "has uncommitted changes"

# Making a dirty tree clean again and fetching it should
# record correct revision information. See: #4140
echo world > "$repo"/hello
[[ $(nix eval --impure --raw --expr "(builtins.fetchGit $repo).rev") = "$rev2" ]]

# Committing shouldn't switch to using 'master'
echo dev > "$repo"/hello
git -C "$repo" commit -m 'Bla5' -a
devRev=$(git -C "$repo" rev-parse HEAD)
path3=$(nix eval --impure --raw --expr "(builtins.fetchGit $repo).outPath")
path4=$(nix eval --impure --raw --expr "(builtins.fetchGit \"file://$repo\").outPath")
[[ $(cat "$path4"/hello) = dev ]]
[[ $path3 = "$path4" ]]
[[ $(nix eval --impure --raw --expr "(builtins.fetchGit $repo).rev") = "$devRev" ]]

# Using remote path with branch other than 'master' should fetch the HEAD revision.
# (--tarball-ttl 0 to prevent using the cached repo above)
export _NIX_FORCE_HTTP=1
path4=$(nix eval --tarball-ttl 0 --impure --raw --expr "(builtins.fetchGit $repo).outPath")
[[ $(cat "$path4"/hello) = dev ]]
[[ $path3 = "$path4" ]]
unset _NIX_FORCE_HTTP

# Confirm same as 'dev' branch
path5=$(nix eval --impure --raw --expr "(builtins.fetchGit { url = $repo; ref = \"dev\"; }).outPath")
[[ $path3 = "$path5" ]]


# Nuke the cache
rm -rf "$TEST_HOME"/.cache/nix

# Try again. This should work.
path5=$(nix eval --impure --raw --expr "(builtins.fetchGit { url = $repo; ref = \"dev\"; }).outPath")
[[ $path3 = "$path5" ]]

# Fetching from a repo with only a specific revision and no branches should
# not fall back to copying files and record correct revision information. See: #5302
createGitRepo "$TEST_ROOT"/minimal
git -C "$TEST_ROOT"/minimal fetch "$repo" "$rev2"
git -C "$TEST_ROOT"/minimal checkout "$rev2"
[[ $(nix eval --impure --raw --expr "(builtins.fetchGit { url = $TEST_ROOT/minimal; }).rev") = "$rev2" ]]

# Explicit ref = "HEAD" should work, and produce the same outPath as without ref
path7=$(nix eval --impure --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; ref = \"HEAD\"; }).outPath")
path8=$(nix eval --impure --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; }).outPath")
[[ $path7 = "$path8" ]]

# ref = "HEAD" should fetch the HEAD revision
rev4=$(git -C "$repo" rev-parse HEAD)
rev4_nix=$(nix eval --impure --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; ref = \"HEAD\"; }).rev")
[[ $rev4 = "$rev4_nix" ]]
export _NIX_FORCE_HTTP=1
rev4_nix=$(nix eval --impure --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; ref = \"HEAD\"; }).rev")
[[ $rev4 = "$rev4_nix" ]]
unset _NIX_FORCE_HTTP

# The name argument should be handled
path9=$(nix eval --impure --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; ref = \"HEAD\"; name = \"foo\"; }).outPath")
[[ $path9 =~ -foo$ ]]

# Specifying a ref without a rev shouldn't pick a cached rev for a different ref
export _NIX_FORCE_HTTP=1
rev_tag1_nix=$(nix eval --impure --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; ref = \"refs/tags/tag1\"; }).rev")
# shellcheck disable=SC1083
rev_tag1=$(git -C "$repo" rev-parse refs/tags/tag1^{commit})
[[ $rev_tag1_nix = "$rev_tag1" ]]
rev_tag2_nix=$(nix eval --impure --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; ref = \"refs/tags/tag2\"; }).rev")
# shellcheck disable=SC1083
rev_tag2=$(git -C "$repo" rev-parse refs/tags/tag2^{commit})
[[ $rev_tag2_nix = "$rev_tag2" ]]
unset _NIX_FORCE_HTTP

# Ensure .gitattributes is respected
touch "$repo"/not-exported-file
touch "$repo"/exported-wonky
echo "/not-exported-file export-ignore" >> "$repo"/.gitattributes
echo "/exported-wonky export-ignore=wonk" >> "$repo"/.gitattributes
git -C "$repo" add not-exported-file exported-wonky .gitattributes
git -C "$repo" commit -m 'Bla6'
rev5=$(git -C "$repo" rev-parse HEAD)
path12=$(nix eval --raw --expr "(builtins.fetchGit { url = \"file://$repo\"; rev = \"$rev5\"; }).outPath")
[[ ! -e $path12/not-exported-file ]]
[[ -e $path12/exported-wonky ]]

# should fail if there is no repo
rm -rf "$repo"/.git
rm -rf "$TEST_HOME"/.cache/nix
(! nix eval --impure --raw --expr "(builtins.fetchGit \"file://$repo\").outPath")

# A repo without commits has nothing to fetch: a staged file is an
# uncommitted change like any other.
initGitRepo "$repo"
git -C "$repo" add hello
expectStderr 1 nix eval --impure --raw --expr "(builtins.fetchGit \"file://$repo\").outPath" \
    | grepQuiet "has uncommitted changes"

# should succeed for a path with a space
# regression test for #7707
repo="$TEST_ROOT/a b"
createGitRepo "$repo"

echo utrecht > "$repo/hello"
touch "$repo/.gitignore"
git -C "$repo" add hello .gitignore
git -C "$repo" commit -m 'Bla1'
cd "$repo"
# shellcheck disable=SC2034
path11=$(nix eval --impure --raw --expr "(builtins.fetchGit ./.).outPath")

# Test a workdir with no commits: there is no revision to lock to, so the
# fetch is refused rather than answered with the null revision.
empty="$TEST_ROOT/empty"
createGitRepo "$empty"

expectStderr 1 nix eval --impure --expr "(builtins.fetchGit $empty).outPath" | grepQuiet "has no commits"

# An untracked file is not a change, so the repository is still commitless.
echo foo > "$empty/x"
expectStderr 1 nix eval --impure --expr "(builtins.fetchGit $empty).outPath" | grepQuiet "has no commits"

# Staging it is a change, and there is still no commit behind it.
git -C "$empty" add x
expectStderr 1 nix eval --impure --expr "(builtins.fetchGit $empty).outPath" \
    | grepQuiet "has uncommitted changes"

# Test a repo with an empty commit.
git -C "$empty" rm -f x

git -C "$empty" config user.email "foobar@example.com"
git -C "$empty" config user.name "Foobar"
git -C "$empty" commit --allow-empty --allow-empty-message --message ""

nix eval --impure --expr "let attrs = builtins.fetchGit $empty; in assert attrs.lastModified != 0; assert attrs.rev != \"0000000000000000000000000000000000000000\"; assert attrs.revCount == 1; true"

# Test exportHistory: structured, deterministic commit history as eval data.
histRepo="$TEST_ROOT/history"
rm -rf "$histRepo"
createGitRepo "$histRepo"

echo one > "$histRepo/a"
git -C "$histRepo" add a
git -C "$histRepo" commit -m 'first'
hrev1=$(git -C "$histRepo" rev-parse HEAD)

echo two > "$histRepo/a"
mkdir "$histRepo/sub"
echo s > "$histRepo/sub/file"
git -C "$histRepo" add a sub/file
git -C "$histRepo" commit -m 'second'
hrev2=$(git -C "$histRepo" rev-parse HEAD)

git -C "$histRepo" rm -q a
git -C "$histRepo" commit -m 'third'
hrev3=$(git -C "$histRepo" rev-parse HEAD)

histExpr="builtins.fetchGit { url = \"file://$histRepo\"; rev = \"$hrev3\"; exportHistory = true; historyDepth = 0; }"

# The attributes are gated by the git-export-history experimental feature.
expectStderr 1 nix eval --impure --expr "($histExpr).history" | grepQuiet "experimental Nix feature 'git-export-history' is disabled"

histEval() {
    nix eval --extra-experimental-features git-export-history --impure "$@"
}

# Full history: all three commits, newest first, in topological order.
[[ $(histEval --expr "builtins.length ($histExpr).history") = 3 ]]
[[ $(histEval --raw --expr "builtins.concatStringsSep \",\" (map (c: c.rev) ($histExpr).history)") = "$hrev3,$hrev2,$hrev1" ]]

# Commit metadata comes from the commit objects.
[[ $(histEval --raw --expr "(builtins.head ($histExpr).history).message") = 'third' ]]
[[ $(histEval --raw --expr "builtins.concatStringsSep \",\" (builtins.head ($histExpr).history).parents") = "$hrev2" ]]
[[ $(histEval --expr "builtins.length (builtins.elemAt ($histExpr).history 2).parents") = 0 ]]
[[ $(histEval --raw --expr "(builtins.head ($histExpr).history).author.name") = 'Foobar' ]]

# Paths touched, relative to the first parent: root commit adds, deletions are 'D'.
[[ $(histEval --json --expr "(builtins.head ($histExpr).history).paths") = '[{"path":"a","status":"D"}]' ]]
[[ $(histEval --json --expr "(builtins.elemAt ($histExpr).history 1).paths") = '[{"path":"a","status":"M"},{"path":"sub/file","status":"A"}]' ]]
[[ $(histEval --json --expr "(builtins.elemAt ($histExpr).history 2).paths") = '[{"path":"a","status":"A"}]' ]]

# historyDepth bounds the exported set by commit count;
# parents still name commits outside the exported set.
histDepthExpr="builtins.fetchGit { url = \"file://$histRepo\"; rev = \"$hrev3\"; exportHistory = true; historyDepth = 1; }"
[[ $(histEval --json --expr "map (c: c.rev) ($histDepthExpr).history") = "[\"$hrev3\"]" ]]
[[ $(histEval --raw --expr "builtins.concatStringsSep \",\" (builtins.head ($histDepthExpr).history).parents") = "$hrev2" ]]

# historyDepth without exportHistory is rejected.
expectStderr 1 histEval --expr "builtins.fetchGit { url = \"file://$histRepo\"; rev = \"$hrev3\"; historyDepth = 1; }" | grepQuiet "requires 'exportHistory = true'"

# exportHistory requires the full history: shallow is rejected.
expectStderr 1 histEval --expr "(builtins.fetchGit { url = \"file://$histRepo\"; rev = \"$hrev3\"; exportHistory = true; shallow = true; }).history" | grepQuiet "cannot be combined with 'shallow"

# The history never leaks into the input attributes (and thus lock files):
# the same fetch without .history has no history-shaped attributes.
histEval --expr "assert !(builtins.fetchGit { url = \"file://$histRepo\"; rev = \"$hrev3\"; } ? history); true" > /dev/null

# historyPaths = false skips path extraction: entries keep the commit
# metadata but carry no paths attribute at all.
histNoPathsExpr="builtins.fetchGit { url = \"file://$histRepo\"; rev = \"$hrev3\"; exportHistory = true; historyDepth = 0; historyPaths = false; }"
[[ $(histEval --expr "builtins.length ($histNoPathsExpr).history") = 3 ]]
[[ $(histEval --raw --expr "(builtins.head ($histNoPathsExpr).history).message") = 'third' ]]
histEval --expr "assert !(builtins.head ($histNoPathsExpr).history ? paths); true" > /dev/null

# historyPaths without exportHistory is rejected.
expectStderr 1 histEval --expr "builtins.fetchGit { url = \"file://$histRepo\"; rev = \"$hrev3\"; historyPaths = false; }" | grepQuiet "requires 'exportHistory = true'"

# historyDepth bounds the number of commits, not the generation distance:
# under merge fan-out a depth of 2 exports exactly 2 commits (the tip plus
# the numerically smallest parent), not the whole parent level.
histBranch=$(git -C "$histRepo" symbolic-ref --short HEAD)
git -C "$histRepo" checkout -q -b history-side "$hrev2"
echo side > "$histRepo/b"
git -C "$histRepo" add b
git -C "$histRepo" commit -q -m 'side'
hrevSide=$(git -C "$histRepo" rev-parse HEAD)
git -C "$histRepo" checkout -q "$histBranch"
git -C "$histRepo" merge -q --no-ff -m 'merge' history-side
hrevMerge=$(git -C "$histRepo" rev-parse HEAD)
hrevSmallerParent=$(printf '%s\n%s\n' "$hrev3" "$hrevSide" | sort | head -n1)
histMergeExpr="builtins.fetchGit { url = \"file://$histRepo\"; rev = \"$hrevMerge\"; exportHistory = true; historyDepth = 2; }"
[[ $(histEval --json --expr "map (c: c.rev) ($histMergeExpr).history") = "[\"$hrevMerge\",\"$hrevSmallerParent\"]" ]]
