# Source positions in the Rust evaluator

**What you get.** An error from `eval-backend=rust` names the line and column
it happened on, in cppnix's format, and `builtins.unsafeGetAttrPos` answers a
real record instead of `null`. ENG-12137. Before this, the crate carried no
positions at all: every error read `error: <message>` with no `at` line, and
that builtin answered `null` for everything under an owner-approved
divergence (ENG-12591), now retired along with its three allowlist entries.

**What you do not get.** Traces. cppnix prints a chain of `… while calling
the 'head' builtin` frames, each with its own position; this crate carries one
position, the innermost. ENG-12714 tracks the chain. There are no known
attribute-position cases where cppnix answers a record and this evaluator
answers `null`.

## How it is stored: a side table, not a wider instruction

`ir::Op` is a fixed-width `Copy` enum the interpreter fetches in its innermost
loop. Widening it by four bytes to serve a path that only runs when an
evaluation is already failing would charge every evaluation that does not, and
this crate had just recovered 37.5% of cold-eval CPU.

So each `CodeUnit` carries `spans: Vec<u32>`, parallel to `ops`, holding the
byte offset of the construct that emitted each op (or `ir::NO_POS`, which is
`u32::MAX` and not `0`, because `0` is the first byte of the file). The
`Module` carries `line_starts: Vec<u32>` so an offset resolves to a line and a
column with no IO -- the VM performs none, so it cannot re-read the file the
way cppnix's `PosTable::operator[]` does. The source text itself is not kept:
the only thing the VM would ever do with it is count newlines, which is that
array.

Nothing in the interpreter reads `spans` on a successful run. The one cost on
the hot path is `self.at_ip = u.ip` once per op in `Vm::advance_unit`, a store
to a field already in cache.

`unsafeGetAttrPos` needs a second table, because it asks about an attribute
reached through a *value* rather than about an instruction. `CodeUnit::attr_sites`
records each static name and position once, in emission order (every
`inherit` first, then the bindings): since 2026-09-04 the site is also the
op's NAME table (`MkAttrs { statics, dynamics }` pops `statics` bare values
and zips them with the static prefix through the link table, so no static
name is on the stack or re-interned per build), which fixes the order.
A suffix of the same vector records only the runtime-computed `(index among
the dynamic pairs, position)` entries. A `u32` split index occupies the
padding after `AttrSite::ip`, so `AttrSite` remains 32 bytes and still has one
vector allocation. `MkAttrs` joins only the suffix with the evaluated names.

`value2::Attrs` carries one 16-byte `AttrOrigin { module, unit, ip }` and
`Value` is unchanged. Compilation rejects a module before a real `unit` reaches
the three reserved slab tags or a real instruction reaches the `FORMALS` `ip`
sentinel.

Each `Module` has a 16-byte lazy slab handle: an 8-byte `RefCell` borrow flag
and an 8-byte optional box. A module that uses the slab allocates its 144-byte
body: three inline 48-byte slabs (dynamic literal origins, `listToAttrs`
results, projected-`Update` results), no sub-allocations
(`dynamic_origin_slab_layout_matches_the_documented_cost` in `value2.rs` pins
every number in this section). A dynamic literal slot
remains 40 bytes: an
8-byte reference count, a 16-byte boxed-slice pointer and length, and an
optional 16-byte fallback origin. `Option` uses the box pointer's null niche,
so a vacant slot is also 40 bytes. A mixed literal falls back to its compiled
static site. `MkAttrsOnto` falls back to the post-`Update` base, which
preserves override positions for names the dynamic operation did not add.

Static sets keep the same per-set storage, allocation count, and hot-path work:
one module refcount bump, two `u32`s, and the normally-false `dynamics > 0`
branch already on `MkAttrs`. They do not borrow or touch the dynamic slab.

Let `S` be the number of static bindings, `D` the number of runtime-computed
bindings, and `R <= D` the number whose evaluated name is not `null`. A mixed
site stores `S + D` 8-byte entries in one compiled vector: `(module symbol,
offset)` for the static prefix and `(pair, offset)` for the dynamic suffix.
The static prefix is in emission order, unsorted (the VM zips it with the
values it pops). Each execution sorts the `R` retained dynamic origins by VM
symbol once in O(R log R).

Each execution takes one vacant 40-byte slab slot or appends one, and persists
exactly `R` 8-byte `(VM symbol, offset)` entries in its boxed slice. Building
the slice is O(D + R log R) and adds no work proportional to `S` beyond the
ordinary insertion of those static bindings into the set. The temporary vector
reserves space for `D` entries while names are inserted. `unsafeGetAttrPos`
itself takes one shared slab borrow, then performs an O(log R) binary search of
the dynamic entries followed, on a miss, by an O(S) text scan of the compiled
static entries (a lookup arrives with text; `unsafeGetAttrPos` calls are
orders of magnitude rarer than set builds, which pay nothing for the scan).
No position is stored in a `Slot`.

