# What read-set invalidation reaches, and what it does not

Written 2026-08-04 against `5d39e1691`. Records the measurements behind the
input-source recording in `derivationStrictInternal`, because they are easy to
misread in a way that makes a cache built on them unsound.

## Recall is a property of the edit, not of the implementation

Two edits to the same file, same host, same evaluation target
(`nixosConfigurations.hil-compute-1.config.system.build.toplevel.drvPath`),
same binary. One is complete and the other is not:

| edit | derivations moved | reached |
| --- | --- | --- |
| `network.tailscaleIpv4` `.115` -> `.117` | 22 entries over 17 paths | 22 of 22 |
| `network.publicIpv4` `.75` -> `.76` | 73 | 68 of 73 |

**Do not quote 22 of 22 as a general claim.** It is complete for edits whose
effect reaches a derivation through a store reference, which is what the
input-source recording closed. It says nothing about values.

## What the input-source change was, and what it was not

It is not value provenance, not string-context taint, not a side table and not
`Env` tagging. Those were all considered and none of them shipped, because the
measurement said the problem was somewhere else.

Diffing every field of all 17 distinct derivations the tailscaleIpv4 edit moves:
16 move because their `inputDrvs` moved, exactly 1 because its `inputSrcs` moved
(`claude-skills-farm`, which link-farms `paths.skills + "/${name}"` and so
consumes the flake's whole-tree store path), and none by value. The 7 that
nix#41 missed were one chain, five deep, whose every edge was already in the
trace and whose root was never seeded:

    claude-skills-farm -> claude-skills -> claude-code-launch-spec.json
      -> claude-code-2.1.220 -> ix-vm-tools

`derivationStrictInternal` read `drv.inputDrvs` and never `drv.inputSrcs`. That
is the whole defect and the whole fix.

## The class that is still open

On the publicIpv4 edit, 5 derivations are unreachable. They trace to 3 roots
identifiable by one property: **every input set is byte-identical and only the
structured attributes differ**, so no input source, no input derivation and no
file read by that boundary names the change.

    unit-10-wan.network                     Address=15.204.111.75/32 -> .76/32
    ix-server.toml
    unit-ix-system-deploy-record.service

`ENG-12310` tracks this. The mechanism it names as cheapest-untried is
propagation along attribute selection: `noteProduces` registers only an import's
top-level value, so selecting a sub-value off it finds nothing in
`entryByValue`.

## The evidence that says what not to try

Closing each missed root backwards over its recorded edges (`~/gap.py` on
dev-compute-6) shows:

- The import boundary `/nix/inventory/nodes/hil-compute-1.nix` **is a direct
  seed**. Its read set changed. The evaluator observed the edit.
- The three roots reach 819, 3,231 and 8,772 producing entries respectively,
  and **not one is any inventory node import**.

The producer is correctly dirty and the consumers are richly connected, so the
gap is neither under-instrumentation nor mis-seeding. More seeding cannot help
(the seed is present), better input naming cannot help (the input is named and
already compares unequal), and further input classes cannot help (no input of
any class is what is missing). Only an edge between a value's origin and its use
closes it.

The distinction worth carrying: a string **read from a file** and embedded in a
derivation is already covered, because the read lands in the consuming entry's
read set. A string **originating as a Nix literal in an imported file** is not,
because only the import boundary records anything.

## Traps that produced confident wrong answers

**Comparing derivations.** A naive reader of `nix derivation show` returns a
clean zero that reads as "nothing moved". Three separate ways, all hit in one
session: the output is wrapped in a `derivations` envelope; schema version 4
nests inputs as `inputs.srcs` and `inputs.drvs` rather than `inputSrcs` and
`inputDrvs`; and `structuredAttrs` sits beside `env` rather than inside it, so a
derivation whose only moving part was a string reports no differing env key at
all. Use a comparison that asserts the top-level fields it saw are ones it knows
how to compare, so the next schema move fails loudly instead of returning zero.

**Contention sensitivity differs by metric.** Recall is a set comparison and is
unaffected by load. The invalidation share is cpu-weighted and the root entry's
exclusive time is exactly what absorbs contention, so a build running alongside
a traced eval moved a reading from 4.2% to 5.6%. Measure invalidation on an idle
box; recall can run anywhere.

**Entries are not derivations.** The 22 that move are 17 distinct store paths;
five are second instantiations of the same derivation. Which denominator you use
changes what a recall number means.

**Runtime.** A `--no-eval-cache` evaluation of that host is about 180s untraced
and about 210s traced, not the 25s quoted in
`doc/nix-incremental-eval/design.md`.

## Reproducing

`contrib/readset-analyze.py --compare before.jsonl after.jsonl` reports recall
and invalidation, keyed four ways; the `tree` keying is the model. Trace with
`--option read-set-trace-file`. (Inputs are always lazily mounted now; the
`lazy-trees` setting the original runs disabled no longer exists.)

Harnesses used for the numbers above are on dev-compute-6 under `~/measure5`
through `~/measure8` (trace pairs and analyses; 7 and 8 are the like-for-like
base-versus-patched arms, 6 is the value-flow arm), with `~/missed.py`,
`~/pairs.py`, `~/drvdiff2.py`, `~/gap.py` and `~/offcost3.sh`. Claim the node
with `ix-dev-claim` before using it.
