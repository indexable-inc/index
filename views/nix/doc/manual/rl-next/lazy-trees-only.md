---
synopsis: Flake inputs are always mounted lazily; mutable directories are refused
---

The `lazy-trees` setting is gone and lazy mounting is the only mode: an
input's tree is mounted at its store path inside the evaluator and copied
into the store only when something forces it (instantiating a derivation
that references the path, `builtins.storePath`, import-from-derivation).

That is sound only for a tree that cannot change during the evaluation. An
input backed by a content-addressed object (a jj tree, a git commit, a store
path) qualifies. A bare directory on the filesystem does not, so such an
input is now refused with an error naming the directory, instead of being
copied eagerly or snapshotted. Put the directory in a Jujutsu repository and
reference it as `jj+file://`; a git checkout is read through `git+file://`,
which serves the commit it has checked out.

An input read out of jj's native object store is content-addressed by its
tree id (`jj-tree`) and the store path costs no file reads. Every other
input, git included, is NAR-addressed: a git input is locked by `narHash`,
and that is its one identity on every road, with or without the
`git-hashing` experimental feature (git tree ids are no longer announced for
mounting). A relative `path:./sub` input written with a `narHash` inside a
jj-backed flake is refused, naming the attribute to remove from `flake.nix`:
the subtree is addressed by its tree id and never NAR-ingested.