`listToAttrs` has a separate entry shape in the same slab. Let `P` be the
number of unique, first-winning input pairs whose attrset has an origin. The
builtin is already walking those pairs, interning each name, and inserting
each winning value. The insertion path adds one origin-presence branch for
each unique winner. For each of the `P` sourced winners it adds one 24-byte
`(VM symbol, AttrOrigin)` entry and one `AttrOrigin` clone. It does not resolve
the pair's `value` position, line, or file. At completion the temporary vector
is sorted by VM symbol in O(P log P), then becomes one boxed slice; its backing
allocation may be shrunk to the final length. Each clone bumps its source module
`Rc`; cloning a dynamic or nested
`listToAttrs` origin also increments that slab slot's checked reference count.
The result origin holds one additional module `Rc` to the first retained
pair's module, which owns the new slab slot.

The `listToAttrs` slab is one of the three inline slabs of the body above (no
separate allocation). Each simultaneously live result occupies
one 32-byte slot: an 8-byte reference count, a 16-byte boxed-slice pointer and
length, one 4-byte `value` symbol, and 4 bytes of padding. A vacant slot is
also 32 bytes. The result's 16-byte `AttrOrigin` and `Value` stay unchanged.
If `P` is zero, the result has no origin and allocates no list slot or boxed
slice.

This is the ordinary-call cost. It is O(P) projected provenance while the set
is already being built, with no work proportional to unrelated values and no
global per-value cost. Retaining the original input list would reduce this to
constant bookkeeping but is invalid: an unused pair attribute can refer to
the result and close an `Rc` cycle through the module slab. The projected
origins contain no pair values and cannot form that cycle.

On a query, the list entry is binary-searched in O(log P), then the winning
pair's origin resolves its `value` attribute by the ordinary static or dynamic
path. Resolution returns both the source module and its byte offset. Keeping
the module is required because one `listToAttrs` call can combine pairs from
different files; interpreting every offset in the slab owner's module could
report a real line from the wrong file.

