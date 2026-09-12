# nix-cargo-unit

`packages/nix-cargo-unit` renders a Cargo unit graph into composable Nix
derivations: one `stdenv.mkDerivation` per rustc invocation, wired into a graph
that mirrors Cargo's own. It is the bootstrap build tool for the whole repo's
Rust tree. The nix-lib domain
(`lib/rust/cargo-unit.nix`) wraps this binary into `ix.cargoUnit`, which every
Rust package's `default.nix` consumes via `selectBinaryWithTests` /
`buildWorkspace`. This page covers the purpose, CLI, and the Nix surface it
emits; the hashing, source-scoping, merge, and panic-scan mechanics are in
[internals](internals.md).

## Why it exists

Cargo's unit graph (`cargo build --unit-graph -Z unstable-options`) is the exact
plan Cargo would execute: every crate, in every mode, with its resolved features
and flags, and edges between them. nix-cargo-unit transcribes that plan into Nix
so each unit is a separately cached, separately invalidated derivation. Editing
one crate moves only that unit's identity hash (and its dependents'), so the
store reuses everything else: far finer-grained than one derivation per crate
(`buildRustPackage`) or one per crate-version (crate2nix), and shared by the rest
of the repo through `ix.cargoUnit`.

## CLI surface (`src/main.rs`)

Three subcommands (`src/main.rs:25`):

- **`render`** (`src/main.rs:60`): read a unit-graph JSON on stdin, write
  `units.nix` to stdout. Flags:
  - `--workspace-root <PATH>` (`:62`): canonical workspace root from the graph.
  - `--vendor-root <PATH>` (`:67`): Cargo vendor dir for registry/git crates.
  - `--cargo-lock <PATH>` (`:71`, required): resolves each external unit's exact
    registry/sparse/git source identity (`src/render.rs:102`).
  - `--content-addressed` (`:75`): emit CA-derivation attributes on units.
  - `--toolchain-id <ID>` (`:79`): salt every unit identity hash with a Rust
    toolchain id, so a toolchain bump invalidates the graph.
  - `--deny-unused-crate-dependencies` (`:83`): emit a per-package check that
    fails on dependencies unused across all of a package's local units.
  - `--deny-panics` (`:88`): emit a per-unit panic-freedom policy check (see
    [internals](internals.md)).
- **`merge`** (`src/main.rs:53`): merge several unit-graph JSON files into one,
  deduplicating units by identity hash and recording each input's roots as a
  `root_set`. Used when a workspace is built for several `cargoTargets`
  (e.g. a host plus a wasm target) and the graphs are unioned before render.
- **`scan-panics`** (`src/main.rs:38`): scan compiled `.rlib`/`.o` artifacts for
  functions that can reach panic machinery, scoped to `--crate-name <NAME>`
  (repeatable). Exits 1 and lists `function -> panic_entrypoint` on any finding.
  This is the engine behind `--deny-panics`; see [internals](internals.md).

`main` installs `color_eyre` and dispatches (`src/main.rs:179`). `render` and
`scan-panics` are pure transforms over their inputs; nothing shells out to Cargo
or Nix (the wrapper in `lib/rust/cargo-unit.nix` runs Cargo and the IFD).

## The `units.nix` it emits

`render_units_nix` (`src/render.rs:271`) fills the template
(`templates/units.nix.in`, filled by plain `{{ slot }}` substitution) whose output is a Nix function. The function takes
`pkgs`, `rustToolchain`, `src`, `workspaceRoot`, and many optional knobs
(`vendorSources`, `extraEnv`, `packageBuildEnv`, `extraUnits`, `clippyEnabled`,
`includeIgnored`, ...), and returns an attrset. The load-bearing outputs
(template lines `812`-`843`):

| output | what |
| --- | --- |
| `units` | every rustc unit as `mkUnit` derivation, keyed `<name>-<version>-<hash>`; merged over `extraUnits` so a prebuilt unit can be injected or override one. |
| `roots`, `checkedRoots` | root unit references; `default` is the first root wrapped in `withPolicyChecks`. |
| `packages`, `binaries`, `libraries` | root units filtered by kind, keyed by target name. |
| `tests`, `doctests`, `benchmarks` | per-target entries; each `.all` runs the whole binary, `.cases` fans out one derivation per `#[test]`/doctest via a manifest IFD. |
| `testPlan`, `benchmarkPlan`, `coverageReport`, `compareTangoBenchmarks` | aggregate test/bench plans and an `llvm-cov` coverage report builder. |
| `clippyUnits`, `clippyByPackage` | per-unit clippy-driver derivations, joined per package so editing one crate rebuilds only its clippy gate. |
| `policyChecks`, `unusedCrateDependenciesByPackage` | workspace panic-freedom / audit checks (aggregate) and the per-package unused-dependency gate. |
| `sourceAudit` | per-source `{ base, scope, relative, includeRelatives, sourceKey }` records for the coverage and audit paths. |

