# How a change is validated in this fork

Hosted CI is gone. A change is validated by running the full test suite on a
fleet dev node, and the person making the change is the one who runs it.

This replaced GitHub Actions on 2026-08-03 by operator instruction. The workflows
removed in that commit were `.github/workflows/ci.yml`, `labels.yml` and
`backport.yml`, plus the now-orphaned `.github/labeler.yml`.

## What to run

On any dev node (`dc1` through `dc6`), in a checkout of the revision under test:

```console
$ nix develop --command nix shell nixpkgs#cargo nixpkgs#rustc --command bash -c 'meson setup build --prefix="$out" --buildtype=debugoptimized && ninja -C build && meson test -C build --print-errorlogs'
```

`configurePhase` and `buildPhase` from [HACKING.md](../../HACKING.md) are shell
functions defined by the dev shell's `shellHook`, so they exist only in an
interactive shell and are absent from `nix develop --command bash -c`. Call meson
and ninja directly, as above. Note `--prefix="$out"`, not `"$prefix"`: the dev
shell exports `out` but not `prefix`, and an empty prefix silently puts the
system `nix` on `$PATH` instead of the one you just built.

**The Rust evaluator always links in.** There is one build configuration:
`src/libcmd/meson.build` builds `libnix_eval_rs.a` with cargo unless
`-Dnix-cmd:rust-eval-prefix` hands in a prebuilt archive (which is what
`index/packages/nix/default.nix` does, since the sandbox has no cargo). The
dev shell has no cargo either (ENG-12458, ENG-12464), which is why the
command above wraps meson in `nix shell nixpkgs#cargo nixpkgs#rustc`: without
that layer `meson setup` fails on `find_program('cargo')`.

Note the `nix:` prefix on any `src/nix` option: it is a meson subproject, so
its options are namespaced and the unqualified spelling is rejected as
unknown. There used to be a second, no-rust-eval configuration whose `#else`
stubs had to match every declaration by arity; it failed to link twice
(ENG-12495; the 2026-09-04 memo lane) because nobody built it locally, and it
was deleted rather than fenced.

When the change touches the Rust evaluator's caches or any `ixe_set_*`
setting, also run the cache-semantics gate. It differs along the setting
rather than along the evaluator, which is the axis `lang-diff.sh` cannot
cover:

```console
$ ./maintainers/ix/cache-semantics-gate.sh
```

265 files across six configurations, plus a cross-configuration arm that
fills a cache under one and evaluates under another. It prints its
denominators and refuses a corpus that shrank. It is blind to a cache that
misses silently, so run `cargo test -p nix-eval-rs` beside it; that is where
the witness-codec round trip and the cache-hit assertions live.

When the change touches anything under `rust/`, run the crate gate. It is
separate from `pre-commit` because the dev shell has no cargo (ENG-12458), and
separate from the C++ suite because nothing else in this fork runs clippy at
all: hosted CI is gone and the ciCheck derivations `rust/Cargo.toml` defers to
do not exist here, so `[workspace.lints.clippy]` was unenforced from the day it
was written until this gate. It needs no dev node and no built nix, and takes
about a minute warm:

```console
$ nix shell nixpkgs#cargo nixpkgs#rustc nixpkgs#clippy \
    --command ./maintainers/ix/rust-crate-gate.sh
RESULT rust-crate-gate pass nix-eval-rs/default=0/456 nix-eval-rs/no-perf=0/456 \
nix-eval-rs/all-perf=0/456 ix-kernel/only=0/92 (clippy-errors/tests-passed)
```

It lints **and tests** both crates in three feature configurations, and the
combination is the point. `--all-targets` is load-bearing on the clippy side:
all 41 errors it was written for were in `#[cfg(test)]` modules and integration
tests, which a library-only run does not see. The three configurations are
load-bearing on the test side: two perf tests asserted counter accumulation
while the counters were compiled out and failed deterministically under
`--no-default-features` for as long as that configuration existed, because
`--no-default-features` was only ever *built* (ENG-13005).

