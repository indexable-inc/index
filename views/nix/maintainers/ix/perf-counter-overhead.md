# What the evaluator counters cost, and what they answered first

What this document records: three attempts on this laptop failed to measure
what the counters cost, and a fourth on a claimed dev node succeeded. The
answer ("Fourth attempt" below): the shipping counters are unresolvable
against a 95% upper bound of +0.43%, and `perf-ops` costs +0.36%. What the
counters themselves answered is solid and is the rest of the file, starting
with ENG-12862's question, which was **13,672 `Entries` questions** on a
minimal NixOS toplevel.

Read the two halves differently. A count here is exact and repeatable to the
unit. Every timing here has been wrong at least once.

## RETRACTED: the first on/off pair compared one binary to itself

The pair originally published here, 8.802s against 8.688s, **was not an A/B.**
Both numbers came from the same binary.

`src/nix/meson.build`'s cargo `custom_target` carries `build_always_stale :
true`, so every `ninja` re-runs `cargo build --release -p nix-eval-rs` with
default features and overwrites `build-rust/src/nix/libnix_eval_rs.a`. The
measurement script built the crate `--no-default-features`, copied the archive
into the build directory, and then relinked, which threw the copy away and
rebuilt the default. The 1.3% difference was noise between two runs of one
program, and the "under about 2%" bound had nothing behind it.

Caught by trying to run `perf-ops` and noticing the report said
`ops_counted=false` after a build that asked for the feature. The same
mechanism had made `perf-ops` unreachable from the only build that links the
crate: there was no way to turn it on at all.

Two fixes, both in the same change as this correction. A meson option,
`rust-eval-cargo-features`, passes cargo flags through the custom target so a
configuration difference survives a relink. And `ixe_perf_snapshot` returns
null when the counters are compiled out, because with them off it had been
rendering `compiles=0 questions=0 interns=0`, and a stats block of zeros reads
as "the evaluator did no work" rather than "this build cannot count", which is
the same ambiguity this file claims elsewhere to have designed away.

## The on/off pair, done with configurations that differ

Three configurations, each reached by `meson setup --reconfigure` rather than
by copying an archive, seven runs each:

```
counters off (--no-default-features)  median= 9.420s  n=7  8.813 .. 11.262
counters on  (shipping default)       median= 8.801s  n=7  8.739 ..  9.725
per-op counter (--features perf-ops)  median=21.092s  n=7 10.773 .. 30.448
```

**The coarse counters are still not resolvable, and now for an honest reason.**
The "off" configuration measured *slower* than "on", which no counter cost can
explain, so noise dominates. These runs put this machine's noise floor at
roughly plus or minus 1.5 seconds, about 17%, after a session of concurrent
builds. So the correct statement is not "under 2%": it is that **this setup
cannot measure the coarse counters at all**, and a quiet machine is needed to
put a number on them. The earlier bound was wrong twice over, once for the
invalid A/B and once for claiming a resolution finer than the noise.

**The per-op counter is expensive and that much is clear through the noise.**
21.1s against 8.8s is 2.4x. It stays off by default and the spread on that row
(10.8 to 30.4) is why no tighter figure is quoted.

> **This paragraph is also withdrawn.** See the section below: a third attempt,
> seven runs on each of three genuinely distinct binaries, put the per-op arm
> at 11.4s against a shipping arm at 11.7s. Whatever produced the 21.1s median
> was not the counter.

The counters that are always on are coarse by construction -- one increment
per compile, per host question, per store-path computation -- plus two that
are not obviously coarse and are the reason this was measured rather than
assumed: 8.7 million interns and 793 thousand constant-pool insertions on the
run above.

Since 2026-09-04 the default-on set also carries the allocation census
(`alloc.*`: every slot cell, attribute set and entry, list and element,
frame and frame slot; cppnix's `nrValues` family). That one is not coarse.
On the hil-compute-1 NixOS toplevel it counts 160.7M slot cells (52.7M
values, 91.4M thunks, 16.6M pending applies), 12.4M attribute sets holding
279.6M entries, 16.1M lists holding 31.5M elements and 39.5M frames holding
60.3M slots: about 340M thread-local increments per run, predicted ~0.34 s
at ~1 ns each, 0.5% of a 66 s evaluation. The bed pair on dev-compute-4
(same tree with and without the census, plain env, one run per arm, output
identical): with 65.76 s cpuTime, without 66.40 s and 66.33 s. The predicted
cost sits below what one run per arm resolves: two runs of one binary were
0.07 s apart, two binaries 0.6 s apart the other way, so the honest figure
is "under 1%, sign not resolved", and the +0.43% bound at the end of this
file, measured before the census existed, does not cover it.

`perf-ops`, the per-IR-op counter, is a **separate feature and off by
default**, because that one really is in the innermost loop. The fourth
attempt below prices it at +0.36%; a run with it on reports `ops_counted=true`
so a reader can tell a real zero from an uncompiled one.

Reproduce with the loop in this file's git history, or:

```
cargo build --release -p nix-eval-rs                        # on
cargo build --release -p nix-eval-rs --no-default-features  # off
```

then relink `ninja -C build-rust src/nix/nix` between the two.

## What they said, first run

aarch64-darwin, `build-rust/src/nix/nix-instantiate` sha256
`5e3bc84b5e77ad5048237952e00b4bc62437ff16c9cf056e1faa5a5808a70b5a`,
`eval-backend = rust`, nixpkgs
`/nix/store/llgwlxshmy0ifvxh7f8wq53vk5x7vd13-source`, the minimal NixOS
`system.drvPath`. `NIX_SHOW_STATS=1`, `rustEvalPerf` block:

```
cpuTime                  8.63 s
compiles                3,172
compile_ns      3,254,687,743      37.7% of cpuTime
compile_bytes      36,229,437
questions              39,152
question_ns     2,068,823,146      24.0% of cpuTime
hashes                 13,135
hash_ns            14,744,502       0.17%
interns             8,738,139
konsts                793,343
ops                         0      (ops_counted=false)
```

By question kind:

```
q.Entries          13,672
q.StorePath        12,078
q.WriteDrv          5,666
q.Import            5,576
q.StoreText           643
q.FindFile            634
q.NixPath             634
q.Env                 156
q.StoreFiltered        40
q.Kind                 29
q.Contents             18
q.Exists                6
```

By yield kind, which is the task-machine layer:

```
y.Force         7,354,807
y.Done          4,078,659
y.Apply         1,695,480
y.Sub             269,301
y.Need             38,121
```

**These are not cppnix's `nrThunks` and `nrFunctionCalls`, and comparing them
directly is a mistake.** A `Yield::Force` is a *task machine* asking for a slot
to be forced; a thunk forced inline through `Op::GetLocal`'s fast path never
becomes one. A `Yield::Apply` is a machine applying a function, not every
application in the program, most of which are IR ops. So `y.Force` at 7.35M
against cppnix's `nrThunks` at 6.09M is a coincidence of magnitude, not a
like-for-like ratio.

What they do measure is the continuation layer: how often a builtin or task
suspended and had to be resumed. The op layer underneath needs `perf-ops`, and
that is the number still missing for a real per-instruction cost.

## The residual, and what an op costs

`perf-ops` on the same expression: **56,042,300 IR ops**. That count is a
property of the program rather than of the instrumentation, so unlike the
timings above it is not affected by the counter being expensive.

The residual is what neither the compile timer nor the question timer accounts
for: about 38% of an 8.6 second run, so roughly 3.3 seconds. Over 56.0M ops
that is about **59 nanoseconds per op**.

**Read that as an upper bound on dispatch, not a dispatch cost.** The residual
is everything outside compile and host questions, and that includes the
8,738,139 interner probes (about 8% of the run on its own), the allocator
traffic the sampled profile put at 13.7% of self time, and every attrset and
list operation. A dispatch loop costing 59ns per op would be remarkable; the
likelier reading is that dispatch is a small part of 59ns and the rest is work
the ops do.

Splitting that further needs per-op-kind counts, which is one more array
indexed by `Op` discriminant. **That is now built** (ENG-12994) and the split
is in `maintainers/ix/nixos-toplevel-profile.md` under "The residual,
decomposed by op": 29% of the dispatches are call and return, 23% allocate,
20% are environment lookups, and the largest single kind is `Ret` at 17%.

It remains a decomposition of the op population and not of time, for the
reason this section already gives: a clock read costs more than the op it
would time.

## Third attempt, and why this machine cannot answer the question

The pair above was taken while the machine was busy. So was this one, and this
one is worse for the earlier conclusions rather than better. Seven runs on each
of three configurations, reached by `meson setup --reconfigure` through the
`rust-eval-cargo-features` option, so the three binaries genuinely differ --
which is the thing the first attempt got wrong, and the sha256s prove it here:

```
off      (--no-default-features)  sha256 352f1c90cd87a63e...  median 10.895s  10.578 .. 12.185
on       (shipping default)       sha256 c60763706bf99078...  median 11.656s  11.164 .. 11.957
perf-ops (--features perf-ops)    sha256 0bb1c22e34330dc1...  median 11.353s   9.084 .. 11.810
```

**The ordering is impossible, which is the finding.** Counters can only cost
time, so the medians must run off <= on <= perf-ops. They do not: the arm with
the most counters is *faster* than the arm with fewer, and the gap between the
extreme configurations, 0.8s, is smaller than the run-to-run spread within
either of them, 1.6s and 2.7s. Nothing here measures a counter. It measures
this laptop.

That also disposes of the one timing claim the previous section thought had
survived. `perf-ops` is not 2.4x. On these seven runs it is indistinguishable
from the default build, and the honest position is that **all three counter
costs are unmeasured**, not just the coarse ones.

**Why `cpuTime` did not save it.** These are CPU seconds, not wall clock, so
scheduling delay is already excluded, and the numbers still will not sit
still. On this host, with roughly fifty agent sessions sharing it and a load
average of 44, the two mechanisms that inflate *CPU* time for identical work
are core-type placement -- a run landing on efficiency cores burns more cycles
for the same instructions -- and memory and cache contention. Neither is
visible in a load average and neither is excluded by measuring CPU rather than
wall.

**What it would take.** A host with nothing else on it, which on this fleet
means a claimed dev-compute node rather than this laptop, and the run pinned
to performance cores. That run has now happened and is the next section; the
figures there supersede everything above, and they are the only counter
overhead figures in this file worth quoting.

## Fourth attempt: a quiet dev node answers it

dev-compute-4, claimed, machine settled to load 0.42, every run pinned to
core 8 with `taskset`. Twenty rounds per configuration, interleaved
off/on/perf-ops within each round so drift hits all three arms equally,
60/60 rows, zero failures, zero bad answers. Workload: `nix-instantiate
--eval --strict` of the minimal NixOS toplevel, `eval-backend = rust`,
`cpuTime` from `NIX_SHOW_STATS`. The three binaries genuinely differ:

```
off      (--no-default-features)  sha256 29b89962f3aeee02...
on       (shipping default)       sha256 29020ad014c80c13...
perf-ops (--features perf-ops)    sha256 89973cecf82e33ba...
```

and the stronger identity check holds across all 60 stat dumps: `off` emits
no `rustEvalPerf` block at all, `on` emits it with `ops=0`, `perf-ops` with
`ops=56042300` on every one of its 20 rounds. The `on` arm reproduces the
published counts exactly (interns 8,738,139, konsts 793,343, questions
39,152, compiles 3,172).

```
off      n=20 median=12.4890s  sd=0.080
on       n=20 median=12.4755s  sd=0.123
perf-ops n=20 median=12.5415s  sd=0.072

