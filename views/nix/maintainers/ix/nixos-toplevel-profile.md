# Where a NixOS toplevel evaluation spends its time on the Rust backend

A minimal NixOS `system.drvPath` evaluates to the same store path on both
backends, and takes 9.4 seconds on the Rust one against 1.8 on cpp. This
records where those seconds go, what is a named defect with a fix behind it,
and what is not yet explained. It is a measurement, not a plan: nothing here
was optimized, and each recoverable item is a ticket.

## What was measured, exactly

One binary, differing only in `eval-backend`. Anything comparing two binaries
would be comparing compilers as much as evaluators.

```
host      aarch64-darwin (M-series laptop)
rev       ix-patched 770f0f0fa16ea08d860925e504733049bc7969f8
          plus this branch's builtins.filterSource commit. The branch was
          later rebased onto 262a294bb, which added only maintainers/ix
          documents and touched nothing this measures; the numbers below are
          from the build named by the sha256 on the next line and were not
          retaken.
bin       build-rust/src/nix/nix-instantiate
          sha256 ffa4b4f2a3a2d829bbc3ed67d12943e6e6d922b338cf55dce18a1e519d49125c
nixpkgs   /nix/store/llgwlxshmy0ifvxh7f8wq53vk5x7vd13-source
config    extra-experimental-features = rust-eval
          lint-{url,short-path,absolute-path}-literals = warn
```

The expression, and both answers:

```nix
(import <nixpkgs/nixos> {
  configuration = {
    boot.loader.grub.enable = false;
    fileSystems."/" = { device = "/dev/sda1"; fsType = "ext4"; };
    system.stateVersion = "24.05";
    documentation.enable = false;
  };
  system = "x86_64-linux";
}).system.drvPath
```

```
rust  9.42s  "/nix/store/hhng3cfvzlypji8zmhg13pwrwjmis12a-nixos-system-nixos-26.11pre-git.drv"
cpp   1.73s  "/nix/store/hhng3cfvzlypji8zmhg13pwrwjmis12a-nixos-system-nixos-26.11pre-git.drv"
```

Before `builtins.filterSource` landed, the rust arm did not reach an answer at
all; it refused 10.4 seconds in. A profile of a run that fails is a profile of
a different program, so everything below is from runs that completed.

## How, and why not the obvious way

`sample` cannot decompose this from outside. The VM is a flat trampoline and
the whole dispatch loop inlines into one function, so sampling the *crate*
probe put 4284 of 4291 samples on `nix_eval_rs::eval::drive+3380` with nothing
beneath it. The bridged binary works only because the C++ callbacks are not
inlined and act as accidental instrumentation.

The crate had no evaluator counters when this was written (ENG-12859). It has
them now, and **they contradict the compile row below.** Read the correction
section first; the sampled decomposition is kept as history, not as the
current answer.

Numbers are the median and range of five full-run `sample` captures,
8560 to 12314 main-thread samples each, inclusive of callees, as a share of
the run.

## Correction: the counters disagree, and the sampled compile share is low

`maintainers/ix/perf-counter-overhead.md` has the current decomposition, taken
from counters inside the evaluator rather than from outside it. On the same
expression and the same machine:

| phase | counter | sampled here |
|---|---|---|
| compile | **37.7%** | 27.2% median, 12.9 - 32.6 range |
| host questions | **24.0%** | ~16% (`read_dir` 12.2 + `read_file` 3.5) |
| store paths and hashes | **0.17%** | < 1% each, consistent |

The counter figure is **above the top of the sampled range**, so the sampling
here was biased low rather than merely imprecise, and any sizing done against
the 27.2% understated compile by about a third.

### Why the sampler was low: the obvious explanation is wrong

The natural story is the one this document originally told: compile is
front-loaded, the sampler attaches late, short captures miss it. That predicts
captures with *fewer* samples report a *lower* compile share.

The five captures say the opposite:

```
run   samples   compile%   konst%
r1     12,314      12.9       3.1
r2     12,219      15.9       3.1
r3     11,072      27.2      16.1
r4     10,044      30.6      18.2
r5      8,560      32.6      19.6
```

