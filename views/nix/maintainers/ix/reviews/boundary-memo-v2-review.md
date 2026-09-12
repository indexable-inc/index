1. **BLOCKER: The world-question witness is not exhaustive.**

   Evidence: `rust/nix-eval-rs/src/readset.rs:49-200` defines 19 `Question` variants. The v2 witness lists at `maintainers/ix/boundary-memo.md:128-139` omit `ReadFileBytes`, `FindFile`, `Fetch`, `LockFlake`, `ParseFlakeRef`, and `FlakeRefToString`. These are not redundant:

   - `ReadFileBytes` is the raw-byte dependency of `hashFile` (`readset.rs:51-57`).
   - `FindFile` depends on the explicit search entries and winning path (`readset.rs:77-89`).
   - Pinned `Fetch` is allowed under pure evaluation and still has a store-validity effect (`purity.rs:309-329`, `readset.rs:150-161`).
   - `LockFlake` observes the lock file/input graph; its existing sound cache records the answer (`readset.rs:171-186`).
   - `ParseFlakeRef` and `FlakeRefToString` depend on embedder settings or feature gates not in `Settings::fingerprint()` (`readset.rs:187-200`).

   The exhaustive audit is:

   - Covered by v2: `ReadFile`, `ReadDir`, `PathExists`, `FileType`, `FileTypeResolved`/`Import`, `CopyToStore`, `StoreText`/`WriteDrv`, `Realise`, `EnsurePath`, `StoreFiltered`, `FetchTree`.
   - Fixed under `pure_eval`: `GetEnv` returns empty and `NixPath` is empty (`purity.rs:226-228,377-383`).
   - Missing: the six variants above.

   Builtin audit: `currentTime` is unavailable, `getEnv` is empty, `storePath` errors, unlocked fetches are refused, and `__curPos` is covered by the module origin (`builtins.rs:1062-1085`, `primops.cc:2022-2027`, `modcache.rs:120-143`). `hashFile`, explicit `findFile`, locked fetches and flake locking remain uncovered here. `unsafeGetAttrPos`, `seq`, string-context rewriting, and derivation modes depend on the separate failures below. IFD is covered only when both `Realise` and the subsequent import reads are witnessed.

   Failure: an overlay calls `builtins.hashFile` on a data file in foreign `self` and incorporates the digest in a derivation name. Editing that file changes `ReadFileBytes`, but the proposed witness records neither that question nor a tree-relative raw-byte read, so the old payload hits.

   Fix: define one exhaustive, compiler-checked classification over every `Question`/`NeedPath` variant. Every case must be keyed, replayed with its recorded answer digest, or make the extent uncacheable. Do not maintain a prose allow-list.

2. **BLOCKER: The read rule does not run on ordinary already-forced reads.**

   Evidence: `GetLocal` reads a `Value` through `Slot::peek()` and pushes it directly (`vm.rs:2175-2188`). `yield_force` has the same fast path (`vm.rs:2077-2094`), and builtin `forced()` is just another raw `peek()` (`vm.rs:3195-3201`). The slot identity is then gone because stacks, flows, tasks, and apply frames carry bare `Value`s (`vm.rs:314-378`, `task.rs:20-33`). Direct constructors cannot assign the current extent because `Slot::value`, `Slot::pending`, and `Slot::unimplemented` take no `Vm` or fiber context (`value2.rs:1199-1219`).

   This covers the prompt’s `PendingApply`, captured environment slots, with-subjects, task values, sibling waiters, `repoint_thunk`, and container copies. In particular, `repoint_thunk` preserves any already-forced slot unchanged (`vm.rs:3269-3276`). The derivation wrapper is also a persistent shared cell across VM evaluations (`vm.rs:910-935`).

   Failure: an identifiable root thunk reads a shared slot that an unidentifiable sibling or user computation already forced. `GetLocal` obtains the bare value without observing `forced_by=0` or the producing extent. The root payload is recorded with no edge and can later be served after the shared input changes. `seq` and context-rewriting builtins reach this exact path because their forced arguments are subsequently read through `forced()`.

   Fix: make every slot consumption pass through `Vm::observe_slot(&Slot)` before matching or peeking. Carry the slot or explicit provenance through `Flow::Deliver`, `StackEntry::Val`, task `Yield`, and `ApplyFrame`. Remove evaluator access to an untracked `peek()` API. Slot constructors that write values must receive provenance from the current fiber.

