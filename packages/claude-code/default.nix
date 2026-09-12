{
  lib,
  ix,
  stdenv,
  fetchurl,
  runtimeShell,
  makeBinaryWrapper,
  runCommand,
  autoPatchelfHook,
  darwin,
  procps,
  ripgrep,
  git,
  bubblewrap,
  socat,
  gnupg,
  python3,
  formats,
  jq,
  binName ? "claude",
  # Default posture: bake `--dangerously-skip-permissions` into the wrapper so
  # every session starts with the permission layer skipped. We run a trusted
  # config inside disposable sandboxes (ix guest VMs, the dev image, throwaway
  # checkouts) where a per-tool approval dialog buys nothing and only stalls an
  # agent that has nowhere unsafe to go. Mind the upstream uid-0 guard: the CLI
  # refuses this flag for an unsandboxed root user (no IS_SANDBOX=1 is baked
  # here, since a bare host genuinely is not a sandbox), so root consumers
  # either carry their own IS_SANDBOX=1 wrapper (the dev image does, plus a
  # managed-settings layer) or turn this off with
  # `claude-code.override { dangerouslySkipPermissions = false; }`.
  dangerouslySkipPermissions ? true,
  # Extra settings.json keys folded into the computed settings render
  # (`passthru.settings`), deep-merged UNDER the controlled keys this package
  # owns (so those always win on a conflict) and OVER the house posture
  # defaults (`houseSettingsDefaults` in the let-block, so any of those can be
  # overridden per consumer). Lets a consumer keep its whole static Claude
  # config (hooks, statusLine, enabledPlugins, marketplaces, ...) in Nix. The
  # wrapper itself injects no settings flag (#3180): a consumer materializes
  # the render into the writable user layer (the Home Manager module seeds
  # ~/.claude/settings.json through a mutable-json merge) or enforces it via
  # Claude's managed layer (lib/dev/agents.nix). `{ }` (default) ships the
  # house defaults plus the controlled keys.
  extraSettings ? {},
  # Typed Claude Code feature posture, rendered to the CLAUDE_CODE_* env vars
  # so no consumer has to spell (or misspell) the raw names. Booleans gate
  # features: false bakes the feature's CLAUDE_CODE_DISABLE_<NAME> var both as
  # a soft launch-env default (export the var empty to re-enable for one
  # session) and into settings `env` (read at CC startup even when the launch
  # env is missing); true bakes nothing, i.e. stock behavior.
  # `autoCompactWindow` is the token count baked as
  # CLAUDE_CODE_AUTO_COMPACT_WINDOW into settings `env` (null bakes nothing).
  # Unknown keys throw, like systemTools. Merged over `defaultFeatures` in the
  # let-block:
  #  - context1M = false: every 1M path in the CLI (the [1m] model suffix, the
  #    silent auto-upgrade, the context-1m beta header) is ~5x input price;
  #    past-the-window work belongs in subagents. Cost tradeoff, not
  #    perf-neutral: the Fable 5 system card measured its agentic scores at
  #    max-effort adaptive thinking, 128K max output tokens per turn, and
  #    contexts up to 1M (footnote 27: raising the per-turn cap from 16K to
  #    128K moved the OSWorld score). Do not additionally lower per-turn
  #    output caps (CLAUDE_CODE_MAX_OUTPUT_TOKENS) or effort; tight caps
  #    silently degrade agentic performance. This posture is Claude Code
  #    harness only; other harnesses and raw API callers set their own.
  #    Subagents need not all run fable; match model strength to subtask
  #    difficulty. Topology note: the card's best multi-agent harness is NOT
  #    a peer mesh. Of the three tested (sec 8.15.3), "async subagents"
  #    (hierarchy: a lead keeps the task tools, spawns async long-lived
  #    subagents that see only the lead's instructions, only the lead's
  #    answer is graded) reached the highest score (BrowseComp 93.3, sec
  #    8.15.1); the peer mesh ("fixed-agent team", identical peers all
  #    seeing the full task) wins on latency (2.2-2.7x speedups). Both
  #    non-blocking variants beat the blocking orchestrator on latency and
  #    tokens. Messaging is mesh-capable in both (any agent can message any
  #    other); the task flow of the top scorer is hierarchical. index#3700
  #    tracks the reusable BEAM implementation.
  #  - cron = false: drops the scheduling/loop tools.
  #  - fableFallback = true: stock behavior. When Fable 5's safety classifiers
  #    flag a turn, the CLI re-serves it on Opus 4.8 and keeps the session
  #    there (Fable 5 system card sec 1.5; the /config toggle "switch models
  #    when a message is flagged"). false bakes
  #    CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK so a flagged turn stops visibly
  #    instead: on eval or perf-sensitive work a visible failure beats a
  #    silent Opus 4.8 degradation.
  #  - autoCompactWindow = 300000: native-1M models (Fable 5, Sonnet 5,
  #    Opus 4.8) otherwise autocompact near the 1M cliff; 300K matches the
  #    standard (non-[1m]) working window the picker labels "300K High".
  features ? {},
  # Claude Code built-in orchestration and hosted-service tool posture. True
  # means Claude sees the tool; false renders the bare tool name into settings
  # `permissions.deny`, which removes the tool from Claude's available
  # tool set. Core shell/file/search tools stay in sharedPermissions because
  # their defaults depend on which MCP replacements the wrapper bakes.
  systemTools ? {},
  # False drops the protected-merge `gh pr merge --admin/--force` Bash denies
  # from the rendered permissions (policy/permissions.nix); pair with omitting
  # the `forceMerge` prompt rule so prompt and permissions agree.
  protectedMergeGuard ? true,
  # Strict opt-in mode: the index Elixir kernel is the ONLY tool surface. Every
  # built-in tool and every non-index MCP server is dropped, so the agent does
  # its work in the kernel's persistent Elixir workspace -- bindings, modules
  # and jobs that survive across calls -- rather than in ad-hoc shell that
  # leaves nothing behind. See `agent/policy/permissions.nix` for the policy and
  # the two-layer enforcement; this wrapper adds the two things only it can do,
  # which are emptying the built-in tool table and refusing to bake the other
  # MCP servers at all. Default false, and false is a no-op: the render is
  # byte-identical to a build that never passed it.
  kernelOnly ? false,
  # True when the HOST delivers the MCP server set through Claude Code's
  # managed layer (`/Library/Application Support/ClaudeCode/managed-mcp.json`,
  # rendered by modules/darwin/claude-managed-settings `mcpServers`). That
  # file is exclusive: with it present, Claude Code 2.1.228 REFUSES to start
  # when the command line also carries `--mcp-config` ("You cannot
  # dynamically configure MCP servers when an enterprise MCP config is
  # present", every launch on hydra after the 2026-09-03 switch), so this
  # wrapper then bakes no MCP flag at all. The set the host declares must
  # still carry `index` under the same name: `indexKernelBaked` (the guard
  # attestation) and the by-name denies keep deriving from the intended set,
  # because the kernel is attached either way. Default false is a no-op.
  managedMcp ? false,
  # Kernel-superseded native tools (Read, Write, Edit, Glob, Grep, WebSearch,
  # WebFetch) this consumer keeps despite the baked kernel, by name. Rendered
  # out of the shared deny list; `kernelOnly` denies them regardless (see
  # policy/permissions.nix `keepClaudeTools`). Default empty renders
  # byte-identically to a build that never passed it.
  keepNativeTools ? [],
  # False bakes `--no-chrome`, keeping the Claude in Chrome browser bridge off
  # in every session this wrapper starts regardless of ~/.claude.json's
  # `claudeInChromeDefaultEnabled` toggle (Claude Code rewrites that file, so
  # a declaration there is the same race as settings.json, #4312). True is
  # the upstream default and renders byte-identically to a build that never
  # passed it.
  chrome ? true,
  # Directories baked into the wrapper as `--add-dir=<dir>` flags, one per entry.
  # `--add-dir` grants tool file-access to a directory, AND (the reason this arg
  # exists) Claude Code loads any `<dir>/.claude/skills/` and `<dir>/CLAUDE.md`
  # found under it — the documented exception that makes `.claude/skills` under an
  # added dir discoverable as BARE `/<skill-name>` commands, regardless of the
  # session's cwd. This is the declarative, cwd-independent way to ship a fixed
  # set of skills globally (parallel to how `mcpServers` bakes `--mcp-config`):
  # point an entry at a store dir whose `.claude/skills/<name>/SKILL.md` tree is a
  # materialized `skills.mkSkillsDir` output. The skills the CLI's own
  # `.claude/skills` discovery (project + `~/.claude/skills`) finds still load
  # alongside; this only adds. `[ ]` (default) bakes no flag. See the `=`-form
  # note in `wrapperFlags`: `--add-dir` is variadic, so the space form would
  # swallow the next argv token.
  addDirs ? [],
  # Directories baked into the wrapper as `--plugin-dir=<dir>` flags, one per
  # entry: load a Claude Code plugin (a dir with `.claude-plugin/plugin.json`,
  # bundling its own `skills/`, `agents/`, `hooks/`, `.mcp.json`, ...) for every
  # session. Plugin skills/agents are NAMESPACED (`/<plugin>:<skill>`), unlike the
  # bare names `addDirs` yields, so reach for this when you want a self-contained,
  # provenance-tagged bundle rather than loose global skills. `[ ]` (default)
  # bakes no flag.
  pluginDirs ? [],
  # Shell glob patterns for the durable primary checkouts the PreToolUse
  # worktree guard protects (the claude-hooks `worktree-guard` subcommand): a file-edit tool call
  # whose target resolves into a PRIMARY checkout (git-dir == git-common-dir,
  # i.e. not a linked worktree) whose toplevel matches one of these globs is
  # denied, regardless of the session's cwd. The list deliberately names the
  # long-lived shared checkouts rather than blocking every primary checkout:
  # a scratch clone in /tmp is also "primary" for its own repo and must stay
  # editable. Globs are matched by the shell `case` builtin, where `*` crosses
  # `/`. Override per machine with the colon-separated
  # CLAUDE_CODE_PRIMARY_CHECKOUTS env var; `[ ]` disables the guard.
  primaryCheckouts ? [
    "/home/*/index"
    "/home/*/ix"
  ],
  # Andrew-only local startup context: cached notes and ~/Projects inventory.
  # Disabled for the shared wrapper because those hooks print workstation-local
  # context that is not meaningful for other users.
  personalStartupContext ? false,
  # Arms the PostToolUse edit logger and the Stop review gate. Off by default:
  # Opus 5 over-triggers the review agent relative to the size of the change.
  alwaysOnReview ? false,
  # Consumer-supplied SessionStart context commands
  # ({ package, exeName ? null, args ? [], timeout ? 10 }): each runs at
  # session start and its stdout is injected as session context. The generic
  # seam per-user startup context (e.g. a memory digest) hangs off
  # (index#3849).
  extraSessionStart ? [],
  # Sibling repo packages from the flake package set. lib/packages.nix threads
  # the lazily-recursive set in under this one name so a repo package can
  # depend on another by id without a flat merge into callPackage's top-level
  # namespace (where ids like `btop` or `kitty` would shadow the nixpkgs attrs
  # other packages resolve, and a self-named override like packages/btop would
  # recurse into itself). The overlay eval context does not provide it (the
  # `mcp-ex` package needs the flake package set, which
  # only the flake package set does), so the overlay build of
  # `pkgs.claude-code` falls back to `{ }` and drops the defaults below that
  # need a sibling.
  repoPackages ? {},
  # MCP servers baked into the wrapper as a generated `--mcp-config=<file>`
  # layer, one plain server per entry (tool prefix `mcp__<name>`). This is the
  # final Claude `mcpServers` JSON; the default is rendered from the shared
  # `ix.mcp` registry (lib/util/mcp.nix) so `index` is declared once and the
  # Codex wrapper bakes the same server from the same source. CLI `--mcp-config`
  # layers MERGE: a user's own `--mcp-config` and a discovered project
  # `.mcp.json` still load alongside this set, so baking the flag here replaces
  # the old pattern of consumers symlinkJoin-wrapping this wrapper a second time
  # just to add it. Defaults to the default pair, additions only (no stock tool is
  # disabled or overridden):
  #  - `index`: the Elixir ix kernel (`ix-mcp-ex`, packages/mcp-ex) over
  #    stdio. Present only when the `mcp-ex` sibling is in scope, i.e. in the
  #    flake package set but not the overlay (see `repoPackages`).
  #  - `exa`: Exa's hosted web-search server over streamable HTTP at
  #    https://mcp.exa.ai/mcp. Keyless works with rate limits; for higher
  #    limits add a keyed copy in user scope (`claude mcp add --transport http
  #    exa "https://mcp.exa.ai/mcp?exaApiKey=..."`), which merges alongside and
  #    is preferred over baking a secret into the world-readable store.
  # `{ }` bakes no flag.
  mcpServers ?
    ix.mcp.toClaudeJson
    (import (ix.paths.packagesRoot + "/agent/common.nix") {inherit lib ix repoPackages;})
      .defaultServers,
  # Claude Code "channels" (research preview, needs claude-code >= 2.1.80): MCP
  # servers whose events push into the running session, so the agent reacts to
  # things that happen while you are away. Our `index` server (baked above via
  # `mcpServers`, packages/mcp-ex) is a channel: kernel `notify(...)` and
  # interactive-resource actions emit `notifications/claude/channel` events. It
  # is our OWN stdio server, baked into this package from the same trusted
  # registry as `mcpServers`. Each entry is a channel spec:
  # `server:<mcpServersKey>` or `plugin:<name>@<marketplace>`; baked as
  # `--dangerously-load-development-channels <spec>...` because the bundled
  # `index` server is local development channel code, not an Anthropic allowlist
  # entry. Defaults to the `index`
  # server WHEN it is baked (so notify()/interactive resources reach a session
  # with no per-launch flag), and to nothing otherwise (the overlay build has no
  # `index` server, so referencing it would be a dead flag). `[ ]` bakes no flag.
  # Loading a channel is only half of it: the org policy key `channelsEnabled`
  # decides whether a loaded channel may push at all, and it lives in the
  # settings render (`houseSettingsDefaults` below, on by default) rather than
  # here, because managed settings are the only scope that can set it.
  developmentChannels ? lib.optional (mcpServers ? index) "server:index",
  # Rule names dropped from the default house prompt (forwarded to
  # ../prompt's `omitRules`). Only affects the computed `systemPrompt`
  # default below; ignored when `systemPrompt` is passed explicitly. Lets a
  # consumer bake a variant minus a rule without restating the whole prompt, e.g.
  # `claude-code.override { omitRules = [ "htmlDeliverable" ]; }`. `[ ]` keeps all.
  omitRules ? [],
  # Topic names dropped from the baked house prompt (prompt `omitTopics`).
  omitTopics ? [],
  # Text used AS Claude Code's system prompt, REPLACING the stock prompt. The
  # string is materialized to a store file and baked into the wrapper as
  # `--system-prompt-file=<path>`: passing by path (not inline text) keeps
  # arbitrary content free of shell quoting, and the store path makes the flag
  # one self-contained argv token (see `wrapperFlags` for why every injected
  # option-argument uses the `=` form).
  # Set, not append: this wholly replaces the stock prompt (tool guidance,
  # safety rules, coding conventions) rather than riding on top of it, so the
  # baked text owns the entire system prompt. Prepended before the user argv so
  # an explicit `--system-prompt`/`--system-prompt-file` on the CLI still wins
  # (single-value options are last-wins), and a caller who wants the stock
  # prompt plus additions can still pass `--append-system-prompt[-file]`.
  # Defaults to the shared house prompt (`systemPrompt` in ../common.nix,
  # authored in ../prompt/rules.nix: craft, pre-v1, worktree, and reporting
  # rules); set to `null` to bake no flag and ship the stock prompt alone.
  systemPrompt ?
    (import (ix.paths.packagesRoot + "/agent/common.nix") {
      inherit lib ix repoPackages;
      promptOmitRules = omitRules;
      promptOmitTopics = omitTopics;
    }).systemPrompt,
  # Writer used to build `passthru.updateScript`. Only the flake package set
  # supplies it (lib/packages.nix); the overlay eval context leaves it null. The
  # updater is a maintainer-facing flake output, so the overlay build of
  # `pkgs.claude-code` simply omits `passthru.updateScript`.
  updateScriptWriter ? null,
}: let
  # Read the package set from `ix`, not a `pkgs` callPackage formal: a `pkgs`
  # arg in the formal set breaks `.override` (astlog no-pkgs-in-callpackage),
  # and the rebound `ix.pkgs` is the set the rest of this file already uses
  # (see `inherit (ix) pkgs` below).
  inherit (ix) pkgs;

  # Version and per-platform SRI hashes are generated, never hand-edited. Bump
  # with `nix run .#claude-code.updateScript -- <version>`, which refetches
  # Anthropic's per-version manifest and rewrites manifest.json. We pin by raw
  # version (not the npm `latest` tag) because Anthropic ships new builds to the
  # `next` prerelease tag days before promoting them to `latest`, and every
  # channel that normally surfaces an upgrade (the built-in updater, `claude
  # doctor`, sadjow/claude-code-nix) only watches `latest`.
  manifest = lib.importJSON ./manifest.json;
  inherit (manifest) version;

  # Prebuilt agent content consumed by the repo SessionStart materializer. Claude
  # Code can load store-backed skills through `--plugin-dir` / `--add-dir`, but
  # bare subagents still have to exist under `.claude/agents`; exporting store
  # paths lets that hook do a plain copy instead of running `nix build` during
  # interactive startup. The overlay package has no sibling `mcp-ex` package in
  # scope, so it skips rendered agents whose MCP frontmatter depends on it.
  agentSkillsDir = ix.skills.mkSkillsDir {inherit pkgs;};
  agentAgentsDir =
    if repoPackages ? mcp-ex
    then let
      definitions = import (ix.paths.packagesRoot + "/agent/subagents.nix") {
        inherit
          ix
          lib
          repoPackages
          ;
      };
    in
      ix.agents.mkAgentsDir {
        inherit pkgs;
        agents = definitions.renderedAgents;
        inherit (definitions) rawFiles;
      }
    else null;

  # Typed feature table (see the `features` arg): one row per feature key,
  # owning its CLAUDE_CODE_* env var name and default, so the raw strings
  # exist exactly once. Toggle rows render DISABLE-style (feature off ⇒ var
  # "1"); value rows render their scalar.
  featureToggleEnvVars = {
    context1M = "CLAUDE_CODE_DISABLE_1M_CONTEXT";
    cron = "CLAUDE_CODE_DISABLE_CRON";
    # Read by the CLI (verified via strings on 2.1.215: gates the refusal
    # fallback path alongside the switchModelsOnFlag setting).
    fableFallback = "CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK";
  };
  defaultFeatures = {
    context1M = false;
    cron = false;
    fableFallback = true;
    autoCompactWindow = 300000;
    # Experimental upstream (mesh teammate sessions + the "Team:" panel).
    # Off, matching upstream's default. Distinct from subagents, which are
    # stock: SendMessage to a running or completed subagent works without
    # this (sub-agents.md: "SendMessage doesn't require agent teams to be
    # enabled; only structured team-protocol messages ... do").
    agentTeams = false;
  };
  unknownFeatures = lib.subtractLists (builtins.attrNames defaultFeatures) (builtins.attrNames features);
  effectiveFeatures =
    if unknownFeatures != []
    then throw "claude-code.features: unknown feature(s): ${lib.concatStringsSep ", " unknownFeatures}"
    else defaultFeatures // features;
  disabledFeatureEnv =
    lib.mapAttrs' (_: envVar: lib.nameValuePair envVar "1")
    (lib.filterAttrs (name: _: !effectiveFeatures.${name}) featureToggleEnvVars);
  # The full render, for settings `env` below. The launch layer gets only the
  # toggles (as `env_defaults`, which leave caller-provided values alone:
  # exporting the full CLAUDE_CODE_DISABLE_* name to empty re-enables that
  # feature for one session).
  featureSettingsEnv =
    disabledFeatureEnv
    // agentTeamsEnv
    // lib.optionalAttrs (effectiveFeatures.autoCompactWindow != null) {
      CLAUDE_CODE_AUTO_COMPACT_WINDOW = toString effectiveFeatures.autoCompactWindow;
    };
  # Historically this env var was derived from the SendMessage tool row
  # because the tool was believed teams-only (#4224). Upstream docs now state
  # the opposite: SendMessage-to-subagents is stock, and only team-protocol
  # messages need teams (sub-agents.md, verified 2026-08-04). So the two are
  # decoupled: the tool row keeps subagent continuation working (denying it
  # reintroduces ENG-10401, the duplicate-agent spawn the Agent description
  # provokes when SendMessage is denied), and teams is an ordinary feature
  # toggle, default off like upstream.
  agentTeamsEnv = lib.optionalAttrs effectiveFeatures.agentTeams {
    CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS = "1";
  };
  wrapperEnvDefaults = disabledFeatureEnv // agentTeamsEnv;

  # The servers actually baked. Strict mode keeps `index` and drops the rest:
  # a server that is never baked exports no tools, which is a stronger and
  # cheaper exclusion than denying its tools by name after loading it. The
  # by-name denies below are the belt to this braces, and both are derived from
  # this one attrset rather than from a hand-written list of server names.
  intendedMcpServers =
    if kernelOnly
    then lib.filterAttrs (name: _: name == "index") mcpServers
    else mcpServers;
  # What the wrapper itself bakes: nothing under `managedMcp`, where the host's
  # managed-mcp.json carries the same set and a baked flag is fatal (see the
  # argument doc).
  effectiveMcpServers =
    if managedMcp
    then {}
    else intendedMcpServers;

  # Servers strict mode dropped, rendered as whole-server denies. Redundant
  # with not baking them, deliberately: `--mcp-config` layers MERGE, so a user's
  # own config or a discovered project `.mcp.json` can still introduce a server
  # this wrapper never baked, and only the deny reaches that.
  droppedMcpServerDenies = map (name: "mcp__${name}") (
    builtins.attrNames (builtins.removeAttrs mcpServers (builtins.attrNames intendedMcpServers))
  );

  # Whether the index kernel rides along, which decides the tools the kernel
  # supersedes (shared policy below).
  indexKernelBaked = intendedMcpServers ? index;

  # Disabling a tool here puts its BARE name in `permissions.deny`, which
  # strips the tool's schema from the model context entirely; Claude Code has
  # no lazy/deferred-description mode for built-in tools (scoped patterns
  # like `Bash(...)` leave the schema loaded), so denying is the only way to
  # reclaim their tokens. The MCP resource browsers are kernel-superseded.
  defaultSystemTools = {
    # On: subagent delegation from inside the session. The same system-card
    # eval that keeps SendMessage off (see below) found async subagents fine:
    # the loss shows up in the peer mesh, not in spawn-and-collect.
    Agent = true;
    # Off: no real benefit in this harness (previews ship as files or URLs),
    # and enabling it surfaces Claude-bundled design skills that inject
    # Anthropic style guidelines we do not want steering output. Denying the
    # bare name also drops the companion `artifact-design` skill from the
    # skills listing (verified 2026-07, index#3607); the sibling `dataviz`
    # skill is removed via `skillOverrides` in houseSettingsDefaults below.
    Artifact = false;
    # On: kernel Ask.user (index#3856) rides MCP elicitation, and Claude
    # Code renders elicitation as an awkward raw dialog; the native tool is
    # the better question UI, so it owns the user-facing fork and
    # prompt/rules.nix points at it (#4095).
    AskUserQuestion = true;
    DesignSync = false;
    # Plan mode on both ends: cheap, and the plan/act split earns its two
    # schemas (#4095).
    EnterPlanMode = true;
    EnterWorktree = false;
    ExitPlanMode = true;
    ExitWorktree = false;
    ListMcpResourcesTool = false;
    # On, kernel or not: the Bash tool description tells the model that
    # foreground `sleep` is blocked and to use Monitor with an until-loop to
    # wait on a condition. Jobs.watch does not supersede that. It watches jobs
    # the kernel spawned, not an arbitrary log tail or a polled remote API,
    # which is what the instruction is for, so gating this on the kernel left a
    # native instruction with no satisfiable form and produced the foreground
    # sleep loops it forbids (#4224). Lives here rather than in the shared deny
    # policy so a consumer can still turn it off with `systemTools` like any
    # other tool (#4312).
    Monitor = true;
    PushNotification = false;
    ReadMcpResourceDirTool = false;
    ReadMcpResourceTool = false;
    RemoteTrigger = true;
    # On: how a subagent hands structured findings back to its caller, which
    # is what the code-review skills render through. Same agent-to-caller
    # family as SendMessage below, and cheap (#4224).
    ReportFindings = true;
    # Off, and unlike its neighbours here for a reason that holds: nothing the
    # model is told names it. The wakeup hooks in agent/policy/hooks.nix speak
    # only after a wakeup was armed, so with the tool absent that path is
    # unreachable rather than false. Consumers whose loops pace themselves opt
    # in through `systemTools`.
    ScheduleWakeup = false;
    # On, against a measured cost: the Claude Fable 5 system card's
    # Multi-Agent ProgramBench (sec. 8.15.2) found the SendMessage peer mesh
    # reaches similar solutions faster in wall clock but with no better final
    # quality and much worse token efficiency than one agent working
    # sequentially (#4095). It is on anyway because the Agent tool's own
    # description tells the model to use SendMessage to continue a running
    # subagent, so with the tool denied the model follows that instruction into
    # the only fallback it has left and spawns a SECOND agent on the files the
    # first one is editing (observed, ENG-10401). A wasteful tool beats an
    # instruction whose only available fallback corrupts state. Turn this back
    # off once the Agent description stops naming a tool the permission set may
    # deny (#4224).
    SendMessage = true;
    SendUserFile = true;
    ShareOnboardingGuide = true;
    Skill = true;
    # Task* on: the shared task list tracks multi-step work and collects
    # background subagent output (TaskOutput/TaskStop); useful without the
    # agent-teams mesh (#4095).
    TaskCreate = true;
    TaskGet = true;
    TaskList = true;
    TaskOutput = true;
    TaskStop = true;
    TaskUpdate = true;
    ToolSearch = true;
    WaitForMcpServers = true;
    Workflow = true;
  };
  unknownSystemTools = lib.subtractLists (builtins.attrNames defaultSystemTools) (builtins.attrNames systemTools);
  # Strict mode turns every row off, and does it AFTER the caller's
  # `systemTools`, so an explicit `systemTools.Agent = true` cannot punch a hole
  # in it. That ordering is the point: `kernelOnly` is a boundary the operator
  # asked for, not a default to be overridden a layer down. A consumer that
  # wants a tool back turns the mode off.
  kernelOnlySystemTools = lib.genAttrs (lib.attrNames defaultSystemTools) (_: false);
  effectiveSystemTools =
    if unknownSystemTools != []
    then throw "claude-code.systemTools: unknown tool(s): ${lib.concatStringsSep ", " unknownSystemTools}"
    else defaultSystemTools // systemTools // lib.optionalAttrs kernelOnly kernelOnlySystemTools;
  disabledSystemTools = builtins.attrNames (lib.filterAttrs (_: enabled: !enabled) effectiveSystemTools);

  # The computed settings render leaves this package only through
  # `passthru.settings` / the rendered file below; nothing rides argv. Per
  # Claude Code's precedence, CLI args outrank the local/project/user settings
  # files, so the old injected `--settings` flag silently shadowed the user's
  # own writable settings (#3180). Materializing into the user layer keeps
  # every default overridable and the live config explainable from disk.

  # House posture defaults every wrapped session starts from: agent-neutral
  # preferences that used to live in per-machine extraSettings. They form the
  # LOWEST-priority layer of the computed settings (under the caller's
  # extraSettings, which sits under the controlled keys in settingsPolicy),
  # so a consumer can override any of them without this package losing its
  # invariants.
  houseEffortLevel = "high";
  houseSettingsDefaults = {
    # No Claude attribution trailers on commits or PRs.
    attribution = {
      commit = "";
      pr = "";
    };
    worktree.baseRef = "fresh";
    autoMemoryEnabled = true;
    # Behavioral posture, not taste: these ride the managed layer with the rest
    # of the render, so a session cannot quietly drop its own effort. The
    # preference keys Claude Code writes back itself (`theme`, `verbose`,
    # `model`) are deliberately absent -- declaring one would either freeze a
    # toggle the app offers or need a writable file for Nix to fight over
    # (#4312).
    effortLevel = houseEffortLevel;
    fastMode = true;
    fileCheckpointingEnabled = false;
    autoUpdatesChannel = "latest";
    skipAutoPermissionPrompt = true;
    # Channels on. Loading a channel (`developmentChannels` above) only wires
    # the transport; this key is the org-policy gate that decides whether a
    # loaded channel may push, and only the MANAGED scope can set it. Worse,
    # the mere EXISTENCE of a managed-settings file flips the default from on
    # to off: 2.1.220 blocks on `e!==null&&e.channelsEnabled!==!0`, and the
    # key's own schema text reads "claude.ai Teams/Enterprise: default off.
    # Console: default on unless managed settings exist." Every consumer of
    # this render installs it as managed settings, so before this line the
    # baked `--dangerously-load-development-channels server:index` was
    # loaded-but-muted -- the session printed "Channels are not enabled for
    # your org" and silently dropped every kernel `notify(...)`. Sitting in
    # the house defaults (not `controlledSettings`) keeps the opt-out real: a
    # host that must refuse inbound pushes sets
    # `programs.claude-code.defaults.channelsEnabled = false`, which outranks
    # this layer. Key verified present in the settings schema of every pinned
    # CLI from 2.1.159 through 2.1.220.
    channelsEnabled = true;
    # Remove the Claude-bundled `dataviz` skill outright: it injects
    # Anthropic's own chart-style guidance with no benefit to this harness,
    # and before this override it sat permission-denied yet still listed,
    # spending context on a skill that could never run. `"off"` both drops
    # the skill from the listing and refuses invocation ("disabled for model
    # invocation in skillOverrides settings"), and both hold under
    # `--dangerously-skip-permissions` (probed headlessly on 2.1.206,
    # index#3659; the older prompt-path stub where user/project
    # `skillOverrides` was ignored no longer reproduces). The sibling
    # `artifact-design` skill needs no row here: denying the bare Artifact
    # tool in defaultSystemTools above already delists it (index#3607).
    skillOverrides.dataviz = "off";
    # House statusline (./statusline.nu): context bar, model, effort, and the
    # running CLI version with an update marker against Anthropic's `latest`
    # release pointer. The house effortLevel also rides argv as the script's
    # last resort for a settings.json that does not carry the key (nothing
    # materialized this render, or the user pruned it); the writable settings
    # files win whenever they answer.
    statusLine = {
      type = "command";
      command = "${lib.getExe pkgs.nushell} ${./statusline.nu} --default-effort ${houseEffortLevel}";
    };
  };

  # Build the hook runner once; shared policy renders it for each wrapper.
  hookRunner = import (ix.paths.packagesRoot + "/agent/policy/hook-runner.nix") {
    inherit
      lib
      runCommand
      makeBinaryWrapper
      ix
      git
      primaryCheckouts
      repoPackages
      ;
  };
  # Claude settings.json hook block rendered from shared agent policy.
  sharedHooks = import (ix.paths.packagesRoot + "/agent/policy/hooks.nix") {
    inherit
      lib
      hookRunner
      primaryCheckouts
      personalStartupContext
      alwaysOnReview
      extraSessionStart
      kernelOnly
      ;
  };

  # Claude-native permission deny list rendered from shared agent policy. The
  # gates fold in the native tools each baked MCP server supersedes: with the
  # `index` kernel present the stock shell/file/search tools are denied, and
  # the overlay build (no kernel) keeps them.
  sharedPermissions = import (ix.paths.packagesRoot + "/agent/policy/permissions.nix") {
    inherit lib indexKernelBaked;
    exaSearchBaked = intendedMcpServers ? exa;
    inherit protectedMergeGuard kernelOnly;
    keepClaudeTools = keepNativeTools;
  };

  # Controlled keys this package always owns: the highest-priority settings
  # layer, merged over the house defaults and the caller's extraSettings below.
  controlledSettings =
    {
      # Keep transcripts and wrapper debug logs long enough for troubleshooting.
      cleanupPeriodDays = 365;
      # settings `env` is read at Claude Code startup (even when launch env is
      # missing), so the typed feature render (see the `features` arg) bakes
      # here as well as in the launch layer's env_defaults.
      env = (extraSettings.env or {}) // featureSettingsEnv;
      permissions = {
        # Concatenate manually: deepMerge treats lists as leaves.
        deny = lib.unique (
          (extraSettings.permissions.deny or [])
          ++ disabledSystemTools
          ++ sharedPermissions.claude.deniedToolPatterns
          ++ droppedMcpServerDenies
        );
      };
      # Full Claude hook set rendered from shared agent policy.
      hooks = sharedHooks.claude;
    }
    // lib.optionalAttrs dangerouslySkipPermissions {
      # Suppress the one-time warning that the skip flag alone still shows.
      skipDangerousModePermissionPrompt = true;
    };

  # Three layers, rhs winning at each leaf: house posture defaults, then the
  # caller's extraSettings, then the controlled keys this package always owns.
  # The caller's other keys (hooks aside — enabledPlugins, marketplaces, ...)
  # pass through untouched.
  settingsPolicy = ix.deepMerge.rhs (ix.deepMerge.rhs houseSettingsDefaults extraSettings) controlledSettings;

  # What the installCheck expects the settings file to carry from the two
  # lower layers: the merged house+extraSettings render, minus any top-level
  # key the controlled layer shadows. Derived (not restated) so the check
  # holds for overridden builds too.
  houseSettingsRender =
    builtins.removeAttrs (ix.deepMerge.rhs houseSettingsDefaults extraSettings)
    (builtins.attrNames controlledSettings);
  settingsPolicyFile =
    (formats.json {}).generate "claude-code-default-settings.json"
    settingsPolicy;

  mcpConfigFile = (formats.json {}).generate "claude-code-mcp-config.json" {
    mcpServers = effectiveMcpServers;
  };

  # Dirs prepended to PATH at launch (the old `--prefix PATH :`): ps for process
  # checks, the pinned ripgrep, and the Linux sandbox helpers. Passed to the
  # launcher as `path_prepend` (it joins them ahead of the caller's PATH).
  pathPrepend = map (p: "${lib.getBin p}/bin") (
    [
      procps
      ripgrep
    ]
    ++ lib.optionals stdenv.hostPlatform.isLinux [
      bubblewrap
      socat
    ]
  );

  # Argv tokens Claude Code dispatches positionally: the launcher withholds
  # every prepended flag when the user's first argument is one of these.
  # Prepending ahead of a subcommand token does not just fail to help, it
  # breaks the subcommand: the CLI selects the command from the first token and
  # then re-reads the raw `process.argv` by fixed index, so `claude
  # remote-control` handed its parser our `--dangerously-load-development-channels`
  # and died with `Unknown argument` (index#4269). Generated from the pinned
  # CLI (`--help` for the registered commands, a curated list with byte proofs
  # for the hidden ones); see ./extract-subcommands.py for what each source
  # covers and why a feature-gated name is deliberately absent.
  subcommands = let
    rows = lib.filter (
      line: line != "" && !lib.hasPrefix "#" line
    ) (lib.splitString "\n" (builtins.readFile ./subcommands.tsv));
  in
    map (row: builtins.head (lib.splitString "\t" row)) rows;

  # Prepend root flags. Use `--opt=value` for every option that takes a value:
  # space-form options can be swallowed by subcommands or variadic flags.
  wrapperFlags =
    [
      # Write ~/.claude/debug telemetry; cleanupPeriodDays controls retention.
      "--debug"
    ]
    # Load our own MCP servers as local development channels (research preview).
    # This flag is VARIADIC (it consumes every following non-`--` token as a spec),
    # so it must be followed by a `--`-prefixed flag, never placed last, where it
    # would swallow the user's argv (a prompt, a subcommand). It sits here so the
    # always-present `--thinking-display=` below terminates the spec list.
    ++ lib.optionals (developmentChannels != []) (
      ["--dangerously-load-development-channels"] ++ developmentChannels
    )
    ++ [
      # Opus 4.7+ otherwise omits thinking from the UI/transcript.
      "--thinking-display=summarized"
    ]
    # Default posture for sandboxed ix environments.
    ++ lib.optional dangerouslySkipPermissions "--dangerously-skip-permissions"
    # Browser bridge off for the whole session (see the `chrome` argument).
    ++ lib.optional (!chrome) "--no-chrome"
    # Replace the stock prompt when a house prompt is configured.
    ++ lib.optional (
      systemPrompt != null
    ) "--system-prompt-file=${builtins.toFile "claude-code-system-prompt.txt" systemPrompt}"
    # Bake the shared MCP server set when present.
    ++ lib.optional (effectiveMcpServers != {}) "--mcp-config=${mcpConfigFile}"
    # `--add-dir` is variadic, so the `=` form is required.
    ++ map (d: "--add-dir=${d}") addDirs
    # Plugins carry namespaced skills, agents, hooks, and MCP declarations.
    ++ map (d: "--plugin-dir=${d}") pluginDirs;

  # The launch spec consumed by the shared Rust launcher (packages/config-launch):
  # it sets env/PATH, prepends `wrapperFlags`, then execs the real binary
  # preserving argv0. No settings ride argv: the computed defaults materialize
  # into the writable user settings layer via `passthru.settings` (#3180),
  # where they stay overridable and readable. The store output is read-only so
  # the bundled self-updater must never mutate it. DISABLE_UPDATES blocks every
  # update path, including `claude update` and `claude install`:
  # https://code.claude.com/docs/en/getting-started#disable-auto-updates
  # The install checks are skipped, and USE_BUILTIN_RIPGREP=0 pins search to the
  # Nix ripgrep on PATH so the wrapper owns the version pin. `target` is an
  # `@helper@` placeholder substituted at install time (the real binary lives
  # under `$out/libexec`, unknowable here). Covered by the installCheck argv
  # tests below.
  launchSpec = (formats.json {}).generate "claude-code-launch-spec.json" {
    target = "@helper@";
    env =
      {
        DISABLE_UPDATES = "1";
        DISABLE_INSTALLATION_CHECKS = "1";
        USE_BUILTIN_RIPGREP = "0";
        IX_CLAUDE_SKILLS_DIR = "${agentSkillsDir}";
      }
      // lib.optionalAttrs (agentAgentsDir != null) {
        IX_CLAUDE_AGENTS_DIR = "${agentAgentsDir}";
      }
      # Attest to the kernel-only-guard hook that this invocation really
      # carries the index kernel. The hook arms machine-wide through
      # managed-settings.json, which the desktop app and ccd sessions also
      # read -- but only wrapper launches bake the MCP server, and a guard
      # that fires without a kernel bricks the session (guards.rs
      # KERNEL_BAKED_ENV). Launch env, never settings `env`: settings ride
      # the managed layer to exactly the sessions that must not see this.
      // lib.optionalAttrs indexKernelBaked {
        IX_KERNEL_MCP_BAKED = "1";
      };
    env_defaults = lib.mapAttrs (_: toString) wrapperEnvDefaults;
    path_prepend = pathPrepend;
    flags = wrapperFlags;
    inherit subcommands;
  };

  inherit (stdenv.hostPlatform) system;
  target =
    manifest.platforms.${system}
      or (throw "claude-code: no prebuilt binary for ${system}; supported: ${lib.concatStringsSep ", " (builtins.attrNames manifest.platforms)}");

  # Primary host is the Anthropic-branded CDN so the source is verifiable; the
  # GCS bucket is the direct origin and stays as a mirror if the CDN is down.
  # The hash pin guarantees both resolve to identical bytes, so this is a
  # mirror list, not a behavioral fallback.
  nativeBinary = fetchurl {
    urls = [
      "https://downloads.claude.ai/claude-code-releases/${version}/${target.slug}/claude"
      "https://storage.googleapis.com/claude-code-dist-86c565f3-f756-42ad-8dfa-d59b1c096819/claude-code-releases/${version}/${target.slug}/claude"
    ];
    inherit (target) hash;
  };

  # The dev-channels-gate byte patch is DISABLED: the launched helper is the
  # untouched download. Turned off while investigating a Claude Code session
  # where subagent output disappeared from the transcript and subagents queued
  # without ever starting, to remove a modified binary as a variable. Nothing
  # links the patch to either symptom and the mechanism makes it unlikely (the
  # swap forces one onboarding branch and touches neither the agent scheduler
  # nor the renderer), so this is variable elimination, not a diagnosis.
  #
  # What it costs while off: every interactive launch stops on the full-screen
  # "WARNING: Loading development channels" confirm, because this wrapper bakes
  # our own `index` stdio server as a development channel.
  #
  # To restore: uncomment the block below and point `patchedBinary` back at the
  # `applyBytePatch` fold, then invert `tests.dev-channels-gate-intact` back to
  # asserting the patched bytes. `devChannelsGateAnchor` is commented with the
  # rest because nothing consumes it while the patch is off; its four entries
  # were counted against 2.1.220 and are STALE for any later version, since
  # upstream reminifies the surrounding identifiers between releases. Re-derive
  # all four from the pinned binaries before turning the patch back on.
  patchedBinary = nativeBinary;

  /*
  # Shared equal-length byte-patch layer primitive (also used by
  # claude-code-rainbow), so patches compose as a cacheable DAG over the single
  # download. See ./byte-patch.nix.
  applyBytePatch = import ./byte-patch.nix {inherit runCommand python3;};

  # The DevChannelsDialog gate. Because this wrapper bakes our own trusted
  # `index` stdio server as a `--dangerously-load-development-channels` channel
  # (see `developmentChannels`), every interactive launch otherwise stops on a
  # full-screen "WARNING: Loading development channels" confirm before the
  # session starts. Upstream already loads the channels either way (the dialog
  # is only a confirmation UI): the onboarding flow renders it solely in the
  # `else` of
  #   if(!g()||En()!=="firstParty"||_(y("policySettings"))) <load silently>
  #   else <show DevChannelsDialog, load on accept>
  # so forcing that condition true always takes the silent-load branch. The
  # swap is equal-length (`!g()` -> `true`, both 4 bytes): the only safe edit to
  # a Bun single-file executable, since it leaves every downstream offset and
  # the appended trailer byte-identical (see ./patch-binary.py). The `expect`
  # count gate fails the build loudly if a version bump reminifies the
  # surrounding identifiers so the anchor no longer lands exactly once; re-derive
  # the find/replace against the new binary when that happens.
  #
  # Each platform build minifies the expression differently, so the anchor is
  # keyed by system. Every entry was counted exactly once against the pinned
  # 2.1.220 binaries, whose SRI hashes were checked against manifest.json first
  # so the counts came from the same bytes the fetcher pins.
  #
  # 2.1.215 -> 2.1.220 reminified three of the four, which is exactly what the
  # `expect` gate exists to catch (it failed with "COUNT DRIFT ... expected 1,
  # found 0" rather than silently not patching): En -> xn, Sn -> xn, vn -> Hn
  # on x86_64-linux and vn -> An on aarch64-linux. Note the two Linux anchors
  # were identical under 2.1.215 and are NOT under 2.1.220, so they cannot be
  # collapsed into one entry. Only the callee names move; the argument order
  # (`_(y(...))` on darwin, `y(_(...))` on linux) is unchanged.
  devChannelsGateAnchor = {
    aarch64-darwin = ''if(!g()||xn()!=="firstParty"||_(y("policySettings")))'';
    x86_64-darwin = ''if(!g()||xn()!=="firstParty"||y(_("policySettings")))'';
    x86_64-linux = ''if(!g()||Hn()!=="firstParty"||y(_("policySettings")))'';
    aarch64-linux = ''if(!g()||An()!=="firstParty"||y(_("policySettings")))'';
  };

  devChannelsGatePatch = [
    (
      let
        find =
          devChannelsGateAnchor.${system}
            or (throw "claude-code: no dev-channels gate anchor for ${system}; re-derive it from the pinned binary (index#3788)");
      in {
        inherit find;
        replace = lib.replaceStrings [''!g()''] [''true''] find;
        expect = 1;
      }
    )
  ];

  # The one-mapping byte-patch layer that disables the gate: fold it over the
  # single download. Shared primitive with claude-code-rainbow (./byte-patch.nix)
  # -- both express their patches as cacheable layers rooted at `nativeBinary`.
  # The layer is raw bytes; the wrapper leaf below interpreter-patches (Linux)
  # and ad-hoc re-signs (darwin) the result, since the byte edit invalidates
  # Anthropic's Developer-ID signature. Only this launched helper is patched;
  # `stockCli` above keeps the unmodified download for the prompt/env extractors.
  patchedBinary = applyBytePatch {
    name = "dev-channels-gate";
    input = nativeBinary;
    rules = devChannelsGatePatch;
  };
  */

  stockCli = stdenv.mkDerivation {
    pname = "claude-code-stock";
    inherit version;
    dontUnpack = true;
    dontStrip = true;
    strictDeps = true;
    # Upstream Bun-standalone binary repack; nothing to test at build time.
    doCheck = false;
    nativeBuildInputs = lib.optional stdenv.hostPlatform.isElf autoPatchelfHook;
    installPhase = ''
      # shell
      runHook preInstall
      install -D -m755 ${nativeBinary} $out/bin/claude
      runHook postInstall
    '';
  };

  # Maintainer-facing updater that refreshes manifest.json from Anthropic's
  # signed per-version manifest (fails closed on a bad GPG signature); see
  # ./update.nix. Built only when this eval context supplied a writer (the flake
  # package set), so the overlay build of `pkgs.claude-code` omits
  # `passthru.updateScript`.
  updateScript =
    if updateScriptWriter == null
    then null
    else
      import ./update.nix {
        writeNushellApplication = updateScriptWriter;
        inherit gnupg python3;
        nix = ix.nixPackageFor "packages/claude-code: updateScript";
      };