on - off:       median -0.0250s  sign test p=0.263   95% CI [-0.52%, +0.43%]
perf-ops - off: median +0.0450s  sign test p=0.0118  95% CI [+0.02%, +0.72%]
perf-ops - on:  median +0.0735s  sign test p=0.0118
```

**The shipping counters of that build are free at any resolution that
matters.** (The allocation census postdates this run; it is priced above.) The
prediction, derived from the source before the timings were read, was ~23.1M
thread-local `Cell<u64>` increments per run (13.44M yields, 8.74M interns,
0.79M konsts, the rest smaller) at ~1ns each: +0.023s, or 0.19% -- which is
0.76x the 30.1ms standard error of the difference, so the null result is the
*predicted* outcome, not evidence of exactly zero. The measured 95% upper
bound is +0.43%.

**`perf-ops` is real and small: +0.36%.** Implied cost ~1.31ns per dispatch
over 56,042,300 ops, so the ~1ns increment model is confirmed to within
resolution. This retires the retracted 2.4x figure by a factor of about 65:
that number was this laptop, never the counter.

The medians, sign tests and build identities are recorded on ENG-12859
(comment of 2026-08-06); the sweep itself was the nixos-frontier pass-2 run
on dev-compute-4. The sign tests are paired within rounds, which is what the
interleaving buys.

## What that changes

**The compile share is 37.7%, not a range.** Five sampled runs put it between
12.9% and 32.6% and the median at 27.2%
(`maintainers/ix/nixos-toplevel-profile.md`), because the phase is
front-loaded and a sampler attaches at a variable offset. The counter says
3.25 seconds of 8.63. That is above the top of the sampled range, so the
sampled estimate was not merely imprecise, it was biased low, and every
sizing done against it understated compile.

**Host questions are 24.0%, and `Entries` is the largest single kind.** 13,672
of 39,152. ENG-12862 asked for this count and the shape it implies: there are
40 `StoreFiltered` copies in the same run, so the walk opens roughly 342
directories per filtered copy. cppnix's `readDirectory` share on the same
expression was 0-3% of a run a fifth as long.

**Denominators for the two floors.** ENG-12860's `Compiler::konst` linear scan
runs 793,343 times across 3,172 modules, so the quadratic is per-module over
an average of 250 constants. ENG-12861's interner is probed 8,738,139 times,
which at the ~8% of runtime the profile attributed to it is roughly 80
nanoseconds per intern -- consistent with a `BTreeMap` doing several full
string comparisons, and the number to beat with a hash map.

## What these counters are not

They are not in the memo key and are not reachable from a Nix program. The
evaluator performs no IO for them: it accumulates, and
`ixe_perf_snapshot` hands the numbers to the embedder, which decides whether
anyone looks. A perf module that read an environment variable itself would be
the same defect as `getEnv` reaching `std::env` behind `Host`'s back, which
this crate has had once already.

The block is **absent** rather than zeroed when the Rust arm did not run. A
block of zeros would read as "the rust arm did no work" instead of "the rust
arm did not run", and the difference is the whole point of having it.
