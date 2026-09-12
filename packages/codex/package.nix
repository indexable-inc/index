{
  id = "codex";
  workspaceIfdRoots = true;
  # `packageSet` here means the index package set (`index.packages.<sys>.codex`,
  # built via packageSetFor), NOT a nixpkgs overlay: it does not inject into
  # `pkgs`, so `pkgs.codex` stays the plain nixpkgs CLI (see the `flake`-only
  # note below).
  packageSet = true;
  # Flake-output only, deliberately NOT an overlay: `pkgs.codex` must stay the
  # plain nixpkgs CLI because the room-server wrapper pins `pkgs.codex`
  # as the binary it spawns over JSON-RPC. Our wrapper is an additive output
  # (`nix run .#codex`, `index.packages.<sys>.codex`) that bakes our defaults on
  # top of that same base, without changing what the overlay hands other code.
  flake = true;
  overlay = false;
  # RFC 0009 cross lane: on a Linux build host, also expose codex cross-compiled
  # to Darwin (default target aarch64-apple-darwin), so the darwin cache lane can
  # substitute it instead of building codex-rs natively on a Mac. default.nix
  # reads the `ix.cross` signal and threads the target into rust.nix.
  cross = true;
}
