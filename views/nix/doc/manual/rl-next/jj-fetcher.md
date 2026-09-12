---
synopsis: Flake support for Jujutsu (jj) repositories, addressed by tree id
issues: [15651]
---

Nix reads [Jujutsu](https://jj-vcs.github.io/) repositories on jj's native
object store in-process, through the `jj-tree-abi` library, and identifies a
jj input by the blake3 id of its root tree:

```nix
builtins.fetchTree { type = "jj"; url = "file:///path/to/working-copy"; }
```

A flake reference to a local path whose root has a `.jj` directory and no
`.git` is routed to this fetcher automatically. Without `rev` or `ref` the
input is the working copy: jj snapshots it and the `@` commit is the source.
jj has no dirty state, so there is nothing to commit first: a new file is
part of the source as soon as it exists, and `.gitignore` decides what is
not.

An explicit revision (64 hex characters) or bookmark can also be fetched:

```nix
builtins.fetchTree { type = "jj"; url = "file:///path/to/repo"; rev = "<commit-id>"; }
builtins.fetchTree { type = "jj"; url = "file:///path/to/repo"; ref = "<bookmark>"; }
```

The result carries `treeHash` (an SRI blake3 hash of the root tree) and
neither `narHash` nor `revCount`. jj's index stores a generation number (the
longest-path distance from the root commit) rather than a count of
ancestors; the two agree only on a linear history, so the fetcher emits no
`revCount` at all instead of publishing one number under the other's name.
A `jj` input carrying a `revCount` attribute is rejected. `treeHash` is the lock identity for `jj` inputs: the store path
derives from it with no file reads, a metadata-only rewrite of the commit
(`jj describe`, `jj new`) keeps the store path, and a lock file is verified
by comparing ids. A lock that carries a `narHash` for a `jj` input is
rejected; regenerate it with `nix flake update`.

The fetcher never runs the `jj` command and never materialises a tree
outside the store. It refuses:

- a revision with unresolved conflicts (no conflict markers reach a build);
- a git-backed jj repository, with a message naming `git+file` (a colocated
  checkout is already routed there).

The `jj-export-dir` setting is gone with the export it configured.
