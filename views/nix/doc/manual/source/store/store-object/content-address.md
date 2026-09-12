# Content-Addressing Store Objects

Just [like][fso-ca] [File System Objects][File System Object],
[Store Objects][Store Object] can also be [content-addressed](@docroot@/glossary.md#gloss-content-address),
unless they are [input-addressed](@docroot@/glossary.md#gloss-input-addressed-store-object).

For store objects, the content address we produce will take the form of a [Store Path] rather than regular hash.
In particular, the content-addressing scheme will ensure that the digest of the store path is solely computed from the

- file system object graph (the root one and its children, if it has any)
- references
- [store directory](../store-path.md#store-directory)
- name

of the store object, and not any other information, which would not be an intrinsic property of that store object.

For the full specification of the algorithms involved, see the [specification of store path digests][sp-spec].

[File System Object]: ../file-system-object.md
[Store Object]: ../store-object.md
[Store Path]: ../store-path.md

## Content addressing each part of a store object

### File System Objects

With all currently-supported store object content-addressing methods, the file system object is always [content-addressed][fso-ca] first, and then that hash is incorporated into content address computation for the store object.

### References

#### References to other store objects

With all currently supported store object content addressing methods,
other objects are referred to by their regular (string-encoded-) [store paths][Store Path].

#### Self-references

Self-references however cannot be referred to by their path, because we are in the midst of describing how to compute that path!

> The alternative would require finding as hash function fixed point, i.e. the solution to an equation in the form
> ```
> digest = hash(..... || digest || ....)
> ```
> which is computationally infeasible.
> As far as we know, this is equivalent to finding a hash collision.

Instead we have a "has self-reference" boolean, which ends up affecting the digest:
In all currently-supported store object content-addressing methods, when hashing the file system object data, any occurrence of store object's own store path in the digested data is replaced with a [sentinel value](https://en.wikipedia.org/wiki/Sentinel_value).
The hashes of these modified input streams are used instead.

When validating the content address of a store object after the fact, the above process works as written.
However, when first creating the store object we don't know the store object's store path, as explained just above.
We therefore, strictly speaking, do not know what value we will be replacing with the sentinel value in the inputs to hash functions.
What instead happens is that the provisional store object --- the data from which we wish to create a store object --- is paired with a provisional "scratch" store path (that presumably was chosen when the data was created).
That provisional store path is instead what is replaced with the sentinel value, rather than the final store object which we do not yet know.

> **Design note**
>
> It is an informal property of content-addressed store objects that the choice of provisional store path should not matter.
> In other words, if a provisional store object is prepared in the same way except for the choice of provision store path, the provisional data need not be identical.
> But, after the sentinel value is substituted in place of each provisional store object's provision store path, the final so-normalized data *should* be identical.
>
> If, conversely, the data after this normalization process is still different, we'll compute a different content-address.
> The method of preparing the provisional self-referenced data has *failed* to be deterministic in the sense of not *leaking* the choice of provisional store path --- a choice which is supposed to be arbitrary --- into the final store object.
>
> This property is informal because at this stage, we are just described store objects, which have no formal notion of their origin.
> Without such a formal notion, there is nothing to formally accuse of being insufficiently deterministic.
> Where we cover [derivations](@docroot@/store/derivation/index.md), we will have a chance to make this a formal property, not of content-addressed store objects themselves, but of derivations that *produce* content-addressed store objects.

### Name and Store Directory

These two items affect the digest in a way that is standard for store path digest computations and not specific to content-addressing.
Consult the [specification of store path digests][sp-spec] for further details.

## Content addressing Methods

For historical reasons, we don't support all features in all combinations.
Each currently supported method of content addressing chooses a single method of file system object hashing, and may offer some restrictions on references.
The names and store directories are unrestricted however.

### Flat { #method-flat }

This uses the corresponding [Flat](../file-system-object/content-address.md#serial-flat) method of file system object content addressing.

References are not supported: store objects with flat hashing *and* references can not be created.

### Text { #method-text }

This also uses the corresponding [Flat](../file-system-object/content-address.md#serial-flat) method of file system object content addressing.

References to other store objects are supported, but self-references are not.

This is the only store-object content-addressing method that is not named identically with a corresponding file system object method.
It is somewhat obscure, mainly used for "drv files"
(derivations serialized as store objects in their ["ATerm" file format](@docroot@/protocols/derivation-aterm.md)).
Prefer another method if possible.

### Nix Archive { #method-nix-archive }

This uses the corresponding [Nix Archive](../file-system-object/content-address.md#serial-nix-archive) method of file system object content addressing.

References (to other store objects and self-references alike) are supported so long as the hash algorithm is SHA-256, but not (neither kind) otherwise.

### Git { #method-git }

> **Warning**
>
> This method is part of the [`git-hashing`][xp-feature-git-hashing] experimental feature.

This uses the corresponding [Git](../file-system-object/content-address.md#git) method of file system object content addressing.

References are not supported.

Only SHA-1 is supported at this time.
If [SHA-256-based Git](https://git-scm.com/docs/hash-function-transition)
becomes more widespread, this restriction will be revisited.

### Jujutsu tree { #method-jj-tree }

This uses the corresponding [Jujutsu tree](../file-system-object/content-address.md#jj-tree) method of file system object content addressing: the hash is the BLAKE3 tree id that jj's native object store assigned the root directory.

References are not supported.

Only BLAKE3 is supported, because that is the algorithm jj's object store uses; the method is not gated on the `blake3-hashes` experimental feature, since the algorithm is the method's and not a choice the user makes.

Nix never computes this hash. A store object with this address is created only by a fetcher that read the tree out of a jj object store and knows its id (`SourceAccessor::knownTreeRoot`), which is what lets the store path of a `jj` flake input be derived without reading a single file. The store still records and verifies the NAR hash of the bytes it wrote.

A filtered or partial view of such a tree is addressed the same way. `builtins.path` and `builtins.filterSource` on a directory served from a jj object store, and a path naming a subdirectory of one, are answered by the id of the tree the filter leaves: a Merkle operation on tree objects that reads no file (`SourceAccessor::getFilteredTree`), after which the object is mounted lazily at its store path exactly as a flake input is and written only when a consumer forces it (a build input, an evaluation result carrying its context, `nix flake archive`). The id depends on the kept content alone, so an edit outside the kept files leaves the store path, and its validity, untouched. Two things keep the NAR road: a `sha256` argument, which names a NAR hash that only a NAR walk can check, and a source string carrying store references. One visible difference between the roads: a directory the filter empties is dropped, since a jj tree has no empty directories, where a NAR copy keeps it.

#### Trust { #method-jj-tree-trust }

A `jj-tree` store object is content-addressed but not *self-certifying*. For every other content-addressing method the address is a function of the bytes, so a store can check any object it is handed against the address it claims and no further trust is needed. A Jujutsu tree id is minted by jj's object store, and Nix has no way to recompute it from the files: the address says which object this should be, not that these bytes are it. The store therefore treats such an object exactly like an [input-addressed](@docroot@/glossary.md#gloss-input-addressed-store-object) one for trust purposes:

- Registering it needs a trusted registrant or a signature. A local store adds it directly (the process reading the jj object store vouches). Through the daemon, the write is an `AddToStoreNar` with signature checking disabled, which the daemon honours only for a user in [`trusted-users`](@docroot@/command-ref/conf-file.md#conf-trusted-users); for anyone else the check is forced back on, the object counts zero signatures, and the write is refused with a trace naming `trusted-users` and signing as the ways out. Evaluation is unaffected: a `jj` input is mounted lazily and only a forced materialization (a build reading the tree, `nix flake archive`) writes to the store.
- Copying it between stores (`nix copy`, `nix flake archive --to`) needs `--no-check-sigs` at the destination or a signature by a key in [`trusted-public-keys`](@docroot@/command-ref/conf-file.md#conf-trusted-public-keys).
- `nix store verify` counts its signatures as it does for input-addressed paths, and `builtins.fetchClosure` accepts it only with `inputAddressed = true`.

A registered object is a voucher: a `treeHash`-locked `jj` input whose repository is absent from the host is served from the store when the object is valid there and its recorded content address names the locked id. When the repository exists it is always the source, because a store object is a flattened copy with no subtree objects: a relative `path:./sub` input of a flake served from the store is refused, naming the repository, rather than addressed as a subpath of the parent.

[fso-ca]: ../file-system-object/content-address.md
[sp-spec]: @docroot@/protocols/store-path.md
[xp-feature-git-hashing]: @docroot@/development/experimental-features.md#xp-feature-git-hashing