Correlation between samples captured and compile share is **-0.919** (n = 5),
and the *smallest* capture is the one closest to the counter's 37.7%. More
capture meant less compile, not more.

**The cause is unexplained.** The five points fall in two clusters rather than
along a line, so with n = 5 the correlation is suggestive and not conclusive,
and I did not find the mechanism. What can be said is narrower and still
useful: the sampled compile share moves with something about the capture
rather than with the work, the direction is opposite to the front-loading
story, and the front-loading story should not be repeated because it has been
tested and failed.

The practical rule is the one ENG-12859 was filed for: a flat trampoline is
not a thing to sample, and where a counter and a sampler disagree about this
evaluator, the counter is the measurement.

## The Rust arm, partitioned (sampled, superseded)

```
Vm::poll  (the VM loop, compile included)     79.7%   (64.2 - 82.8)
  of which compile_source                     27.2%   (12.9 - 32.6)
    of which Compiler::konst                  16.1%   ( 3.1 - 19.6)
capi::read_dir_through_embedder               12.2%   ( 8.8 - 17.7)
capi::read_file_through_embedder               3.5%   ( 2.9 -  7.9)
Host::resolve_import                           0.6%
Host::copy_to_store, write_derivation,
  find_file, storepath, nixhash               < 1% each
```

Self time, same runs:

```
_platform_memcmp                               6.8%   (5.8 - 7.9)
allocator (xzm_malloc/free/cache) + memmove   13.7%   (13.1 - 15.1)
__getdirentries64                              4.4%
__open_nocancel                                3.7%
Vm::intern                                     2.6%
```

Caller attribution on the memcmp leaf: 5.5 of 6.9 points under `Vm::intern`,
0.4 under `Compiler::konst`.

**The compile row is the noisy one.** The claim originally made here -- that
compile is front-loaded, `sample` attaches at a variable offset, and a short
capture therefore *under-weights* it -- is wrong, and the correction section
above shows the data falsifying it. The numbers above are what five captures
said; they are not what the evaluator does.

## The cpp arm, for comparison

cpp finishes in 1.8s, so `sample` catches only a fragment of it; the best
capture is 1493 samples and the numbers below are from that plus one earlier
1645-sample capture. Treat them as a sanity check, not as a matched
measurement.

```
parseExprFrom (parsing)               16.7% - 30.9%
fetchToStore                           5.2% -  8.8%
computeStorePath                       4.4% -  6.4%
readDirectory                          0.0% -  2.9%
dumpPath, addPath, prim_filterSource  < 0.3%
```

cppnix's own counters for the same expression, which are the denominators any
per-unit claim should use (`NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH=...`):

```
cpuTime           1.3695
nrFunctionCalls   3943863
nrThunks          6088194
nrPrimOpCalls     1872889
nrLookups         2548104
nrExprs           1472868
```

## Correction: the three recoverables were taken, and two of the three explanations below are wrong

This sits above the section it corrects, as the compile-share correction
does: a reader meets the amended claim before the superseded one. The section
below is left as it was measured. This is what happened when the three
tickets were taken, on `perf-recover` merged forward onto `8eebe7e28`, and it
contradicts that section in two places worth reading before trusting a share
quoted there.

Measured with `maintainers/ix/nixos-toplevel-bench.sh`, which is that
measurement as a script rather than a paragraph, against the nixpkgs
`flake.lock` pins (`p5cm66j33sbpn8ni9f2hlr279sfhvgwq-source`) rather than the
tree above. Nine **interleaved** pairs -- one binary then the other, repeating
-- because this laptop's load drifts by more than the effects, and a blocked
A-then-B run charges the drift to the change. The first attempt here was
blocked, and it reported the same unchanged binary at 6.42 s and 7.98 s CPU an
hour apart.

```
                          ix-patched      after      delta
median cpu                 7.490 s      4.681 s    -2.809 s   (37.5%)
median real                8.341 s      5.004 s    -3.337 s   (40.0%)
median user                7.492 s      4.683 s    -2.809 s
compile_ns                 3.117 s      1.041 s    -2.076 s   (2.99x)
drvPath                 hrn1fq805j... hrn1fq805j...  unchanged
```

