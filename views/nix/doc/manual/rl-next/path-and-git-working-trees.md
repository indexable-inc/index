---
synopsis: "`path:` and Git working trees: sources must have an identity"
---

A fetched source must be something a lock file can name. A directory on the
filesystem is not: it changes while an evaluation reads it, so two reads of
one flake reference can see two different trees, and nothing that ends up in
the lock file says which one was used. Nix used to hide that by copying the
whole directory into the store on every evaluation, which cost a full read of
the tree per evaluation and did not actually close the race.

Both fetchers that served such a directory now refuse it instead.

- A `path:` input must be a store path. Any other absolute path is an error
  naming the two ways to give the directory an identity (`jj+file://` after
  `jj init`, or `git+file://` plus a commit). A store path is served in place,
  under its own name: it is already content-addressed, so nothing is copied.
  Relative `path:./sub` inputs are unaffected -- they resolve inside the
  flake that declares them and are locked by its identity.

- A `git+file://` input, or a bare path in a Git repository, is now the commit
  that repository has checked out, fetched by exactly the road an explicit
  `?rev=` takes. A bare checkout and `?rev=<HEAD>` therefore produce one store
  path rather than two, and the input is locked. A working tree with
  uncommitted changes to tracked files has no such commit and is refused, with
  the differing files listed. Untracked files are not changes: they belong to
  no commit, so they neither block a fetch nor appear in the result, exactly
  as before.

Searching upward from a subdirectory for a `flake.nix` now stops at a
Jujutsu workspace as well as at a Git checkout. It stopped only at `.git`
before, which meant a jj workspace with no `flake.nix` of its own was
silently captured by whatever flake happened to live above it, even though
the step that picks a fetcher had understood `.jj` all along.

Consequently the `dirtyRev` and `dirtyShortRev` attributes, and the
`dirtyRevision` field of `nix flake metadata --json`, are gone: there is no
dirty tree left to describe. The `allow-raw-repo-paths` setting is gone too,
since a raw `path:` fetch of a repository is no longer expressible at all.
