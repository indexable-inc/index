# Rust evaluator host questions

The Rust VM performs no filesystem or store IO. A host question crosses the
C ABI and asks cppnix to do that work. On the 2026-09-01 cold home target,
host answers occupied 32.4 seconds of a 67.2-second run. This document names
the expensive work and the counters used to check it.

## Cost by question

`rustEvalPerf` reports every ask as `q.<Kind>`, and accumulated latency for
answers actually delivered to the VM as `q.<Kind>_ns`. An asynchronously begun
question that speculative evaluation no longer needs remains in `q.<Kind>` and
increments `q.<Kind>_abandoned`; it contributes no invented completion latency.
The logical counts do not fall when a host cache hits. A recording host still
records each answer the evaluation receives.

| question | pre-change asks | host work on a miss |
|---|---:|---|
| `StorePath` | 170,672 | Use the path value's recorded accessor root, dump the path as a NAR, hash it, and copy it unless the evaluator is read-only. |
| `WriteDrv` | 73,520 | Parse the ATerm, materialise its lazy input sources, and call cppnix's canonical `writeDerivation` path (temporary root, validity/repair branch, and write). |
| `Realise` | 11,573 | Validate string context and, for IFD, ask the build store to realise derivation outputs. |
| `Entries` | 139,298 | Resolve and list a directory. `dir_hits` counts asks answered by the Rust driver's directory cache. |
| `Import` | 25,069 | Resolve and read a Nix source file. Compilation happens after the answer and is counted under `compile_ns`. |
| `Kind` | 2,459 | Resolve the path and call `lstat`. |
| `FetchTree` | 22 | Parse and check the input, fetch through `inputCache`, mount it, and serialise its tree attributes. |
| `StoreText` | 2,778 | Hash text plus references and write it outside read-only mode. |

The remaining question kinds are printed even at zero. Their names come from
`rust/nix-eval-rs/src/purity.rs`, so adding a new `NeedPath` also adds a count
and latency bucket.

Slow questions (`Fetch`, `FetchTree`, `Flake`, and `Realise`) may run on host
workers while the VM advances another strand. `slow.<Kind>` increments when
`Host::begin` returns a ticket; `slow.<Kind>_ns` is begin-to-collect latency for
completed tickets. `collect_call_ns` measures the whole `Host::collect` call,
including its receive, locking/map work, and post-receive answer finishing. It
is deliberately not called blocking time. The sum of slow latencies can exceed
wall time because those latencies overlap.

## Host caches

### Source copies

The host holds no source-copy cache. `rustCopyToStore` builds
no resolver. A Rust path value is `(root, absolute spelling)`: `root` is the
ambient filesystem or the exact store-path mount point whose accessor read the
module. Relative literals take their module's root at compile time. `dirOf`
and coercions that return an existing path retain it. Interpolated paths and
`path + string` rebuild the finished spelling under the ambient root, matching
`EvalState::rootPath` in `eval.cc`. This makes a mounted `p` unequal to
`p + ""`, and lets `..` canonicalise in the ambient tree.

For an ambient root the bridge constructs `state.rootPath(path)`. An ambient
import answer stays ambient, even when its spelling is inside a mounted store
object. A mounted import answer is possible only when the caller asked with a
mounted path. For a mounted root the bridge calls `storeFS->getMount` on the
exact mount point and builds `SourcePath{mount, relativePath}`. A missing named
mount fails loudly; it never falls back to `rootPath`.

The wire decoder rejects non-absolute or non-canonical roots and accessor
paths. The C++ boundary also requires a mounted root to be one complete store
object path. It compares the original bytes with the canonical spelling before
looking up the mount, so doubled separators, `..` and trailing slashes cannot
alias another key. The mounted-lookup functional fixture also returns a
canonical key below a store object and requires the Rust boundary to reject it
as an incomplete store path.

Recorded path questions use the same `(root, accessor-relative path)` pair.
An import records the caller's original pair as its question. The resolved
path and source bytes contribute only to the answer digest. Replay therefore
re-resolves `current.nix` instead of replacing the question with yesterday's
`v1.nix` answer.

The format constants beside the codecs in `rust/nix-eval-rs/src/readset.rs`
own the answer-digest, result-key, identity, and witness versions. Root-aware
questions encode both fields under those current formats. The VM's in-memory
compile memo also includes the module root, so identical text and spelling
compiled once Ambient and once Mounted cannot share relative-path constants.

`fetchToStore` owns reuse. Its cache key contains the selected accessor's
fingerprint, ingestion method, and subpath. A lazy flake input with a
fingerprint reuses cppnix's cached content hash on later asks. An ambient or
otherwise mutable accessor has no fingerprint, so cppnix dumps it on every
ask. The host adds no rule beside that one.