`ix-patched` moved four times while this was in flight, so the pair above was
taken four times against four different bases. The absolute seconds move with
the base and the machine's load; the ratio does not:

```
base                          cpu before   cpu after   delta
af70196e0 (branch point)         8.267 s     5.329 s   35.5%
a593e9ee7 (#159 to #163)         8.646 s     5.381 s   37.8%
b038f0854 (#165, #166)           8.148 s     5.022 s   38.4%
8eebe7e28 (#169 to #171)         7.490 s     4.681 s   37.5%
```

Quote the ratio, not the seconds. 37.5% of CPU against the ~36% predicted
above. The prediction was good; two
of the three explanations behind it were not.

The same comparison against the branch point before merging forward, which is
the pair the per-ticket numbers below were taken from, read 8.267 s to
5.329 s (35.5%). The base moved under it; the change did not.

**`Compiler::konst` was worse than 16%, and the scan was deeper than the
totals suggest.** Counting comparisons rather than calls: 3,327,228,909
`Const` comparisons for 800,884 calls, an average scan depth of 4,154 against
a floor of one lookup each. That is far above the 269 calls per module the
totals imply, because the cost is per module and superlinear in module size --
a handful of enormous nixpkgs files are essentially the whole figure. The
symbol pool, which the ticket asked about and this document did not measure,
had the identical shape and another 148,888,726 comparisons.

**ENG-12861 was right about the cost and wrong about the cause.** Replacing
the `BTreeMap` interner with a `HashMap` -- the fix as specified -- moved the
median 0.096 s, inside the noise. Only replacing the *hash function* recovered
the ~8% this document attributed to that leaf:

```
BTreeMap<String, Sym>                 5.786 s
HashMap<Rc<str>, Sym>, std SipHash    5.690 s     -0.096 s   (1.7%)
FxHashMap<Rc<str>, Sym>               5.395 s     -0.517 s   (8.7%)
```

SipHash-1-3 over 8.5 million short strings costs about what the sixteen
comparisons it replaced did. The data structure was not the expensive part.

**Most of the probes that swap made cheaper are about to be deleted, so it
will not keep being worth 0.517 s.** ENG-13018 found that the interner is
mostly asked for names the compiler already knew. Counting the three classes
it names, at their sites in this tree:

```
Op::MkAttrs, attrset names re-interned per execution   3,078,064
Vm::msym, Select/SelectSoft/HasAttr/ResolveWith        3,179,122
lambda formals, re-interned per call                     906,669
                                                       ---------
                                                       7,163,855
```

That is **84.1% of the 8,514,655 probes** the interner A/B above ran against,
and 87.6% of the 8,177,507 left once the directory cache below removes the
filename interning. Independently measured here and agreeing with ENG-13018's
own 84%, on a different branch.

The consequence is a sequencing one, and it runs the opposite way to the
intuition that two fixes to one leaf add up. **They are substitutes, not
complements.** The hash swap made each probe about 61 ns cheaper across 8.51
million of them; ENG-13018 deletes 84% of those probes. Apply the same
per-probe improvement to only the 1,350,800 irreducible probes and it is worth
0.082 s, not 0.517 s.

So whichever of the two lands second will measure roughly six times smaller
than it would have alone, and neither is wrong when it does.

The absolute cost this paragraph used to say nobody had measured is measured
now, and it closes ENG-13018. See "What the interner costs now" below: the
whole of what that ticket would delete is 0.089 s of a 4.707 s evaluation,
1.9%, and that is an upper bound. ENG-13018 landed second, and there is
almost nothing left for it to recover.

**ENG-12862's mechanism was not the one hypothesised.** This document guessed
two traversals where cppnix has one, which is a factor of two. The counts say
13,485 `Entries` questions cover **767 distinct directories**: the average
directory was read 17.6 times, by 33 filtered copies walking overlapping
trees. A per-evaluation directory cache removes 12,718 of the questions. The
double traversal is real, is untouched, and was a factor of two hiding inside
a factor of 17.6.

