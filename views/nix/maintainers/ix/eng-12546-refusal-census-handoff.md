# ENG-12546 part 2: where the refusal census stands

Handoff for whoever continues branch `task-58-census`. Written so the reasoning
does not have to be reconstructed from chat.

## State: the Rust side is done, the C++ side compiles but is not wired up

Two commits on the branch, neither merged, no PR open.

- `54aa0d992` — the refusal-token vocabulary and its ABI enumeration.
  **Verified**: 240 crate tests, `cargo clippy --all-targets` rc 0 adding no
  warning over `ix-patched`, and the enumeration guard watched failing.
  **The clippy claim was true of this commit and not of the tree.** It was
  measured as a delta -- this branch adds nothing over `ix-patched` -- and
  `ix-patched` was already red: at 4c02bed96 `cargo clippy --all-targets -p
  nix-eval-rs` exited 101 with 41 errors against the workspace's own
  deny-list, all of them in test and example code that predates this branch.
  Read as an absolute "clippy is clean" it was wrong, and that reading is the
  easy one. Fixed and gated by `maintainers/ix/rust-crate-gate.sh`.
- `1202cb6a6` — `RefusalCensus` (libexpr) and the `refuse()` helper
  (then `src/nix/rust-eval-refusal.hh`, moved to
  `src/libexpr/include/nix/expr/rust-eval-refusal.hh` by ENG-12711 once the
  evaluator itself needed to raise one). Its subject says `[NOT YET COMPILED]`.
  **That label is now stale and this file is the correct account**: the commit
  was built on dev-compute-2 after it was written, `ninja` rc 0. The label was
  accurate when written and is left rather than rewritten, because a
  force-push to correct a subject is a worse trade than a note that supersedes
  it.

What is *not* true yet: `rust-eval-refusal.hh` is included from nowhere, so
`refuse()` itself has never been compiled. The census beneath it has.

## Proven on a dev node: the `<4>` prefix, both directions

This is the claim the census rests on, and it holds. On dev-compute-2:

```
=== POSITIVE (<4> prefix) ===
PRIORITY=4  MESSAGE=rust-eval refusal token=command-apply detail=nix eval --apply
=== NEGATIVE (no prefix, severity in the body) ===
PRIORITY=6  MESSAGE=warning: rust-eval refusal token=command-apply detail=nix eval --apply

pos visible at -p warning: 1
neg visible at -p warning: 0
```

The two `command-apply` lines above are what the journal held on the day; the token was retired when `nix eval --apply` was served (rust-eval round 5), so a fresh census will not show it.

The unprefixed line — the one that carefully says "warning" in its own text —
is invisible to the severity filter. It is in the journal, it looks right to
anyone reading raw output, and it does not exist to the query a census runs.

Also worth keeping: journald **strips** the `<4>` from the stored `MESSAGE`, so
the census reads a clean line and the prefix costs nothing downstream.

### How this nearly went wrong

The first run of that pair returned **empty output from both queries**. The
tempting reading is "no priority difference". The correct one is "I do not know
whether anything ran" -- and it was the second: `systemd-run --user` returned
rc 0 while writing to a journal the query was not reading, so the `||` fallback
to the system bus never fired and both sides produced nothing.

An empty result and a negative result look identical and mean opposite things.
Anyone re-running this should confirm the units actually executed before
reading anything into what the journal does or does not contain.

