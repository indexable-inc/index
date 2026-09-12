#!/usr/bin/env bash

source ./common.sh

requireGit

templatesDir=$TEST_ROOT/templates
flakeDir=$TEST_ROOT/flake
nixpkgsDir=$TEST_ROOT/nixpkgs

nix registry add --registry "$registry" templates "git+file://$templatesDir"
nix registry add --registry "$registry" nixpkgs "git+file://$nixpkgsDir"

createGitRepo "$nixpkgsDir"
createSimpleGitFlake "$nixpkgsDir"

# Test 'nix flake init'.
createGitRepo "$templatesDir"

cat > "$templatesDir"/flake.nix <<EOF
{
  description = "Some templates";

  outputs = { self }: {
    templates = rec {
      trivial = {
        path = ./trivial;
        description = "A trivial flake";
        welcomeText = ''
            Welcome to my trivial flake
        '';
      };
      default = trivial;
    };
  };
}
EOF

mkdir "$templatesDir/trivial"

cat > "$templatesDir"/trivial/flake.nix <<EOF
{
  description = "A flake for building Hello World";

  outputs = { self, nixpkgs }: {
    packages.$system = rec {
      hello = nixpkgs.legacyPackages.$system.hello;
      default = hello;
    };
  };
}
EOF
echo a > "$templatesDir/trivial/a"
echo b > "$templatesDir/trivial/b"

git -C "$templatesDir" add flake.nix trivial/
git -C "$templatesDir" commit -m 'Initial'

nix flake check templates
nix flake show templates
nix flake show templates --json | jq

createGitRepo "$flakeDir"
(cd "$flakeDir" && nix flake init)
(cd "$flakeDir" && nix flake init) # check idempotence
git -C "$flakeDir" add flake.nix
# `nix flake init` already staged the template's other files (a, b) via
# `git add --intent-to-add`; commit everything now so the repo has a
# revision before it is fetched as a flake below.
git -C "$flakeDir" commit -a -m 'Initial'

# The generated flake.nix has a real "nixpkgs" input, so it needs a lock file,
# and writing one into a Git source is only allowed as part of a commit. Lock
# it once here: the three commands below then read a lock that is already
# current, so each of them leaves the repo with a revision to be fetched by.
nix flake lock "$flakeDir" --commit-lock-file
[[ -e "$flakeDir/flake.lock" ]]
[[ -z "$(git -C "$flakeDir" status --porcelain)" ]]

nix flake check "$flakeDir"
nix flake show "$flakeDir"
nix flake show "$flakeDir" --json | jq

# Test 'nix flake init' with benign conflicts
createGitRepo "$flakeDir"
echo a > "$flakeDir/a"
(cd "$flakeDir" && nix flake init) # check idempotence

# Test 'nix flake init' with conflicts
createGitRepo "$flakeDir"
echo b > "$flakeDir/a"
pushd "$flakeDir"
(! nix flake init) |& grep "refusing to overwrite existing file \"$flakeDir/a\""
popd
git -C "$flakeDir" commit -a -m 'Changed'

# Test 'nix flake new'.
# `nix flake new` writes the template into the directory but does not give the
# directory an identity: it only runs `git add` when a `.git` already exists.
# A plain directory is not a flake source, so create the destination as a jj
# workspace first. That also covers the other half of the lock file rule: the
# generated flake's "nixpkgs" input still needs a lock file, and writing one
# into a jj source needs no flag, because the snapshot taken at the next fetch
# is the commit.
rm -rf "$flakeDir"
jjFlakeDir "$flakeDir"
nix flake new -t templates#trivial "$flakeDir"
nix flake new -t templates#trivial "$flakeDir" # check idempotence
nix flake check "$flakeDir"
[[ -e "$flakeDir/flake.lock" ]]