So the honest general lesson: the sampled shares above ranked the three items
correctly and explained two of them wrongly. A share tells you where to look;
it does not tell you what you will find.

### What this leaves

The gap on this expression is now roughly 3.9x wall against the cpp arm's
1.535 s, down from 6.3x. The residual is still the VM's own per-unit cost, and
nothing here has measured it to a cause -- the paragraph above about the
allocator stands, unimproved and still not a fix. Two named leftovers now have
numbers behind them: the VM-side filter walk still duplicates the embedder's,
and an `Entries` answer is still an attrset whose every filename is interned
when the filter walk wants only (name, type) pairs.

## What is recoverable, and what that leaves (superseded)

Three named items, each with a ticket and a fix that is not speculative:

- **`Compiler::konst` is quadratic** (`compile.rs:123`). It linear-scans the
  constant pool for every constant emitted. 16.1% of the run, about 59% of all
  compile time, ~1.37s of CPU. A side `HashMap<Const, u32>` removes it.
  ENG-12860.
- **`Vm::interner_idx` is a `BTreeMap`** (`vm.rs:281`), so interning costs
  O(log n) full string comparisons and allocates each name twice. About 8% of
  the run, ~0.68s, against cppnix's ~0.03s in the same leaf. ENG-12861.
- **The source-filter walk reads directories cppnix does not.** 12.2% of the
  run in `Entries` questions against cppnix's 0-3% in `readDirectory`, roughly
  1.1s against 0.05s. The likely mechanism is two traversals where cppnix has
  one -- cppnix consults the filter from inside `dumpPath` while it is already
  reading the tree to hash it, and this evaluator cannot, because the filter is
  a Nix function and the copy is a host question. Stated as a hypothesis:
  `rustStoreFiltered` never appears in any sample, and nothing counted the
  questions. ENG-12862.

Those three total about 36% of 8.5s of CPU, so ~3.1s of the 7.6s gap.

**The remaining ~4.5s is the VM's own per-unit cost and is not explained by
anything named above.** Excluding compile, the VM loop is ~52% of 8.5s CPU
against cppnix's ~1.1s for the same 3.94M function calls, so roughly 1.1 us
per call against 0.28. The allocator is the largest single suspect at 13.7%
self time against cppnix's ~5.8% of a much shorter run, but "allocate less" is
not a fix, and nobody has yet counted allocations per call. **Somebody has
now**, and the answer is in "The residual, decomposed by op" below: about 3.3
allocating op dispatches per cppnix function call. Do not quote a
speedup for this part; it has not been measured to a cause.

## The residual, decomposed by op

The section above priced the residual as one number with nothing inside it:
about 38% of the run, roughly 3.3 seconds, over 56.0M ops, so about 59ns per
op as an upper bound on dispatch. `perf-ops` now counts every IR op under its
own kind (ENG-12994), so this is what those 56 million are.

```
bin       build-rust/src/nix/nix-instantiate
          sha256 35822fabb205f556a40ee35d814c01e04f6f91b10a12353ac65c1124c0a838ac
rev       ix-patched bdd56c031 plus this branch's ENG-12994 commit
nixpkgs   /nix/store/llgwlxshmy0ifvxh7f8wq53vk5x7vd13-source
expr      the same one this document opens with
config    extra-experimental-features = rust-eval, eval-backend = rust,
          lint-{url,short-path,absolute-path}-literals = warn
build     -Dnix-cmd:rust-eval-cargo-features='--features perf-ops'
answer    /nix/store/hhng3cfvzlypji8zmhg13pwrwjmis12a-nixos-system-nixos-26.11pre-git.drv
          which is the same store path the two arms agree on above
```

The per-kind counts sum to `ops` exactly, and the total is deterministic:
seven runs of the same expression gave 56,042,300 every time, to the op. It is
a property of the program rather than of the instrumentation, so unlike a
timing it did not need a quiet machine.

