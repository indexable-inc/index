---
synopsis: "Writing a lock file into a commit-identified source needs `--commit-lock-file`"
---

Nix writes a flake's lock file into the flake's own source. That mutates the
source, and for a source identified by a commit there is then no revision
describing what is on disk: the write lands, and the re-read that follows it
refuses the very flake the write was for. Since a Git working tree with
uncommitted changes is now refused outright, this was reachable simply by
building a Git flake whose lock file was out of date.

Nix now refuses up front instead, naming the three ways forward:

- `--commit-lock-file`, which commits the lock file as part of the update, so
  the source has a revision again when the flake is re-read.
- `--no-write-lock-file`, to evaluate without writing anything into the source.
- keeping the flake in a jj workspace, where no flag is needed at all: jj
  snapshots at every fetch, so the lock file is part of the next revision by
  construction.

Two smaller consequences of the same rule:

- Writing a file into a Git input without a commit message is now an error
  rather than a half-done `git add --intent-to-add`, and writing to a path Git
  is configured to ignore is an error too. That write used to succeed while
  producing nothing a later fetch could see.
- `builtins.fetchGit` no longer reports the all-zero `rev` and `shortRev` it
  used for a dirty repository, because no fetch produces a tree without a
  revision any more. It still reports `revCount = 0` for a shallow fetch,
  where the count genuinely is not computed.
