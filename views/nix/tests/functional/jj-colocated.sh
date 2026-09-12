#!/usr/bin/env bash

# Git is the boundary bridge. A jj repository on the Git backend, colocated
# (`jj git init --colocate`, so both `.jj` and `.git`) or not, is never read
# by the jj fetcher: the fetcher reads jj's native object store only, and
# refuses a git-backed repository with a message naming `git+file`. A
# colocated checkout is routed to `git+file` by flakeref.cc, where the
# checkout is pinned to the commit it has checked out: a clean working tree
# is served as HEAD's tree out of the Git object store, at the store path the
# explicit `?rev=` spelling gets, and a dirty one is refused, because a
# working tree is not a source. Under `git-hashing` too, a git input is
# NAR-addressed: its lock carries `narHash`, and that is its one identity.
# This file pins those answers so that the routing stays a deliberate
# property rather than a surprise.

source common.sh

TODO_NixOS

requireJj
requireGit

# `jj`, `jjConfig` and `jjInit` come from common/functions.sh: one fixture
# for every jj test.
jjConfig

repo=$TEST_ROOT/colocated
mkdir -p "$repo/sub"
cat > "$repo/flake.nix" <<EOF
{ outputs = _: { probe = "probe"; }; }
EOF
echo alpha > "$repo/a.txt"
echo beta > "$repo/sub/b.txt"
ln -s a.txt "$repo/link"

initGitRepo "$repo" "-q -b main"
git -C "$repo" add -A
git -C "$repo" commit -qm init
jj git init --colocate "$repo" >/dev/null
[[ -e $repo/.jj && -e $repo/.git ]] || fail "test setup: the fixture is not colocated"
headRev=$(git -C "$repo" rev-parse HEAD)

# 1. Explicit `jj+file` on a git-backed repository is refused, naming the
#    scheme that reads it. Both addressing modes, because `?rev=` is parsed
#    before any repository is opened and refuses on the 40-character id.
expectStderr 1 nix flake metadata "jj+file://$repo" | grepQuiet "git+file"
expectStderr 1 nix flake metadata "jj+file://$repo?rev=$headRev" | grepQuiet "git+file"

# 2. The bare path is routed to `git+file` (the `.git` branch in flakeref.cc
#    takes precedence over `.jj`), which pins the checked-out commit. Clean:
#    HEAD's tree, and one store path for the three spellings.
revPath=$(nix flake prefetch --json "git+file://$repo?rev=$headRev" | jq -r '.storePath')
[[ $(nix flake prefetch --json "$repo" | jq -r '.storePath') = "$revPath" ]]
[[ $(nix flake prefetch --json "git+file://$repo" | jq -r '.storePath') = "$revPath" ]]
[[ $(nix flake metadata --json "$repo" | jq -r '.locked.rev') = "$headRev" ]]
#    Dirty: refused with the changed file named; nothing is copied.
echo dirty >> "$repo/a.txt"
expectStderr 1 nix flake metadata "$repo" | grepQuiet "has uncommitted changes"
expectStderr 1 nix flake metadata "git+file://$repo" | grepQuiet "a.txt"
git -C "$repo" checkout -q -- a.txt

# 3. A pinned revision reads out of the Git object store, which has no
#    physical path and is immutable: the bridge that keeps git checkouts
#    usable. Denominator: the tree that arrives is the committed one.
[[ $(cat "$revPath/a.txt") = alpha ]]
[[ $(cat "$revPath/sub/b.txt") = beta ]]
[[ -L $revPath/link && $(readlink "$revPath/link") = a.txt ]]
[[ ! -e $revPath/.git && ! -e $revPath/.jj ]]
[[ $(nix eval --raw "git+file://$repo?rev=$headRev#probe") = probe ]]

# 4. A git input is NAR-addressed under `git-hashing` as well. Its lock
#    carries `narHash`, and a mount that addressed the input by git tree id
#    could not check that hash: it refused every locked git input with a hint
#    (`nix flake update`) that wrote the same `narHash` back. One identity:
#    the NAR hash, on every road, feature or not.
consumer=$TEST_ROOT/git-consumer
jjFlakeDir "$consumer"
cat > "$consumer/flake.nix" <<EOF
{
  inputs.dep.url = "git+file://$repo?rev=$headRev";
  outputs = { self, dep }: { probe = dep.probe; depPath = dep.outPath; };
}
EOF
nix flake lock "$consumer"
[[ $(jq -r .nodes.dep.locked.narHash "$consumer/flake.lock") == sha256-* ]]
[[ $(nix eval --raw "$consumer#probe") = probe ]]
[[ $(nix eval --raw --extra-experimental-features git-hashing "$consumer#probe") = probe ]]
[[ $(nix eval --raw --extra-experimental-features git-hashing "$consumer#depPath") = "$revPath" ]]

# A non-colocated git-backed jj repository (`jj git init`, `.jj` only) is
# routed to the jj fetcher by its `.jj`, and refused there for the same
# reason: the error names `git+file`, since colocating (a `.git` beside the
# `.jj`) is what makes a Git-backed repository readable to Nix's git
# fetcher.
plain=$TEST_ROOT/git-backed-plain
# `--no-colocate`: `jj git init` colocates by default now, and a colocated
# repository has a `.git` at its root, which is a different fixture (the
# routing case above). The premise here is `.jj` with the git store hidden
# under it.
jj git init --no-colocate "$plain" >/dev/null
echo x > "$plain/f"
cat > "$plain/flake.nix" <<EOF
{ outputs = _: { probe = "probe"; }; }
EOF
[[ -e $plain/.jj && ! -e $plain/.git ]] || fail "test setup: expected .jj without .git"
expectStderr 1 nix flake metadata "$plain" | grepQuiet "git+file"