Each unit is built with `dontUnpack`/`dontConfigure` and an `env` merged from the
unit's own env, workspace `extraEnv`, and per-package `packageBuildEnv`
(`units.nix.in:161`). A render-time `packageName` tag lets per-package env
target a package's own compile and build-script-run units without touching the
shared dependency closure.

## Source scoping

Each unit references a `sources.<name>` entry rather than the whole tree, so one
workspace crate edit does not invalidate its siblings. The renderer classifies
every unit's source into one of four `SourceBase` shapes (workspace package,
workspace closure, vendor package, vendor closure; `src/render.rs:182`) and emits
a `builtins.path` with a filter scoped to just that crate's directory (or the
include set a closure needs). The `Cargo.lock` source map disambiguates which
vendored path a registry/git unit resolves to (`src/render.rs:102`-`155`). Detail
in [internals](internals.md).

Nested Cargo packages remain part of their parent source by default. A package
may declare that those child packages are separate inputs:

```toml
[package.metadata.ix.inputs]
exclude-nested-packages = true
```

Before enabling this boolean, audit the package's build tools, procedural
macros, module paths and included files. A child file read while compiling the
parent is a parent input even if Cargo treats the child as another package.
The renderer preserves recognized explicit includes and module-path attributes,
and keeps the full package when a build script or unresolved syntax prevents a
safe split. This conservative scan supplements the package owner's audit; it
cannot infer arbitrary filesystem reads by procedural macros. Revisit the audit
when adding a macro, build tool or cross-package source input. The rendered
`sourceAudit.excludedRelatives` records any excluded child roots.

## How it is built and wired

- **Package** (`default.nix:16`): built as a plain `ix.buildRustPackage` with
  `srcRoot = ./.`, NOT through `ix.cargoUnit`: it bootstraps the unit graph, so
  it cannot consume itself. The version is read from `Cargo.toml`
  (`default.nix:20`) so it cannot drift.
- **Standalone workspace** (`Cargo.toml:32`): its own `[workspace]` and
  `Cargo.lock`, excluded from the root workspace (root `Cargo.toml` `exclude`),
  so a root-lock bump elsewhere never recompiles it and its Nix build keys only
  on `packages/nix-cargo-unit/`.
- **Flake output** (`package.nix`): `flake = true`, `packageSet = true`,
  `passthruTests = true`. Run as `nix run .#nix-cargo-unit`. NOT
  `inRustWorkspace` (it is its own workspace).
- **Consumed by** `lib/rust/cargo-unit.nix`: `buildWorkspace` runs
  `cargo ... --unit-graph` per `cargoTargets` entry, pipes the graphs through
  `nix-cargo-unit merge`, then `nix-cargo-unit render --cargo-lock ...`
  (two IFD stages), imports the resulting `units.nix`, and exposes the result as
  `ix.cargoUnit` / `ix.rustWorkspace.units`. The `--deny-panics` check calls back
  into this same package as the `scan-panics` scanner
  (`src/render.rs:509`).

## Dependencies

`clap` (CLI), `color-eyre` (errors), `object` (rlib/object
parsing for the panic scan), `serde`/`serde_json` (unit graph), `sha2` (identity
hashing), `toml` (`Cargo.lock` and manifests), `url` (package-id parsing),
`proc-macro2` (Rust token boundaries for source-input checks).

## Module map

- `model.rs`: unit-graph types, `parse_pkg_id`, identity hashing, `merge`.
- `render.rs`: the whole `units.nix` renderer (sources, units, tests, policy).
- `panic_scan.rs`: relocation-based panic-reachability scan.
- `hash.rs`: `short`/`short_digest` (16-hex SHA-256 prefix).
- `shell.rs`: shell single/double quoting helpers for generated scripts.

See [internals](internals.md) for hashing, merge dedup, source closures, and the
panic scanner.
