# In-evaluation memoisation: decision record

Status: decided 2026-09-01. Two designs for serving parts of an evaluation
from a cross-process memo were written and adversarially reviewed on the
same day; both were blocked. This file records the decision, the evidence,
what ships instead, and the one route that remains open. The surviving design
materials and both reviews are under `reviews/`; the v1 proposal itself was
folded into the v2 document and was not retained as a separate file.

## The prize, restated

On `homeConfigurations."andrewgazelka@hydra"`, one edited line in
`home/common.nix` moves 18 of 18,646 derivations, and the read-set pricing
instrument attributes 75.8% of evaluation CPU to nixpkgs code whose inputs
did not change (`goals/rust-eval.md`, Phase 2, in the private nix repo).
The whole-evaluation memo (`readset.rs`) cannot collect any of it: its one
row keys on every world question the run asked, so one edited byte is a
miss for the whole evaluation.

## What was tried

**v1, replay by navigation** (`reviews/boundary-memo-v1.md` is folded into
the v2 text's "what v1 got wrong" table; `reviews/boundary-memo-v1-review.md`):
key a `derivationStrict` on the construction identity of its argument,
record every foreign data leaf its extent read as a path from the argument,
and on lookup navigate those paths in the live heap, forcing along the way.
Blocked on 12 counts. The structural ones: navigation is speculative
evaluation that mutates the heap and cannot turn errors into misses
without a transactional VM; a sound witness needs an exhaustive observation
model (set shapes, formal schemas, failures, string contexts, attribute
origins, interner order); and provenance has to cross every builtin that
walks a set natively. Together that is a second evaluator.

**v2, rooted memoisation** (`reviews/boundary-memo-v2.md`,
`reviews/boundary-memo-v2-review.md`): borrow the flake eval cache's
theorem (under pure evaluation a value reached by selection from a locked
tree is a function of tree and settings), tag values reached from roots,
look up before a thunk runs, never navigate, and treat any read of a value
forced outside the current extent as a dependency edge or as "uncacheable".
Blocked on 10 counts. The decisive one: the read rule has to see every
consumption of an already-forced slot, and the VM reads values through
`Slot::peek()` on the `GetLocal` fast path, in `yield_force`, in every
builtin's `forced()`, and through bare `Value`s on stacks, tasks and apply
frames. Enforcing the rule means routing every value read through an
observer and carrying provenance on every stack entry. The others: aggregate
observations (`attrNames inputs`, `inputs ? foo`) bypass tags; closures lose
root provenance at application; `u32` extent indices alias across VM runs;
per-entry emission order cannot reproduce sibling-fiber interleaving; and
the cost premise (padding in `SlotState::Value`) is false because `NixStr`
is a fat pointer.

Both reviews reached the same conclusion independently of each other and of
this document's author: **sound in-evaluation memoisation of a lazy
language needs evaluator-wide instrumentation of value reads**, and the
cross-process form additionally needs an identity for forced values that
the evaluator does not retain. `index/doc/nix-incremental-eval/design.md`
Decision 1 reached it first: a persistent evaluator, where identity is
pointer identity and nothing is serialised.

The retired C++ retained-evaluator experiment also rules out using an
`EvalState` pool as that persistent evaluator. It matched 5,432 untouched
answers, but its working set grew about 1.5 GiB per repeat, an edited pass
reached 63.3 GiB and did not finish, and one failed attribute terminated the
whole session. The viable measurement remains a retained Rust `Vm` with
module-scoped invalidation; the C++ experiment and its invocation script were
deleted when that conclusion was folded into this decision.

## Decision

1. No in-evaluation cross-process memo is built. `eval-cache-dir` stays a
   whole-evaluation memo.
2. The whole-evaluation memo is made correct and cheap where it was not
   (below). Warm-unchanged is its case and it should win that case
   outright.
3. The cold Rust arm is made as fast as cppnix (the HOST lane), because the
   profile showed the VM at under a fifth of wall and host questions at
   half.
4. Early cutoff on an edit is pursued only through a **persistent
   evaluator**, and only after the measurement design.md Phase 1 asked for:
   keep one `Vm` alive across two evaluations of the home target with one
   edited module, recompile only that module, and measure how much of the
   heap a re-evaluation from the root actually re-forces. Until that number
   exists, every estimate of the prize is the pricing instrument's, which
   assumed an identity the evaluator does not have.

## What ships in place of a boundary memo

- **HOST** ([`host-questions.md`](./host-questions.md)): rooted path values name either the ambient
  accessor or one exact store-path mount. The host owns no source-copy cache;
  cppnix's fingerprinted `fetchToStore` cache owns reuse. The same document
  owns lazy-mount recovery, canonical `writeDerivation`, and the counters.
  Target: Rust cold within 10% of cpp cold on the home target.
- **WITNESS**: `Question::WriteDrv` replayed through `write_derivation`
  (today a derivation write is recorded as `StoreText`, whose replay calls
  `allowPath` and the original did not); ATerms stored once in the CAS;
  identical rows recorded once; key tags bumped. Target: the home target's
  witness under 20 MB from 399 MB, warm-unchanged under 5 s.
- **EvalId carries the evaluator fingerprint** (from the positions
  review): an evaluator upgrade can no longer serve the previous
  evaluator's answer from a persistent row.
- Sampled verification bypasses the memo VM-wide (so a check arm cannot be
  served the row it is checking). Owned by WITNESS.

## What must be true before anyone revisits

- The persistent-evaluator measurement above, with a number.
- If the number justifies it: the design starts from `Vm` retention and
  in-place invalidation by module, keyed on pointer identity, with the
  dependency graph at tracked boundaries only, and its review starts from
  the two reviews under `reviews/` as the checklist. Any design that
  proposes to identify forced values across processes has to answer v2
  finding 2 first.
