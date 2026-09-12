#!/usr/bin/env bash

# Where a flake's lock file is allowed to be written, now that a source has to
# have an identity before it can be fetched at all.
#
# Writing flake.lock into a flake's own source mutates that source. When the
# source is identified by a commit, nothing describes what is on disk once the
# write lands: the fetcher refuses the tree (git.cc, pinCheckedOutCommit), and
# libflake re-reads the flake immediately after writing, so the write would
# destroy the very evaluation that asked for it. libflake therefore refuses up
# front unless the write is going to be committed.
#
# A jj workspace re-derives its identity from whatever is on disk at the next
# fetch, so there is nothing to refuse: the same update needs no flag, and the
# re-read sees the snapshot that includes the new lock file.

source ./common.sh

TODO_NixOS

requireGit
requireJj
jjConfig

# The dependency both flakes below lock against, so that a lock file is
# actually required rather than trivially empty.
createFlake1

###############################################################################
# A git source: refused without --commit-lock-file.
###############################################################################

gitFlake=$TEST_ROOT/lock-writes-git
createGitRepo "$gitFlake" "--initial-branch=main"

cat > "$gitFlake/flake.nix" <<EOF
{
  inputs.flake1.url = "git+file://$flake1Dir";
  outputs = inputs: {
    packages.$system.default = inputs.flake1.packages.$system.foo;
  };
}
EOF

git -C "$gitFlake" add flake.nix
git -C "$gitFlake" commit -m 'Add flake'

[[ ! -e "$gitFlake/flake.lock" ]]

# The refusal names all three ways forward, because a user who hits it has to
# pick one of them and the error is the only place that says so.
expectStderr 1 nix flake lock "$gitFlake" > "$TEST_ROOT/refusal.txt"
grepQuiet -- "--commit-lock-file" < "$TEST_ROOT/refusal.txt"
grepQuiet -- "--no-write-lock-file" < "$TEST_ROOT/refusal.txt"
grepQuiet -- "jj workspace" < "$TEST_ROOT/refusal.txt"

# Fail closed: a refused write leaves no lock file behind and no change in the
# working tree. Without this the refusal could still have dirtied the source,
# which is the state it exists to prevent.
[[ ! -e "$gitFlake/flake.lock" ]]
[[ -z "$(git -C "$gitFlake" status --porcelain)" ]]

# With the flag the update goes through, as exactly one commit.
commitsBefore=$(git -C "$gitFlake" rev-list --count HEAD)
nix flake lock "$gitFlake" --commit-lock-file
[[ -e "$gitFlake/flake.lock" ]]
commitsAfter=$(git -C "$gitFlake" rev-list --count HEAD)
[[ $commitsAfter -eq $((commitsBefore + 1)) ]]
# The commit is the whole change: nothing is left uncommitted, so the source
# still has a revision to be fetched by.
[[ -z "$(git -C "$gitFlake" status --porcelain)" ]]

# Evaluation with the lock file in place needs no flag at all, because nothing
# has to be written.
nix eval --raw "git+file://$gitFlake#packages.$system.default.outPath" > /dev/null

# --no-write-lock-file, the second remedy the error names: evaluate without
# writing anything into the source.
git -C "$gitFlake" rm --quiet :/:flake.lock
git -C "$gitFlake" commit --quiet -m 'Remove flake.lock'
[[ ! -e "$gitFlake/flake.lock" ]]
nix eval --raw --no-write-lock-file "git+file://$gitFlake#packages.$system.default.outPath" > /dev/null
[[ ! -e "$gitFlake/flake.lock" ]]
[[ -z "$(git -C "$gitFlake" status --porcelain)" ]]

###############################################################################
# A jj source: the same update, no flag.
###############################################################################

jjFlake=$TEST_ROOT/lock-writes-jj
jjFlakeDir "$jjFlake"

cat > "$jjFlake/flake.nix" <<EOF
{
  inputs.flake1.url = "git+file://$flake1Dir";
  outputs = inputs: {
    packages.$system.default = inputs.flake1.packages.$system.foo;
  };
}
EOF

[[ ! -e "$jjFlake/flake.lock" ]]

jjAt() { jj --repository "$jjFlake" log --no-graph -r @ -T "$1"; }

beforeCommit=$(jjAt commit_id)
beforeChange=$(jjAt change_id)

# One command that writes the lock file into the source AND re-reads the flake
# afterwards. That re-read is what fails for a commit-identified source, so it
# is the load-bearing half of this test rather than the lock file's existence.
nix eval --raw "$jjFlake#packages.$system.default.outPath" > /dev/null

[[ -e "$jjFlake/flake.lock" ]]

afterCommit=$(jjAt commit_id)
afterChange=$(jjAt change_id)

# @ moved: the working-copy commit now contains the lock file.
[[ "$beforeCommit" != "$afterCommit" ]]
# It moved rather than being replaced: still the same change, so the write
# landed in the working copy instead of starting a new one.
[[ "$beforeChange" == "$afterChange" ]]

# It moved exactly once for one update: with the lock file already correct,
# re-running writes nothing and @ stays where it is.
nix eval --raw "$jjFlake#packages.$system.default.outPath" > /dev/null
[[ "$(jjAt commit_id)" == "$afterCommit" ]]

# A bare path to a jj workspace resolves to jj+file:// (flakeref.cc), so the
# spelling above and the explicit one are the same input.
nix eval --raw "jj+file://$jjFlake#packages.$system.default.outPath" > /dev/null
