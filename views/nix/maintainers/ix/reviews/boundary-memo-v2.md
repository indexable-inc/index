# Rooted memoisation: entries under pure roots, dependency edges, no navigation

Status: design v2, 2026-09-01. v1 (replay by navigating recorded paths into
foreign slots) was reviewed and blocked on 2026-09-01; the findings are
folded in under "What v1 got wrong" and every mechanism v1 needed that v2
does not is named there. This refines Decision 2 of
`index/doc/nix-incremental-eval/design.md` for the Rust evaluator.

## The problem

`eval-cache-dir` keys one row on a whole evaluation (`EvalId` +
`ReadSet::key` over every world question, `readset.rs`). Unchanged tree:
served. One edited byte: the single row misses and the whole 33 to 67 seconds
are paid again. Measured on `homeConfigurations."andrewgazelka@hydra"`, an
edit to `home/common.nix` moves 18 of 18,646 derivations, and 75.8% of
attributed evaluation CPU sits behind nixpkgs code whose inputs did not
change. The goal is to serve that part without ever serving a wrong value.

## The one idea

Nix's own flake evaluation cache is sound for a simple reason: under pure
evaluation, `nixpkgs#legacyPackages.x.hello.drvPath` is a pure function of the
locked nixpkgs tree and the evaluator settings. Nothing an evaluation can
observe in pure mode varies between two runs with the same tree and
settings except the store's *contents*, and those are content-addressed. The
cache keys on (tree fingerprint, attribute path) and stores the string.

That cache lives at the command line. This design moves the same theorem
inside the evaluation: a value the Rust VM reaches by selecting attributes
from a **root** is keyed by (root identity, attribute path) and, when it is
data, stored and served. A root is a flake input's tree, or the application
of one to arguments the evaluator can identify. `home-manager` evaluating a
user's configuration reaches thousands of `pkgs.<name>.outPath` leaves this
way, and every one is a pure function of the nixpkgs tree plus the overlay
code, not of `home/common.nix`.

Two things make this sound where the whole-eval memo is coarse and v1 was
unsound:

1. **Purity carries the proof for values; witnesses only carry effects.** In
   pure mode the value under (root, path) is determined; the recorded world
   questions of its extent exist to re-perform store effects on a hit (write
   the `.drv`, copy the source) and to re-check the one class of read purity
   does not fix: files read from a tree that is *not* part of the root's
   identity (an overlay in the user's tree importing `./pkgs/foo.nix`). Those
   are recorded tree-relative and re-read on replay.

2. **A dependency between entries is an edge, never an observation.** When
   an extent reads a value another tagged extent produced, it records the
   producer's key. On a hit the producer's witness is replayed transitively.
   Nothing is navigated, nothing is compared against a recorded slot, and
   the heap is never mutated speculatively: lookup happens *before* a thunk
   runs, and a miss simply lets the thunk run.

## Roots

A root is a value with a **root identity** the evaluator can compute without
forcing anything user-controlled:

- **A flake input's tree.** Identity = the input's locked content
  fingerprint (`narHash`, or the jj tree id for `jj+file://`). The value is
  the input's attribute set as `call-flake.nix` builds it; `inputs` itself
  is a root named `inputs` whose members are roots.
- **An application of a root-derived closure to identifiable arguments.**
  `import nixpkgs { localSystem = "x"; overlays = [ o1 o2 ]; config = { allowUnfree = true; }; }`
  is the case this machine's configuration is built on (`outputs.nix`,
  `pkgsFor`). Identity = H(identity of the function, identity of each
  argument). An argument is identifiable when it is data, a root, a value
  reached by selection from a root (identity = (root, path)), a list or set
  of identifiable values, or a closure whose identity is computable (below).
  Anything else makes the application unidentifiable and the value simply
  is not a root; evaluation proceeds as today.
- **A closure**: identity = H(module content hash, unit index,
  identity(env)). A frame's identity = H(creating site, identity(up),
  identity of each argument slot) for `Apply` frames and H(site,
  identity(up)) for `let`/`rec`/`with` frames. A slot in `Thunk` state
  contributes H(module, unit, identity(env)); a slot holding data
  contributes the data digest; a slot holding a root or a root-derived
  value contributes its (root, path); a slot holding anything else makes
  the closure unidentifiable. Roots inside an env are identified **by
  name** (the input name `inputs`, the root's own key), never by their
  contents: an overlay `final: prev: { ... }` defined in `overlays.nix` as
  a function of `inputs` has identity H(hash of overlays.nix, unit,
  H(site, ROOT, "inputs")) whatever `inputs.self` currently is. What the
  overlay *reads* from `inputs` becomes a dependency edge when it reads it.

Closure and frame identities are computed on demand, when a root
application asks for them, and cached in a run-local table keyed by
`Rc` pointer. Nothing is stored per slot for this. The walk is bounded by a
budget (depth and node count); exceeding it means "unidentifiable", which is
a lost hit and never a wrong one.

## Tags and stamps

An `Attrs` value that is a root, or was produced by forcing a slot reached
from a root by selection, carries a **tag**: an index into a run-local
table of (root key, attribute path as name bytes). `Attrs` gains one `u32`.

Every `Slot` carries, in the padding `SlotState` already has (the `Value`
variant uses 16 of 24 bytes), two `u32`s: `tag` and `forced_by`.

- **Stamping.** When a slot is read *by name* from a tagged `Attrs` (the
  `Select` family of ops, and the one central accessor every builtin that
  walks a set by name must use), the slot's `tag` is set to (parent tag,
  name). A builtin that iterates a set without going through that accessor
  stamps nothing: the value is then evaluated normally, which is a lost hit,
  not an error. Completeness here is a performance property.