`copyMounted` counts host copy operations whose path value named a mount;
`copyAmbient` counts host copy operations whose path value named the ambient
root. Both come directly from the VM's root and neither inspects the
filesystem. `q.StorePath_unique` is the number of distinct 128-bit argument
digests seen by the VM.

One evaluation memoises successful `StorePath` answers for every root
(`JobMemo::copies`, beside the directory listings and, since round 8, the
`Import` answers in `JobMemo::imports`). The key is the complete
`PathValue`, including its mount and accessor-relative path; a hit counts as
`copy_hits` (`import_hits` for imports) and `q.StorePath` keeps counting
every ask, as `q.Entries` does beside `dir_hits`. The functional test asks
for 1,000 copies each of two read-only lazy flake inputs, before and after
materialisation. Both runs retain the 2,000 logical questions and two unique
arguments, but only two cross the host boundary (`copyAmbient = 2`, the
spelling being ambient either way, `copy_hits = 1998`); two sources rather
than one because a memo that answered every argument with its first answer
would pass with one. The fixture also compares mounted concatenation,
Ambient import identity, and mounted `builtins.storePath` with cppnix.

`Realise` answers are memoised the same way (`JobMemo::realised`, keyed on
the whole context, `realise_hits` beside `q.Realise`), on both the
synchronous and the begun route: a context this evaluation has built is not
begun again. Measured before the memo, one home-manager evaluation asked
2,269 times for 48 distinct contexts and every ask cost the embedder about
2 ms (a validity check per element, a worker thread, `buildPaths`) whether
or not the outputs existed, 4.4 s in all. A failed build is not remembered.

Until round 8 ambient paths were excluded from this memo on the argument
that the ambient filesystem is mutable. That bought nothing: one evaluation
is one snapshot of the world (the read set records a question once, with
one answer, and a world that changes under a running evaluation makes the
key unreproducible -- a miss, never a wrong answer -- whether or not the
repeat reached the host), which is the assumption cppnix's parse cache and
its per-evaluation `srcToStore` already make. And it cost what a profile of
one home-manager evaluation showed: 170,644 `StorePath` asks for 780
distinct paths, every one crossing the ABI to `copyPathToStore`, which in
this fork had lost cppnix's `srcToStore` and so reached `fetchToStore` --
two daemon round trips (a temporary root and `isValidPath`) per ask, 12 s of
a 59 s wall, paid identically by the cpp arm (its wall exceeded its CPU by
the same 12 s). Round 8 restored `srcToStore` in `EvalState` for both arms
and memoises the ask in the VM so the repeat never leaves the evaluator.

`builtins.findFile` is the provenance boundary where cppnix may return a new
accessor. Ordinary path-list and pseudo-URL results use the ambient root. When
a lookup hook returns the exact child accessor mounted at one `storeFS` key,
the bridge reverse-maps that accessor and returns the mount plus its relative
path. A mounted `main.nix` can then import `./dep.nix` through the same child.
An accessor mounted at multiple keys is ambiguous and gets no guessed name.
`corepkgs` and other unnameable accessors keep the virtual ambient-file
fallback. The public `ixe_find_file_fn` comment specifies this rooted wire
format for external embedders. The `rust-eval-find-file-root.sh` functional
test installs a custom cppnix lookup hook, compares both evaluator arms, and
immediately imports the sibling dependency through that mount. Its negative
arm mounts the same accessor below a store object and requires the wire
boundary to reject that incomplete store-path key.

`PathValue::accessor_path()` borrows the existing spelling. Ambient paths,
mounted roots and mounted children are slices, so repeated host questions do
not allocate a temporary path string.

### Existing store paths and existence checks

`builtins.storePath` is a rooted host question. The bridge performs cppnix's
symlink resolution and in-store checks, then skips `ensurePath` only for a
lazy mounted store object or read-only evaluation. Its answer carries the
visible path and the complete store object used as string context. The
question is recorded because later store disappearance can invalidate a
prior success.

The seven filesystem-read hooks are supplied as one all-or-none group. Plain
`pathExists` uses ancestor resolution; the string-with-trailing-slash form is
a separate host operation that fully resolves the path and tests for a
directory. Canonicalisation has already removed the slash by the time the host
sees the rooted path, so the evaluator must select that operation while it
still has the original value.

