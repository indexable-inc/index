# Embedding the evaluator {#nix_evaluator_example}

The Rust runtime, `libnix_eval_rs`, owns language evaluation, builtin availability,
and language documentation. Its C interface is documented in
[`ixe.h`](../../rust/nix-eval-rs/include/ixe.h); command evaluation is described in
[`ixe-command.h`](../../rust/nix-eval-rs/include/ixe-command.h).

An `IxeSession` owns evaluation handles. The embedding application supplies an
`IxeHostVtable` for filesystem access, fetching, store operations, and builds
requested during evaluation. The headers specify callback lifetimes, error
results, and how to release returned strings and handles. The production host
implementation is in `src/libcmd/rust-eval-session.cc`.

The store and host-value C APIs support applications that manage store objects
or exchange host values. Local primitive callbacks can be allocated and called
through the host-value API. Language builtins are implemented and documented in
the Rust runtime; plugins can extend configuration through `plugin-files`.