Its tests run parallel. They did not always survive that: ENG-12939 made the
lib suite race on process globals and this gate carried `--test-threads=1`
until #165 gave the `Vm` its settings instead of reading the statics. Measured
here, `cargo test -p nix-eval-rs --lib` parallel went 20/24 (pre-#165) to
24/24 (post-#165). If it regresses, the answer is not the flag: a serial gate
cannot see a newly introduced race, which is the thing worth catching.

When the change touches `rust/nix-eval-driver`, run its parity gate too. The
driver evaluates and instantiates without the C++ CLI, so nothing above
exercises it: the crate gate lints and tests it, and every other gate here
compares two backends **inside one nix binary**, which the driver is not.

```console
$ cargo build --release -p nix-eval-driver   # in rust/
$ ./maintainers/ix/rust-driver-parity.sh build-rust/src/nix
RESULT rust-driver-parity pass cases=21 match=21 mismatch=0 unimplemented=0 \
expected-cases=21 min-match=21 max-unimplemented=0 ...
```

It instantiates every case three ways -- the driver, and the bridge under each
backend -- into three separate store roots, and requires drvPath, outPath and
the `.drv` bytes to agree across all three. Three roots rather than two,
because arms that share a store are not really being compared: the second
write is a no-op onto an existing path and the two "files" are one file.

`rust-driver-parity-selftest.sh` is that gate's own guard, and worth running
whenever the gate changes. Each case hands the gate a wrapper that delegates
to the real driver and corrupts one thing -- the bytes, the file's existence,
the printed path, the outPath arm's exit code, the `--system` it is told --
and requires the gate to fail naming that specific thing. It is slow (a full
gate run per case) and not on the fast path. It has already paid for itself:
the `system` case passed against the gate as first written, which is how the
corpus turned out never to read `builtins.currentSystem`.

Also run, before committing:

```console
$ nix develop --command bash -c 'pre-commit run --all-files'
```

`--all-files` matters. The git hook runs against staged files only, so a
contributor whose own files are clean sees green while the tree is red. That is
how `ix-patched` came to sit with a red gate through two merges before anyone
noticed.

## An evaluator question does not need any of this

`rust/nix-eval-rs/examples/nixpkgs-probe.rs` evaluates real nixpkgs through the
Rust crate alone -- no `nix` build, no C++ bridge, no dev node -- **on any
machine with a Rust toolchain**, macOS included:

```console
$ cd rust
$ NIXPKGS=/nix/store/...-source cargo run -q --example nixpkgs-probe
nixpkgs=/nix/store/llgwlxshmy0ifvxh7f8wq53vk5x7vd13-source (from NIXPKGS)
...
7  one package name         OK       "hello-2.12.3"
8  one package outPath      REFUSED  [SearchPath] resolving '<nix/fetchurl.nix>' to a path ...
RESULT nixpkgs-probe rows=12 ok=11 refused=1 error=0 nixpkgs=/nix/store/llgwl...
```

Measured on an M-series Mac with a warm `/nix/store`, wall clock including
cargo's own overhead:

| | 12 default rows | one expression |
|---|---|---|
| `--release` | 3.5s | 0.6s |
| debug | 17.0s | 2.9s |

Use `--release` for anything iterative; the default rows evaluate the whole
package set several times over. Either way the comparison is against
`nixpkgs-frontier.sh`, which needs a dev node and a C++ build first.

With no arguments it asks the twelve expressions `nixpkgs-frontier.sh` asks, so
the two read side by side. Give it expressions to ask those instead, and
`--cpp <nix-instantiate>` to print a comparison arm. `NIXPKGS` may be omitted if
`nix` can resolve the `nixpkgs` flake; the resolved path is printed either way
and its store hash is the revision.

**Reach for it first when the question is "what stops the evaluator here?"**
That is the common case, it is where hand-bisection happens, and the round trip
through a dev node buys nothing for it. It found ENG-12593's residue and
confirmed the fix with no `nix` build at all.

**It is also the loop that survives a broken tree.** On 2026-08-05 the fork tip
did not compile under `-Dnix:rust-eval=enabled` for several hours (`ixe.h` used
`IxeSession` above its forward declaration; the default configuration built
fine, which is why it went unnoticed). Every command in the section above was
unrunnable. The probe was not, because a crate-level probe cannot be blocked by
a C++ compile error.

### It is not a substitute for the gate

A green probe is not a green gate, and the file says so at the top. It exercises
the evaluator and nothing else: no C++ bridge, so no `ixe_*` entry point, handle
table or error mapping; no CLI, so no `eval-backend` selection, settings or exit
codes; and single-arm unless `--cpp` is given, so it reports what the Rust
evaluator did rather than whether cpp agrees. Bisect with it, then confirm on
`nixpkgs-frontier.sh` and quote *that* in a PR.

One trap it is built to avoid, because an earlier throwaway version fell into
it: the probe refuses `<nix/fetchurl.nix>` exactly as the embedder does. cppnix
answers that from an in-memory accessor this evaluator cannot read. Resolving it
from the nix source tree makes nixpkgs get further and makes the probe *more
capable than the real binary*, which hid a blocker and reported the remaining
work in the wrong order (ENG-12607). `--corepkgs <dir>` re-enables the shortcut
and prints a warning saying results past it are not evidence about the real
binary. A guard test in the example pins both halves of that rule.

Generally: a harness standing in for an embedder callback hides whatever that
callback refuses. Make the stand-in refuse the same things.

## What that costs, measured

On dev-compute-5 (AMD EPYC 9135, 32 threads), on 2026-08-03:

| step | time |
|---|---|
| `ninja -C build` from cold | `real 2m36.421s`, `user 71m0.661s` |
| `meson test -C build`, whole suite | `real 0m34.032s`, `user 2m35.565s` |

The suite reported **286 Ok, 0 Fail, 11 Skipped**. Those are the numbers to
compare against: a run that reports meaningfully fewer than 286 passing tests has
skipped something rather than proved something, and is worth reading before
believing.

Meson caches aggressively, so a one-file edit rebuilds and re-runs in seconds.
The five-minute feedback loop that hosted CI imposed is the thing this replaces.

## What is no longer covered

Two things were hosted-only and have no replacement:

- **Windows unit tests.** The `windows unit tests` job cross-built and ran the
  unit tests on a hosted runner. Nothing on the fleet does this. The last hosted
  run of it, on PR #40, passed. There is no local equivalent, so a change that
  breaks the Windows build will not be caught until somebody builds for Windows
  deliberately.
- **Darwin.** This is not a new loss. The fork already carried no macOS job, and
  the `ci.yml` comment removed with it recorded why: the job was cancelled at
  exactly 60:00 on four consecutive runs, so it had produced no completed verdict
  in days while occupying a required-looking slot. Darwin assurance here was
  already manual, and the standing record is a hand-run smoke of the built client
  against a live 2.34.7 daemon (`nix store ping --store daemon` plus a build
  through it, both exit 0), on indexable-inc/index#4483. That remains the
  template for anyone touching a Darwin-relevant path.

Also gone, and worth knowing rather than rediscovering:

- **CodeQL still runs.** It has no workflow file in this repo; it is GitHub's
  default setup, configured in repository settings, so removing the workflows did
  not remove it. Turning it off, if that is wanted, is a settings change and not
  a commit.
- **`upload-release.yml` was kept.** It is `workflow_dispatch` only and uploads a
  Hydra release rather than testing anything, and
  [release-process.md](./../release-process.md) still instructs you to trigger it.
  `.github/actions/install-nix-action` was kept for the same reason: that workflow
  uses it.

## Recording a validation run

When a change lands, say which node you ran on, which revision you tested, and
the counts, on the same line. "The suite passed" is not a record; "286 Ok, 0
Fail, 11 Skipped on 46d0769b5, dev-compute-5" is, because the next person can
tell whether your run and theirs saw the same suite.

## Measuring what write-through publication costs

[`write-through-throughput.sh`](./write-through-throughput.sh) measures the
publication cost of the `write-through-store` setting, against any destination
you point it at, in a scratch store root that leaves the host's own store alone.

```console
$ ./write-through-throughput.sh --to "file:///tmp/wt-bench-cache" --sizes 1,64,256 --reps 5
```

This exists because publication runs on the build worker thread and blocks the
loop, so a host with the setting on has its build concurrency bounded by
publication throughput rather than by `--max-jobs`. That bound cannot be read off
a derivation count, and the sizing a dispatcher was given under the old
asynchronous queue does not carry over.

Three tiers, in increasing order of what they actually tell you:

1. **A `file://` destination** measures nix-copy protocol overhead with the wire
   taken out. Runs anywhere, needs nothing.
2. **A scratch endpoint on a storage host** measures the wire and disk floor over
   the real path. Needs a node and a scratch endpoint, never a production
   namespace.
3. **The real cache** is the only number that answers the sizing question, and it
   can only be taken on a host that holds the push credential
   (`nixCachePushCredentialFile`, which today is vin-compute-1's inventory
   extras). A dev node cannot authenticate against it, so tiers 1 and 2 are not
   an approximation of tier 3, they are different measurements.

The harness refuses to report a number when the path did not reach the
destination, because a publication that silently did not happen would otherwise
read as enormous throughput. It also gives every build a fresh run id, so no
repetition can be served by a path the destination already had.

### Set `compression` on the destination, or nothing else you tune will matter

Measured on dev-compute-5 (2026-08-04), publishing a 256 MiB incompressible
output to a local `file://` cache, so no network is involved at all:

| destination | publication rate |
|---|---|
| `file:///...` (no parameter, so **xz**) | **2.6 MiB/s** |
| `file:///...?compression=zstd` | 579 MiB/s |
| `file:///...?compression=none` | 621 MiB/s |

That is a 238x spread, and the default is the slow end. The xz run spent 100
seconds of build-worker time to produce a file *larger* than its input
(`FileSize: 268449088` against `NarSize: 268435736`), because xz cannot compress
random bytes but still pays to try.

The payload is deliberately incompressible, which is xz's worst case; real build
outputs compress, so xz would both run faster and actually shrink them. The
ordering does not change. Single-threaded xz is one to ten MiB/s on compressible
input too, still two orders of magnitude under the alternatives, and it is
running on the thread that the build loop is waiting on.

`nixCacheWriteStoreUrl` already carries `?compression=none&parallel-compression=false`,
so the configured path is on the fast end. The hazard is a destination URL written
by hand without those parameters: it does not fail, it just publishes at a few
MiB/s, and a 1 GiB output blocks the build worker for several minutes in a way
that looks like a hang rather than a misconfiguration.

### The transport, measured

Same date, dc5 (192.168.0.9) to hil-stor-2 (192.168.0.8), both `bond-vrack` at
50000 Mb/s, 0.2 ms RTT, 4 GiB of incompressible payload staged in RAM on both
ends so neither source disk nor `/dev/urandom` is in the measurement:

| path | rate |
|---|---|
| hil-stor-2 local write, `/dev/shm` to `dpool` (no wire) | 1349 MB/s |
| dc5 to hil-stor-2, ssh `chacha20-poly1305`, into `dpool`, fsync included | 292 MB/s |
| dc5 to hil-stor-2, ssh `aes256-gcm`, into `dpool`, fsync included | 524 MB/s |
| the same, 4 parallel streams | 805 MB/s aggregate |
| the link itself | 6250 MB/s |

The destination dataset is `dpool/scratch-storewt-throughput` (zstd, recordsize
128K, sync=standard), removed afterwards.

Neither the wire nor the disk is the constraint. A single ssh stream is, and
publication is a single stream on one thread, so the transport ceiling for an
ssh-based destination is the 292 MB/s figure with the default cipher. hil-stor-2's
sshd offers only `chacha20-poly1305` and `aes256-gcm`, and chacha is the one that
gets negotiated by default despite being the slower of the two on these EPYC
parts, which have AES-NI.

None of this is the real-cache number. The real cache is S3 to Garage over TLS,
not ssh, and the push credential lives only on vin-compute-1.