`Update` (`//`, including the compiler's `__overrides` lowering) uses a third
entry shape in the same slab. Let `A` and `B` be the operand attribute counts,
`N` the result count, and `P <= N` the result attributes whose selected
operand has a position. A sorted two-pointer walk over the operands takes at
most `A + B` steps and resolves at most one origin for each of the `N` winning
names. Equality selects only the right origin. If that origin has no position,
the projection omits the name; it never falls back to the shadowed left
origin.

An empty operand contributes no names, so `Update` reuses the other operand's
origin and allocates no projection. The formulas below count every update as
non-empty and are upper bounds.

The walk builds a sorted boxed slice of `P` 16-byte
`{ source module Rc, VM symbol, offset }` entries. Its temporary vector
reserves `A + B` entries and the boxed slice shrinks to `P`. The first live
projection therefore performs `P` source-module `Rc` clones. The first live
projected origin uses the third inline slab of the body above (no separate
allocation). Each simultaneously live result occupies one 24-byte slot: an
8-byte checked reference count and a 16-byte boxed-slice pointer and length.
A vacant slot is also 24 bytes. The result's existing 16-byte `AttrOrigin`,
`Attrs`, `Value`, `Slot`, and `ir::Op` stay unchanged.
The result origin's module `Rc` names the module executing `Update`, which owns
the projected slot; each entry carries its own source module for cross-file
positions.

The projection stores resolved positions, not child origins. A later `Update`
reads the selected positions and writes a new flat slice, so the result owns
no chain through earlier merge results. A query binary-searches the slice in
O(log P) and clones only the matching source module `Rc`.

`vm::tests::an_update_fold_keeps_bounded_projected_origin_storage` exercises
this lifetime bound with 64 attributes and 512 updates. It requires one live
slot for the returned accumulator, no more than two slots at the high-water
mark, and zero live slots after the accumulator drops.

For a fold of `M` updates with both operands bounded by `N` attributes and
every result attribute positioned, provenance reserves at most `32NM` bytes
of temporary-vector capacity cumulatively, examines at most `2NM` operand
entries, and writes at most `NM` projected entries. The boxed payload allocated
cumulatively is at most `16NM` bytes. With only the accumulator retained, the
final live projected provenance is at most `24 + 16N` bytes, independent of
`M`; it is exactly that size when all `N` names have positions and a
projection was needed. If this is the module's first slab use, the lazy
structural bodies add `64 + 48` bytes; the 16-byte handle already exists in
every `Module`. For the usual fold over ordinary source sets, the old and new
accumulators coexist while one update runs: boxed projected payload peaks at
`32N`, temporary capacity adds at most `32N`, and the slot-vector high-water
mark is two 24-byte slots. If the current right operand is projected too,
boxed payload peaks at `48N` across left, right, and result; before the result
is boxed, the two inputs plus temporary capacity peak at `64N`. If a program
deliberately retains all `M` result sets, their provenance is at most
`M * (24 + 16N)` bytes. Each live value owns one flat snapshot, and the final
value retains none of its predecessors.

If every selected origin is itself a projected accumulator, each of the at
most `N` resolutions per update binary-searches at most `N` entries. The fold
therefore adds at most `MN * ceil(log2(N + 1))` symbol comparisons after the
`2NM`-step merge walk: O(`MN log N`) provenance work.

Resolution keeps the source origin's existing lookup cost. A dynamic origin
with `R` runtime names costs O(log R) before its static O(S) fallback; a
`listToAttrs` origin with `Q` winners costs O(log Q) before resolving the
pair's `value`. For one-level inputs with `R, S, Q <= N`, a fold adds O(`MN log
N`) provenance work. Nested `listToAttrs` delegation adds the logarithmic
lookup costs of the retained pair origins exactly as described above. It does
not add projected entries or change any byte bound.

This flat snapshot was chosen over a depth-limited chain because `Update`
already copies the left `BTreeMap` and inserts the right values. A threshold
chain would add membership slices, tombstone rules, recursive lookup, and a
second representation after flattening. The snapshot has one provider rule,
one lookup shape, and no depth state. Storing a position in every `Slot` would
charge sets that never pass through `Update`; this scheme charges only the
derived set that needs more than one origin.

Reclamation is reference counting, not a scan. Cloning a dynamic `AttrOrigin`
bumps the module `Rc`, borrows the slab, and increments one checked `usize`;
dropping it performs the matching decrement. The last drop frees the boxed
slice and fallback, marks the 40-byte slot vacant, and pushes its 4-byte index
onto the free list. The slab retains capacity at the high-water mark of
simultaneously live origins and reuses those slots, so repeated sequential
requests do not grow it. Static origin clone/drop remains one module `Rc`
operation. `Vm::reset` drops frame-owned origins; releasing the last C handle
drops handle-owned origins; session destruction drops both. Origins reachable
through retained handles stay live.

`listToAttrs` and projected result origins use the same reference-counting
rule in their own slot vectors. The last drop releases the boxed slice, marks
the slot vacant, and pushes its 4-byte index onto that sub-slab's free list.
All three slot vectors retain capacity at their independent high-water marks.

A dynamic literal has one fallback edge, not an accumulating chain. For a
mixed literal the edge reaches the static view of the same compiled site. For
`MkAttrsOnto` it reaches the one post-`Update` base. The compiler emits at most
one `MkAttrsOnto` for a rec set, so this edge does not grow with repeated
execution. A `listToAttrs` slot owns only the `P` winning pair origins, and its
last reference releases those origins after the slab borrow has ended. A
projected slot owns no `AttrOrigin`, so `Update` cannot extend either chain.

## What has a position

Errors, everywhere the VM raises them. Attribution happens in `Vm::advance`,
for every frame kind and not only `Frame::Unit` -- `throw`, `abort` and a
builtin's argument type errors all raise from inside a `Frame::Task`, and
those are the errors users see most.

Every op the compiler emits except the synthesised `Ret`, which has no token
and cannot fail. Measured over the corpus in `compile::span_tests`: 331 ops,
250 positions, all 81 gaps a `Ret`.

Attributes for a set that came from source: a literal, a `rec` literal, an
`inherit` list, a dynamic `${e} =` binding (captured when `MkAttrs` knows the
evaluated name), and every component of a nested attrpath. `inherit (e)` uses
the inherited name in the list, not the selected attribute's old position.
`listToAttrs` attributes take the winning input pair's `value` position. Both
it and `//` retain the source module per name, so one result can report
positions from several imported files.

Formal parameters give `builtins.functionArgs` its positions. `//` projects
each surviving name from the operand that supplied its value. `removeAttrs`
keeps its input origin, and `intersectAttrs` takes the second set's origin. The
Nix-level `filterAttrs` composition is `removeAttrs` over the rejected names,
so it follows the same survivor rule.

Selectors and list producers copy value slots. `getAttr`, `catAttrs`,
`attrValues`, `head`, and `elemAt` therefore preserve the origin of an attrset
stored inside the selected slot. `attrValues` sorts by attribute text before it
copies those slots.

The `nix-eval-driver` entry point (#184) does not print positions. Its
failure text is compared byte for byte against the C++ CLI's by
`rust-driver-parity.sh`, and cppnix's CLI puts no `at file:line:col` inside
that string, so `run.rs::failure_of` drops the position on purpose rather than
for want of one. The bridge path (`eval-backend = rust` under `nix` and
`nix-instantiate`) is the one that renders them.

## What answers `null`

The remaining cases match cppnix. No known attribute case remains where one
evaluator has a source position and the other does not.

| case | cppnix | here | |
|---|---|---|---|
| attribute not in the set | `null` | `null` | matches |
| text with no file behind it (`--expr`, the REPL) | `null` | `null` | matches (`eval.cc`'s `mkPos` builds a record only for a `SourcePath` origin) |
| `mapAttrs` or `zipAttrsWith` result name | `null` | `null` | both build new no-position attributes |
| `fromJSON` or `fromTOML` result name | `null` | `null` | parsed data has no source attribute token |
| direct `derivationStrict` result name | `null` | `null` | the primop synthesizes the result set |

The Nix `derivation` wrapper is mixed rather than positionless. Fields copied
from the caller keep the caller's file. `all`, `drvAttrs`, `out`, `outPath`,
`drvPath`, `type`, and `outputName` point into
`/derivation-internal.nix`; an absent `outputs` answers `null`.

The former `listToAttrs` and mixed-`Update` rows could not remain divergences
because this answer feeds a derivation hash. Home Manager's `lib.mapAttrs'`
lowers to `builtins.listToAttrs`. The module definition-provenance walker asks
`unsafeGetAttrPos` about the result. A missing answer changed
`provenance.json`, which is a derivation input:

```text
files.".config/fish/functions/y.fish".definitions[1].line
cpp = 793      rust = null
```

That produced a reproducible `mismatched=1 value-mismatch` at
`homeConfigurations."andrewgazelka@hydra".activationPackage.drvPath`. The
builtin's answer is therefore evaluator output that can determine store
paths.

`{ a = 1; } // { b = 2; }` now answers both columns from its flat projection.
The compiler-generated `Update` for
`rec { __overrides = { a = 20; }; a = 1; b = 2; }` selects the override's
position for `a` and the rec literal's position for `b`. Appending dynamic
bindings with `MkAttrsOnto` delegates to that projection for names it did not
add.

## Reading a position back out

`SrcPos { file: Option<Rc<str>>, line, column }` travels out of the crate three
ways, all added by ENG-12137: `EvalError::pos()`, the `IxePos` out-parameter on
`ixe_eval_expr` and `ixe_session_take_error`, and a `"pos"` key in the cached
`EvalResult`. The current result schema requires that key; an empty array means
the evaluation had no position, while a missing or wrongly typed field is
corruption.

`EVAL_ID_TAG` beside the identity codec in `rust/nix-eval-rs/src/readset.rs`
owns the result-cache identity version. The identity includes
`modcache::compiler_fingerprint()`, so evaluator semantics can retire a row
even when the source modules stay unchanged. Compiled module objects carry
their format inside the canonical map and reject an untagged earlier object
during decoding.

On the C++ side `rustEvalPos` turns that back into a `std::shared_ptr<const
Pos>` and `rustEvalThrow` attaches it, which is what makes the `at
/path:LINE:COL:` line and its source excerpt appear. That needed a new public
`EvalErrorBuilder<T>::atPos(std::shared_ptr<const Pos>)`: `ErrorInfo err` is
protected, and the existing `atPos` overloads all take a `PosIdx` into
cppnix's own `PosTable`, which an embedder computing positions elsewhere has
no way to produce.

## Columns are bytes, and lines end three ways

cppnix's column is `1 + (offset - lineStart)` over a **byte** offset, so a line
with multi-byte characters before the column reports the same number on both
evaluators only if this one also counts bytes. It does; `columns_count_bytes`
pins it with an `é`.

A line ends at `\n`, at `\r\n`, or at a bare `\r`, which is what
`Pos::LinesIterator` accepts. Counting only `\n` would drift on any file
written with `\r`, silently and only on those files.

## Verifying against cppnix on macOS: use `/private/tmp`

`/tmp` is a symlink to `/private/tmp` and cppnix does not resolve it, so
`SourcePath::readFile()` throws, `getSource()` returns nullopt, and
`PosTable::operator[]` degenerates to `lines = [0]`: **every position reports
line 1 and column `offset + 1`, with no source excerpt.** It looks like a
positions bug and it is not one -- the system nix does the same. Any oracle run
under `/tmp` is measuring that instead of what you asked. Every expectation in
`tests/positions.rs` was taken under `/private/tmp`.
