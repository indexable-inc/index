---
synopsis: "`builtins.path` on a Jujutsu-backed tree is addressed by the filtered tree's id"
---

`builtins.path`, `builtins.filterSource` and a path naming a subdirectory
of a tree served from a Jujutsu object store (a `jj` flake input, including
a flake's own `./.`) no longer copy the tree into the store to find its
hash. The filter's verdicts are applied to the tree objects themselves,
reading no file, and the result is addressed by the id of the tree that
remains, then mounted lazily at that store path as a flake input is and
written only when a consumer forces it: a build input, an evaluation
result carrying its context, `nix flake archive`. A subtree the filter leaves intact
keeps its own id, so the id, and the store path, depend on the kept files
alone: an edit anywhere else in the source leaves a filtered copy, and its
validity in the store, untouched.

The store paths of such copies change once, since they are now
`jj-tree`-addressed. A `sha256` argument still names a NAR hash and keeps
the NAR road, as does a source string carrying store references. A
directory the filter empties is dropped from the result, where the NAR
road kept it empty: a jj tree has no empty directories.