Both existence operations have three outcomes. Missing and
`RestrictedPathError` are `false`; a vanished mount and every other exception
are errors. They are separate read-set questions, and their digests preserve
the old true and false encodings while adding a separate error encoding. For
an ambient path a witness recorded for a missing child cannot replay a
vanished mount as the same negative fact, including for
`builtins.pathExists ".../"`. Under a mounted root the row is not re-asked at
all (next section): the mount's content is pinned by its name, so a re-ask
could only repeat the recorded answer or fail because the mount is gone, and
a served result needs nothing from the mount.

### Reads under a mounted root replay from the record

Witness format v4 stores every row as `(question, answer digest)`. On a
cache hit the verifier takes the recorded digest, with no host call, for a
read whose whole answer is a function of a mounted tree
(`Question::replays_from_record`): `Import`, `ReadFile`, `ReadFileBytes`,
`ReadDir`, `PathExists`, `DirExists`, `FileType`, `FileTypeResolved` with a
`Root::Mounted` path. The argument has two halves. `rootedPath` refuses any
mount point that is not a complete store path, and the fetcher that mounted
it computed that name from the tree's content, so the name pins the bytes.
And the bridge resolves the path inside the mount's own accessor, where
`SourceAccessor::resolveSymlinks` restarts an absolute symlink target at that
accessor's root, so nothing outside the tree can reach the answer. "Observed
now" and "recorded" are therefore the same value by construction, and the
key is still computed from the observation. Ambient paths, and every effect
(copies, filtered copies, writes, realisations, fetches), are asked again or
replayed by validity as before. `replay.<Kind>_recorded` counts the rows
served this way; on the home-manager witness they are 139k `ReadDir` and
25k `Import` rows, 2.9s of a 10.7s hit before this existed.

### One witness row per immutable tree

Witness format v7, key tag `ixe-eval-result-v3`: before a result is
published, `ReadSet::fold_trees` replaces every row
`Question::replays_from_record` vouches for (a read under a mounted root, or
under a sealed store object away from its leaving links) with one
`Question::Tree` row per tree, at the position of the tree's first such row:
a mounted root at its mount point, a sealed object at `<store-dir>/<object>`.
The folded rows' digests were pinned by the tree's name, so the tree row
carries the same fact in one row, and the witness holds one row per tree
read instead of one per file read (the home-manager witness above was about
165k rows and hundreds of megabytes). On a hit the tree row replays from its
record exactly as the rows it stands for did (`Question::replays_from_record`:
`immutable_root` on the root; whether the store still holds the object is
not asked, since the name pins what any re-read could see and a served
result reads nothing from it); a root that is not immutable (unsealed) is
asked and answers a constant the record never carries, so the key misses
and the evaluation re-records. Rows under an unsealed object, or at or below a
leaving link, are not folded and ask as before. The fold is where the
sealing question is first asked now (`replay.sealing_*` count it on the
recording side too), before the publication lock; `witness_rows_folded`
counts the rows removed, and a tree row served from its record counts under
`replay.PathExists_recorded`, the kind of the need it stands as. A version-6 witness is refused at its format
marker: same evaluation, different key, nothing to gain from parsing it.

### Store effects replay by validity

