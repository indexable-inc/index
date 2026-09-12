# Shared agent permission policy: one agent-neutral fact per row, rendered per
# agent CLI. The tool vocabularies differ (Claude Code denies tool names via
# settings `permissions.deny`; codex disables `features.*` leaves via the
# forced `-c` layer; cursor-agent denies `Shell(...)` patterns via
# `cli-config.json`), so each capability row carries both handles and the
# renderers at the bottom fold in the rows a wrapper's baked MCP servers make
# redundant.
#
# Claude runtime semantics, verified empirically on the pinned CLI (2.1.197,
# headless `claude -p --settings` probes): `permissions.deny` is a hard block
# even under the wrapper's default `--dangerously-skip-permissions` posture
# (bypass skips prompts, not deny rules). A SUBAGENT whose definition declares
# an explicit `tools:` allowlist re-grants only SOME settings-denied tools:
# in a bg-dispatched session a code-reviewer spawn (declares Read/Bash/Glob/
# Grep) got Glob and Grep but neither Bash nor Read (#2077, #2153; probed
# 2026-07-07 on 2.1.197). Subagents that need shell must bring their own
# index kernel via `mcpServers` (see subagents.nix `executor` and
# `cursor-search`); do not rely on a declared Bash surviving the deny.
# The inline server must carry a name the parent session does not already
# bind: a same-named inline `index` is silently discarded and the parent's
# connection shared instead (#2382), which is no isolation at all.
{
  lib,
  # True when the wrapper bakes the `index` MCP server. The kernel owns stateful
  # data and fleet work. Native file tools are disabled wherever it is present,
  # while the native shell remains available as the direct command path.
  indexKernelBaked ? false,
  # True when the wrapper bakes the `exa` MCP server, which supersedes the
  # stock web search/fetch surface.
  exaSearchBaked ? false,
  # False drops the protected-merge command denies from every render, for
  # consumers whose operator deliberately permits merge-protection bypasses
  # (pairs with omitting the `forceMerge` prompt rule).
  protectedMergeGuard ? true,
  # Strict opt-in mode: the index Elixir kernel is the ONLY tool surface the
  # agent gets. Every native tool and every non-index MCP server is denied, so
  # the agent works in the kernel's persistent Elixir workspace -- one shared
  # session whose bindings, modules and jobs survive across calls -- instead of
  # ad-hoc shell. That persistence is the whole point: a `Cmd.run` and a
  # `Read.file` in the kernel leave state a later cell can build on, where the
  # same work through Bash and Read leaves nothing behind but transcript.
  #
  # The strictest end of the same axis `indexKernelBaked` opens: that gate
  # supersedes the file tools and keeps the native shell as the direct command
  # path, this one closes the shell too. Default false, and false renders
  # byte-identically to a build that never passed the argument.
  #
  # Enforcement is two-layer, deliberately. The deny list below strips schemas
  # for the tools this repo knows by name (the token win, and the model is
  # never tempted by a tool it cannot see). The exhaustive guarantee is the
  # `kernel-only-guard` PreToolUse hook (policy/hooks.nix), which allowlists
  # `mcp__index__*` and denies everything else by construction -- no
  # enumeration to keep in sync as the CLI grows tools, and unlike a bare-name
  # deny it can carry a message telling the agent where to go instead.
  kernelOnly ? false,
  # Kernel-superseded native tools a consumer keeps anyway, by Claude tool
  # name. Subtracted from the `indexKernelBaked` and `exaSearchBaked` denies
  # only: `kernelOnly` re-denies them, because strict mode is the choice to
  # give the native surface up, and an exemption there would be the hole the
  # wrapper's `systemTools` ordering exists to close. A name outside the
  # superseded rows throws: an entry that denies nothing is a typo, not a
  # no-op.
  keepClaudeTools ? [],
}: let
  # One list of protected-merge command globs; the Claude render wraps them in
  # Bash(...) deny patterns, the codex render ships them verbatim for hook use.
  protectedMergeCommandPatterns = lib.optionals protectedMergeGuard [
    "gh pr merge*--admin*"
    "gh pr merge*--force*"
  ];

  # Native capabilities the index kernel supersedes. `claudeTools` are Claude
  # Code tool names for `permissions.deny`; `codexFeatures` are codex
  # `features.*` leaves for the forced `-c` layer. The file IO and search rows
  # carry no codex handle: codex reads, writes, and searches through its shell
  # and its `apply_patch` tool is enabled per-model upstream with no config
  # toggle to reach it.
  kernelSuperseded = {
    fileRead = {
      claudeTools = ["Read"];
      codexFeatures = {};
    };
    fileWrite = {
      claudeTools = ["Write" "NotebookEdit"];
      codexFeatures = {};
    };
    fileEdit = {
      claudeTools = ["Edit"];
      codexFeatures = {};
    };
    fileSearch = {
      claudeTools = ["Glob" "Grep"];
      codexFeatures = {};
    };
  };
  kernelClaudeTools = lib.concatMap (row: row.claudeTools) (lib.attrValues kernelSuperseded);
  kernelCodexFeatures = lib.mergeAttrsList (
    map (row: row.codexFeatures) (lib.attrValues kernelSuperseded)
  );

  # Web search/fetch superseded by the exa server.
  exaSuperseded = {
    claudeTools = [
      "WebSearch"
      "WebFetch"
    ];
    codexFeatures.standalone_web_search = false;
  };

  unknownKeptTools = lib.subtractLists (kernelClaudeTools ++ exaSuperseded.claudeTools) keepClaudeTools;
  checkKeptTools = lib.throwIf (unknownKeptTools != []) (
    "agent/policy/permissions.nix: keepClaudeTools names no superseded tool: "
    + lib.concatStringsSep ", " unknownKeptTools
  );

  # Unconditional house policy: no browser/computer/media surfaces in baked
  # wrappers, independent of which MCP servers ride along.
  codexHouseFeatures = {
    browser_use = false;
    browser_use_external = false;
    computer_use = false;
    image_generation = false;
    in_app_browser = false;
  };

  claudeHouseDeniedTools = [
    "CronCreate"
    "CronDelete"
    "CronList"
  ];

  # Claude-bundled skills that inject Anthropic's own style guidance with no
  # real benefit to this harness. `artifact-design` vanishes from the listing
  # once the Artifact tool itself is denied in the wrapper's tool table, and
  # this scoped deny hard-blocks any blind invocation (verified 2026-07,
  # index#3607). It HOLDS under `--dangerously-skip-permissions` (probed
  # 2026-07), like every deny; bypass skips prompts, not deny rules.
  # Unconditional: unlike the kernel-superseded rows it has no non-kernel
  # fallback role. The sibling `dataviz` skill needs no deny: the wrapper
  # removes it outright via `skillOverrides` in claude-code/default.nix
  # (index#3659), which both delists it and refuses invocation.
  claudeBundledSkillDenies = [
    "Skill(artifact-design)"
  ];
  # The native shell, denied only under `kernelOnly`. It is deliberately absent
  # from `kernelSuperseded` above: with the kernel merely baked, Bash stays as
  # the direct command path and the kernel-outage path (index#4080). Strict
  # mode is exactly the choice to give that up, so it is named here separately
  # rather than folded into a row whose normal reading is "keep the shell".
  claudeNativeShellTools = ["Bash"];

  # Every non-index MCP server the strict mode shuts off, named at server
  # granularity: a bare `mcp__<server>` deny covers all of that server's tools
  # without this file having to track the tool names each one happens to
  # export. exa is the only one the shared policy knows by name; the claude-code
  # wrapper additionally derives a deny for every other server it bakes and
  # drops them from the baked MCP config outright, so a server that is not
  # named here is still not reachable.
  #
  # The exa CAPABILITY is not what goes away. Web search and fetch stay
  # available in strict mode through the kernel's in-language `Web.search/1`
  # and `Web.fetch/1`, which run against the same exa API. One capability, one
  # door, and in strict mode the door is the kernel's.
  claudeKernelOnlyMcpDenies = ["mcp__exa"];

  # Strict mode's full Claude deny list. Unconditional in the tools it names:
  # the `indexKernelBaked` / `exaSearchBaked` gates describe which MCP server
  # supersedes which native tool, and under `kernelOnly` the answer is "the
  # kernel supersedes all of them" regardless of what else is baked.
  claudeKernelOnlyDenies =
    claudeNativeShellTools
    ++ kernelClaudeTools
    ++ exaSuperseded.claudeTools
    ++ claudeHouseDeniedTools
    ++ claudeKernelOnlyMcpDenies;

  # `kernelOnly` without the kernel is an agent with no tools at all -- a
  # config that builds, deploys, and then produces a session that cannot do
  # anything, with nothing in the failure to say why. Refuse at eval instead.
  checkKernelOnly = lib.throwIf (kernelOnly && !indexKernelBaked) (
    "agent/policy/permissions.nix: kernelOnly requires indexKernelBaked. "
    + "Strict mode denies every native tool and every non-index MCP server, "
    + "so without the index kernel baked the agent would have no tools at all."
  );
