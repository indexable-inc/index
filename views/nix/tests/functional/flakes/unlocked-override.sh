#!/usr/bin/env bash

source ./common.sh

requireGit

flake1Dir=$TEST_ROOT/flake1
flake2Dir=$TEST_ROOT/flake2

createGitRepo "$flake1Dir"
cat > "$flake1Dir"/flake.nix <<EOF
{
    outputs = { self }: { x = import ./x.nix; };
}
EOF
echo 123 > "$flake1Dir"/x.nix
git -C "$flake1Dir" add flake.nix x.nix
git -C "$flake1Dir" commit -m Initial

createGitRepo "$flake2Dir"
cat > "$flake2Dir"/flake.nix <<EOF
{
    outputs = { self, flake1 }: { x = flake1.x; };
}
EOF
git -C "$flake2Dir" add flake.nix
git -C "$flake2Dir" commit -m Initial

[[ $(nix eval --json "$flake2Dir#x" --override-input flake1 "$TEST_ROOT/flake1") = 123 ]]

# The git fetcher now serves a plain-path git repo as its checked-out
# commit, so this edit has to be committed to become visible to the next
# evaluation; the point of this assertion (the override picks up the new
# value) still holds.
echo 456 > "$flake1Dir"/x.nix
git -C "$flake1Dir" commit -am 456

[[ $(nix eval --json "$flake2Dir#x" --override-input flake1 "$TEST_ROOT/flake1") = 456 ]]

# There is no such thing as a dirty override any more. An override naming a
# checkout is the commit that checkout has checked out, a locked input, so
# the lock file it lands in is fully locked; a checkout with uncommitted
# changes has no commit to be, and the override is refused with the changed
# file named, before anything is locked.
echo 789 > "$flake1Dir"/x.nix
expectStderr 1 nix eval --json "$flake2Dir#x" --override-input flake1 "$TEST_ROOT/flake1" \
  > "$TEST_ROOT/dirty-override.err"
grepQuiet -F "has uncommitted changes" "$TEST_ROOT/dirty-override.err"
grepQuiet -F "x.nix" "$TEST_ROOT/dirty-override.err"
git -C "$flake1Dir" checkout -- x.nix

# The lock file is written into flake2Dir's own Git source. That write is only
# allowed as part of a commit, and the commit is also what leaves flake2Dir
# with a revision for the fetches below.
nix flake lock "$flake2Dir" --override-input flake1 "$TEST_ROOT/flake1" --commit-lock-file
[[ -e "$flake2Dir/flake.lock" ]]
[[ -z "$(git -C "$flake2Dir" status --porcelain)" ]]

# The entry the override produced is locked: it names the commit, and using
# the lock file warns about nothing.
[[ $(jq -r .nodes.flake1.locked.rev "$flake2Dir/flake.lock") = $(git -C "$flake1Dir" rev-parse HEAD) ]]
expectStderr 0 nix eval "$flake2Dir#x" |
  grepQuietInverse -F "is unlocked"

[[ $(nix eval "$flake2Dir#x") = 456 ]]
