---
name: rust-style
description: "Rust house style for repo-owned crates: edition 2024, naming, module layout, unsafe validation (Miri/loom/shuttle), mutation testing, fuzzing. Use when writing or reviewing Rust."
---

## Rust style

Repo-owned crates, fixtures, examples, and generated manifests use Rust edition
2024. Fix compatibility issues directly and document unavoidable upstream
blockers next to the exception.

Whether to format Rust depends on the repo, and this skill loads in both.

In **ix**, run `nix fmt -- <paths>`. Formatting is enforced: the `treefmt-check`
stage of `just lint` runs the exact rustfmt the `rust-toolchain.toml` nightly
pins, over `crates/**/*.rs` and `nix/**/*.rs`
(`nix/checks/treefmt.nix`). It has been enforced since ix#8162, before which
committed code had drifted from that rustfmt.

In **index**, do not run `cargo fmt`. Nothing enforces `rustfmt` there -- the
repo has no `rustfmt.toml` and none of the fifteen `nix run .#lint` stages
formats Rust -- so running it produces diff noise unrelated to the change.
Style is enforced by Clippy and code review.

To run the same per-unit clippy that CI runs (the `llm-clippy` fork with
`fallible_int_fallback` and `anonymous_tuple_return_type`), reach for the
attribute belonging to the repo you are in. They are spelled differently and
neither resolves in the other, so the wrong one fails with a bare
`does not provide attribute`, whose obvious recovery is the bare `cargo clippy`
this whole section exists to stop you running.

In **index**:

```sh
nix build .#ciChecks.x86_64-linux.<unit>.clippy   # <unit> is usually rust-<crate-name>
```

`<unit>` is the package's `passthruTests.prefix`, which defaults to
`rust-<crate-name>` but is overridden by some packages. `unibind-gen` uses its
own name. To find the right one rather than guess,
filter one level in, because no unit is NAMED `clippy` and grepping the
top-level names for it returns a single false positive:

```sh
nix eval --json .#ciChecks.x86_64-linux \
  --apply 'cs: builtins.filter (n: cs.${n} ? clippy) (builtins.attrNames cs)'
```

In **ix**, there is no `ciChecks`. The per-crate checks hang off
`legacyPackages`, keyed by the **cargo package name** -- the `[package] name` in
the crate's `Cargo.toml`, not the directory and not a `rust-` prefix:

```sh
nix build .#legacyPackages.x86_64-linux.rustClippyChecksByPackage.<cargo-package-name>
```

Before choosing ix Clippy work, read `lib/rust-clippy-gate-packages.nix`: available checks include both `gated` and `knownRed` packages. Compile affected dependencies and run required checks; historical known-red cleanup needs its own task scope. A new package must be classified after measuring its check. In the 2026-09-08 storage integration, selecting CAS-disk Clippy unnecessarily expanded into 1,519 diagnostics; that draft was parked and the required segstore checks resumed.

The CLI lives at `crates/ix/cli` and its package is `ix`, so its check is
`rustClippyChecksByPackage.ix`. It sits under `legacyPackages` rather than
`checks` because the names come from a planner IFD and the values are nested
attrsets, and it evaluates only on Linux systems
(`nix/flake/outputs/workspace.nix`). To list the 250 keys rather than guess:

```sh
nix eval --json .#legacyPackages.x86_64-linux.rustClippyChecksByPackage \
  --apply builtins.attrNames
```

After integrating Cargo manifest changes, reconcile each affected workspace's
lock with the pinned Cargo and its actual offline vendor configuration in a
private writable planner copy. Run the original `--frozen --offline` unit-graph
commands afterward and verify the lock stays unchanged. Parsing or byte-matching
a manually edited lock does not prove dependency consistency. Inspect every
changed package, dependency edge, source, version and checksum; keep the
production frozen check enabled.

Prefer names that preserve the concept's path. Local aliases may shorten noisy
source paths only when the shape remains visible at the call site. Keep singular
names for single values and plural names for bags of constructors, helpers, or
registry entries.

Use local type annotations when they make the data shape clearer. Keep turbofish
for expression-local cases where an intermediate binding would add noise.

Use normal module layout. Move files so `mod` declarations follow the filesystem
instead of using `#[path = ...]`.

Avoid anonymous tuple-shaped domain data once a value crosses a function
boundary. Prefer named structs or full paths for values that carry real meaning.

Use blank lines as paragraph breaks inside functions: set up, act, then validate
or return. Keep tightly coupled statements together.

When parsing, normalizing, serializing, traversing graphs, handling archives, or
speaking protocols, start from a maintained crate. Hand-written logic is for the
thin glue around that crate unless the dependency boundary is measurably worse.

Validate `unsafe` Rust with runtime checks before trusting normal tests. Run
Miri where it works; for blocks Miri rejects because they need FFI, platform
syscalls, or real native execution, run [`cargo-careful`](https://github.com/RalfJung/cargo-careful)
with `cargo +nightly careful test -p <crate>`. cargo-careful exercises code
against a debug-assertion standard library and surfaces some unsafe-precondition
and stdlib-invariant breakage, but it does not model aliasing, uninitialized
reads, or data races, so it complements Miri rather than replacing it.

Use [`loom`](https://docs.rs/loom/latest/loom/) for small deterministic
concurrency primitives whose state fits inside modeled threads, atomics, and
`std::sync` replacements. Use [`shuttle`](https://docs.rs/shuttle/latest/shuttle/)
for larger randomized scheduler tests, especially Tokio-shaped workflows; skip
both when the test would mainly prove a dependency's lock, channel, or runtime
works instead of a repo-owned invariant.

When auditing a crate with deterministic, fast tests, run
[`cargo-mutants`](https://mutants.rs/) with
`nix shell nixpkgs#cargo-mutants -c cargo mutants --package <name>` to surface
behavior that coverage cannot prove protected. Let the default copy-to-`target`
mode hold; `--in-place` is faster but leaves the source tree dirty on interrupt
or panic, so reserve it for disposable checkouts. Treat surviving mutants as
candidates for tighter assertions, equivalent-mutant write-offs, or
unreachable-by-test code, and keep cargo-mutants a package-owner tool rather
than a CI gate: equivalent mutants need human judgment, runtime scales with
mutant count, and a survivor is a prompt to look, not a regression to block.

Fuzz Rust surfaces that read untrusted bytes: parsers, codecs, deserializers,
protocol handlers, archive readers, and unsafe or FFI-adjacent input edges.
Scaffold with `cargo fuzz init` so targets land in
`packages/<crate>/fuzz/fuzz_targets/<name>.rs`; the fuzz crate keeps its own
`[workspace]` table so it stays off the main `cargo --workspace` graph. Commit
hand-picked seeds under `fuzz/seeds/<target>/`, gitignore `fuzz/corpus/`, and
minimize crashes with `cargo fuzz cmin <target>` or
`cargo fuzz tmin <target> <path>` before committing the reduced input as a
regression seed. `packages/minecraft/nbt/fuzz/` is the worked example; see
[the cargo-fuzz book](https://rust-fuzz.github.io/book/cargo-fuzz.html) for
the libFuzzer flag surface.