3. **BLOCKER: `Failed` and `Unimplemented` are still invisible terminal reads.**

   Evidence: these are terminal `SlotState` variants (`value2.rs:1160-1196`). `advance_force` rethrows them directly without delivering a value (`vm.rs:1759-1767`). `tryEval` catches the resulting error and converts it to data (`vm.rs:1638-1675`, `primops_pure.rs:841-858`). V2’s rule applies only to “a value delivered from a slot” (`boundary-memo.md:158-166`), so its table row claiming the same rule handles failures is false.

   Failure: pre-force `x = if pathExists flag then 1 else throw "boom"` outside every extent. Inside a rooted extent, cache `(tryEval x).success`. The first run observes a foreign `Failed` slot and records `false` without any dependency. After the flag changes, cold evaluation yields `true`, but the cached `false` can still hit.

   Fix: observe slot ownership before dispatching on every `SlotState`, including `Failed`, `Unimplemented`, `Blackhole`, and `PendingApply`. A foreign terminal failure must record a producer edge or make the catching extent uncacheable. A foreign blackhole must always make it uncacheable.

4. **BLOCKER: Aggregate shape and origin observations invalidate the name-only `inputs` root.**

   Evidence: v2 calls `inputs` a root “named `inputs`” and claims iteration without stamping is merely a lost hit (`boundary-memo.md:60-63,102-107`). Actual code observes aggregates without selecting a child slot:

   - `HasAttr` and `builtins.hasAttr` inspect membership (`vm.rs:2680-2688`, `primops_pure.rs:1458-1462`).
   - `attrNames` and `attrValues` iterate raw maps (`primops_pure.rs:1424-1445`).
   - Equality observes list length, set length, and key names (`task.rs:1193-1232`).
   - `unsafeGetAttrPos` observes membership and `Attrs.origin` (`primops_pure.rs:1668-1704`).
   - `functionArgs` observes formal names and whether each has a default (`primops_pure.rs:2724-2755`).

   `Attrs` deliberately exposes raw `Deref` and `DerefMut` access to its map (`value2.rs:797-813`), so the proposed “one central accessor” is not the current ownership boundary.

   Failure: an overlay returns `marker = if inputs ? foo then "a" else "b"`. Changing flake input membership leaves the name-only `inputs` identity and overlay code unchanged. No child slot is selected, so no tag or dependency edge is recorded. The old scalar can hit. `attrNames inputs` has the same failure.

   The borrowed C++ cache does not justify this shortcut. Its flake fingerprint includes the complete lock file plus `revCount` and `lastModified` (`src/libflake/flake.cc:1130-1152`), not a constant name for the inputs set.

   Fix: include the complete locked input graph in the `inputs` identity, or track typed aggregate observations such as membership, names, length, formal schema, and attribute origin. Raw `Attrs` map access must not remain available to evaluator code if the latter approach is chosen.

5. **BLOCKER: Root-application identity both collides and loses the fact that a closure came from a root.**

   Evidence: v2 makes sets of identifiable values valid root arguments but does not include attribute origin (`boundary-memo.md:64-70`). Origin is observable state stored separately from values (`value2.rs:33-54`). Thus two equal-valued argument sets can have different observable `unsafeGetAttrPos` results.

   Separately, only `Attrs` carries a tag (`boundary-memo.md:95-100`). Forcing a selected closure produces a bare `Value::Closure`; `ClosureData` contains only module, unit, and environment (`value2.rs:1143-1148`). `Op::Apply` and `ApplyFrame` retain no producing slot or tag (`vm.rs:356-359,2237-2249`). The VM therefore cannot tell that this closure was selected from a root. Treating every structurally identifiable closure as root-derived would silently broaden the theorem.

   The frame identity is also not constructible from current environments: `EnvNode::Frame` and `EnvNode::With` contain no creation site or distinction between apply, let, rec, and with (`value2.rs:1348-1366`). Both closure application and lexical scopes allocate the same `Frame` shape (`vm.rs:1804-1809,2252-2274`).

   Failure: apply root-derived `f = x: builtins.unsafeGetAttrPos "a" x` to two sets with the same key and value but different source origins. V2 hashes both argument sets identically, while the returned file, line, and column differ.

   Fix: include complete observable aggregate identity, including per-attribute origin, or reject such sets as root arguments. Carry root derivation provenance with forced closures through application. Store or side-register stable environment kind and creation-site identity at every environment constructor.