Witness format v5 lets a row keep the answer text instead of its digest
(`readset::Recorded::Answer`) when the question can replay from it:
`Question::named_object` names the store object the answer named, and it is
the only gate, on the recording side and in the decoder (a kept answer on
any other question makes the witness unreadable, so a miss). The rule: the
question's inputs cannot change under its spelling and its answer named a
store object. `CopyToStore` and `StoreFiltered` under a `Root::Mounted`
path (the mount's store path name fixes the bytes), `Fetch` with a `sha256`
(cppnix's `fetch` answers the fixed-output path as soon as it is valid):
these replay while the store holds the object. `FetchTree` through
`fetchFinalTree` with a `narHash` (`FetchTreeRequest::locked_final`; every
emitted attribute is a locked one or computed from them) replays whether or
not the store holds the tree, the object named for the allow list alone.
Whether a copy's
source could have changed is decided at replay (a mounted root, or a sealed
object away from its leaving links, cannot); unpinned fetches and
`fetchTree`/`fetchGit` trees still ask.

On a hit the verifier asks one batched question, `Host::valid_paths`
(`rustValidPaths`: one `queryValidPaths` over printed store paths, plus
every path at which the evaluator has a tree mounted; the embedder decides
nothing else), over every path the rows stand on
(`Question::validity_lines`): the derivation paths `WriteDrv` rows answered,
the paths `Realise` rows stand on, and the objects kept copy and pinned-fetch
answers name (a copy is kept whatever its root; whether its source could have
changed is decided at replay, below; a locked final tree's kept answer is
asked of no store). What a realised context element stands on is
resolved in Rust (`readset::realisable`): an opaque path or `=drv` on
itself; `!out!drv` on the derivation and its output path, read from the
derivation's ATerm that the witness's own `WriteDrv` row keeps in the CAS
(a floating output, a derivation the witness did not write, or a derivation
named by a derivation cannot be resolved and its row asks). A realisation
standing wholly on held paths answers what `realiseContext` returns with
nothing to build: the empty rewrite map, or under `ca-derivations` (on for
hydra: nix.conf line 12) each built output's `downstream_placeholder` mapped
to its path, as `realiseContextBuild` does; and its outputs go through
`Host::allow_closures` (the realise protocol's phase 3, `allowClosure`) as
`realiseContext` allows what it realised: under pure eval the root accessor
is an allow list and the rows after the realisation read through the output.
A copy, pinned fetch or final tree served by validity goes through
`Host::allow_paths` (`rustAllowPaths`, `allowPath`) instead, the form its
live hook uses: the path alone, never what it references. Policy the host
would enforce on the live path and never sees on a hit is in the key
instead: `allow-import-from-derivation` (`realiseContextCheck`) and
`allowed-uris` (`checkURI`); under `--repair` the memo serves nothing.
`replay.<Kind>_changed` counts, per kind, the rows whose replayed digest
differs from the record, and `IXE_REPLAY_TRACE=1` names each such row on
stderr: the instrument for a warm run that misses.
Then `Question::answer_by_validity` answers each row from
what the store said now (`replay.<Kind>_validated`), never from the record:
a held derivation is the path the write answered, a held context is the
empty rewrite map, a held copy is the kept answer; a locked final tree,
whose answer the store cannot change, is the one row answered from its
record here. A path the batch did not
hold is asked again at the row that needs it (`readset::Validity::holds`,
one `valid_paths` of one path, counted in `replay.validity_late`): a row
replays what the rows before it left behind, and the batch ran before any
of them (a derivation is absent until its `WriteDrv` row writes it). A
positive answer is remembered for the replay; a negative is not. Presence
is asked only of what a served result hands out: the derivations its
writes answered, the objects its copies and `sha256`-pinned fetches named,
the paths its realisations stand on. A read under a sealed object or a
mounted root replays from its record whether or not the store still holds
the tree (`Question::replays_from_record`): the name pins what any re-read
could see and a served result reads nothing from it. And a final tree with
a `narHash` (`fetchFinalTree` over a flake input's locked attributes,
`FetchTreeRequest::locked_final`) replays from its record without a fetch
or a presence question, its answer being a function of those attributes;
the host is asked only to allow the tree's path as the fetch would have,
and the path is a mount for the rows after it (`readset::Validity::mounted`):
a realisation of an opaque lazy path passes `realiseContextCheck` while the
path is mounted, and otherwise only when the store holds it. The mount is
not presence: a copy or pinned fetch naming the same path still needs the
store to hold the object (`allowPath` materialises nothing; the asked copy
would have), so such a row asks.
(Round 14 asked presence at the tree row and round 15 stopped: the four
crane inputs of the home-manager witness are lazily mounted trees the
store never registers, so each was fetched again on every hit, 1.6s of a
6.5s warm run.) The one impurity this admits is a fetch that would fail
now: a hit answers what the fetch answered before, as cppnix's own flake
evaluation cache does. A host that fails one validity question is not
asked another: a path not already known held is then absent and its row
asks. `replay.validity_lines` and
`replay.validity_ns` are the `valid_paths` questions' size and cost (late
asks included), `replay.sealing_objects` and `replay.sealing_ns` the
`sealed_paths` batch's; `replay.validity_failed` and `replay.sealing_failed`
count a host that could not answer (every row then asks: correct, slow, and
otherwise invisible). A row the store does not hold whole asks as before.

### Sealed store objects: the mounted-root rule for ambient paths

On a host without lazy trees every flake input is an ordinary
`/nix/store/<hash>-<name>/...` path on the ambient filesystem, not a
mounted accessor: the first measurement of round 10 on the home-manager
witness showed `copyMounted` 0 and no `replay.<Kind>_recorded` line at all,
so the mounted-root rule served nothing there. (`path:` and `jj+file` inputs
ARE mounted -- an accessor at a store-path name the fetcher derived from the
content, never registered with the store -- but the evaluator sees them
through the root accessor and spells their paths as ambient store paths, so
they take the sealed-object route below: `rustValidPaths` counts a storeFS
mount as held and `rustSealedPaths` walks the mount. Round 11 measured 2702
of 2939 asked rows under one such unregistered object, the config tree
itself.) The same argument holds for
an ambient path inside a SEALED store object: one the store holds, that is
content-addressed (`ValidPathInfo::ca`, so the name pins the bytes; an
input-addressed output can be rebuilt with other bytes and does not
qualify). Every byte of such an object is pinned by its name, symlink text
included; the one read that can reach bytes the name does not pin is one
that resolves a symlink OUT of the object. So `rustSealedPaths`
(`ixe_sealed_paths_fn`) answers, in one batched question walking each object
through the store's own accessor, the sealed objects and for each the
symlinks that leave it (a relative target is resolved against the symlink's
depth below the object; `..` past the root or an absolute target leaves; a
dangling relative target that stays below does not). nixpkgs has three
symlinks, all relative and staying inside; a dirty flake input with a
`result -> /nix/store/...` link has one leaving. Round 11 measured why the
finer rule matters: under the whole-object rule that one link unsealed the
input and its `flake.nix` import asked, and the asks under unsealed objects
cost 1.96s of re-made filtered copies and 0.69s of directory listings on the
warm arm. `readset::sealed_objects` asks once per object ever: `DirSealed`
(`<cache>/sealed/<hash>-<name>`, body = the leaving links one per line,
never swept) records each object the embedder called sealed, and a recorded
object is not sent again. Under a sealed object every read row not at or
below a leaving link replays from its record (`Question::replays_from_record`,
via `immutable_root`) and a copy or a filtered copy replays by validity of
its kept answer (a copy takes links as links, so a leaving link below the
copy root changes nothing); under an unsealed object, or at or below a
leaving link, they ask as before. `Settings::store_dir` is what places a
path in an object, so an embedder that never said where the store is gets
no sealing.

### Filtered copies: the per-question memo

The rules above serve a whole evaluation. On a miss (every evaluation after
an edit) each `StoreFiltered` is asked live, and cppnix has no cache for a
filtered copy: `fetchToStore` voids the fingerprint when a filter is passed,
the filter being an opaque callback, so every `builtins.path { filter = ...; }`
walks and NAR-hashes its tree. Measured on the home-manager edit case
(2026-09-03, lm-f6a5-edit and the -vvv memo-miss run): 1044 `StoreFiltered`,
of which the 968 unfiltered copies cost 0.25 s (fingerprint cache hits) and
the 76 filtered ones 5.6 s of a 33 s evaluation, two of them the 11k-file ix
tree at 2.8 s and 1.4 s; 21 of the 76 were exact repeats inside one
evaluation.

In this evaluator the filter is not opaque: the question carries the
accepted set as data, so `readset::CopyMemo` (`<cache>/copies/<key>`, key =
the question's own `key_material` plus the store directory, body = the
store path it answered) remembers each filtered copy and serves it when two
things hold. The root's bytes cannot change under its spelling, decided by
the same `immutable_root` the replay uses (a mounted root, or an ambient
path inside a sealed object away from its leaving links, the object sealed
per `DirSealed` or by one `sealed_paths` question recorded into it); and the
store confirms the remembered path is valid right now (`valid_paths`, one
round trip against a walk), so a swept object is a miss. A root that fails
the first test is neither served nor remembered. The memo sits in
`RecordingHost::store_filtered`, below the recording: the row is the same
whether the memo or the store answered, so the served run's read set is the
walked run's, and the per-job rule `JobMemo` follows (the job that hit
records nothing) does not apply. Unlike `sealed` it has a leaver: every
edit of a filtered tree mints new keys, so `DirCopies` prunes to 8192
entries by recency of use, at open and every 64 inserts per handle, and
removes temporaries older than an hour (a writer that died). The key
does not carry the embedder's version: a copy's store path is the store's
own content address of (bytes, name, method, references), the same
contract the whole-evaluation memo and cppnix's `sourcePathToHash` rely
on. `q.StoreFiltered_served` in
the perf line counts the hits; `IXE_REPLAY_TRACE` prints one line per ask
(`served=memo|walk|mutable`) with the memo's steps (`readset::CopyCost`:
immutable, key, get, valid, allow) and the ask's `record_ns` and
`total_ns`, so `q.StoreFiltered_ns` can be accounted for step by step.
That trace found the served lookup's cost (3.7 ms against 0.25 ms for a
cppnix fingerprint hit) in `valid_ns`: p50 65 us, p99 31 ms, 71 of 1006
asks over 5 ms costing 1.4 s of 1.9 s. Not the daemon (a four-connection
pool changed nothing, l2ca1): `EmbedderHost::valid_paths` flushed the
pending derivation writes before every ask, so the copy memo paid for the
write batches. It now flushes only when an asked path is, or lies under, a
pending derivation (`settle_writes_if`, as `ensure_path` does). An object
the host declines to call sealed is asked about once per `DirSealed`
handle, not once per question (`DirSealed::decline`). `total_ns` in the
trace ends before the line is written; the perf counter includes the
write. `ixe slow: Realise check_ns=` (evaluation thread, before the
ticket) and `work_ns=` (the build thread) name what a begun realise
costs, since `slow.Realise_ns` is begin-to-collect wall.

