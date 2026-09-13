---
name: nix-debugger
description: "Find why a Nix build rebuilt, cascaded, ran slow, or failed without naming an error: nix-dag plan scoring, whence provenance, full build logs, drvPath eval before build, in-flight daemon builds, and generation-to-generation unit diffs. Use when something rebuilt that should not have, a deploy triggered a restart or handoff, a build is slower than expected, or a failure excerpt does not contain the error."
---

## Nix debugger

Something rebuilt, and you want to know why. The cause is usually a store path
carried in a place nobody thinks of as an input: an environment variable or an
`ExecStart` line. Measure before forming a theory; each tool below answers one
question from data that already exists.

## Rank what invalidates the rest: `nix-dag`

`nix-dag <installable>` scores a build plan from evaluation alone. No builder,
seconds rather than hours. It prints the critical path, the width per level, and
a ranking of nodes whose dependents reach them only through an environment
variable naming the store path.

In **index**:

```
nix-dag .#whence
  929 derivations, 5466 edges
  critical path  165 nodes
  parallelism    165 levels, widest 163 at level 111, median 2

  1. python3-3.14.6
     sole 4 of 85 direct  blast 125  own deps 600
     4 of 85 dependents name it only in pythonInterpreter and reach it no
     other way: drop that and they stop rebuilding when this changes
```

Read the **sole** count, not blast. A compiler with a thousand dependents is
normal and uninteresting; a node reached only through `pythonInterpreter` is a
rebuild nobody asked for.

Then check the top entry is built here. The ranking is cost per change and
cannot see how often a node moves, so a node pinned to an upstream input that
already invalidates the graph another way costs nothing however high it ranks.
One session skipped this check and filed a defect against a nixpkgs-pinned JDK,
then retracted it.

`--top N` widens the ranking, `--json` makes it machine-readable, and
`--from-json <dump>` reads a captured `nix derivation show --recursive` instead
of evaluating, so you can score a plan from a host you cannot evaluate on.

## Read the whole build log, not the excerpt: `nix log`

The excerpt nix prints on failure is a fixed tail. When a chatty phase precedes
the error, the error scrolls off and the excerpt shows nothing but noise. This
has sent sessions after the wrong layer.

```sh
nix log /nix/store/f7d1ak6zackgdmkxn9b1pgrinqcwwrgf-whence.drv > /tmp/whence-build.log 2>&1
```

Get the derivation path from `nix eval --raw .#<attr>.drvPath`, or from the
`logFile` and `drvPath` fields of `nix store builds --json`.

A crate whose log stops at the compiler invocation with no diagnostic is usually
a store problem, not a compile error.

A worked instance, 2026-07-29. ix#8885's `nix` job failed. Neither `gh run view
--log-failed` nor the full 4,348 line `gh run view --log` contains the word
`error` anywhere; both end in hundreds of lines of `added 117 signatures` from a
cache-push phase that ran after the build failed, plus a pointer to a file on an
unnamed host. The cause was on line 634 of an 85 KB log on `hil-compute-2`:

```
error: Cannot build '/nix/store/lwpynj4...-cargo-unit-nextest-ix-vm-guest.drv'.
       >   panicked at /build/vm-guest-daemon-0.1.0/src/vsock.rs:276:10:
       >   AF_VSOCK connect: ENODEV
```

That is not truncation, it is a filter that kept the chattiest phase and dropped
the diagnostic, so the job reads as failed with no stated reason under a wall of
successful-looking output. Two habits follow. The store excerpt names your next
command itself (`For full logs, run: nix log <drv>`). And a wrapper's own log
usually lives on the host that ran the job rather than dying with an ephemeral
runner, so `full log (unfiltered, on this runner)` means find the host, not give
up. Filed as ix#9086.

## Evaluate before you build

When a planner predicts a build but execution returns a cached output, compare
the exact derivations and serialized inputs before changing cache or hook rules.
In the September 2026 mechanical-owner integration, the planner encoded candidate
JSON without a final newline while the adapter included one. `builtins.toFile`
produced different inputs and derivations. Sharing the adapter's encoder made the
forecast resolve the existing output with no missing build dependencies; native
output metadata and content verification passed. This proved an input encoding
defect, not a cache-engine defect or complete CI success.

Use one encoder for planning and execution. Query the exact `drv^out` target and
bind its returned output to the adapter's result. Preserve cold-build hook checks;
do not delete cached outputs or loosen checks to conceal a derivation mismatch.

An attribute costs minutes; its closure costs hours. `nix eval --raw
.#<attr>.drvPath` proves the expression is sound without building anything.

In **both repos**, one eval finds every host's eval errors at once, across a
whole fleet:

```sh
nix eval --json .#nixosConfigurations \
  --apply 'cs: builtins.mapAttrs (n: c: c.config.system.build.toplevel.drvPath) cs'
```

That eval is not free of builds: import-from-derivation pulls a couple of small
generated files through a builder first, so a few `building '...'` lines before
the result are expected rather than a sign it went wrong.

Do not use a deploy as a linter. A prod deploy here measured 74 minutes and
failed on a single eval error; the fix exposed the next one, three times over.
The command above would have found all three locally.

## See what the daemon is doing: `nix store builds`

```sh
nix store builds --json
```