How stable "deterministic" is, concretely: seven runs on one binary, then
every counter again on a second binary built after merging #165, which gave
the VM its settings and rewrote 180 lines of `vm.rs`. Same 56,042,300, same
per-kind table to the row, same 8,738,139 interner probes, same answer.

```
op                  dispatches    share  cumulative
Ret                  9,651,841   17.22%      17.22%
Thunk                8,374,098   14.94%      32.16%
GetLocal             7,826,434   13.97%      46.13%
Const                5,804,261   10.36%      56.49%
Apply                5,334,308    9.52%      66.01%
GetLocalLazy         3,341,334    5.96%      71.97%
JumpIfFalse          2,649,632    4.73%      76.70%
Closure              2,235,427    3.99%      80.68%
Select               1,617,113    2.89%      83.57%
Jump                 1,062,443    1.90%      85.47%
MkAttrs              1,014,374    1.81%      87.28%
Eq                     880,726    1.57%      88.85%
SelectSoft             876,576    1.56%      90.41%
OrDefault              770,569    1.37%      91.79%
MkList                 706,649    1.26%      93.05%
Not                    625,116    1.12%      94.16%
PopEnv                 572,121    1.02%      95.18%
PushEnv                570,114    1.02%      96.20%
HasAttr                469,971    0.84%      97.04%
Builtin                362,716    0.65%      97.69%
Update                 338,787    0.60%      98.29%
Neq                    200,187    0.36%      98.65%
HasAttrDyn             163,605    0.29%      98.94%
ConcatLists            143,867    0.26%      99.20%
Add                    101,284    0.18%      99.38%
ConcatStrings           69,059    0.12%      99.50%
SelectSoftDyn           61,673    0.11%      99.61%
Sub                     53,469    0.10%      99.71%
Assert                  51,302    0.09%      99.80%
SelectDyn               38,857    0.07%      99.87%
Div                     27,291    0.05%      99.92%
ResolveWith             14,853    0.03%      99.94%
Lt                      14,487    0.03%      99.97%
DerivationGlobal         7,099    0.01%      99.98%
Leq                      4,586    0.01%      99.99%
PushWith                 2,007    0.00%      99.99%
Gt                       1,838    0.00%     100.00%
Geq                        805    0.00%     100.00%
NixPathGlobal              634    0.00%     100.00%
Negate                     344    0.00%     100.00%
BuiltinsSet                313    0.00%     100.00%
Mul                        130    0.00%     100.00%
```

Three of the 45 kinds never execute here: `CallBuiltin` (vestigial, the
compiler emits it nowhere), `UnimplementedGlobal` (this program references no
unimplemented global) and `ConcatPath` (no interpolated path literal in this
configuration).

### Read these as dispatches, not as instructions the program contains

An op that has to force something yields *without advancing `ip`*
(`vm.rs:1627` is the clearest case), so it is fetched and counted again when
the task resumes. The counts are therefore dispatches through the loop, which
is the right denominator for dispatch cost and an upper bound on distinct op
executions.

`Ret` is the opposite: it is a lower bound on unit exits. A unit whose ops run
out returns from `advance_unit` before `note_op` is reached (`vm.rs:1145`), so
those exits are counted as no op at all. Do not derive a count of thunk
forcings by subtracting `Apply` from `Ret`.

### By what the op has to do

Grouping the same 56.0M by the work each kind performs, which is as close to a
cost decomposition as counting gets:

```
class                       dispatches   share
call and return frames      16,128,384   28.8%
heap allocation             12,882,261   23.0%
environment lookup          11,184,628   20.0%
constant push                6,175,023   11.0%
attribute lookup             3,998,364    7.1%
branch                       3,712,075    6.6%
arithmetic and compare       1,961,565    3.5%
total                       56,042,300  100.0%
```

The two comparisons worth making against cppnix's own counters, which the
section above lists for the same run:

```
                       rust          cppnix     ratio
lazy values created    8,374,098   6,088,194    1.38   Op::Thunk vs nrThunks
function applications  5,334,308   3,943,863   <1.35   Op::Apply vs nrFunctionCalls
```