A live realisation of a derivation this evaluation wrote, every path of
which the store holds now, is answered without the build
(`RecordingHost::realised_by_validity`): the verifier's answer for a
recorded row, given to a live one through the one definition of "nothing
to build" (`readset::nothing_to_build`), so the two routes cannot drift.
The recorder resolves what each element stands on from the ATerms it saw
written, asks `valid_paths` once for them all (which lands a queued write
of the derivation first), allows the outputs as `realiseContext` would, and
records the row the asked route records with nothing to build. Such a
realisation is not begun on a thread either; `begin` declines it and the
synchronous route answers, so the store is asked twice for it. Bed l2db5c:
17 realisations per edit of the real config, each 110 ms of daemon-side
`buildPaths` with nothing to build. `q.Realise_validated` counts them;
`q.Realise - realise_hits - q.Realise_validated` is how many reached the
embedder's realise.

### Derivation writes

`rustWriteDerivation` parses the evaluator's ATerm with `parseDerivation` and
hands that object to cppnix's own derivation path. Read-only mode uses
`computeStorePath`; writable mode streams the batch to the store.

The write is deferred and batched. `derivationStrict` asks `WriteDrv` once per
distinct derivation per evaluation (`Vm::note_drv_written` answers a repeat of
the same bytes itself; `q.WriteDrv_skipped` counts those). The bridge host
(`EmbedderHost::write_derivation`) answers from the bytes -- the path is a
function of the ATerm and its references, computed with the evaluator's own
`text_store_path` -- and queues `name`, ATerm and expected path
(`q.WriteDrv_deferred`). `EmbedderHost::flush_derivations` hands the queue to
`ixe_write_drvs_fn` as one batch (`q.WriteDrv_flushes`, and `drvFlushes` on the
bridge) before any question whose answer could observe a `.drv` missing: a
realise (blocking or begun), `valid_paths`, `sealed_paths`, `ensure_path` or
`builtins.storePath` of a pending path, any read of a pending path, a `toFile`
referencing one, and when the scheduler settles the finished evaluation
(`Host::settle`, called once per job by `eval::drive_concurrent`; the bridge
host's settle IS the flush). The settle is the only crossing back to the
embedder, so every entry point -- a forced handle, `ixe_render`, a memo
verification, a derivation-set walk -- is covered by construction; a flush at
each entry point was the first design and missed `ixe_render`, where
`nix-instantiate --eval --strict` builds its derivations (the functional test
counted `flushes: 0`). A refused batch poisons the host: every later store
question and every later settle fail, and the result is never memoised. cppnix
pays one daemon round trip per
`derivationStrict` call; measured on the home-manager closure that was 73,518
asks, 23,315 distinct, 13.9 s of an 87 s edited-config evaluation.

`rustWriteDerivations` parses every ATerm, derives the reference set as
`writeDerivation` does (`inputSrcs` plus `inputDrvs` keys), verifies the
evaluator's expected path against `makeFixedOutputPathFromCA`, and streams
`count, (ValidPathInfo, NAR)*` into `Store::addMultipleToStore(Source &)`:
one `AddMultipleToStore` daemon op for the batch, the same protocol
`nix copy` uses. The daemon's `addToStore(info, source)` adds the temporary
root and skips the write when the path is already valid (`repair` rewrites),
which is `Store::writeDerivation`'s contract per path without its per-path
`addTempRoot` + `isValidPath` round trips. `drvWrites` counts derivations
streamed. Under `readOnlyMode` the hook writes nothing, as `writeDerivation`
does.

Before the batch is streamed, every parsed `drv.inputSrcs` entry passes through
`EvalState::ensureLazyPathCopied`. These are precisely the `Opaque` context
elements cppnix materialises in `derivationStrictInternal`; input derivations
are not broadened into that set.
`q.WriteDrv_unique` hashes the recording host's exact write arguments -- the
name and the ATerm's content address -- with a 128-bit digest. The
evaluator-only expected path is excluded, and there is no references field:
the ATerm names its inputs and the embedder derives the reference set from
the parsed derivation, as `writeDerivation` does.

## Store-GC divergence

The observed failure was:

```text
path '/nix/store/...-source' is not valid
at derivation-internal.nix:36
```

The cpp arm keeps accessor provenance:

```text
lockFlake
  -> inputCache.getAccessor
  -> mountInput
  -> callFlake / imported flake source
  -> derivationStrictInternal
  -> ensureLazyPathCopied(Opaque source)
  -> writeDerivation
```

The Rust path now crosses the copy boundary with its parse-time root intact:

```text
imported path literal
  -> PathValue { mounted root, absolute spelling }
  -> StorePath(root, accessor-relative path)
  -> rustCopyToStore -> SourcePath{exact mount, relative path}

derivationStrict
  -> Vm::note_drv_written: a repeat of the same bytes is answered in the VM
  -> WriteDrv question; EmbedderHost answers text_store_path(bytes), queues
  -> flush_derivations at the first realise / validity / read / toFile /
     storePath naming a pending .drv, or the return to the embedder
  -> rustWriteDerivations: parseDerivation, ensureLazyPathCopied for every
     parsed inputSrc, verify the expected path, one addMultipleToStore stream
```

The StorePath chain never resolves or probes `rootFS`; the VM has already
named the accessor. Separately, the derivation writer calls
`ensureLazyPathCopied` for parsed input sources before every canonical write
or known-set return. A mounted flake source can therefore be re-materialised
after store GC on the Rust arm, as it is on cpp.

On a cache hit the witness verifier does not replay a `WriteDrv` row as a
write. It first asks the host which of the witness's expected derivation
paths the store holds now, in one `queryValidPaths` batch up front and one
late question at the row for a path the batch did not hold
(`ixe_valid_paths_fn`, `Host::valid_paths`), and a row whose path is held
answers with that path; only a missing derivation is rewritten from its
ATerm in the CAS. A served derivation answer is then rooted by the bridge
(`rootServedDerivation`: `addTempRoot` on each answered drvPath, whose
closure covers the input derivations), which is the temporary root the
recording run's writes took. Before this, a hit on the home-manager witness
re-parsed, re-hashed and re-sent all 23,309 derivations.

## Witness size

The whole-evaluation witness records the first occurrence of each distinct
`(question, answer)` pair. A repeated question with the same answer costs no
second row. If a re-ask answers differently, its answer digest makes it a
different pair and it remains in first-occurrence order.
`witness_rows` counts retained rows, `witness_rows_deduped` counts rows
removed as duplicates, and `witness_rows_folded` rows the tree fold removed
(previous section; the evaluation recorded `witness_rows +
witness_rows_folded`). `witness_bytes` is the encoded file size.

A `WriteDrv` row contains the question tag, the derivation name, the expected
`.drv` path, and a 32-byte ATerm `ObjId` -- exactly those four, since witness
format 3; a row with more is not this format and is refused. In the canonical
CBOR encoding the object address occupies 34 bytes: a two-byte byte-string
header and the 32-byte address. For names shorter than 24 bytes, paths shorter
than 256 bytes, and fewer than 24 row fields, the rest of the row is five bytes
of framing plus the name and path bytes. The ATerm itself is a separate CAS
object and identical ATerms share one object.

Each witness carries the current top-level marker owned by `WITNESS_FORMAT` in
`rust/nix-eval-rs/src/readset.rs`. The codec checks that marker before decoding
any question tag, so an unmarked legacy tag 10 derivation row cannot be
replayed as the current tag 10 `StoreText` effect.
Result publication and sweeping also share the store-root
`.record-sweep.lock`. A persistent record holds it from its first CAS check
through witness and row publication, and a sweep holds it for the whole pass.
Any sweep sees either the store before a record or all of the record's objects
and roots.

The sweep itself (`Store::sweep`) reads no witness body. Beside each witness
the writer puts `witness/<id>.refs`, the CAS object names the rows refer to
(one ATerm per `WriteDrv` row), and the sweep's inputs are directory
listings, row files and those sidecars: its cost is the number of entries,
not their size. Rows of every domain and witnesses are one LRU (a hit touches
both), an entry's removal frees the objects only it named, and objects nothing
names go regardless of the cap. A witness without a readable sidecar, or naming
an object that is gone, is dead; `.tmp-*` files and sidecars without a witness
are leftovers and go too. The whole census (`Store::census`, the same one
`eval-server --scrub` reports from) is taken under the lock before anything is
removed, and any read failure aborts the sweep: a directory read as empty would
delete what it referenced. `eval-cache-max-bytes` (default 4 GiB) is the cap;
`ResultCache::record` sweeps to it after each publication and
`evaluate_value_once` once at its end; the `sweep.*` perf counters report runs
(including under-cap checks), time, entries, bytes and failures.

The previous home-target witness was 399 MB for 433,065 questions. It included
73,520 full ATerms and 170,672 `StorePath` rows, many of them repeated. Measure
before and after with separate empty cache directories and the same evaluator
binary, expression, store state, and cold-start procedure. The orchestrator
owns those runs. After each run, record the witness file size and the five
compaction counters:

```bash
cache=$(mktemp -d)
stats=$(mktemp)
NIX_SHOW_STATS=1 \
NIX_SHOW_STATS_PATH="$stats" \
nix eval \
  --extra-experimental-features rust-eval \
  --option eval-backend rust \
  --option eval-cache-dir "$cache" \
  --builders '' \
  '.#homeConfigurations."andrewgazelka@hydra".activationPackage.drvPath'
find "$cache/witness" -type f -exec stat -f '%z %N' {} \;
jq '.rustEvalPerf | {
  witness_rows,
  witness_rows_deduped,
  witness_rows_folded,
  witness_bytes,
  aterms_stored,
  aterms_reused
}' "$stats"
```

The constants beside the result, identity, and witness codecs in
`rust/nix-eval-rs/src/readset.rs` identify their current formats. Old identities
are unreachable rather than migrated, and unversioned witnesses fail inside
the witness codec.

## Measurement recipe

Run from the ix configuration flake that defines the home target. Use one
binary for before and after measurements, with a cold evaluator and store
state chosen by the orchestrator. This is the command shape:

```bash
stats=$(mktemp)
NIX_SHOW_STATS=1 \
NIX_SHOW_STATS_PATH="$stats" \
/usr/bin/time -l nix eval \
  --extra-experimental-features rust-eval \
  --option eval-backend rust \
  --builders '' \
  '.#homeConfigurations."andrewgazelka@hydra".activationPackage.drvPath'
jq '.rustEvalPerf' "$stats"
```

Record wall, user, system, maximum RSS, the output drvPath, and the complete
`rustEvalPerf` object. The drvPath must match the cpp arm before comparing
performance.

The gate5 pre-change cold numbers were:

```text
wall 67.2s  user 38.8s  sys 13.6s  maximum RSS 12.1 GB
cpp wall 33.6s  cpp maximum RSS 4.06 GB
questions 433,065  question_ns 32.4s
q.StorePath 170,672  q.WriteDrv 73,520  q.Realise 11,573
q.Entries 139,298     q.Import 25,069   q.Kind 2,459
q.FetchTree 22        q.StoreText 2,778
```

The derivation closure contains 18,646 unique derivations. The post-change
run should report `q.WriteDrv + q.WriteDrv_skipped` at nixpkgs' structural
count, `q.WriteDrv` and `q.WriteDrv_unique` near that closure count, and
`drvWrites` equal to `q.WriteDrv_deferred` in `drvFlushes` batches (a whole
evaluation that never returns to the embedder flushes every 4096 or once at
the end). `q.StorePath` should remain the
logical ask count, while `q.StorePath_unique` reports argument variety and
`copyMounted` and `copyAmbient` report which parse-time root the VM retained,
and `copy_hits` how many asks the per-evaluation memo answered; `q.Realise -
realise_hits - q.Realise_validated` is how many realise asks reached the
embedder's realise (the rest were answered by the per-evaluation memo or by
validity).
Compare `q.<Kind>_ns`, `collect_call_ns`, and the wall clock; a lower logical
question count would indicate changed evaluator behaviour, not a host-cache
win. Check `q.<Kind>_abandoned` before interpreting a gap between asks and
completed-answer latency.

`tests/functional/rust-eval-host-questions.sh` is the small regression gate.
Its first section makes 1,000 coercions each of two read-only lazy flake
inputs and compares the Rust JSON with a cpp oracle byte for byte. It requires
two unique questions and two mounted host operations before and after
materialisation, and two distinct answers.
It byte-compares a filtered Git input's resulting store path. The Git
accessor's `export-ignore` removes `drop`; no Nix filter repeats that decision,
so the assertion is causal for accessor selection. The next case
coerces one plain path below `TEST_ROOT` once and requires that one ambient
ask to cross the host boundary. The derivation case
evaluates the same derivation at two attribute paths through independent
applications and requires one canonical write plus one known-set skip. The GC
case uses a local tarball input, whose immutable unpack accessor can stay
lazily mounted on every functional-test platform. A read-only cpp evaluation
computes one JSON oracle containing the lazy source and derivation paths while
both paths are absent. The test compares a first writable Rust answer with
that oracle byte for byte, removes the derivation and then its lazy source, and
compares the Rust reevaluation with the same cpp oracle. It requires the paths
to reappear with `drvWrites = 1` in `drvFlushes = 1`, at least two `StorePath`
asks, and at least one fingerprinted copy.
