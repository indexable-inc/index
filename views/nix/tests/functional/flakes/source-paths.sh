#!/usr/bin/env bash

source ./common.sh

requireGit

# A Git flake is the commit its repository has checked out, so what a path
# expression can read is exactly what that commit contains. There is no
# "tracked but not yet committed" state left to report on: an uncommitted
# change is refused before any path is read, and a path the commit does not
# contain simply does not exist.

repo=$TEST_ROOT/repo

createGitRepo "$repo"

cat > "$repo/flake.nix" <<EOF2
{
  outputs = { ... }: {
    x = 1;
    y = assert false; 1;
    z = builtins.readFile ./foo;
    a = import ./foo;
    b = import ./dir;
  };
}
EOF2

# Nothing is committed yet, so there is no source at all.
expectStderr 1 nix eval "$repo#x" | grepQuiet "has no commits"

# Staging is not committing.
git -C "$repo" add flake.nix
expectStderr 1 nix eval "$repo#x" | grepQuiet "has uncommitted changes"

git -C "$repo" commit -a -m foo

[[ $(nix eval "$repo#x") = 1 ]]

# Positions name the fetched commit, however the repository is spelled: a bare
# directory and an explicit ref are the same fetch.
expectStderr 1 nix eval "$repo#y" | grepQuiet "at «git+file://$repo?ref=.*&rev=.*»/flake.nix:"
expectStderr 1 nix eval "git+file://$repo?ref=master#y" | grepQuiet "at «git+file://$repo?ref=master&rev=.*»/flake.nix:"

# A path the commit does not contain does not exist.
expectStderr 1 nix eval "$repo#z" | grepQuiet "»/foo' does not exist"
expectStderr 1 nix eval "git+file://$repo?ref=master#z" | grepQuiet "error: '«git+file://$repo?ref=master&rev=.*»/foo' does not exist"
expectStderr 1 nix eval "$repo#a" | grepQuiet "»/foo"

# An untracked file is invisible: it belongs to no commit. It is not a change
# either, so the fetch itself still succeeds.
echo 123 > "$repo/foo"

expectStderr 1 nix eval "$repo#z" | grepQuiet "»/foo' does not exist"
expectStderr 1 nix eval "$repo#a" | grepQuiet "»/foo"

# Staging it does make the tree differ from its commit, which is refused.
git -C "$repo" add "$repo/foo"
expectStderr 1 nix eval "$repo#z" | grepQuiet "has uncommitted changes"

git -C "$repo" commit -m 'add foo'

[[ $(nix eval --raw "$repo#z") = 123 ]]

expectStderr 1 nix eval "$repo#b" | grepQuiet "»/dir"

mkdir -p "$repo/dir"
echo 456 > "$repo/dir/default.nix"

# Still untracked, so still not part of the commit.
expectStderr 1 nix eval "$repo#b" | grepQuiet "»/dir"

git -C "$repo" add "$repo/dir/default.nix"
git -C "$repo" commit -m 'add dir'

[[ $(nix eval "$repo#b") = 456 ]]