6. **BLOCKER: The `u32` indirection does not solve extent or tag reuse across VM runs.**

   Evidence: v2 stores `tag` and `forced_by` as `u32` indices into a “run-local” table while claiming the actual 64-bit extent IDs are never reused within the process (`boundary-memo.md:99-115`). A run-local table necessarily reuses index values. Slots outlive individual runs: the VM retains `builtins_value`, `derivation_slot`, module values, and external handles, while `reset()` clears only fibers and scheduler queues (`vm.rs:597-613,910-935,1219-1237`).

   Failure: a retained slot contains `forced_by = 5` from run A. Run B resets the table and assigns index 5 to its current extent. A read in B now treats the old value as locally produced, omitting the required edge or uncacheable verdict. Tags have the same stale-index problem.

   An append-only process table avoids immediate aliasing but restores the v1 exhaustion issue: at roughly 73,520 extents per large evaluation, a `u32` index runs out after about 58,000 evaluations.

   Fix: store a non-reused generation and extent identity in the slot, preferably a full `u64`, or prove a bounded process lifetime and fail closed before exhaustion. A reusable run-local index is not lifetime-safe.

7. **BLOCKER: The existing whole-evaluation memo can bypass the safer rooted key across evaluator upgrades.**

   Evidence: `EvalId` hashes only module object ID, settings, arguments, and output question (`readset.rs:1427-1485`). `session::evaluate` calls this cache before entering the VM and returns immediately on a hit (`session.rs:124-160`). The module compile request includes `compiler_fingerprint()`, but the resulting module object is content-addressed from serialized IR. A runtime-only evaluator change can recompile to identical IR and therefore the same module object ID.

   V2’s rooted key includes `compiler_fingerprint()`, but that key is never consulted when the outer memo hits. Sampled verification does not repair served answers; even disagreement returns the already-served value (`session.rs:185-193`).

   Failure: upgrade the VM or builtin runtime in a way that changes evaluation semantics but leaves the compiled IR unchanged. The old `EvalId` still hits and returns before rooted memoisation or its verifier can run.

   Fix: add the full evaluator fingerprint and result-schema version directly to `EvalId`, and bump `EVAL_ID_TAG`. The fingerprint must cover the Rust runtime and any C++ embedder semantics treated as fixed rather than replayed.

8. **BLOCKER: Existing `WriteDrv` replay is not the exact host effect v2 requires.**

   Evidence: the whole-evaluation recorder represents `Host::write_derivation` as `Question::StoreText` (`readset.rs:1156-1183`), and replay calls `Host::store_text` (`readset.rs:658-664`). These are observably different in the embedder: `rustStoreText` calls `allowPath` (`rust-eval-session.cc:389-397`), while the derivation writer deliberately does not (`rust-eval-session.cc:394-395,420-509`).

   Failure: a cached pure evaluation containing `derivationStrict` replays the `.drv` through `store_text`, adding that path to the persistent allow-list. A later evaluation in the same session can read a path that cold `derivationStrict` would not have allowed.

   Read-only mode also disproves the unconditional theorem sentence that replay makes the store hold every derivation: `rustWriteDerivation` returns the computed path without writing in read-only mode (`rust-eval-session.cc:461-472`). A served string may legitimately carry a `.drv` context whose object is absent; correctness then depends on restoring `drv_hashes` and reproducing read-only behavior, not on store presence.

   Fix: add a distinct persisted `WriteDrv` question/effect that replays `write_derivation`, including its expected path. Do not reuse `StoreText`. State the store theorem conditionally on writable mode.

9. **MAJOR: Per-entry emission order cannot preserve sibling-fiber output order.**

   Evidence: v2 stores an ordered emission vector per entry (`boundary-memo.md:146,192-195`). The existing recorder deliberately uses one global ordered list because warnings and traces interleave (`readset.rs:905-975`). Sibling fibers can run while another fiber is parked on a slow question (`vm.rs:484-548`).

   Failure: extent E emits `A`, parks on `Realise`, sibling C emits `C`, then E resumes and emits `B`. Cold output is `A,C,B`. A hit for E replays `A,B` contiguously, producing `A,B,C` or `C,A,B`. Per-fiber extent stacks and ticket attribution do not encode the global output sequence.

   Fix: maintain a globally sequenced event tape with extent references, or mark any extent whose emissions interleave with another fiber uncacheable. Apply the same rule to non-commutative host effects.

