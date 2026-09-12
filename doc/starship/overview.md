# starship

`packages/starship` repackages [starship](https://github.com/starship/starship),
the cross-shell prompt, from a repo-owned fork instead of the upstream source.
It is the minimal repackage shape: take the nixpkgs `starship` derivation and
override only its `src`.

## What this repo changes

The fork carries one commit on top of `v1.26.0`: starship resolves the
repository root through `Context::get_repo`, which is gix discovery and so
answers for git only. A non-colocated jj workspace -- a `.jj` with no `.git`,
which is jj's default -- has nothing for gix to find, so two callers that only
wanted "where does this checkout start" behave as if the directory were not in
a repository at all:

- `directory.truncate_to_repo` prints the absolute path where it should print
  the repository name (`/Volumes/Projects/nix` instead of `nix`).
- `custom.require_repo` skips every module gated on it, silently.

The commit adds `Context::get_vcs_workdir`, one upward walk looking for both
`.jj` and `.git`, and points those two callers at it. The nearest marker wins,
so a jj root inside a git worktree resolves to jj and a plain git checkout
nested inside a jj workspace resolves to git; when git is at least as near, the
lookup falls through to gix, which stays the authority on worktree boundaries
(linked worktrees, `GIT_DIR`, ceiling directories). `get_repo` itself is
untouched, so `git_branch`, `git_status` and the rest still report git only.

The patch is upstream-shaped and not yet offered upstream; starship's in-flight
jj work (starship#7613, starship#7224) adds `jj_*` content modules and does not
touch directory truncation.

## Build and wiring

- Flake output: `nix build .#starship`. `package.nix` sets `packageSet = true`
  and `flake = true`; `overlay = false`, so `pkgs.starship` stays the plain
  nixpkgs prompt and only consumers that name this package get the fork.
- Consumer: `users/andrewgazelka/profiles/workstation.nix` sets
  `programs.starship.package`.
- `meta.homepage` points at `indexable-inc/starship` so the built package
  advertises the fork, not upstream.
- Update: `jj view refresh starship` from a jj workspace, then move the
  `starship.version` assert in `default.nix` to the new tag.

## The version assert

nixpkgs pins both the tag it fetches and the `cargoHash` for that tag's
`Cargo.lock`. Overriding `src` alone would let a nixpkgs starship bump build the
old view tree under the new version label, so `default.nix` asserts
`starship.version == "1.26.0"` and fails eval instead.

## Why `cargoHash` needs no regeneration

nixpkgs derives `cargoDeps = fetchCargoVendor { src = finalAttrs.src; hash =
args.cargoHash; }`, so overriding `src` does re-point that fixed-output
derivation. Its store path is a function of `hash` plus `name` (from the
unchanged `pname`/`version`), both unchanged, and the fork touches no
`Cargo.toml` and no `Cargo.lock`, so the vendored content is identical too.

**This holds only while the view adds no dependency.** The moment a fork commit
edits `Cargo.toml` or `Cargo.lock`, `cargoHash` has to be recomputed and set
here explicitly.