- **Forcing a stamped slot** first consults the memo (below). On a miss it
  opens an **extent** for the tag, runs the thunk, and closes the extent
  when the value is delivered or the force unwinds.
- **`forced_by`** is written at every slot write in `deliver` and `unwind`
  with the id of the innermost open extent on the fiber, `0` outside every
  extent. A slot built directly by a builtin (`Slot::value`) gets the
  current extent too. Extent ids are 64-bit and never reused within a
  process; the `u32` in the slot indexes a run-local table of them.

## Extents and witnesses

Each fiber carries its own extent stack (a fiber parked on a slow question
and a sibling fiber can be in different extents; a resume token and a slow
question ticket carry the extent that asked, so the answer lands in the
right witness). `RecordingHost` records each question into every open
extent on the asking fiber *and* into the root log, so the whole-eval memo's
read set is unchanged by any of this.

A witness for entry E = (root key, path) holds:

- **Store-effect questions** the extent asked: `WriteDrv`, `StoreText`,
  `CopyToStore`/`StorePath`, `StoreFiltered`, `EnsurePath`, `Realise`,
  `FetchTree`. Recorded without their payload bytes: a `WriteDrv` is recorded
  as its drv path with the ATerm stored once in the CAS under its own
  content address (18,646 unique drvs at about 2 KB each is 40 MB shared by
  every witness that names them; the whole-eval witness today stores the
  ATerm inline per question and reaches 399 MB for one evaluation).
- **Tree-relative reads**: every `ReadFile`/`ReadDir`/`PathExists`/
  `FileType`/`Import` whose path lies under a mounted flake tree that is NOT
  part of E's root identity, recorded as (tree role, relative path, answer
  digest). The host maps store paths of mounted inputs to roles; `self` is
  the role whose store path changes on every edit.
- **Dependency edges**: the keys of every other entry whose value the extent
  read, and the extent-ids of the same-run extents it read from (resolved to
  keys at record time).
- **Side state** the extent produced that the VM needs after a hit: the
  `(drvPath, DrvHash)` pairs `derivationStrict` inserted into
  `Vm::drv_hashes`.
- **Emissions** (`warn`, `trace`) the extent produced, in order.

The **payload** is the value, and only a value in data normal form is
recorded: strings with their full context (sorted `ContextElem`s), paths,
integers, floats, booleans, null, and lists of those. Attribute sets are
never payloads (cppnix answers `unsafeGetAttrPos` from a set's origin, which
a served set would not have). An extent whose value is not data records no
payload; it still records its witness, because other entries depend on it
through edges.

### The read rule

Inside extent E, a value delivered from a slot whose `forced_by` is not E
and not an ancestor of E on this fiber is **foreign**:

- foreign and the slot has a tag: record an edge to that tag's key;
- foreign and produced by another open or closed extent of this run:
  record an edge to that extent (resolved to its key, or, if that extent
  has no key because its root was unidentifiable, mark E uncacheable);
- foreign and `forced_by = 0` (forced outside every extent): mark E
  **uncacheable**.

Uncacheable is a verdict on the entry for this run, not an error: the value
is computed normally and nothing is recorded. This is the rule that replaces
v1's foreign-leaf observations. It is conservative in exactly one direction.

Why the third case is rare: a value forced outside every extent is one the
user's own code forced. Nixpkgs code runs inside extents because it is only
reachable through tagged selections from `pkgs`. The exception, a user
holding a nixpkgs closure and applying it at top level (`pkgs.callPackage`
in `outputs.nix`), produces an untagged result whose deep forcing of
`pkgs.stdenv.*` happens inside *those* stamped slots' extents, so those are
still served.

## Lookup and serving

At a stamped slot's force, with the slot in `Thunk` state:

1. Key K = H(schema version, evaluator fingerprint
   (`modcache::compiler_fingerprint()`), `Settings::fingerprint()`, root
   identity, path). Fetch the entry.
