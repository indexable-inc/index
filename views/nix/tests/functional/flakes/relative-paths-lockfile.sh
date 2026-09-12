#!/usr/bin/env bash

source ./common.sh

requireGit

unset _NIX_TEST_BARF_ON_UNCACHEABLE

# Test a "vendored" subflake dependency. This is a relative path flake
# which doesn't reference the root flake and has its own lock file.
#
# This might occur in a monorepo for example. The root flake.lock is
# populated from the dependency's flake.lock.

rootFlake="$TEST_ROOT/flake1"
subflake="$rootFlake/sub"
depFlakeA="$TEST_ROOT/depFlakeA"
depFlakeB="$TEST_ROOT/depFlakeB"

rm -rf "$rootFlake"
jjFlakeDir "$rootFlake"
jjFlakeDir "$depFlakeA"
jjFlakeDir "$depFlakeB"

# `sub` is only ever reached through its parent: as `./sub` from the root
# flake, and as a subdirectory of the root workspace on the command line.
# It is part of that workspace's tree, so it gets no workspace of its own.
mkdir -p "$subflake"

cat > "$depFlakeA/flake.nix" <<EOF
{
  outputs = { self }: {
    x = 11;
  };
}
EOF

cat > "$depFlakeB/flake.nix" <<EOF
{
  outputs = { self }: {
    x = 13;
  };
}
EOF

[[ $(nix eval "$depFlakeA#x") = 11 ]]
[[ $(nix eval "$depFlakeB#x") = 13 ]]

cat > "$subflake/flake.nix" <<EOF
{
  inputs.dep.url = "jj+file://$depFlakeA";
  outputs = { self, dep }: {
    inherit (dep) x;
    y = self.x - 1;
  };
}
EOF

cat > "$rootFlake/flake.nix" <<EOF
{
  inputs.sub.url = ./sub;
  outputs = { self, sub }: {
    x = 2;
    y = sub.y / self.x;
  };
}
EOF

[[ $(nix eval "$subflake#y") = 10 ]]
[[ $(nix eval "$rootFlake#y") = 5 ]]

# The subflake is addressed bare: it has no identity of its own, so
# flakeref.cc resolves it through the root workspace that contains it.
nix flake update --flake "$subflake" --override-input dep "$depFlakeB"

[[ $(nix eval "$subflake#y") = 12 ]]

# Changes to sub/flake.lock are propagated to the root flake (#7730):
# the child's own lock file is authoritative for the subtree of a
# relative path input. A read-only evaluation must already see the new
# pin without modifying the parent's lock on disk.
cp "$rootFlake/flake.lock" "$TEST_ROOT/lock-before"
[[ $(nix eval --no-write-lock-file "$rootFlake#y") = 6 ]]
cmp "$TEST_ROOT/lock-before" "$rootFlake/flake.lock"

# A lock-writing evaluation refreshes the stale copied nodes in the
# parent's lock, with no manual `nix flake update` needed.
[[ $(nix eval "$rootFlake#y") = 6 ]]

# With the child unchanged, further evaluations must leave the parent
# lock byte-identical (no churn on every operation).
cp "$rootFlake/flake.lock" "$TEST_ROOT/lock-stable"
[[ $(nix eval "$rootFlake#y") = 6 ]]
cmp "$TEST_ROOT/lock-stable" "$rootFlake/flake.lock"

# `nix flake update` on an already in-sync lock is a no-op.
nix flake update --flake "$rootFlake"
cmp "$TEST_ROOT/lock-stable" "$rootFlake/flake.lock"
[[ $(nix eval "$rootFlake#y") = 6 ]]

# A NEW input added to the child's flake.nix and flake.lock must become
# visible to the parent without a manual update of the parent's lock.
# Previously this failed with "function 'outputs' called without
# required argument 'dep2'" (indexable-inc/index#3627).
cat > "$subflake/flake.nix" <<EOF
{
  inputs.dep.url = "jj+file://$depFlakeA";
  inputs.dep2.url = "jj+file://$depFlakeB";
  outputs = { self, dep, dep2 }: {
    y = dep.x + dep2.x;
  };
}
EOF
nix flake lock "$subflake"
[[ $(nix eval "$rootFlake#y") = 13 ]]

# And the refreshed parent lock is again stable.
cp "$rootFlake/flake.lock" "$TEST_ROOT/lock-stable2"
[[ $(nix eval "$rootFlake#y") = 13 ]]
cmp "$TEST_ROOT/lock-stable2" "$rootFlake/flake.lock"
