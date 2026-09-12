# Evaluator command surface

Rust is the sole evaluator. There is no backend setting or experimental opt-in.
C++ supplies store operations, fetching, flake locking, and CLI plumbing.

| Command | Supported evaluation |
|---|---|
| `nix eval` | Expressions, plain files, and flake attributes; JSON/raw/plain rendering, arguments, and `--apply`. |
| `nix-instantiate` | Rendering with `--eval`; derivation-set instantiation otherwise; `-A`, `--arg`, and `--argstr`. |
| `nix build`, `nix shell`, derivation-based installable commands | Rust selects derivations and outputs; the store builds or queries them. Direct store paths require no evaluation. |
| `nix-build` | Derivation sets and lists, attribute selection, arguments, and direct `.drv` paths. |
| `nix run` | Apps and derivations, including application precedence and `meta.mainProgram`. |
| `nix develop`, `nix print-dev-env` | Development derivations; `develop` also resolves interactive Bash through Rust. |
| `nix flake show` | Lazy output inspection with text/JSON, legacy packages, and system selection. |
| Flake metadata, lock, update, archive, prefetch-inputs | Rust reads each metadata document; C++ manages the lock and fetching. |
| Command help | Rust evaluates the manual-page generator. |

Flake metadata may be computed, including file reads. Reading a document does
not call its outputs function. Metadata and output questions use the same
persistent cache protocol: replay observed dependencies before serving a
previous answer, and retain prior answers so reverting an edit can reuse them.

Operations still awaiting Rust command support fail explicitly: value-handle
commands such as search/edit/fmt/bundle/REPL, flake check and templates, profile
installation requiring origin metadata, legacy nix-shell, `eval --write-to`,
`develop --redirect`, stdin sources, parser-tree printing, and some lazy/XML
render modes. Attribute completion currently completes only flake references.

`tests/functional/rust-eval-*.sh` checks command results, store effects, and cache
counters. The `nix-ix.tests.rustEvalTests` package runs that suite and rejects
skipped tests. `nix-ix.nixEvalRs` runs the Rust crate tests;
`nix-ix.tests.nixEvalRsClippy` checks all targets in both feature configurations.
