#!/usr/bin/env bash

# Test circular flake dependencies.
source ./common.sh

requireGit

flakeA=$TEST_ROOT/flakeA
flakeB=$TEST_ROOT/flakeB

createGitRepo "$flakeA"
createGitRepo "$flakeB"

cat > "$flakeA"/flake.nix <<EOF
{
  inputs.b.url = "git+file://$flakeB";
  inputs.b.inputs.a.follows = "/";

  outputs = { self, b }: {
    foo = 123 + b.bar;
    xyzzy = 1000;
  };
}
EOF

git -C "$flakeA" add flake.nix
git -C "$flakeA" commit -m 'Foo'

cat > "$flakeB"/flake.nix <<EOF
{
  inputs.a.url = "git+file://$flakeA";

  outputs = { self, a }: {
    bar = 456 + a.xyzzy;
  };
}
EOF

git -C "$flakeB" add flake.nix
git -C "$flakeB" commit -a -m 'Foo'

# flakeA has a real "b" input, so it needs a lock file, and writing one into a
# Git source is only allowed as part of a commit. Produce it once up front so
# the evaluations below read a lock that is already current and never have to
# write into flakeA themselves.
nix flake lock "$flakeA" --commit-lock-file
[[ -z "$(git -C "$flakeA" status --porcelain)" ]]

[[ $(nix eval "$flakeA#foo") = 1579 ]]
[[ $(nix eval "$flakeA#foo") = 1579 ]]

sed -i "$flakeB"/flake.nix -e 's/456/789/'
git -C "$flakeB" commit -a -m 'Foo'

# Re-locking "b" rewrites flakeA's lock file, so it needs the commit flag too;
# the commit is the whole change, which is what keeps flakeA fetchable below.
nix flake update b --flake "$flakeA" --commit-lock-file
[[ -z "$(git -C "$flakeA" status --porcelain)" ]]
[[ $(nix eval "$flakeA#foo") = 1912 ]]

# Test list-inputs with circular dependencies
nix flake metadata "$flakeA"