2. Replay its witness: re-ask every store-effect question through the host
   (with the HOST lane's caches these are validity checks, not rewrites);
   re-read every tree-relative read against the current tree of that role
   and compare digests; recursively serve or verify every dependency edge
   (memoised per run by key). Any disagreement or refusal: miss.
3. Hit: write the payload into the slot as a `Value`, set `forced_by` to
   a fresh extent id that maps to K (so later readers record an edge to K),
   insert the side state into `Vm::drv_hashes`, replay the emissions, and
   count `served_from_memo`. Miss: open an extent and run the thunk.

Nothing in step 2 executes Nix code. A dependency edge to an entry whose
value is not data is verified (its witness replayed) without producing a
value; the value itself is recomputed if and when something forces it,
which is correct because its own leaves are separately keyed.

Verification mode (sampled hits re-evaluated, as `serve`/`settle` do today)
sets a VM-wide bypass so the check arm cannot be served the same row it is
checking.

## Soundness argument

Claim: a served payload under K equals what forcing the slot would produce
in the current process.

The slot's thunk is (module, unit, env) reached by a path from a root whose
identity determines the function and its arguments. Under pure evaluation,
the value of that thunk is a function of: the trees named by the root
identity (their content is in K), the evaluator (in K), the settings (in K),
the answers to world questions its computation observed, and the values it
read that other computations produced. World questions in pure mode are
either fixed by K (files under identified trees, `currentSystem`, empty
`getEnv`), content-addressed (store objects, locked fetches, IFD outputs),
recorded tree-relative and re-read (files under other trees), or refused.
Values read from other computations are, by the read rule, either
in-extent (recomputed identically, by induction), reachable through an edge
to an entry (verified transitively), or cause the entry to be uncacheable.
Therefore every input of the value is fixed by K or re-checked, and the
value is the same. Store effects are re-performed so a served drvPath names
a derivation the store holds. Side state is restored so consumers behave as
after a cold force.

Under impure evaluation none of this is attempted: `Settings::fingerprint`
distinguishes the modes and stamping is disabled unless `pure_eval` is set.

## What v1 got wrong, and what v2 does about each

| v1 finding (review 2026-09-01) | v2 |
| --- | --- |
| key omitted settings, schema, evaluator | all three in K |
| served value omitted `drv_hashes` side state | side state in the witness, restored on hit |
| foreign closures/builtins unobserved | no observations; foreign reads are edges or uncacheable |
| foreign `Failed` slots unobserved | a failing extent records nothing; a foreign failure read is foreign, same rule |
| scalar digest dropped string context | payload and digests carry sorted context elements |
| provenance could not cross Rust-native iteration | stamping is completeness-only; an unstamped slot is a lost hit |
| `ShapeOf` not an exhaustive observation model | no shape observations exist |
| persisted `Select(sym)` | paths are name bytes |
| one recorder scope stack across fibers | per-fiber extent stacks; tickets carry the asking extent |
| executed boundaries vanished from the whole-eval log | every question also records to the root log |
| replay mutated the heap | lookup precedes forcing; nothing speculative runs |
| replay duplicated emissions and effects | a hit replays the entry's emissions once; a miss runs the thunk once |
| verifier could be served the row under check | VM-wide bypass during verification |
| `forced_by` reuse and untotal identity | 64-bit extent ids; identity is computed on demand for root applications only |
| cost model unmeasured, reverses "no per-thunk" | two `u32` in existing padding, one `u32` per `Attrs`, no per-slot hashing |
| no warm-edit target | preregistered below |
| a second evaluator | no navigation, no observation model, no transactions |

## Cost

- `SlotState`: 0 bytes (padding). `Attrs`: +4 bytes. Frames: 0 bytes.
- Per force of a stamped slot: one key hash and one table lookup on a warm
  run; on a cold run additionally an extent open/close. Stamped forces are
  the order of 10^5 per evaluation, not 10^7.
- Per force of any slot: one `u32` compare of `forced_by` against the
  current extent; when unequal, a walk of the fiber's extent stack (short).
- Root identity: one bounded graph walk per root application (a handful
  per evaluation), memoised by pointer.
- Witness store: dominated by the drv ATerms, 40 MB shared, plus a few
  hundred bytes per entry.

The lane measures cold wall and RSS with the feature compiled in but the
cache dir unset (must be within noise of today), and with it set (records).

## Preregistered targets

On `homeConfigurations."andrewgazelka@hydra".activationPackage.drvPath`,
against cpp cold at 33.6 s and Rust cold after the HOST lane:

- warm-unchanged: under 5 s;
- **warm after editing one line of `home/common.nix`: under 10 s**, with
  `served_from_memo` at least 15,000 and the 18 moved derivations among the
  misses;
- `eval-backend = shadow` on both real configurations at `agreed: 1` with
  the memo on, cold and warm-edited.

## Staging

Each stage is a write-only codex lane plus an adversarial review lane.

**Stage A: extents, forced_by, tags, stamping, root identity for flake
inputs and root applications; recording only** (no serving). Counters:
`entries_recorded`, `uncacheable`, per-reason. The witness store under
`Domain::mint("ix-eval.entry", "rooted")` beside the two existing domains.
Deliverable is a census on the home target: how many entries, how many
uncacheable and why, and the witness store size.

**Stage B: serving**, the verifier bypass, `served_from_memo`, the
incremental gate's new arm (edit an unrelated file, assert served counts),
and the three preregistered numbers.

**Stage C: witness compaction of the whole-eval memo** (ATerms to the CAS,
tree-relative reads), which is the same codec and shares the store.