10. **MAJOR: The cost model relies on a false layout premise and omits the operations required by its own proof.**

   Evidence: v2 says the `Value` variant uses 16 of 24 bytes and that two `u32`s fit at zero cost (`boundary-memo.md:99-100,253-264`). Current `Value::Str` contains `NixStr`, itself a fat `Rc<[u8]>` plus another pointer (`value2.rs:930-949`), while `PendingApply` contains a `Slot` and a three-word `Vec` (`value2.rs:1173-1188`). Rust also does not expose an enum variant’s internal padding for unrelated sibling fields without a manual representation change.

   The repaired read rule must instrument every `peek`, `GetLocal`, task value, apply, and terminal state, not merely stamped forces. The measured continuation layer alone has 7,354,807 `Yield::Force` events, explicitly excluding inline `GetLocal` forcing (`perf-counter-overhead.md:128-148`). The code also reports 5,334,308 applies and millions of aggregate operations (`nixos-toplevel-profile.md:371-377`). Environment creation-site storage, extent stacks, identity side tables, dependency edges, answer digests, payload decoding, cache indices, and global event sequencing are absent from the estimate.

   Failure: the implementation can pass the stated functional targets while exceeding cold cost and RSS, or `warm-unchanged <5s` can be satisfied by the pre-existing whole-evaluation memo without exercising rooted entries. `served_from_memo >=15,000` can count cheap leaves rather than demonstrate useful cutoff.

   Fix: require static layout assertions and allocator-rounded measurements before implementation; count every observation path identified above; record cache bytes, witness bytes, replay time, and peak-live metadata. Compare warm-edit performance against feature-off Rust as well as cold Rust and C++. Given the evaluator-wide tracking surface, first measure the persistent evaluator/zygote alternative described at `design.md:327-350`, or use a narrower post-`DrvStrict` canonical cache.

Table-row audit:

| v1 table row | Result |
|---|---|
| settings/schema/evaluator in key | Fail due outer `EvalId` bypass, finding 7 |
| `drv_hashes` side state | Addressed in principle; code confirms it is the semantic VM map at `vm.rs:572-588` and all derivation branches insert it at `drvstrict.rs:850-870` |
| foreign closures/builtins | Fail, findings 2 and 5 |
| foreign `Failed` | Fail, finding 3 |
| string context | Addressed as a data-format rule; `NixStr` confirms context is separate observable state |
| Rust-native iteration | Fail; it can hide reads and shapes, findings 2 and 4 |
| no shape observations | Fail, finding 4 |
| name bytes instead of `Sym` | Addressed |
| per-fiber stacks/tickets | Partial; attribution does not preserve global order, finding 9 |
| root question log | Addressed as a design rule; external reads are centralized through `NeedPath` |
| no speculative heap mutation | Addressed; witness replay can be host-only |
| emissions/effects once | Fail for ordering and `WriteDrv` equivalence, findings 8 and 9 |
| verifier bypass | Addressed as a required VM-wide mode, but the outer key remains unsound |
| extent IDs and identity | Fail, findings 5 and 6 |
| measured cost | Fail, finding 10 |
| warm-edit target | Partial; target exists but lacks a feature-off baseline and useful-work accounting |
| not a second evaluator | Fail in substance; soundness still requires evaluator-wide slot, aggregate, task, application, error, and scheduler instrumentation |

Verdict: v2 removes speculative navigation and correctly adds settings, context payloads, `drv_hashes`, root logging, and a warm-edit objective. It remains unsound. The proposed witness omits live question kinds, the read rule is bypassed by normal VM fast paths and terminal failures, aggregate observations make the name-only `inputs` identity collide, closure provenance is lost before application, and the `u32` lifetime scheme aliases across runs. The existing whole-evaluation memo adds two independent blockers by omitting the evaluator fingerprint and replaying `WriteDrv` as the observably different `StoreText`. These are architectural failures in the purity theorem, not implementation details.

LANE-DONE-77420000 VERDICT=BLOCK