**Only the first row is a count.** `Op::Thunk` cannot yield (`vm.rs:1226`
pushes and advances), so 8,374,098 is exactly the number of thunks this
program creates. `Op::Apply` *can* yield, to force the callee
(`vm.rs:1245`), and yields there without advancing `ip`, so its dispatch count
includes retries and 1.35 is an upper bound rather than a ratio.

**Even the first row is not the same counter on both sides**, which is why it
is a lead and not a verdict. It is like-for-like in a way `y.Force` against
`nrThunks` was not -- both count a lazy value being created, where `y.Force`
counted task suspensions, a concept cppnix has no equivalent of. What keeps it
approximate is that two different compilers decide what needs a thunk: this
evaluator has a separate `GetLocalLazy` for rec-attrset construction, and
cppnix has `maybeThunk` shortcuts with no analogue here.

It is a lead worth chasing because the profile above puts the allocator at
13.7% self time against cppnix's 5.8%, and creating 2.3M more lazy values than
cppnix does for the same program is the most obvious way to arrive there. The
section above says "nobody has yet counted allocations per call". Now somebody
has: 12,882,261 allocating op dispatches over cppnix's 3,943,863 function
calls is about 3.3 per call, and that is a floor, since one `MkAttrs`
allocates a map and its nodes rather than one object.

### Where the 8.7M interner probes come from

ENG-12861's denominator is 8,738,139 interner probes and the profile above
attributes about 8% of the run to them. Two counters at the hottest sites, plus
what the op counts bound, say where a fix belongs:

```
source                                             probes   share
measured by a counter at the site
  attribute names, Op::MkAttrs (vm.rs:1592)     3,323,867   38.0%
  formal parameter names (vm.rs:902)            1,030,273   11.8%
derived from op dispatch counts, so upper bounds
  ops carrying a symbol, via Vm::msym           2,978,513   34.1%
  dynamic attribute names                         264,135    3.0%
the remainder, not attributed
  builtins, printing, derivation assembly       1,141,351   13.1%
total (interns)                                 8,738,139 100.0%
```

**Almost all of it is names the compiler already knew.** `Op::MkAttrs` interns
every attribute name at runtime, static ones included, because names reach it
as strings pushed by `Op::Const`: `compile.rs:727` emits the push,
`vm.rs:1592` interns it, and both run again every time the attrset is built.
The apply path does the same for a lambda's formals, once per call
(`vm.rs:902`). `Vm::msym` is the third instance and the least obvious one --
it maps a module-local symbol index to a global `Sym` by pulling the string
out of `module.symbols` and interning it, on every dispatch of every
`Select`, `SelectSoft`, `HasAttr` and `ResolveWith`.

Those three are 49.8% measured, or 83.9% counting the `msym` bound. None of
the names involved can change after compilation.

**That is a different fix from ENG-12861, and complementary to it.**
ENG-12861 makes one probe cheaper; this removes most of the probes. `msym` is
the cheapest of the three to fix and the most contained: one
`Vec<Sym>` per module, built when the module is loaded, turns a string intern
into an array index. Whoever measures ENG-12861's improvement should know the
denominator is mostly this, or the win will read as smaller than the change
deserves. Filed as ENG-13018.

### What the interner costs now, and why that closes ENG-13018

The paragraph above and the correction section both defer to a number nobody
had taken: what the interner *actually costs* now that ENG-12861 made a probe
cheap. Taken on `10c995143`, aarch64-darwin, the pinned-tree toplevel.

Sampled in situ rather than profiled: one call in 31 is bracketed by
`Instant::now()`, and a second clock read is taken immediately after so the
clock's own cost is subtracted rather than assumed small. It is a large
correction and ignoring it would have halved the answer: the raw per-call
figures are 25.4, 29.4 and 33.4 ns and the clock accounts for 15.9, 15.6 and
16.2 of those.

The region timed is everything the fix would delete, not just the hash lookup:
at `msym` and the formals site that includes the `String` clone each performs
before interning, which the interner-only measurement misses and which is
about half the cost at those two sites.

