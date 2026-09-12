#!/usr/bin/env bash

source common.sh

requireGit

TODO_NixOS

# Submodules can't be fetched locally by default (see fetchGitSubmodules.sh).
export GIT_CONFIG_COUNT=1
export GIT_CONFIG_KEY_0=protocol.file.allow
export GIT_CONFIG_VALUE_0=always

subRepo=$TEST_ROOT/lastmodified-sub
rootRepo=$TEST_ROOT/lastmodified-root

# Distinct deterministic commit times so the assertions cannot pass by accident.
subTime=1700000000
rootTime=1710000000

createGitRepo "$subRepo"
cat > "$subRepo"/flake.nix <<EOF
{
  outputs = { self }: { x = 1; };
}
EOF
git -C "$subRepo" add flake.nix
GIT_AUTHOR_DATE="$subTime +0000" GIT_COMMITTER_DATE="$subTime +0000" \
  git -C "$subRepo" commit -m sub

createGitRepo "$rootRepo"
git -C "$rootRepo" submodule add "$subRepo" sub
mkdir "$rootRepo"/plain
cat > "$rootRepo"/plain/flake.nix <<EOF
{
  outputs = { self }: { x = 2; };
}
EOF
cat > "$rootRepo"/flake.nix <<EOF
{
  inputs.sub.url = "path:./sub";
  inputs.plain.url = "path:./plain";
  outputs = { self, sub, plain }: {
    subLastModified = sub.lastModified or "missing";
    plainLastModified = plain.lastModified or "missing";
    subRev = sub.rev or "missing";
    plainRev = plain.rev or "missing";
  };
}
EOF
git -C "$rootRepo" add flake.nix plain/flake.nix
GIT_AUTHOR_DATE="$rootTime +0000" GIT_COMMITTER_DATE="$rootTime +0000" \
  git -C "$rootRepo" commit -m root

flakeref="git+file://$rootRepo?submodules=1"

# Evaluate without writing the lock so the tree stays clean (this test
# environment rejects dirty, uncacheable trees).

# A relative path input that is a submodule reports the submodule's own
# commit time, not the parent's.
[[ $(nix eval --no-write-lock-file --json "$flakeref#subLastModified") = "$subTime" ]]

# A plain subdirectory is deliberately not stamped: its time equals the
# parent's (derivable there), and stamping it would churn the lock on
# every parent commit.
[[ $(nix eval --no-write-lock-file --json "$flakeref#plainLastModified") = '"missing"' ]]

# A submodule-backed input also reports the gitlink commit; a plain
# subdirectory has no rev of its own (the parent's rev does not pin the
# subdirectory alone).
subRev=$(git -C "$subRepo" rev-parse HEAD)
[[ $(nix eval --no-write-lock-file --raw "$flakeref#subRev") = "$subRev" ]]
[[ $(nix eval --no-write-lock-file --raw "$flakeref#plainRev") = missing ]]

# The stamped values are recorded in the lock file. A git source takes its
# lock file only as a commit (flakes/lock-file-writes.sh), so the commit that
# used to follow the write is the write.
GIT_AUTHOR_DATE="$rootTime +0000" GIT_COMMITTER_DATE="$rootTime +0000" \
  nix flake lock "$flakeref" --commit-lock-file
[[ $(jq -r '.nodes.sub.locked.lastModified' "$rootRepo"/flake.lock) = "$subTime" ]]
[[ $(jq -r '.nodes.plain.locked.lastModified' "$rootRepo"/flake.lock) = null ]]
[[ $(jq -r '.nodes.sub.locked.rev' "$rootRepo"/flake.lock) = "$subRev" ]]
[[ -z "$(git -C "$rootRepo" status --porcelain)" ]]

# The enriched lock must round-trip: a fresh evaluation of the committed
# lock still works and reproduces the same values.
[[ $(nix eval --json "$flakeref#subLastModified") = "$subTime" ]]
