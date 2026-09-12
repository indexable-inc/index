# ahujasid/blender-mcp, built from the pinned GitHub source (not `uvx` from
# PyPI at runtime): the same rev carries both halves of the bridge — the stdio
# MCP server (this package's `blender-mcp` binary) and the in-Blender addon
# (`passthru.addon`) — so pinning one source keeps them version-matched and the
# whole closure nix-managed. Chosen over Blender Lab's official MCP because
# that project ships as a manual release bundle with no pinnable source
# install; revisit when it grows one.
{
  ix,
  lib,
  pkgs,
  # Writer for `passthru.updateScript` (flake-package path only; the package
  # is not registered in the overlay). Same nullable-writer pattern as
  # vector-bin / wasm-bindgen-cli.
  updateScriptWriter ? null,
}: let
  # Rev + SRI hash live in the sibling pins.json, never inline here (repo
  # policy: no `hash = "sha256-..."` literals in tracked .nix). Bump the
  # rev/url in pins.json, then `nix run .#update` re-pins the hash.
  pin = ix.pins.loadPin ./pins.json "blender-mcp";
  updateScript = ix.pins.mkOptionalUpdater {
    writeNushellApplication = updateScriptWriter;
    nix = ix.nixPackageFor "packages/blender-mcp: updateScript";
    pname = "blender-mcp";
    relPath = "packages/blender-mcp/pins.json";
  };

  src = pkgs.fetchzip {inherit (pin) url hash;};

  # Upstream builds with setuptools, not uv's backend, so build isolation
  # would try to fetch setuptools from an index the sandbox cannot reach;
  # provide the backend on the interpreter and build unisolated instead.
  python = pkgs.python3.withPackages (ps: [
    ps.setuptools
    ps.wheel
  ]);
in
  (ix.buildUvApplication pkgs {
    pname = "blender-mcp";
    inherit (pin) version;
    inherit src python;
    buildFlags = ["--no-build-isolation"];
    # Third-party source: upstream is untyped, so the repo's strict
    # zuban/ruff-ANN gates (meant for repo-owned Python) cannot pass here.
    check = false;
    meta = {
      description = "MCP server bridging agents to a running Blender via its companion addon";
      homepage = "https://github.com/ahujasid/blender-mcp";
      license = lib.licenses.mit;
      mainProgram = "blender-mcp";
    };
  }).overrideAttrs
  (old: {
    passthru =
      (old.passthru or {})
      // {
        # The in-Blender half of the bridge, from the SAME pinned rev as the
        # server. Consumers load it into Blender (e.g. a scripts/startup hook)
        # so the socket protocol on :9876 cannot drift from the server.
        addon = "${src}/addon.py";
      }
      // lib.optionalAttrs (updateScript != null) {inherit updateScript;};
  })