in
  # `allowVendoredUnfree` strips the honest `meta.license` tag below so the
  # per-system flake package set (evaluated without `allowUnfree`) can build
  # `nix run .#claude-code`; see lib/util/vendored-unfree.nix.
  ix.allowVendoredUnfree (stdenv.mkDerivation (finalAttrs: {
    pname = "claude-code";
    inherit version;

    # The source is a single fetched binary, not an archive.
    dontUnpack = true;

    # Stripping rewrites the binary and corrupts the trailer Bun appends to its
    # single-file executables, so the stripped CLI aborts on launch.
    dontStrip = true;
    strictDeps = true;

    # The CLI reads macOS's ICU data during startup initialization, and the build
    # sandbox exposes only the stdenv default host paths, so ICU's init traps and
    # the kernel force-exits the process with SIGKILL before it writes a byte
    # (EXC_BREAKPOINT, two nested call_once frames off `dyld start`). That kills
    # the `claude doctor` check below, the only check that execs the real binary.
    # Eight builds failed with the stock sandbox and three passed with this path
    # added, same derivation, nothing else changed (index#4324). nixpkgs appends to
    # the stdenv defaults and drops the attribute off darwin, so this neither
    # replaces those nor affects Linux. The host has to permit the request
    # separately: without /usr/share/icu in its `allowed-impure-host-deps` a
    # darwin build fails at start with `requested impure path ... not in
    # allowed-impure-host-deps` (andrewgazelka/nix hosts/hydra/default.nix).
    __impureHostDeps = ["/usr/share/icu"];

    nativeBuildInputs =
      [
        makeBinaryWrapper
      ]
      ++ lib.optional stdenv.hostPlatform.isElf autoPatchelfHook
      # The helper carries the byte patch, so its vendor signature is invalid;
      # re-sign ad-hoc on darwin (AMFI SIGKILLs an unsigned Mach-O). The argv
      # install checks run against a stub and never exec this helper, so the
      # darwin re-sign-then-exec-in-sandbox AMFI caveat does not bite here.
      ++ lib.optional stdenv.hostPlatform.isDarwin darwin.autoSignDarwinBinariesHook;

    installPhase = ''
      # shell
      runHook preInstall
      mkdir -p $out/bin $out/libexec $out/share

      # 1Password's "CLI access requested" prompt labels the request with the
      # basename of the process that spawns `op`, which is this real binary rather
      # than the wrapper. Keep it in libexec (off PATH, no leading-dot wrapper
      # convention) and name it for the product so the prompt reads "Claude Code"
      # instead of ".claude-unwrapped". The basename is the human-facing product
      # label, independent of the command alias, since it is only what macOS shows.
      # 1Password docs confirm the prompt shows "the process being authorized (for
      # example, iTerm2 or Terminal)", not the code signature or CFBundleName:
      # https://developer.1password.com/docs/cli/app-integration-security/
      # Everything below `bin/` goes under a per-`binName` directory, because
      # two wrappers built from this package (the ordinary `claude` and an
      # overridden strict `claude-kernel`, say) are meant to sit in ONE user
      # profile, and `buildEnv` refuses any relative path two inputs both
      # provide. Only `bin/${binName}` was derived from the name, so
      # `libexec/Claude Code` and the launch spec collided and the profile
      # would not build at all (ENG-12737, hit on hydra).
      #
      # A directory rather than a name suffix, so the property holds for
      # whatever gets installed here next: anything under this prefix is
      # collision-free by construction, where a `-${binName}` suffix has to be
      # remembered once per file. `checks.claude-code-profile-coexist` is the
      # thing that now fails if that stops being true.
      #
      # Both paths are consumed as absolute paths and neither is looked up by
      # convention, so moving them is safe: the launcher reads the spec from
      # `IX_LAUNCH_SPEC` (config-launch/src/main.rs, which takes the full path
      # out of the environment), and the helper's location reaches the launcher
      # only through the @helper@ substitution below.
      mkdir -p "$out/libexec/${binName}" "$out/share/${binName}"

      # The BASENAME stays "Claude Code" whatever the command is called: it is
      # the human-facing product label 1Password shows in its CLI access
      # prompt, which is why it is spelled this way at all (see above). Only
      # the directory carries the alias.
      helper="$out/libexec/${binName}/Claude Code"
      install -m755 ${patchedBinary} "$helper"

      # All flag/env/PATH injection lives in `launchSpec` (see its let-binding and
      # `wrapperFlags` for the per-flag rationale); bake the helper's real path
      # into the @helper@ placeholder, then point the launcher at the spec.
      spec="$out/share/${binName}/claude-code-launch-spec.json"
      install -m644 ${launchSpec} "$spec"
      substituteInPlace "$spec" --subst-var-by helper "$helper"
      makeBinaryWrapper ${ix.rustWorkspace.units.binaries.config-launch}/bin/config-launch \
        $out/bin/${binName} \
        --inherit-argv0 \
        --set IX_LAUNCH_SPEC "$spec"

      runHook postInstall
    '';

    # Offline argv + hook regression net driven through the real launcher binary
    # against a stub target; see ./install-check.nix for what each check guards.
    doInstallCheck = true;
    installCheckPhase = import ./install-check.nix {
      inherit (pkgs) nushell;
      statuslineCommand = houseSettingsDefaults.statusLine.command;
      inherit
        lib
        runtimeShell
        ix
        git
        jq
        repoPackages
        hookRunner
        launchSpec
        settingsPolicyFile
        wrapperFlags
        subcommands
        wrapperEnvDefaults
        featureSettingsEnv
        houseSettingsRender
        disabledSystemTools
        python3
        binName
        ;
    };

    passthru =
      {
        # The computed settings render (house posture defaults, then the
        # caller's extraSettings, then the controlled keys), for a host to
        # declare through Claude Code's MANAGED layer: root-owned,
        # highest-precedence, and unreachable from any writable scope
        # (darwinModules.claude-managed-settings on darwin,
        # environment.etc."claude-code/managed-settings.json" on NixOS).
        # Nothing writes ~/.claude/settings.json anymore: Claude Code rewrites
        # that file from memory whenever a /model or /config toggle lands, so
        # every declarative owner of it was reduced to racing the app (#4312,
        # replacing the mutable-json merge of #3180). Consequently this render
        # carries policy only -- no `theme`, `verbose` or `model`.
        # `settingsPolicyFile` is the same render as a store JSON file for the
        # install checks and non-module consumers.
        inherit settingsPolicy settingsPolicyFile;

        # The single fetched upstream binary (a fixed-output derivation) and its
        # runnable stock wrapping. Exposed so sibling packages that customize or
        # inspect the same download -- claude-code-rainbow's byte patches,
        # claude-code-debug's inspector -- reuse ONE download derivation instead
        # of re-declaring `fetchurl`. `nativeBinary` is the raw bytes;
        # `stockCli` is the same bytes, unpatched and unwrapped (autopatchelfed
        # on Linux), i.e. genuinely stock behavior.
        inherit nativeBinary stockCli;

        # Byte proof that the SHIPPED helper (post-fixup, post-sign) carries no
        # forced-true dev-channels gate, i.e. the byte patch above is really off
        # and nothing downstream reintroduced it. Deliberately keyed to strings
        # that survive reminification, NOT to `devChannelsGateAnchor`: those
        # anchors are re-derived by hand per upstream binary, and while the patch
        # is off there is nothing a per-version anchor would catch which
        # `patchedBinary = nativeBinary` does not already guarantee by
        # construction. Invert this back to the anchor-keyed assertion when the
        # patch returns, since then the anchors are load-bearing again.
        #
        # `if(true||` was counted as absent from the stock 2.1.220 and 2.1.221
        # darwin-arm64 downloads, so a hit means a patch landed rather than
        # upstream minifying it that way. The `"firstParty"` check keeps the
        # absence assertion from passing vacuously against a JS region grep can
        # no longer read (compression, a packaging change, a corrupted fixup).
        #
        # Both are `grep -c`, i.e. counts of matching LINES in a binary that is
        # not one line: `"firstParty"` lands on 46 of them in stock 2.1.221. So
        # these are presence and absence assertions, and the thresholds must stay
        # `-ge 1` and `-eq 0`; an `-eq 1` here fails on the stock download, which
        # is how this comment got written.
        tests.dev-channels-gate-intact = pkgs.runCommand "claude-code-dev-channels-gate-intact" {__structuredAttrs = true;} ''
          helper="${finalAttrs.finalPackage}/libexec/Claude Code"
          forced=$(grep -cF 'if(true||' "$helper" || true)
          readable=$(grep -cF '"firstParty"' "$helper" || true)
          [ "$readable" -ge 1 ] || { echo "FAIL: gate region not greppable in the shipped helper; the absence check below would pass vacuously" >&2; exit 1; }
          [ "$forced" -eq 0 ] || { echo "FAIL: forced-true gate present on $forced line(s); the byte patch is not off" >&2; exit 1; }
          touch "$out"
        '';

        # Machine-readable knob tables for the commented knob reference at
        # the Home Manager consumption site
        # (packages/agent/home-manager/claude-code.nix):
        # checks.claude-code-knob-reference asserts that reference against
        # these plus `builtins.functionArgs`, so the reference cannot
        # silently go stale. Passthru never reaches the derivation, so
        # exposing them keeps the drvPath byte-identical (index#3710).
        knobDefaults = {
          features = defaultFeatures;
          systemTools = defaultSystemTools;
        };

        # Mechanical extraction of every env var the shipped cli.js reads:
        # the registry behind env-registry.tsv and the env reference block
        # in the Home Manager module (index#3710). Regenerate after a
        # version bump:
        #   nix build .#claude-code.envRegistry
        #   cp result packages/claude-code/env-registry.tsv
        # (checks.claude-code-knob-reference pins the committed TSV to
        # manifest.json's version, so a bump that skips this goes red.)
        envRegistry = pkgs.runCommand "claude-code-env-registry.tsv" {
          nativeBuildInputs = [python3];
        } "python3 ${./extract-env-registry.py} ${stockCli}/bin/claude ${version} > $out";

        # The extractor behind subcommands.tsv, the argv tokens the CLI
        # dispatches positionally (index#4269). Not a derivation, unlike
        # `envRegistry`: it asks the stock CLI for its own command list, and
        # `claude --help` never returns inside the build sandbox (it hung past
        # 6 minutes and swallowed the extractor's own 120s timeout, because a
        # spawned helper keeps the captured stdout pipe open). The updater runs
        # it impurely instead, where the CLI behaves; the committed TSV is
        # pinned to manifest.json's version by
        # checks.claude-code-knob-reference, so a bump that skips the
        # regeneration goes red.
        subcommandExtractor = "${./extract-subcommands.py}";

        # The exact string baked into the `--system-prompt-file` flag in
        # `wrapperFlags` -- same binding as the flag, so passthru and argv
        # cannot drift. Exposed because the `builtins.toFile` output behind
        # that flag is otherwise anonymous: `nix eval --raw
        # .#claude-code.systemPrompt` prints the realized prompt without
        # digging the store path out of the launch spec. Null when the caller
        # opted into the stock prompt (HM `systemPrompt.source = "stock"`),
        # mirroring that no flag is baked then.
        inherit systemPrompt;

        # Same capture path as extractSystemPrompt, but depends only on the fetched
        # upstream binary so prompt snapshots do not rebuild the wrapped package.
        extractStockSystemPrompt = import ./extract-system-prompt.nix {
          inherit ix;
          inherit (ix) pkgs;
          name = "claude-code-extract-stock-system-prompt";
          stockBinary = "${stockCli}/bin/claude";
        };

        # Prints the stock upstream system prompt (no house overrides) by capturing
        # what the unwrapped libexec helper sends to a local ANTHROPIC_BASE_URL
        # server. See ./extract-system-prompt.nix and ./extract-system-prompt.py.
        extractSystemPrompt = import ./extract-system-prompt.nix {
          inherit ix;
          # Read the package set from `ix` rather than a `pkgs` callPackage formal
          # (which `override` can't reach); same value in both build paths.
          inherit (ix) pkgs;
          stockBinary = "${finalAttrs.finalPackage}/libexec/Claude Code";
          wrappedBinary = "${finalAttrs.finalPackage}/bin/${binName}";
        };
      }
      // lib.optionalAttrs (updateScript != null) {
        inherit updateScript;
      };

    meta = {
      description = "Claude Code, Anthropic's agentic coding tool in the terminal";
      homepage = "https://www.anthropic.com/claude-code";
      # Stripped by the `ix.allowVendoredUnfree` wrapping the whole derivation
      # above, so the tag stays honest here without blocking the per-system
      # flake package set. Distribution terms are Anthropic's commercial
      # Claude Code license.
      license = lib.licenses.unfree;
      mainProgram = binName;
      platforms = builtins.attrNames manifest.platforms;
      sourceProvenance = [lib.sourceTypes.binaryNativeCode];
    };
  }))
