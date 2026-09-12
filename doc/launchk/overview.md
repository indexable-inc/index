# launchk

`packages/launchk` builds the checked source at `index/views/launchk`. The package is macOS-only because it talks to launchd over XPC.

## source ownership

`index/lib/default.nix` exposes the tracked path as `ix.launchkSrc`. The ix root `views.toml` declares that path as a git view of `intellekthq/launchk` (`master`); the recorded import names the upstream commit the subtree is based on. Package evaluation needs no source URL or source flake.

The view history owns the ix source change. The window title reads `CARGO_PKG_VERSION`, so a Nix build does not need a `.git` directory or the `git-version` crate.

## build

`index/packages/launchk/default.nix` uses `rustPlatform.buildRustPackage` with the view's committed `Cargo.lock`. `rustPlatform.bindgenHook` supplies libclang for `xpc-sys`. Build and test flags select the `launchk` package. The package and flake outputs are limited to Darwin systems.

## update

1. Create a dedicated jj workspace for ix and edit `index/views/launchk` there.
2. Commit the upstream change and any ix source change in the host history.
3. Run the Launchk package build and tests from the checked view.
4. Land the ix change through the forge (`jj submit`); a view has no push leg.
