R""(

# Description

Evaluate each *installable* through the Rust evaluator and emit one JSON object
per request. The process keeps store connections and immutable input accessors
open. Each request uses a fresh evaluation context and reuses compiled modules
and complete answers from the validated incremental cache.

Before every request, the host refreshes mutable filesystem metadata, input
references, path lookups and source-copy mappings. Edits to files read by an
answer invalidate that answer. Unchanged dependencies and previously seen
revisions can reuse cached work.

The cache directory is `eval-cache-dir` when configured, otherwise
`$XDG_CACHE_HOME/nix/eval` (normally `~/.cache/nix/eval`). Cache size and verification
sampling use `eval-cache-max-bytes` and `eval-cache-verify-rate`. The process
retains decoded dependency witnesses up to 512 MiB of accounted payload bytes
and 64 selected questions. `--memory-cache-size` changes the byte budget; zero
disables this memory retention while keeping validated disk reuse.

With `--interactive`, the command reads one installable per line from standard
input after processing positional installables. Empty lines are ignored. Each
report is written immediately, so a caller can edit the source tree between
requests. The first failed request terminates the command with a nonzero exit
status; completed reports remain on stdout, and the error goes to stderr.

For a bounded noninteractive batch, use `--request-file PATH`. The file must be
a regular UTF-8 JSON file with this schema:

```json
{
  "version": 1,
  "requests": [
    {"id": "batch-0", "installable": ".#checks.x86_64-linux", "apply": "checks: builtins.attrNames checks"},
    {"id": "batch-1", "installable": ".#value", "apply": null}
  ]
}
```

All fields are required; unknown or duplicate fields are rejected. IDs are
opaque, unique strings of 1–128 UTF-8 bytes. The file is limited to 4 MiB and
1–256 requests. An installable is limited to 64 KiB and an apply expression to
1 MiB; neither can contain NUL. Each request uses the same source resolution,
input refresh and evaluation boundary as a positional request. `apply` has the
same meaning as `nix eval --apply`, and is part of the result-cache key. `null`
means no application. `--file` and `--expr` retain their usual process-wide
meaning. Positional requests and `--interactive` cannot be combined with
`--request-file`.

The entire file is validated before evaluating its first request. Each success
report adds `version: 1`, the exact `id`, and `status: "ok"` to the report below.
An evaluation failure emits a line with `version`, `id`, `installable`,
`status: "error"` and diagnostic `error`, then exits nonzero without evaluating
later requests. Parse, framing, output-budget and I/O failures can terminate
without an error report. Each encoded line, including its newline, is limited
to 8 MiB; total encoded output is limited to 64 MiB. These bound serialized
output, not evaluator RSS or the memory needed to produce a value.

A coordinator must require successful process termination and exactly one
successful report for every expected ID in order. A completed prefix is not a
successful batch. Callers own source/request-file immutability, tenant
isolation, deadlines and resource limits. The command does not read stdin in
request-file mode. Existing positional and interactive report formats are
unchanged.

Each report contains the installable, its JSON `value`, elapsed `wallMs` and
`cpuMs`, and the number of mutable input-cache entries evicted. The `stats`
object reports Rust `compiles`, `compileHits`, `memoServed`, `hostQuestions`,
`importHits` and `copyMemoServed` for the selected result question. Timings also
include source resolution. `countersEnabled` distinguishes measured zeroes
from a build with instrumentation disabled.

`witnessMemoryHits` and `witnessDiskLoads` count reuse of retained witnesses and
successfully decoded disk witnesses. `witnessCacheBytes` and
`witnessCacheEntries` report current retained payload occupancy; the byte count
is not process RSS. `subtreeMemoHits`, `subtreeMemoMisses` and
`subtreeEntryForces` distinguish persisted imported-scalar reuse from actual
imported-root evaluation. This finer reuse applies to statically certified pure
imports returning scalars. Host operations, returned closures and containers
are excluded; sampled whole-answer verification executes imports afresh.

# Examples

Evaluate a system derivation twice and inspect the warm cache hit:

```console
$ nix eval-persistent .#nixosConfigurations.hil-compute-1.config.system.build.toplevel.drvPath \
    .#nixosConfigurations.hil-compute-1.config.system.build.toplevel.drvPath
```

Start an interactive process and enter an installable after each source edit:

```console
$ nix eval-persistent --interactive
.#value
```

Use attribute selections from a local file:

```console
$ nix eval-persistent --file ./default.nix package.version package.version
```

)""