```
site                        per call    calls        total      of run
Op::MkAttrs names             9.52 ns   3,078,064   0.0293 s     0.62%
Vm::msym                     13.78 ns   3,179,122   0.0438 s     0.93%
lambda formals               17.24 ns     906,669   0.0156 s     0.33%
                                        ---------   --------    ------
                                        7,163,855   0.0887 s     1.89%
```

**So ENG-13018 is worth at most 1.9%, and that assumes its replacement is
free.** A per-module `Vec<Sym>` still costs an array index per call, so the
realistic figure is lower. Closed on that basis.

Two things make the number trustworthy rather than a single reading:

* **The sampler reproduces the call counts.** At one in 31, the samples imply
  3,086,484 / 3,173,284 / 904,084 calls against the independently counted
  3,078,064 / 3,179,122 / 906,669. Within 0.3% at every site, so the sampling
  is not phase-locked onto a subset of the calls.
* **A second method agrees.** Timing `Vm::intern` alone, across all 8,177,507
  probes and not just these three sites, gives 6.61 ns per probe and 0.054 s
  total. The three classes' share of that is 0.047 s; adding the `String`
  clones the first method excludes lands on the same place as the 0.089 s
  above.

The honest summary of the whole interner thread: ENG-12861 took a probe from
roughly 67 ns to 6.6 ns, and once a probe is 6.6 ns, having 7.16 million
redundant ones is a 1.9% problem rather than the 8% the sampled profile
originally saw. "84% of the probes are deletable" was true and stopped being
worth acting on the moment the probes got cheap.

**`MkAttrs` would also have been the structural one.** `msym` and the formals
site can be fixed with a per-module symbol cache, which is contained. Making
`Op::MkAttrs` stop interning needs the compiler to emit symbols where it
currently emits string constants, which is a new op or an op variant, so the
smaller two thirds of an already small number is also the cheap two thirds.

*Done 2026-09-04, once the number stopped being small.* A larger question
(`nixosConfigurations.hil-compute-1...toplevel.drvPath`, 78 s edit run on
dev-compute-4) showed 50.7M interns, `insert_attr_pairs` the top interner and
`memcmp` under `Vm::intern` at 0.7% by itself. `Op::MkAttrs { statics,
dynamics }` now pops `statics` bare values and takes their names from the op's
`AttrSite` (module symbols in emission order, through the link table); only
`dynamics` (name, value) pairs are on the stack and only those names are
interned. The formals path went through the link table the same day. The
static-name `Op::Const` pushes are gone from the compiler, so the
`attr_name_interns` counter now counts dynamic names only.

**A draft of this section put the `MkAttrs` share at 5,495,491, by subtracting
the intern sites a `vm.rs` grep found from `interns`.** The measured figure is
3,323,867, 65% lower. The subtraction was wrong because the grep was scoped to one
file while the interner is called from ten, so every site outside `vm.rs` piled
into the one bucket being estimated. The two rows labelled measured above are
counters at the site. The two labelled derived are still arithmetic, and are
marked as bounds for that reason.

### What this does not license

It is a decomposition of the op *population*, not of time. Nothing here timed
an individual op, and nothing can cheaply: a clock read costs more than the op
it would measure, which is why the 59ns figure is a quotient and not a
measurement. Per-kind cost needs either a sampling profiler that can see into
the dispatch loop, which the section above explains this one cannot, or a
build that counts one class at a time and is differenced -- neither is done.

So the honest state of the residual is: it is 56.0M dispatches, 29% of them
call and return, 23% allocating, 20% environment lookups, and the largest
single kind is `Ret` at 17%. Which of those is the 3.3 seconds is still not
known.

## What this does not cover

- One host, one architecture, one nixpkgs revision, one configuration. In
  particular a grub-enabled configuration does not finish at all yet
  (`builtins.toXML`, ENG-12863), and a config that filters larger source trees
  would move the `Entries` share.
- The rust arm's absolute wall time includes bridge crossings that a future
  in-process embedder would not pay. Nothing here separates crossing cost from
  the work on the far side of it.
- No warm-start or memo-table run. Every number is a cold process.