in
  checkKernelOnly (checkKeptTools {
    claude = {
      # `lib.unique`, because `kernelOnly` deliberately re-states rows the
      # narrower gates may already have contributed: strict mode asserts the
      # whole set independent of what else is baked, and a deny list is a set.
      # A no-op on every non-strict render.
      deniedToolPatterns = lib.unique (
        map (pattern: "Bash(${pattern})") protectedMergeCommandPatterns
        ++ claudeBundledSkillDenies
        ++ lib.optionals exaSearchBaked (lib.subtractLists keepClaudeTools exaSuperseded.claudeTools)
        ++ lib.optionals indexKernelBaked (
          lib.subtractLists keepClaudeTools kernelClaudeTools ++ claudeHouseDeniedTools
        )
        ++ lib.optionals kernelOnly claudeKernelOnlyDenies
      );
      # Read by policy/hooks.nix to arm the `kernel-only-guard` PreToolUse hook,
      # which is what makes the mode exhaustive rather than a list of names.
      inherit kernelOnly;
    };

    codex = {
      forcedSettings.features =
        codexHouseFeatures
        // lib.optionalAttrs exaSearchBaked exaSuperseded.codexFeatures
        // lib.optionalAttrs indexKernelBaked kernelCodexFeatures
        # codex reaches its shell through two feature leaves and its
        # `apply_patch` through none: upstream enables that one per-model with
        # no config toggle (see the `kernelSuperseded` note above), so strict
        # mode is enforced for codex by the shared PreToolUse hook, and the
        # feature leaves here only save the tokens they can.
        // lib.optionalAttrs kernelOnly {
          shell_tool = false;
          unified_exec = false;
        };
      inherit protectedMergeCommandPatterns kernelOnly;
    };

    # cursor-agent's `cli-config.json` permission vocabulary only verifiably
    # covers shell commands (`Shell(<glob>)` deny entries), so only the
    # protected-merge row renders here; the kernel/exa gates have no cursor
    # handle yet. Delivery is the consumer's config management (see the
    # cursor-cli wrapper's passthru), since the CLI reads permissions from
    # config, not flags.
    #
    # Strict mode reaches exactly as far as that vocabulary does: denying every
    # shell command is the whole of what `cli-config.json` can express, and the
    # file tools it leaves standing have no handle to deny. cursor-agent is
    # therefore NOT a supported `kernelOnly` target -- the shared hook is the
    # enforcement everywhere else, and cursor-agent does not run it. Stated
    # rather than silently half-applied.
    cursor = {
      deniedShellPatterns =
        map (pattern: "Shell(${pattern})") protectedMergeCommandPatterns
        ++ lib.optional kernelOnly "Shell(*)";
      kernelOnlySupported = false;
    };
  })