(For `maintainers/ix/retro-2026-08-05-guards.md`, which gate-hardener is
collecting: an empty result is "I do not know whether anything ran", not "no
difference". Third instance on 2026-08-05, after the clippy baseline that did
not compile and ENG-12599's SIGPIPE.)

## Two designs that must survive

### 1. The journal is the production census; `NIX_SHOW_STATS` is not

In production a refusal is **fatal**: the command throws and the process exits.
So any census that depends on orderly shutdown structurally under-counts the
thing it measures — and under-counts *silently*, reading as "no refusals"
exactly when a run died on one.

The specific hole: `nix-instantiate --eval` is the only command serving the
Rust backend today, and `nix-instantiate.cc` calls `state->maybePrintStats()`
as a straight-line statement on the success path, after the throw. `nix eval`
is better (`EvalCommand::~EvalCommand` runs during unwinding) but four of its
refusal sites throw before `getEvalState()` exists, so there is no `EvalState`
to print from.

Therefore: the journal line fires at the moment of refusal and rides journald
into ClickHouse, a channel that outlives the process, so **the journal is the
production census**. `NIX_SHOW_STATS` is the local-debugging view and, once
shadow mode lands, the shadow view — where a refusal is caught rather than
fatal and the process lives to report it. Size the histogram for shadow in
part 3; do not size it for production, where it cannot see anything.

The counters are process-wide rather than `EvalState` members. That is a
deliberate departure from the `nrCppEvals` mechanism: four sites have no
`EvalState`. The invariant worth keeping is **one accounting path feeding one
derivation in the stats block**, not the storage location, and when the two
conflict the mechanism yields to the invariant.

### 2. Every C++ token must resolve in the ABI enumeration

Command-layer tokens are declared as constants in `rust-eval-refusal.hh`, not
as string literals at each throw site, so that a guard can hold every one of
them against `ixe_refusal_token_count` / `_at`. **That guard is not written
yet and is required.** Without it a typo mints a category that exists in
nothing but that header, and a histogram row nobody can explain is worse than
a missing one.

The vocabulary is deliberately one list, shared with the evaluator's tokens,
in `rust/nix-eval-rs/src/refusal.rs`. `RefusalToken::raised_by` records which
layer raises each, so "the evaluator refused nothing" is a query rather than an
inference from name prefixes, and moving a refusal between layers is a visible
edit. Two hand-maintained vocabularies would drift the moment either side
gained a kind.

## Remaining, in order

1. Include `rust-eval-refusal.hh` and convert the ~13 throw sites in
   `src/nix/eval.cc`, `src/nix/nix-instantiate/nix-instantiate.cc`,
   `src/nix/nix-instantiate/rust-eval.cc` and `src/libcmd/rust-eval-session.cc`.
2. The phantom-key guard from design 2 above.
3. The `nix-instantiate` scope guard so stats survive unwinding, break-tested
   by refusing and asserting stats now appear on stderr where they did not.
4. Whole-feature PR.
5. Part 3: `eval-backend=shadow` at the `requireBackendCanServe` chokepoint,
   with the counted identity `served + refused + mismatched + crashed ==
   shadow_attempts`, attempts incremented **before** the rust call so a shadow
   that dies mid-call leaves a visible hole rather than a quiet zero. Watch it
   fail by killing the rust side mid-evaluation. Comparison bar: tier 1
   byte-exact for anything feeding a hash, tier 2 functional for presentation.

## Where this builds

**Anywhere, including this Mac.** An earlier revision of this file said nix's
C++ does not build on the Mac. That was wrong and cost a day's dev-node chase
on 2026-08-06: a cold build with `-Dnix:rust-eval=enabled` took about six
minutes on an aarch64-darwin laptop and incremental relinks about 40s. What a
Mac build cannot give you is Linux or fleet behaviour, so a C++ change is
still unverified ON THE FLEET until it has run on a dev-compute node. dev-compute-2 has a
warm meson dir at `~/eng-12532/nix/build` (mine, reusable).

dev-compute-6 has warm dirs too, but check before reusing: the paths are
`~/eng12540/nix/build` and `~/eng12540/nix/build-rust` (**not** `~/eng12540/`
directly, which holds only logs), and both were last touched at 13:12–13:13 on
2026-08-05 — minutes before this was written. A build directory that was hot
that recently may still have an owner, and two agents in one build dir is worse
than a cold build. `~/eng12543/base/build-rust` was equally hot. Look at the
timestamp before assuming a dir was left for you.
