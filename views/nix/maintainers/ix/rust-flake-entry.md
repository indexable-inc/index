# Flake evaluation

Flake locking asks the Rust evaluator for a `FlakeDocument`. The document
contains description, inputs, configuration, and output-function formal names.
C++ parses that JSON into the lock model and handles fetching and lock writes.
There is one document producer and one parser.

The source carries both its mounted input root and its original file path.
Relative expressions resolve beside the source's symlink target; document
paths are encoded relative to the original flake directory. Inputs are mounted
before the first metadata read.

Metadata fields may run expressions. Every force and render uses the same
`JobMemo`, so dependencies discovered while reading fields are included in the
persistent answer's read set. The outputs function is never called by this
question. A later output question evaluates `call-flake.nix` with the locked
input graph.

Tests cover computed metadata, mounted roots, symlinked documents, input and
configuration types, cold/warm reuse, dependency edits, and A→B→A reuse.