Lists every in-flight build and substitution machine-wide, with `pid`,
`startTime`, `user`, `logFile`, and a `why` chain naming the root goal that
wanted this path and the cause (`requested`, `outputsMissing`,
`outputInvalid`). Patched nix; it reads `/nix/var/nix/status` directly rather
than connecting to a daemon, so it works when the daemon is wedged. Gated on the
`build-status-dir` experimental feature at both the writer and the reader, so an
empty array can mean the feature is off rather than idle.

A quiet systemd journal does not prove a build is idle. On dev-compute-6
(2026-09-13 UTC), native `nix build --store local` compiled Rust while its unit
journal stayed quiet; `/nix/var/nix/status/*-<nix-pid>.json` exposed the active
work and disappeared at completion. Keep the command's exit status and final
JSON result as the completion witness. For a floating content-addressed output,
`nix build --store local --no-link --json <drv>^out` resolved the finished path
when legacy `nix-store --query --outputs <drv>` could not.

## Which .nix file put this here: `whence`

```
whence ripgrep
  ripgrep 15.2.0
    ix/index/users/andrewgazelka/profiles/workstation.nix:? @ 0b85b77
    defined via:
      ix/index/users/andrewgazelka/profiles/workstation.nix
      ix/index/modules/home/cli-baseline.nix
```

`whence <path|pname>` reads the live generation's `provenance.json` with zero
evaluation. It covers deployed files and installed packages.

It only knows generations that ship that manifest: the home-manager profile and,
on darwin, `/run/current-system`. Fleet NixOS hosts ship neither the manifest nor
the binary, so `whence` on `hil-compute-2` is a command-not-found and the
question there needs the eval-time route instead.

That route is `options.<name>.definitionsWithLocations`, which names every file
that set an option.

In **both repos**:

```sh
nix eval --json .#nixosConfigurations --apply \
  'cs: map (d: d.file) (builtins.head (builtins.attrValues cs)).options.networking.firewall.enable.definitionsWithLocations'
# ["/nix/store/g469d8rgcg594b5hla5q1r1bkwh596f0-source/nix/modules/base/networking.nix"]
```

## A store-path bump is not a change: diff two generations

When a deploy restarts a service or triggers a handoff, the first cut is whether
the unit actually changed or only its store hashes moved. Unit files live under
`/nix/var/nix/profiles/system-<N>-link/etc/systemd/system/`.

```sh
u=etc/systemd/system/ix-cas-fabric-server@.service
norm() { sed -E "s|/nix/store/[a-z0-9]{32}-|H-|g" "$1"; }
diff <(norm /nix/var/nix/profiles/system-1446-link/$u) \
     <(norm /nix/var/nix/profiles/system-1447-link/$u)
```

Raw, that pair differs on one line: an `ExecStart` store hash. Normalized, it is
byte identical. Nothing about the service changed.

Measured on `hil-compute-2` over the 60 most recent generations, that unit
changed in 34 of 59 consecutive pairs, and 29 of the 34 normalized to byte
identical. The 5 real changes were `NonBlocking=true`, `TimeoutStopSec` 35s to
45s, `TasksMax` 1024 to 464, an added `ExecCondition`, and a gcc minor bump
inside `LD_LIBRARY_PATH`. A change to that unit costs a fabric handoff, and
handoff cost is why fleet auto-deploy runs on a 6 hour timer rather than per
commit, so 29 of those 34 deploys paid the timer's whole reason for nothing.

The two findings take different fixes. A semantic change earns its restart. A
pure path bump means the unit is carrying a store path it does not need to
carry, and `nix-dag` on the same closure will name the edge that puts it there.

## Verify the effective unit before a handoff

After `daemon-reload`, inspect `systemctl show <unit> -p ExecStart -p DropInPaths`
before replacing a process. Drop-ins sort lexically: `100-progress.conf` loaded
before `99-no-autofix.conf` on hil-compute-1 (2026-09-13 UTC), so a restart
launched the old executable. Confirm the desired wrapper and arguments in the
effective property, then verify the new PID's `/proc/<pid>/exe` and live output.

A terminal CI progress record can precede landing or recovery. When proving an
idle boundary without a drain API, use the installed executable's actual caller
site and empty owned process groups. Linux `clock_nanosleep` can use the same
pointer for requested and remaining time: a native control changed `[2, 0]` to
`[1, 810764893]` after `SIGSTOP`. Comparing the stopped request to the original
duration rejects a valid boundary. Retire each independent resume watchdog
before another freeze of the same PID; an older watchdog can otherwise resume
the next attempt. Revalidate caller offsets whenever the executable changes.

## NAR integrity and content-address identity are separate checks

A matching NAR hash proves the transferred bytes match their metadata. It does
not alone prove a self-referential content address. In the September 2026 ix
cache incident, an older producer zeroed self references without recording their
positions; NAR verification passed, but the canonical importer rejected the CA.
The native verifier reproduced that rejection on the preserved cache entry and
accepted a newly built output. The canonical hash includes the reference-offset
trailer; accepting either hash would weaken the identity contract.

For this failure, retain the rejected bytes for diagnosis, require the native
consumer's CA check, and rebuild affected outputs with the canonical producer.
Reuse unaffected outputs that pass admission. Do not disable signature or CA
checks, delete shared store paths, or infer that every historical output is bad.
Older `nix store verify` implementations may check only NAR integrity: verify the
actual implementation or use normal native import before claiming CA validity.
