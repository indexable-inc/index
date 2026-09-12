# The OFFICIAL Blender Lab MCP (projects.blender.org/lab/blender_mcp), the
# docs-oriented counterpart to the community bridge in packages/blender-mcp:
# blendfile summaries, Python API / manual lookup, screenshots, plus code
# execution. Same two-half shape (stdio server + in-Blender addon), and the
# same one-pinned-rev rule keeps them version-matched (`passthru.addon`).
#
# Fetched from the bpype/blender_mcp GitHub mirror because the canonical
# projects.blender.org gitea 403s nix's fetcher (curl passes); the pinned
# commit SHA is identical on both remotes, and a git commit hash covers its
# whole tree, so the mirror bytes are provably the upstream bytes.
{
  fetchzip,
  ix,
  lib,
  python3,
  # Writer for `passthru.updateScript` (flake-package path only; the package
  # is not registered in the overlay). Same nullable-writer pattern as
  # blender-mcp / vector-bin.
  updateScriptWriter ? null,
}: let
  # Rev + SRI hash live in the sibling pins.json, never inline here (repo
  # policy: no `hash = "sha256-..."` literals in tracked .nix). Bump the
  # rev/url in pins.json, then `nix run .#update` re-pins the hash.
  pin = ix.pins.loadPin ./pins.json "blender-lab-mcp";
  updateScript = ix.pins.mkOptionalUpdater {
    writeNushellApplication = updateScriptWriter;
    nix = ix.nixPackageFor "packages/blender-lab-mcp: updateScript";
    pname = "blender-lab-mcp";
    relPath = "packages/blender-lab-mcp/pins.json";
  };

  src = fetchzip {inherit (pin) url hash;};
in
  (python3.pkgs.buildPythonApplication {
    pname = "blender-lab-mcp";
    inherit (pin) version;
    inherit src;
    # The server is the repo's `mcp/` subproject (`blmcp` package); `addon/` and
    # `chat_client/` are separate non-installable pieces.
    sourceRoot = "${src.name}/mcp";
    pyproject = true;

    build-system = [python3.pkgs.setuptools];
    # Upstream deps (mcp/pyproject.toml): docutils, mcp[cli], pyyaml. No lock
    # file upstream, so versions pin through nixpkgs rather than a wheelhouse.
    dependencies = [
      python3.pkgs.docutils
      python3.pkgs.mcp
      python3.pkgs.pyyaml
    ];

    # Upstream names its console script `blender-mcp`, colliding with the
    # community package's binary in a merged profile; the flake id is the
    # unambiguous name.
    postInstall = ''
      # shell
      mv "$out/bin/blender-mcp" "$out/bin/blender-lab-mcp"
    '';

    # Catches a missing transitive dep at build time (there is no upstream lock
    # to trust); upstream's own tests need a running Blender, so skip them.
    pythonImportsCheck = ["blmcp"];
    doCheck = false;

    meta = {
      description = "Official Blender Lab MCP server for scene analysis, docs lookup, and code execution";
      homepage = "https://projects.blender.org/lab/blender_mcp";
      license = lib.licenses.gpl3Plus;
      mainProgram = "blender-lab-mcp";
    };
  }).overrideAttrs
  (old: {
    passthru =
      (old.passthru or {})
      // {
        # The in-Blender half, from the SAME pinned rev as the server: a proper
        # addon package (`blender_mcp_addon/`) consumers link into Blender's
        # scripts/addons and enable. Its TCP port preference must match the
        # registry entry's BLENDER_MCP_PORT (see ix.mcp.optionalServers).
        addon = "${src}/addon/blender_mcp_addon";
      }
      // lib.optionalAttrs (updateScript != null) {inherit updateScript;};
  })
