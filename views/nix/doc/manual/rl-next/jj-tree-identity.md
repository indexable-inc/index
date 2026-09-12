---
synopsis: "`jj` inputs are addressed by their Jujutsu tree id"
---

A new content-addressing method, `jj-tree`, addresses a store object by the
BLAKE3 tree id that Jujutsu's native object store already assigned its root
directory. Nix never computes the id: a fetcher that reads a tree out of a jj
store announces it, and the store path follows from it with no file reads,
where the NAR method re-read the whole tree on every edit.

Lock files record `jj` inputs with `treeHash` (SRI, `blake3-...`) in place of
`narHash`; a `narHash` on a `jj` input is rejected. The `treeHash` attribute
name is now reserved for this meaning and is no longer accepted on forge
(`github`/`gitlab`/`sourcehut`) inputs, where it was never written.

A relative `path:./sub` input inside a jj-backed flake evaluates as its own
store object, addressed by the subtree's id, which equals the id the same
content has at the root of any other repository. Its lock entry stays the
literal path plus `parent`, with no hash of its own.

A `jj-tree` store object is content-addressed but not self-certifying: Nix
cannot recompute the id from the files, so the address certifies nothing by
itself, and registering the object needs a trusted user or a signature, the
same rule as for an input-addressed path. In practice: a `jj` input is only
ever materialized in the store when something forces it (a build that reads
the tree, `nix flake archive`), and that write goes through the daemon as
`AddToStoreNar`, which admits an unsigned, non-self-certifying object only
from a user in `trusted-users`. A user outside `trusted-users` can evaluate
jj inputs freely and fails at the first forced materialization, with a trace
naming `trusted-users` and signing as the ways out. `nix copy` and `nix flake
archive --to` between stores need `--no-check-sigs` or a signature by a key
in `trusted-public-keys`; `nix store verify` and `builtins.fetchClosure`
(`inputAddressed = true`) treat the object like an input-addressed path.

Deployment precondition, measured on the reference host (hydra, 2026-08-30):
`trusted-users = @admin root`, and the user running `home-manager switch` is
in `admin`, so forced materializations there pass.

A `treeHash` lock whose repository is absent from the host is served from
the local store when the store holds a registered object under the locked id
(copied there with `nix copy`, or forced earlier): the registration is the
voucher, and the object's `ca` must name the locked id. The lookup is local;
substituters are not consulted. The repository always wins when it exists,
because a store object is a flattened copy that cannot name its subtrees: a
relative `path:./sub` input of a flake served from the store is refused,
naming the repository, rather than composed as a subpath of the parent
(which would be a second identity for the same directory). An input is
locked only with both `rev` and `treeHash`; the id alone is a content check
of an unlocked input, as `narHash` alone is for a `path` input.
