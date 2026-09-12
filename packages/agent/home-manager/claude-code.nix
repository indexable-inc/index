{
  indexPackages,
  # Path to the house prompt module (packages/agent-prompt), injected by the
  # importing flake so this module never climbs the tree with `../`.
  promptModule,
}: {
  config,
  lib,
  options,
  pkgs,
  ...
}: let
  cfg = config.programs.claude-code;
  jsonFormat = pkgs.formats.json {};
  pathLike = lib.types.either lib.types.path lib.types.str;
  indexPkgs = indexPackages pkgs.stdenv.hostPlatform.system;
  systemPromptSource = lib.types.enum [
    "house"
    "stock"
    "text"
  ];

  housePrompt = import promptModule {
    inherit lib;
    omitRules = cfg.houseContext.omitRules;
  };
  houseContextText = lib.concatStringsSep "\n\n" (
    [(housePrompt.contextFor "claude")]
    ++ lib.optional (cfg.houseContext.extraText != "") cfg.houseContext.extraText
  );

  optionalOverride = condition: name: value:
    lib.optionalAttrs condition {${name} = value;};
  # BEGIN claude-code wrapper knob reference (drift-checked by checks.claude-code-knob-reference)
  # Every argument packages/claude-code/default.nix accepts (and every
  # features/systemTools sub-key), each with its stock default, so any
  # override is one uncommented line: in packageOverrides below for
  # wrapper-only knobs, or through the programs.claude-code option named in
  # the note. Key sets (and the features/systemTools default values) are
  # asserted against the wrapper at eval, so this list cannot go stale;
  # values in <angle brackets> are computed defaults, summarized.
  #   binName = "claude";  installed executable name
  #   dangerouslySkipPermissions = true;  option dangerouslySkipPermissions; bakes --dangerously-skip-permissions (upstream refuses it for unsandboxed root)
  #   extraSettings = { };  option defaults; settings deep-merged between the house posture defaults and the controlled keys
  #   features = { };  option features; typed CLAUDE_CODE_* posture, merged over the sub-key defaults below
  #   features.context1M = false;  false bakes CLAUDE_CODE_DISABLE_1M_CONTEXT (every 1M path is ~5x input price; past-the-window work belongs in subagents)
  #   features.cron = false;  false bakes CLAUDE_CODE_DISABLE_CRON (drops the scheduling/loop tools)
  #   features.fableFallback = true;  true is stock; false bakes CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK so a safety-flagged turn stops visibly instead of re-serving on Opus
  #   features.autoCompactWindow = 300000;  token count baked as CLAUDE_CODE_AUTO_COMPACT_WINDOW (the standard 300K working window); null bakes nothing
  #   features.agentTeams = false;  true bakes CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS=1 (experimental mesh teammate sessions); off matches upstream, and subagent SendMessage works without it
  #   systemTools = { };  option systemTools; false renders the bare tool name into permissions.deny, dropping its schema from context
  #   systemTools.Agent = true;  subagent spawning
  #   systemTools.Artifact = false;  claude.ai artifact publishing; enabling surfaces Anthropic design-style skills
  #   systemTools.AskUserQuestion = true;  interactive multiple-choice question dialogs; better UI than kernel Ask elicitation (#4095)
  #   systemTools.DesignSync = false;  hosted design-sync service
  #   systemTools.EnterPlanMode = true;  plan-mode entry
  #   systemTools.EnterWorktree = false;  session worktree switching (entry)
  #   systemTools.ExitPlanMode = true;  plan-mode exit
  #   systemTools.ExitWorktree = false;  session worktree switching (exit)
  #   systemTools.ListMcpResourcesTool = false;  MCP resource browser; kernel-superseded
  #   systemTools.Monitor = true;  event-stream watches; on because the Bash tool description names it as the way to wait on a condition
  #   systemTools.PushNotification = false;  mobile push via Remote Control
  #   systemTools.ReadMcpResourceDirTool = false;  MCP resource browser; kernel-superseded
  #   systemTools.ReadMcpResourceTool = false;  MCP resource browser; kernel-superseded
  #   systemTools.RemoteTrigger = true;  remote-control trigger surface
  #   systemTools.ReportFindings = true;  subagent findings reporting; the code-review skills render through it
  #   systemTools.ScheduleWakeup = false;  timed wakeups (cron orchestration surface)
  #   systemTools.SendMessage = true;  agent-team teammate messaging; on because the Agent tool description names it; subagent continuation is stock and no longer implies the agent-teams env (decoupled 2026-08-04)
  #   systemTools.SendUserFile = true;  send a file to the user's device
  #   systemTools.ShareOnboardingGuide = true;  onboarding guide sharing
  #   systemTools.Skill = true;  skill invocation
  #   systemTools.TaskCreate = true;  shared task list (agent teams)
  #   systemTools.TaskGet = true;  shared task list (agent teams)
  #   systemTools.TaskList = true;  shared task list (agent teams)
  #   systemTools.TaskOutput = true;  shared task list (agent teams)
  #   systemTools.TaskStop = true;  shared task list (agent teams)
  #   systemTools.TaskUpdate = true;  shared task list (agent teams)
  #   systemTools.ToolSearch = true;  deferred tool discovery
  #   systemTools.WaitForMcpServers = true;  block a turn until MCP servers connect
  #   systemTools.Workflow = true;  workflow commands
  #   protectedMergeGuard = true;  false drops the protected-merge gh pr merge --admin/--force Bash denies (pair with omitting the forceMerge prompt rule)
  #   kernelOnly = false;  option kernelOnly; strict mode: deny every built-in tool and every non-index MCP server, leaving the index Elixir kernel as the only tool surface
  #   keepNativeTools = [ ];  kernel-superseded native tools (Read, Write, Edit, Glob, Grep, WebSearch, WebFetch) this consumer keeps despite the baked kernel, by name; kernelOnly denies them regardless
  #   chrome = true;  false bakes --no-chrome: the Claude in Chrome bridge stays off regardless of ~/.claude.json's claudeInChromeDefaultEnabled (#4312)
  #   managedMcp = false;  true bakes no --mcp-config because the host's managed-mcp.json carries the set and 2.1.228 refuses to start with both; guard attestation and by-name denies still derive from the intended set
  #   addDirs = [ ];  option addDirs; --add-dir=<dir> flags: file access plus <dir>/.claude/skills and CLAUDE.md loading
  #   pluginDirs = [ ];  option pluginDirs; --plugin-dir=<dir> flags: namespaced plugin bundles (the house plugin rides this layer)
  #   primaryCheckouts = <"/home/*/index", "/home/*/ix">;  option primaryCheckouts; globs the PreToolUse worktree guard denies edits under
  #   personalStartupContext = false;  option personalStartupContext; Andrew-only startup context hooks
  #   alwaysOnReview = false;  arms the PostToolUse edit logger and Stop review gate
  #   extraSessionStart = [ ];  option extraSessionStart; SessionStart commands ({ package, exeName, args, timeout }) whose stdout becomes session context (index#3849)
  #   repoPackages = { };  plumbing: sibling repo packages threaded by lib/packages.nix, not a config knob
  #   mcpServers = <ix.mcp default pair: index + exa>;  option defaultMcpServers; baked --mcp-config layer (CLI layers merge, additions only)
  #   developmentChannels = <"server:index" when the index server is baked>;  channel specs for --dangerously-load-development-channels
  #   omitRules = [ ];  option systemPrompt.omitRules; rule names dropped from the house prompt
  #   omitTopics = [ ];  topic names dropped from the house prompt
  #   systemPrompt = <house prompt from packages/agent-prompt>;  option systemPrompt; null bakes no flag and ships the stock prompt
  #   updateScriptWriter = null;  plumbing: writer for passthru.updateScript (flake package set only)
  # END claude-code wrapper knob reference

  # BEGIN claude-code env reference (extracted from Claude Code cli.js 2.1.259)
  # Every documented environment variable the pinned CLI reads, one line
  # each: uncomment into a consuming machine's programs.claude-code.defaults
  # under `env` (settings env is read at CC startup even when the launch env
  # is missing) or export at launch. The value shown is the stock default
  # where the CLI or this wrapper bakes one; "" means unset. Vars owned by a
  # typed wrapper knob point at the knob instead of duplicating it. Sources:
  # the env-var registry inside the shipped cli.js, extracted mechanically
  # to packages/claude-code/env-registry.tsv (all 987 names with
  # accessor types; regenerate with `nix build .#claude-code.envRegistry`),
  # cross-checked against https://code.claude.com/docs/en/env-vars. The
  # undocumented remainder in the TSV is internal experiment gates and
  # ambient environment probes. The version in the BEGIN marker is
  # drift-checked against manifest.json by checks.claude-code-knob-reference.
  #   ANTHROPIC_API_KEY = "";  API key sent as `X-Api-Key` header.
  #   ANTHROPIC_AUTH_TOKEN = "";  Custom value for the `Authorization` header (the value you set here will be prefixed with `Bearer `)
  #   ANTHROPIC_AWS_API_KEY = "";  Workspace API key for Claude Platform on AWS, generated in the AWS Console.
  #   ANTHROPIC_AWS_BASE_URL = "";  Override the Claude Platform on AWS endpoint URL.
  #   ANTHROPIC_AWS_WORKSPACE_ID = "";  Required for Claude Platform on AWS.
  #   ANTHROPIC_BASE_URL = "";  Override the API endpoint to route requests through a proxy or gateway.
  #   ANTHROPIC_BEDROCK_BASE_URL = "";  Override the Amazon Bedrock endpoint URL.
  #   ANTHROPIC_BEDROCK_MANTLE_BASE_URL = "";  Override the Amazon Bedrock Mantle endpoint URL.
  #   ANTHROPIC_BEDROCK_SERVICE_TIER = "";  Amazon Bedrock service tier (`default`, `flex`, or `priority`).
  #   ANTHROPIC_BETAS = "";  Comma-separated list of additional `anthropic-beta` header values to include in API requests.
  #   ANTHROPIC_CUSTOM_HEADERS = "";  Custom headers to add to requests (`Name: Value` format, newline-separated for multiple headers)
  #   ANTHROPIC_CUSTOM_MODEL_OPTION = "";  Model ID to add as a custom entry in the `/model` picker.
  #   ANTHROPIC_CUSTOM_MODEL_OPTION_DESCRIPTION = "Custom model (<model-id>)";  Display description for the custom model entry in the `/model` picker.
  #   ANTHROPIC_CUSTOM_MODEL_OPTION_NAME = "";  Display name for the custom model entry in the `/model` picker.
  #   ANTHROPIC_CUSTOM_MODEL_OPTION_SUPPORTED_CAPABILITIES = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_FABLE_MODEL = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_FABLE_MODEL_DESCRIPTION = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_FABLE_MODEL_NAME = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_FABLE_MODEL_SUPPORTED_CAPABILITIES = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_HAIKU_MODEL = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_HAIKU_MODEL_DESCRIPTION = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_HAIKU_MODEL_NAME = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_HAIKU_MODEL_SUPPORTED_CAPABILITIES = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_OPUS_MODEL = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_OPUS_MODEL_DESCRIPTION = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_OPUS_MODEL_NAME = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_OPUS_MODEL_SUPPORTED_CAPABILITIES = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_SONNET_MODEL = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_SONNET_MODEL_DESCRIPTION = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_SONNET_MODEL_NAME = "";  See Model configuration
  #   ANTHROPIC_DEFAULT_SONNET_MODEL_SUPPORTED_CAPABILITIES = "";  See Model configuration
  #   ANTHROPIC_FOUNDRY_API_KEY = "";  API key for Microsoft Foundry authentication (see Microsoft Foundry)
  #   ANTHROPIC_FOUNDRY_AUTH_TOKEN = "";  Bearer token for Microsoft Foundry authentication, such as a Microsoft Entra access token.
  #   ANTHROPIC_FOUNDRY_BASE_URL = "";  Full base URL for the Microsoft Foundry resource (for example, `https://my-resource.services.ai.azure.com/anthropic`).
  #   ANTHROPIC_FOUNDRY_RESOURCE = "";  Microsoft Foundry resource name (for example, `my-resource`).
  #   ANTHROPIC_MODEL = "";  Name of the model setting to use (see Model Configuration)
  #   ANTHROPIC_SMALL_FAST_MODEL = "";  \[DEPRECATED] Name of Haiku-class model for background tasks
  #   ANTHROPIC_SMALL_FAST_MODEL_AWS_REGION = "";  Override AWS region for the Haiku-class model when using Amazon Bedrock or Amazon Bedrock Mantle.
  #   ANTHROPIC_VERTEX_BASE_URL = "";  Override Google Cloud's Agent Platform endpoint URL.
  #   ANTHROPIC_VERTEX_PROJECT_ID = "";  GCP project ID for Google Cloud's Agent Platform requests.
  #   ANTHROPIC_WORKSPACE_ID = "";  Workspace ID for workload identity federation.
  #   API_FORCE_IDLE_TIMEOUT = "";  Override the 5-minute idle timeout that aborts a streaming model response when no bytes arrive.
  #   API_TIMEOUT_MS = "";  Timeout for API requests in milliseconds (default: 600000, or 10 minutes; maximum: 2147483647).
  #   AWS_BEARER_TOKEN_BEDROCK = "";  Amazon Bedrock API key for authentication (see Amazon Bedrock API keys)
  #   BASH_DEFAULT_TIMEOUT_MS = "";  Default timeout for long-running bash commands (default: 120000, or 2 minutes)
  #   BASH_MAX_OUTPUT_LENGTH = "";  Maximum number of characters in bash outputs before the full output is saved to a file and Claude receives the path plus a short preview.
  #   BASH_MAX_TIMEOUT_MS = "";  Maximum timeout the model can set for long-running bash commands (default: 600000, or 10 minutes)
  #   CCR_FORCE_BUNDLE = "";  Set to `1` to force `claude --cloud` to bundle and upload your local repository even when GitHub access is available
  #   CLAUDECODE = "";  Set to `1` in subprocesses Claude Code spawns (Bash and PowerShell tools, tmux sessions, hook commands, status line commands, stdio MCP server subprocesses).
  #   CLAUDE_AFK_COUNTDOWN_MS = "";  How many milliseconds before auto-continue the on-screen countdown appears on an unanswered `AskUserQuestion` dialog.
  #   CLAUDE_AFK_TIMEOUT_MS = "";  How many milliseconds of idle time before an unanswered `AskUserQuestion` dialog auto-continues without you.
  #   CLAUDE_AGENT_SDK_DISABLE_BUILTIN_AGENTS = "";  Set to `1` to disable all built-in subagent types such as Explore and Plan.
  #   CLAUDE_AGENT_SDK_MCP_NO_PREFIX = "";  Set to `1` to skip the `mcp__<server>__` prefix on tool names from SDK-created MCP servers.
  #   CLAUDE_ASYNC_AGENT_STALL_TIMEOUT_MS = "";  Stall timeout in milliseconds for background subagents.
  #   CLAUDE_AUTOCOMPACT_PCT_OVERRIDE = "";  Set the percentage (1-100) of the auto-compaction window at which auto-compaction triggers.
  #   CLAUDE_AUTO_BACKGROUND_TASKS = "";  Set to `1` to force-enable automatic backgrounding of long-running agent tasks.
  #   CLAUDE_AX_SCREEN_READER = "";  Set to `1` to render screen-reader friendly output: flat text without decorative borders or animations.
  #   CLAUDE_BASH_MAINTAIN_PROJECT_WORKING_DIR = "";  Return to the original working directory after each Bash or PowerShell command in the main session
  #   CLAUDE_CLIENT_PRESENCE_FILE = "";  Path to a file that an external tool, such as a screen-lock listener, creates when you unlock your screen and deletes when you lock it.
  #   CLAUDE_CODE_ACCESSIBILITY = "";  Set to `1` to keep the native terminal cursor visible and disable the inverted-text cursor indicator.
  #   CLAUDE_CODE_ADDITIONAL_DIRECTORIES_CLAUDE_MD = "";  Set to `1` to load memory files from directories specified with `--add-dir`.
  #   CLAUDE_CODE_ALT_SCREEN_FULL_REPAINT = "";  Set to `1` to repaint the entire screen on every frame in fullscreen rendering instead of sending incremental updates.
  #   CLAUDE_CODE_ALWAYS_ENABLE_EFFORT = "";  Set to `1` to send the effort parameter with every request, even when Claude Code does not recognize the model ID as effort-capable.
  #   CLAUDE_CODE_API_KEY_HELPER_TTL_MS = "";  Interval in milliseconds at which credentials should be refreshed (when using `apiKeyHelper`)
  #   CLAUDE_CODE_ARTIFACT_AUTO_OPEN = "";  Set to `0` to stop Claude Code from opening the browser automatically when a new artifact is published.
  #   CLAUDE_CODE_ATTRIBUTION_HEADER = "";  Set to `0` to omit the attribution block (client version and prompt fingerprint) from the start of the system prompt.
  #   CLAUDE_CODE_AUTO_COMPACT_WINDOW = "300000";  typed knob: features.autoCompactWindow (baked into settings env by the wrapper)
  #   CLAUDE_CODE_AUTO_CONNECT_IDE = "";  Override automatic IDE connection.
  #   CLAUDE_CODE_AWS_CHAIN_RESOLVE_TIMEOUT_MS = "60000";  Time in milliseconds Claude Code waits for the AWS default credential provider chain to produce credentials before the request fails with `AWS default-chain credential resolve timed out` (default: ...
  #   CLAUDE_CODE_BRIDGE_SESSION_ID = "";  Set automatically in Bash tool and hook command subprocesses while the session has an active Remote Control connection, and removed when the connection ends.
  #   CLAUDE_CODE_CERT_STORE = "bundled,system";  Comma-separated list of CA certificate sources for TLS connections.
  #   CLAUDE_CODE_CHILD_SESSION = "";  Set to `1` in subprocesses Claude Code spawns via the Bash, PowerShell, and Monitor tools, hook commands, and status line commands.
  #   CLAUDE_CODE_CLIENT_CERT = "";  Path to client certificate file for mTLS authentication
  #   CLAUDE_CODE_CLIENT_KEY = "";  Path to client private key file for mTLS authentication
  #   CLAUDE_CODE_CLIENT_KEY_PASSPHRASE = "";  Passphrase for encrypted CLAUDE\_CODE\_CLIENT\_KEY (optional)
  #   CLAUDE_CODE_CONNECT_TIMEOUT_MS = "";  Removed in v2.1.186 and now a no-op.
  #   CLAUDE_CODE_DEBUG_LOGS_DIR = "~/.claude/debug/<session-id>.txt";  Override the debug log file path.
  #   CLAUDE_CODE_DEBUG_LOG_LEVEL = "";  Minimum log level written to the debug log file.
  #   CLAUDE_CODE_DISABLE_1M_CONTEXT = "1";  typed knob: features.context1M (wrapper bakes 1 while context1M = false)
  #   CLAUDE_CODE_DISABLE_ADAPTIVE_THINKING = "";  Set to `1` to disable adaptive reasoning on Opus 4.6 and Sonnet 4.6 and fall back to the fixed thinking budget controlled by `MAX_THINKING_TOKENS`.
  #   CLAUDE_CODE_DISABLE_ADVISOR_TOOL = "";  Set to `1` to disable the advisor tool.
  #   CLAUDE_CODE_DISABLE_AGENT_VIEW = "";  Set to `1` to turn off background agents and agent view: `claude agents`, `--bg`, `/background`, and the on-demand supervisor.
  #   CLAUDE_CODE_DISABLE_ALTERNATE_SCREEN = "";  Set to `1` to disable fullscreen rendering and use the classic main-screen renderer.
  #   CLAUDE_CODE_DISABLE_ARTIFACT = "";  Set to `1` to disable the Artifact tool, which publishes session output as a private web page on claude.ai.
  #   CLAUDE_CODE_DISABLE_ATTACHMENTS = "";  Set to `1` to disable attachment processing.
  #   CLAUDE_CODE_DISABLE_AUTO_MEMORY = "";  Set to `1` to disable auto memory.
  #   CLAUDE_CODE_DISABLE_BACKGROUND_TASKS = "";  Set to `1` to disable all background task functionality, including the `run_in_background` parameter on Bash and subagent tools, auto-backgrounding, and the Ctrl+B shortcut
  #   CLAUDE_CODE_DISABLE_BEDROCK_CONTENT_TYPE_GUARD = "";  Set to `1` to skip the check that an Amazon Bedrock streaming response carries the `application/vnd.amazon.eventstream` content-type.
  #   CLAUDE_CODE_DISABLE_BG_EXIT_HANDOFF = "";  Set to `1` to stop a background session's running background shell commands, dynamic workflows, and, as of v2.1.198, background subagents when the supervisor stops, restarts, or updates that sessio...
  #   CLAUDE_CODE_DISABLE_BG_SHELL_PRESSURE_REAP = "";  Set to `1` to stop Claude Code from terminating background shell commands when the operating system reports memory pressure.
  #   CLAUDE_CODE_DISABLE_BUNDLED_SKILLS = "";  Set to `1` to disable the skills and workflows included with Claude Code: bundled skills and workflows are removed entirely, while built-in commands like `/init` stay typable but are hidden from th...
  #   CLAUDE_CODE_DISABLE_CLAUDE_MDS = "";  Set to `1` to prevent loading any CLAUDE.md memory files into context, including user, project, and auto-memory files
  #   CLAUDE_CODE_DISABLE_CRON = "1";  typed knob: features.cron (wrapper bakes 1 while cron = false)
  #   CLAUDE_CODE_DISABLE_EXPERIMENTAL_BETAS = "";  Set to `1` to strip Anthropic-specific `anthropic-beta` request headers and beta tool-schema fields (such as `defer_loading` and `eager_input_streaming`) from API requests.
  #   CLAUDE_CODE_DISABLE_EXPLORE_PLAN_AGENTS = "";  Set to `1` to disable the built-in Explore and Plan subagents.
  #   CLAUDE_CODE_DISABLE_FAST_MODE = "";  Set to `1` to disable fast mode
  #   CLAUDE_CODE_DISABLE_FEEDBACK_SURVEY = "";  Set to `1` to disable the "How is Claude doing?" session quality surveys.
  #   CLAUDE_CODE_DISABLE_FILE_CHECKPOINTING = "";  Set to `1` to disable file checkpointing.
  #   CLAUDE_CODE_DISABLE_GIT_INSTRUCTIONS = "";  Set to `1` to remove built-in commit and PR workflow instructions and the git status snapshot from Claude's system prompt.
  #   CLAUDE_CODE_DISABLE_LEGACY_MODEL_REMAP = "";  Set to `1` to prevent automatic remapping of Opus 4.0 and 4.1 to the current Opus version on the Anthropic API.
  #   CLAUDE_CODE_DISABLE_MOUSE = "";  Set to `1` to disable mouse tracking in fullscreen rendering.
  #   CLAUDE_CODE_DISABLE_MOUSE_CLICKS = "";  Set to `1` to disable click, drag, and hover handling in fullscreen rendering while keeping mouse-wheel scrolling.
  #   CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC = "";  Set to any non-empty value to disable nonessential network traffic: auto-updates, telemetry, error reporting, the `/feedback` command, release notes, gateway model discovery refreshes, and availabi...
  #   CLAUDE_CODE_DISABLE_NONSTREAMING_FALLBACK = "";  Set to `1` to disable the non-streaming fallback when a streaming request fails mid-stream.
  #   CLAUDE_CODE_DISABLE_NOTIFICATION_PRESENCE_CHECK = "";  Set to `1` to send the `PushNotification` tool's desktop notification even while you are typing in or focused on the terminal.
  #   CLAUDE_CODE_DISABLE_OFFICIAL_MARKETPLACE_AUTOINSTALL = "";  Set to `1` to skip automatic addition of the official plugin marketplace on first run
  #   CLAUDE_CODE_DISABLE_POLICY_SKILLS = "";  Set to `1` to skip loading skills from the system-wide managed skills directory.
  #   CLAUDE_CODE_DISABLE_REFUSAL_FALLBACK = "1";  typed knob: features.fableFallback (wrapper bakes 1 while fableFallback = false); undocumented, in the cli.js registry
  #   CLAUDE_CODE_DISABLE_TERMINAL_TITLE = "";  Set to `1` to disable automatic terminal title updates based on conversation context.
  #   CLAUDE_CODE_DISABLE_THINKING = "";  Set to `1` to omit the `thinking` parameter from API requests entirely.
  #   CLAUDE_CODE_DISABLE_VIRTUAL_SCROLL = "";  Set to `1` to disable virtual scrolling in fullscreen rendering and render every message in the transcript.
  #   CLAUDE_CODE_DISABLE_WORKFLOWS = "";  Set to `1` to disable workflows.
  #   CLAUDE_CODE_EFFORT_LEVEL = "";  Set the effort level for supported models.
  #   CLAUDE_CODE_ENABLE_APPEND_SUBAGENT_PROMPT = "";  Set to `1` to enable appending extra text to the end of every subagent's system prompt.
  #   CLAUDE_CODE_ENABLE_AUTO_MODE = "";  Accepted for compatibility with older releases and has no effect.
  #   CLAUDE_CODE_ENABLE_AWAY_SUMMARY = "";  Override session recap availability.
  #   CLAUDE_CODE_ENABLE_BACKGROUND_PLUGIN_REFRESH = "";  Set to `1` to refresh plugin state at turn boundaries in non-interactive mode after a background install completes.
  #   CLAUDE_CODE_ENABLE_FEEDBACK_SURVEY_FOR_OTEL = "";  Set to `1` to route the "How is Claude doing?" session quality survey to your own OpenTelemetry collector when Anthropic-bound nonessential traffic is blocked.
  #   CLAUDE_CODE_ENABLE_FINE_GRAINED_TOOL_STREAMING = "";  Controls whether tool call inputs stream from the API as Claude generates them.
  #   CLAUDE_CODE_ENABLE_GATEWAY_MODEL_DISCOVERY = "";  Set to `1` to populate the `/model` picker from your gateway's `/v1/models` endpoint when `ANTHROPIC_BASE_URL` points at an Anthropic-compatible gateway such as LiteLLM, Kong, or an internal proxy.
  #   CLAUDE_CODE_ENABLE_OPUS_4_7_FAST_MODE = "";  Removed in v2.1.142, when the fast mode default moved from Opus 4.6 to Opus 4.7
  #   CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION = "";  Set to `false` to disable prompt suggestions (the "Prompt suggestions" toggle in `/config`).
  #   CLAUDE_CODE_ENABLE_TASKS = "";  Controls whether sessions use the structured Task tools (`TaskCreate`, `TaskUpdate`, `TaskGet`, `TaskList`) or the legacy `TodoWrite` tool.
  #   CLAUDE_CODE_ENABLE_TELEMETRY = "";  Set to `1` to enable OpenTelemetry data collection for metrics and logging.
  #   CLAUDE_CODE_EXIT_AFTER_STOP_DELAY = "";  Time in milliseconds to wait after the query loop becomes idle before automatically exiting.
  #   CLAUDE_CODE_EXPERIMENTAL_AGENT_TEAMS = "";  enable experimental agent teams (mesh teammate sessions); default off, opt in via features.agentTeams
  #   CLAUDE_CODE_EXTRA_BODY = "";  JSON object to merge into the top level of every API request body.
  #   CLAUDE_CODE_FILE_READ_MAX_OUTPUT_TOKENS = "";  Override the default token limit for file reads.
  #   CLAUDE_CODE_FORCE_SESSION_PERSISTENCE = "";  Set to `1` to force transcript persistence, prompt history, and `claude agents` registration even when this `claude` was launched from inside another Claude Code session.
  #   CLAUDE_CODE_FORCE_STRIKETHROUGH = "";  Set to `1` to force strikethrough rendering for `~~text~~` in Claude's responses when your terminal supports it but is not auto-detected, such as over SSH without `TERM_PROGRAM` forwarded.
  #   CLAUDE_CODE_FORCE_SYNC_OUTPUT = "";  Set to `1` to force-enable DEC private mode 2026 synchronized output when your terminal supports it but is not auto-detected.
  #   CLAUDE_CODE_FORK_SUBAGENT = "";  Set to `1` to let Claude spawn forked subagents, or `0` to disable them, overriding any server-side rollout.
  #   CLAUDE_CODE_FORWARD_SUBAGENT_TEXT = "";  Set to `1` to emit subagent text and thinking blocks in `claude -p --output-format stream-json` output, the same behavior as the `--forward-subagent-text` flag.
  #   CLAUDE_CODE_GIT_BASH_PATH = "";  Windows only: path to the Git Bash executable (`bash.exe`).
  #   CLAUDE_CODE_GLOB_HIDDEN = "";  Set to `false` to exclude dotfiles from results when Claude invokes the Glob tool.
  #   CLAUDE_CODE_GLOB_NO_IGNORE = "";  Set to `false` to make the Glob tool respect `.gitignore` patterns.
  #   CLAUDE_CODE_GLOB_TIMEOUT_SECONDS = "";  Timeout in seconds for Glob tool file discovery.
  #   CLAUDE_CODE_HIDE_CWD = "";  Set to `1` to hide the working directory in the startup logo.
  #   CLAUDE_CODE_IDE_HOST_OVERRIDE = "";  Override the host address used to connect to the IDE extension.
  #   CLAUDE_CODE_IDE_SKIP_AUTO_INSTALL = "";  Set to `1` to skip auto-installation of IDE extensions.
  #   CLAUDE_CODE_IDE_SKIP_VALID_CHECK = "";  Set to `1` to skip validation of IDE lockfile entries during connection.
  #   CLAUDE_CODE_MAX_CONCURRENT_SUBAGENTS = "";  How many subagents can be running in one session before the Agent tool refuses to spawn another (default: 20). Accepts a positive whole number in plain digits; anything else is ignored, so the variable can ad...
  #   CLAUDE_CODE_MAX_CONTEXT_TOKENS = "";  Override the context window size Claude Code assumes for the active model.
  #   CLAUDE_CODE_MAX_OUTPUT_TOKENS = "";  Set the maximum number of output tokens for most requests.
  #   CLAUDE_CODE_MAX_RETRIES = "";  Override the number of times to retry failed API requests (default: 10).
  #   CLAUDE_CODE_MAX_SUBAGENTS_PER_SESSION = "";  Cap on the number of subagents one session can spawn with the Agent tool (default: 200).
  #   CLAUDE_CODE_MAX_SUBAGENT_SPAWN_DEPTH = "";  Number of subagent layers allowed below the main conversation (default: 1). At the default, subagents can't spawn their own subagents; set `2` or higher to allow it. Accepts a positive whole number in plain d...
  #   CLAUDE_CODE_MAX_TOOL_USE_CONCURRENCY = "";  Maximum number of read-only tools and subagents that can execute in parallel (default: 10).
  #   CLAUDE_CODE_MAX_TURNS = "";  Cap the number of agentic turns when no explicit limit is passed.
  #   CLAUDE_CODE_MAX_WEB_SEARCHES_PER_SESSION = "";  Cap on the total number of WebSearch calls one session can make (default: 200).
  #   CLAUDE_CODE_MCP_ALLOWLIST_ENV = "";  Set to `1` to spawn stdio MCP servers with only a safe baseline environment plus the server's configured `env`, instead of inheriting your shell environment
  #   CLAUDE_CODE_MCP_AUTO_BACKGROUND_MS = "";  Elapsed time in milliseconds before a still-running MCP tool call moves to a background task (default: 120000, or 2 minutes).
  #   CLAUDE_CODE_MCP_TOOL_IDLE_TIMEOUT = "";  Idle timeout in milliseconds for MCP tool calls.
  #   CLAUDE_CODE_NATIVE_CURSOR = "";  Set to `1` to show the terminal's own cursor at the input caret instead of a drawn block.
  #   CLAUDE_CODE_NEW_INIT = "";  Set to `1` to make `/init` run an interactive setup flow.
  #   CLAUDE_CODE_NO_FLICKER = "";  Set to `1` to enable fullscreen rendering, a research preview that reduces flicker and keeps memory flat in long conversations.
  #   CLAUDE_CODE_OAUTH_REFRESH_TOKEN = "";  OAuth refresh token for Claude.ai authentication.
  #   CLAUDE_CODE_OAUTH_SCOPES = "";  Space-separated OAuth scopes the refresh token was issued with, such as `"user:profile user:inference user:sessions:claude_code"`.
  #   CLAUDE_CODE_OAUTH_TOKEN = "";  OAuth access token for Claude.ai authentication.
  #   CLAUDE_CODE_OPUS_4_6_FAST_MODE_OVERRIDE = "";  Removed in v2.1.160 and now a no-op.
  #   CLAUDE_CODE_OTEL_DIAG_STDERR = "";  Set to `1` to write OpenTelemetry exporter diagnostic errors to stderr.
  #   CLAUDE_CODE_OTEL_FLUSH_TIMEOUT_MS = "";  Timeout in milliseconds for flushing pending OpenTelemetry spans (default: 5000).
  #   CLAUDE_CODE_OTEL_HEADERS_HELPER_DEBOUNCE_MS = "";  Interval for refreshing dynamic OpenTelemetry headers in milliseconds (default: 1740000 / 29 minutes).
  #   CLAUDE_CODE_OTEL_SHUTDOWN_TIMEOUT_MS = "";  Timeout in milliseconds for the OpenTelemetry exporter to finish on shutdown (default: 2000).
  #   CLAUDE_CODE_PACKAGE_MANAGER_AUTO_UPDATE = "";  Set to `1` to let Claude Code run your package manager's upgrade command in the background when a new version is available.
  #   CLAUDE_CODE_PERFORCE_MODE = "";  Set to `1` to enable Perforce-aware write protection.
  #   CLAUDE_CODE_PLUGIN_CACHE_DIR = "~/.claude/plugins";  Override the plugins root directory.
  #   CLAUDE_CODE_PLUGIN_GIT_TIMEOUT_MS = "";  Timeout in milliseconds for git operations when installing or updating plugins (default: 120000).
  #   CLAUDE_CODE_PLUGIN_KEEP_MARKETPLACE_ON_FAILURE = "";  Set to `1` to skip the re-clone attempt and keep using the existing marketplace cache when a `git pull` fails.
  #   CLAUDE_CODE_PLUGIN_PREFER_HTTPS = "";  Set to `1` to clone GitHub `owner/repo` shorthand sources over HTTPS instead of SSH.
  #   CLAUDE_CODE_PLUGIN_SEED_DIR = "";  Path to one or more read-only plugin seed directories, separated by `:` on Unix or `;` on Windows.
  #   CLAUDE_CODE_POWERSHELL_RESPECT_EXECUTION_POLICY = "";  Set to `1` to stop Claude Code from passing `-ExecutionPolicy Bypass` when spawning PowerShell for tool calls, hooks, and status line commands, and respect the machine's effective execution policy ...
  #   CLAUDE_CODE_PRIMARY_CHECKOUTS = "";  ours, not upstream: colon-separated globs overriding the wrapper's primaryCheckouts worktree guard
  #   CLAUDE_CODE_PRINT_BG_WAIT_CEILING_MS = "600000";  Maximum time in milliseconds that non-interactive mode with the `-p` flag waits after the final turn for background subagents and workflows whose result is part of the output.
  #   CLAUDE_CODE_PROCESS_WRAPPER = "";  Launch the processes Claude Code starts from its own binary, such as the background service that hosts agent view sessions, through a corporate launcher given as an argv prefix like `/opt/corp/laun...
  #   CLAUDE_CODE_PROPAGATE_TRACEPARENT = "";  Set to `1` to propagate W3C trace context when `ANTHROPIC_BASE_URL` points at a custom proxy.
  #   CLAUDE_CODE_PROVIDER_MANAGED_BY_HOST = "";  Set by host platforms that embed Claude Code and manage model provider routing on its behalf.
  #   CLAUDE_CODE_PROXY_RESOLVES_HOSTS = "";  Set to `1` to allow the proxy to perform DNS resolution instead of the caller.
  #   CLAUDE_CODE_REMOTE = "";  Set automatically to `true` when Claude Code is running as a cloud session.
  #   CLAUDE_CODE_REMOTE_SESSION_ID = "";  Set automatically in cloud sessions to the current session's ID.
  #   CLAUDE_CODE_RESUME_INTERRUPTED_TURN = "";  Set to `1` to automatically resume if the previous session ended mid-turn.
  #   CLAUDE_CODE_RESUME_INTERRUPTED_TURN_MAX_AGE_MS = "";  Maximum age in milliseconds of the last transcript message for a session that ended mid-turn to continue automatically on resume.
  #   CLAUDE_CODE_RESUME_PROMPT = "Continue from where you left off.";  Override the continuation message injected when resuming a session that ended mid-turn.
  #   CLAUDE_CODE_RETRY_WATCHDOG = "";  Set to `1` for unattended sessions such as eval harnesses, CI jobs, or remote workers.
  #   CLAUDE_CODE_SAFE_MODE = "";  Set to `1` to start in safe mode: CLAUDE.md, skills, plugins, hooks, MCP servers, custom commands and agents, output styles, workflows, custom themes, custom keybindings, status line and file-sugge...
  #   CLAUDE_CODE_SCRIPT_CAPS = "";  JSON object limiting how many times specific scripts may be invoked per session when `CLAUDE_CODE_SUBPROCESS_ENV_SCRUB` is set.
  #   CLAUDE_CODE_SCROLL_SPEED = "";  Set the mouse wheel scroll multiplier in fullscreen rendering.
  #   CLAUDE_CODE_SESSIONEND_HOOKS_TIMEOUT_MS = "";  Override the time budget in milliseconds for SessionEnd hooks.
  #   CLAUDE_CODE_SESSION_ID = "";  Set automatically to the current session ID in Bash and PowerShell tool subprocesses, hook command subprocesses, and stdio MCP server subprocesses.
  #   CLAUDE_CODE_SHELL = "";  Set the shell Claude Code uses to run Bash tool commands.
  #   CLAUDE_CODE_SHELL_PREFIX = "";  Command prefix that wraps shell commands Claude Code spawns: Bash tool calls, hook commands, status line commands, and stdio MCP server startup commands.
  #   CLAUDE_CODE_SIMPLE = "";  Set to `1` to run with a minimal system prompt and only the Bash, file read, and file edit tools.
  #   CLAUDE_CODE_SIMPLE_SYSTEM_PROMPT = "";  Set to `1` to use a shorter system prompt and abbreviated tool descriptions on any model.
  #   CLAUDE_CODE_SKIP_ANTHROPIC_AWS_AUTH = "";  Skip client-side authentication for Claude Platform on AWS, for gateways that sign requests themselves
  #   CLAUDE_CODE_SKIP_AWS_CRED_CACHE = "";  Set to `1` to turn off the in-process cache of credentials resolved from the AWS default credential provider chain, so Claude Code resolves the chain on every API request.
  #   CLAUDE_CODE_SKIP_BEDROCK_AUTH = "";  Skip AWS authentication for Amazon Bedrock (for example, when using an LLM gateway)
  #   CLAUDE_CODE_SKIP_FAST_MODE_NETWORK_ERRORS = "";  Set to `1` to treat a failed fast mode availability check as available, for networks that block the check's direct request to `api.anthropic.com`.
  #   CLAUDE_CODE_SKIP_FAST_MODE_ORG_CHECK = "";  Set to `1` to skip the client-side fast mode availability check, for proxies that intercept the check's request rather than refuse it.
  #   CLAUDE_CODE_SKIP_FOUNDRY_AUTH = "";  Skip Azure authentication for Microsoft Foundry, for a proxy or gateway that injects its own `Authorization` header.
  #   CLAUDE_CODE_SKIP_MANTLE_AUTH = "";  Skip AWS authentication for Amazon Bedrock Mantle (for example, when using an LLM gateway)
  #   CLAUDE_CODE_SKIP_PROMPT_HISTORY = "";  Set to `1` to skip writing prompt history and session transcripts to disk.
  #   CLAUDE_CODE_SKIP_VERTEX_AUTH = "";  Skip Google authentication for Google Cloud's Agent Platform (for example, when using an LLM gateway)
  #   CLAUDE_CODE_STOP_HOOK_BLOCK_CAP = "";  Maximum number of consecutive times a Stop or SubagentStop hook may block the turn from ending before Claude Code overrides it and ends the turn anyway (default: 8).
  #   CLAUDE_CODE_SUBAGENT_MODEL = "";  See Model configuration.
  #   CLAUDE_CODE_SUBPROCESS_ENV_SCRUB = "";  Set to `1` to strip Anthropic and cloud provider credentials from subprocess environments (Bash tool, hooks, MCP stdio servers).
  #   CLAUDE_CODE_SYNC_PLUGIN_INSTALL = "";  Set to `1` in non-interactive mode (the `-p` flag) to wait for plugin installation to complete before the first query.
  #   CLAUDE_CODE_SYNC_PLUGIN_INSTALL_TIMEOUT_MS = "";  Timeout in milliseconds for synchronous plugin installation.
  #   CLAUDE_CODE_SYNC_SKILLS = "";  Set to `1` to download your enabled claude.ai skills into `~/.claude/skills/` before the first query and resync every 10 minutes.
  #   CLAUDE_CODE_SYNC_SKILLS_INSTALL_TIMEOUT_MS = "";  Timeout in milliseconds for a mid-session skills resync when `CLAUDE_CODE_SYNC_SKILLS` is set (default: 30000).
  #   CLAUDE_CODE_SYNC_SKILLS_WAIT_TIMEOUT_MS = "";  Timeout in milliseconds for the first query to wait on the initial skills sync when `CLAUDE_CODE_SYNC_SKILLS` is set (default: 5000).
  #   CLAUDE_CODE_SYNTAX_HIGHLIGHT = "";  Set to `false` to disable syntax highlighting in diff output.
  #   CLAUDE_CODE_TASK_LIST_ID = "";  Share a task list across sessions.
  #   CLAUDE_CODE_TEAM_TEARDOWN_PARK_TIMEOUT_MS = "";  Override, in milliseconds, how long a non-interactive session waits at exit for its agent team to finish tearing down.
  #   CLAUDE_CODE_TMPDIR = "/tmp";  Override the temp directory used for internal temp files.
  #   CLAUDE_CODE_TMUX_TRUECOLOR = "";  Set to `1` to allow 24-bit truecolor output inside tmux.
  #   CLAUDE_CODE_USE_ANTHROPIC_AWS = "";  Use Claude Platform on AWS
  #   CLAUDE_CODE_USE_BEDROCK = "";  Use Amazon Bedrock
  #   CLAUDE_CODE_USE_FOUNDRY = "";  Use Microsoft Foundry
  #   CLAUDE_CODE_USE_MANTLE = "";  Use the Amazon Bedrock Mantle endpoint
  #   CLAUDE_CODE_USE_NATIVE_FILE_SEARCH = "";  Set to `1` to discover custom commands, subagents, and output styles using Node.js file APIs instead of ripgrep.
  #   CLAUDE_CODE_USE_POWERSHELL_TOOL = "";  Controls the PowerShell tool.
  #   CLAUDE_CODE_USE_VERTEX = "";  Use Google Cloud's Agent Platform
  #   CLAUDE_CONFIG_DIR = "~/.claude";  Override the configuration directory (default: `~/.claude`).
  #   CLAUDE_DISABLE_ADOPT = "";  Set to `1` to stop in-flight background work instead of carrying it over when you background a session by pressing `` or with `/background`.
  #   CLAUDE_EFFORT = "";  Set automatically in Bash tool subprocesses and hook commands to the active effort level for the turn: `low`, `medium`, `high`, `xhigh`, or `max`.
  #   CLAUDE_ENABLE_BYTE_WATCHDOG = "";  Set to `1` to force-enable the byte-level streaming idle watchdog, or set to `0` to force-disable it.
  #   CLAUDE_ENABLE_BYTE_WATCHDOG_BEDROCK = "";  Set to `1` to enable the byte-level streaming idle watchdog on Amazon Bedrock `vnd.amazon.eventstream` responses.
  #   CLAUDE_ENABLE_STREAM_WATCHDOG = "";  Set to `0` to force-disable the event-level streaming idle watchdog, or set to `1` to force-enable it.
  #   CLAUDE_ENV_FILE = "";  Path to a shell script whose contents Claude Code runs before each Bash command in the same shell process, so exports in the file are visible to the command.
  #   CLAUDE_REMOTE_CONTROL_SESSION_NAME_PREFIX = "";  Prefix for auto-generated Remote Control session names when no explicit name is provided.
  #   CLAUDE_STREAM_IDLE_TIMEOUT_MS = "";  Timeout in milliseconds before the streaming idle watchdog closes a stalled connection.
  #   DEBUG = "";  Set to `1` to enable debug mode, equivalent to launching with `--debug`.
  #   DISABLE_AUTOUPDATER = "";  Set to `1` to disable automatic background updates.
  #   DISABLE_AUTO_COMPACT = "";  Set to `1` to disable automatic compaction when approaching the context limit.
  #   DISABLE_COMPACT = "";  Set to `1` to disable all compaction: both automatic compaction and the manual `/compact` command
  #   DISABLE_COST_WARNINGS = "";  Set to `1` to disable cost warning messages
  #   DISABLE_DOCTOR_COMMAND = "";  Set to `1` to hide the `/doctor` setup checkup skill and its `/checkup` alias.
  #   DISABLE_ERROR_REPORTING = "";  Set to `1` to opt out of error reporting
  #   DISABLE_EXTRA_USAGE_COMMAND = "";  Set to `1` to hide the `/usage-credits` command that lets users purchase additional usage beyond rate limits
  #   DISABLE_FEEDBACK_COMMAND = "";  Set to `1` to disable the `/feedback` command.
  #   DISABLE_GROWTHBOOK = "";  Set to `1` to disable GrowthBook feature-flag fetching and use code defaults for every flag.
  #   DISABLE_INSTALLATION_CHECKS = "1";  baked to 1 by the wrapper launch spec
  #   DISABLE_INSTALL_GITHUB_APP_COMMAND = "";  Set to `1` to hide the `/install-github-app` command.
  #   DISABLE_INTERLEAVED_THINKING = "";  Set to `1` to prevent sending the interleaved-thinking beta header.
  #   DISABLE_LOGIN_COMMAND = "";  Set to `1` to hide the `/login` command.
  #   DISABLE_LOGOUT_COMMAND = "";  Set to `1` to hide the `/logout` command
  #   DISABLE_PROMPT_CACHING = "";  Set to `1` to disable prompt caching for all models (takes precedence over per-model settings)
  #   DISABLE_PROMPT_CACHING_FABLE = "";  Set to `1` to disable prompt caching for Fable models
  #   DISABLE_PROMPT_CACHING_HAIKU = "";  Set to `1` to disable prompt caching for Haiku models
  #   DISABLE_PROMPT_CACHING_OPUS = "";  Set to `1` to disable prompt caching for Opus models
  #   DISABLE_PROMPT_CACHING_SONNET = "";  Set to `1` to disable prompt caching for Sonnet models
  #   DISABLE_TELEMETRY = "";  Set to `1` to opt out of telemetry.
  #   DISABLE_UPDATES = "1";  baked to 1 by the wrapper launch spec: the store output is read-only, the updater must never run
  #   DISABLE_UPGRADE_COMMAND = "";  Set to `1` to hide the `/upgrade` command
  #   DO_NOT_TRACK = "";  Set to `1` to opt out of telemetry.
  #   ENABLE_CLAUDEAI_MCP_SERVERS = "";  Set to `false` to disable claude.ai MCP servers in Claude Code.
  #   ENABLE_PROMPT_CACHING_1H = "";  Set to `1` to request a 1-hour prompt cache TTL instead of the default 5 minutes.
  #   ENABLE_PROMPT_CACHING_1H_BEDROCK = "";  Deprecated.
  #   ENABLE_TOOL_SEARCH = "";  Controls MCP tool search.
  #   FALLBACK_FOR_ALL_PRIMARY_MODELS = "";  Set to any non-empty value to make all models, not only Opus, stop retrying with a repeated-overload error when no fallback model is configured.
  #   FORCE_AUTOUPDATE_PLUGINS = "";  Set to `1` to force plugin auto-updates even when the main auto-updater is disabled via `DISABLE_AUTOUPDATER`
  #   FORCE_HYPERLINK = "";  Set to `1` to enable clickable OSC 8 hyperlinks when your terminal supports them but isn't auto-detected, or `0` to disable them
  #   FORCE_PROMPT_CACHING_5M = "";  Set to `1` to force the 5-minute prompt cache TTL even when 1-hour TTL would otherwise apply.
  #   HTTPS_PROXY = "";  Specify HTTPS proxy server for network connections
  #   HTTP_PROXY = "";  Specify HTTP proxy server for network connections
  #   IS_DEMO = "";  Set to `1` to enable demo mode: hides your email and organization name from the header and `/status` output, and skips onboarding.
  #   MAX_MCP_OUTPUT_TOKENS = "";  Maximum number of tokens allowed in MCP tool responses.
  #   MAX_STRUCTURED_OUTPUT_RETRIES = "";  Number of times to retry when the model's response fails validation against the `--json-schema` in non-interactive mode (the `-p` flag).
  #   MAX_THINKING_TOKENS = "";  Override the extended thinking token budget.
  #   MCP_CLIENT_SECRET = "";  OAuth client secret for MCP servers that require pre-configured credentials.
  #   MCP_CONNECTION_NONBLOCKING = "";  Controls whether startup waits for MCP servers to connect before the first query.
  #   MCP_CONNECT_TIMEOUT_MS = "";  How long blocking MCP startup waits, in milliseconds, for the connection batch before snapshotting the tool list (default: 5000).
  #   MCP_OAUTH_CALLBACK_PORT = "";  Fixed port for the OAuth redirect callback, as an alternative to `--callback-port` when adding an MCP server with pre-configured credentials
  #   MCP_REMOTE_SERVER_CONNECTION_BATCH_SIZE = "";  Maximum number of remote MCP servers (HTTP/SSE) to connect in parallel during startup (default: 20)
  #   MCP_SERVER_CONNECTION_BATCH_SIZE = "";  Maximum number of local MCP servers (stdio) to connect in parallel during startup (default: 3)
  #   MCP_TIMEOUT = "";  Timeout in milliseconds for MCP server startup (default: 30000, or 30 seconds)
  #   MCP_TOOL_TIMEOUT = "";  Timeout in milliseconds for MCP tool execution (default: 100000000, about 28 hours).
  #   NO_PROXY = "";  List of domains and IPs to which requests will be directly issued, bypassing proxy
  #   OTEL_LOG_ASSISTANT_RESPONSES = "";  Set to `1` to include the model's response text on `assistant_response` OpenTelemetry log events.
  #   OTEL_LOG_RAW_API_BODIES = "";  Emit Anthropic Messages API request and response JSON as `api_request_body` / `api_response_body` log events.
  #   OTEL_LOG_TOOL_CONTENT = "";  Set to `1` to include tool input and output content in OpenTelemetry span events.
  #   OTEL_LOG_TOOL_DETAILS = "";  Set to `1` to include tool input arguments, MCP server names, user-authored workflow names, raw error strings on tool failures, the refusal `category` on `api_refusal` events, and other tool detail...
  #   OTEL_LOG_USER_PROMPTS = "";  Set to `1` to include user prompt text in OpenTelemetry traces and logs.
  #   OTEL_METRICS_INCLUDE_ACCOUNT_UUID = "";  Set to `false` to exclude account UUID from metrics attributes (default: included).
  #   OTEL_METRICS_INCLUDE_ENTRYPOINT = "";  Set to `true` to include the session entrypoint in metrics attributes (default: excluded).
  #   OTEL_METRICS_INCLUDE_RESOURCE_ATTRIBUTES = "";  As of v2.1.161, Claude Code attaches `OTEL_RESOURCE_ATTRIBUTES` keys to metric datapoint labels.
  #   OTEL_METRICS_INCLUDE_SESSION_ID = "";  Set to `false` to exclude session ID from metrics attributes (default: included).
  #   OTEL_METRICS_INCLUDE_VERSION = "";  Set to `true` to include Claude Code version in metrics attributes (default: excluded).
  #   SLASH_COMMAND_TOOL_CHAR_BUDGET = "";  Override the character budget for skill metadata shown to the Skill tool.
  #   TASK_MAX_OUTPUT_LENGTH = "";  Maximum number of characters in subagent output before truncation (default: 32000, maximum: 160000).
  #   USE_BUILTIN_RIPGREP = "0";  baked to 0 by the wrapper launch spec: PATH carries the pinned nix ripgrep
  #   VERTEX_REGION_CLAUDE_3_5_HAIKU = "";  Override region for Claude 3.5 Haiku when using Google Cloud's Agent Platform
  #   VERTEX_REGION_CLAUDE_3_5_SONNET = "";  Override region for Claude 3.5 Sonnet when using Google Cloud's Agent Platform
  #   VERTEX_REGION_CLAUDE_3_7_SONNET = "";  Override region for Claude 3.7 Sonnet when using Google Cloud's Agent Platform
  #   VERTEX_REGION_CLAUDE_4_0_OPUS = "";  Override region for Claude 4.0 Opus when using Google Cloud's Agent Platform
  #   VERTEX_REGION_CLAUDE_4_0_SONNET = "";  Override region for Claude 4.0 Sonnet when using Google Cloud's Agent Platform
  #   VERTEX_REGION_CLAUDE_4_1_OPUS = "";  Override region for Claude 4.1 Opus when using Google Cloud's Agent Platform
  #   VERTEX_REGION_CLAUDE_4_5_OPUS = "";  Override region for Claude Opus 4.5 when using Google Cloud's Agent Platform
  #   VERTEX_REGION_CLAUDE_4_5_SONNET = "";  Override region for Claude Sonnet 4.5 when using Google Cloud's Agent Platform
  #   VERTEX_REGION_CLAUDE_4_6_OPUS = "";  Override region for Claude Opus 4.6 when using Google Cloud's Agent Platform
  #   VERTEX_REGION_CLAUDE_4_6_SONNET = "";  Override region for Claude Sonnet 4.6 when using Google Cloud's Agent Platform
  #   VERTEX_REGION_CLAUDE_4_7_OPUS = "";  Override region for Claude Opus 4.7 when using Google Cloud's Agent Platform.
  #   VERTEX_REGION_CLAUDE_4_8_OPUS = "";  Override region for Claude Opus 4.8 when using Google Cloud's Agent Platform.
  #   VERTEX_REGION_CLAUDE_5_SONNET = "";  Override region for Claude Sonnet 5 when using Google Cloud's Agent Platform.
  #   VERTEX_REGION_CLAUDE_FABLE_5 = "";  Override region for Claude Fable 5 when using Google Cloud's Agent Platform.
  #   VERTEX_REGION_CLAUDE_HAIKU_4_5 = "";  Override region for Claude Haiku 4.5 when using Google Cloud's Agent Platform
  # END claude-code env reference

  packageOverrides =
    {
      inherit
        (cfg)
        addDirs
        dangerouslySkipPermissions
        extraSessionStart
        features
        kernelOnly
        personalStartupContext
        primaryCheckouts
        systemTools
        ;
      # The index plugin (skills as `/index:<skill>`) rides the wrapper's
      # `--plugin-dir` layer ahead of any user-specified plugin dirs.
      pluginDirs =
        lib.optional cfg.housePlugin.enable indexPkgs.agent-plugin
        ++ cfg.pluginDirs;
      omitRules = cfg.systemPrompt.omitRules;
      extraSettings = cfg.defaults;
    }
    // optionalOverride (cfg.defaultMcpServers != null) "mcpServers" cfg.defaultMcpServers
    // optionalOverride (cfg.systemPrompt.source == "text") "systemPrompt" cfg.systemPrompt.text
    // optionalOverride (cfg.systemPrompt.source == "stock") "systemPrompt" null;
  defaultedPackage = cfg.basePackage.override packageOverrides;
in {
  options.programs.claude-code = {
    basePackage = lib.mkOption {
      type = lib.types.package;
      default = indexPkgs.claude-code;
      defaultText = lib.literalExpression "inputs.index.packages.\${pkgs.stdenv.hostPlatform.system}.claude-code";
      description = "Base index Claude Code wrapper package before Home Manager applies defaults.";
    };

    defaults = lib.mkOption {
      inherit (jsonFormat) type;
      default = {};
      description = ''
        Claude Code settings folded into the wrapper's computed render
        (between the house posture defaults and the controlled keys the
        package owns). The render surfaces as the package's
        `passthru.settingsPolicy`, which a host declares through Claude
        Code's managed layer: on darwin
        {option}`programs.claude-code.managedSettings` in
        `darwinModules.claude-managed-settings`, on NixOS
        `environment.etc."claude-code/managed-settings.json"`. Nothing is
        written to {file}`~/.claude/settings.json`, which stays app-owned.
      '';
    };

    dangerouslySkipPermissions = lib.mkOption {
      type = lib.types.bool;
      default = true;
      description = "Bake Claude Code's bypass-permissions flag into the wrapper.";
    };

    features = lib.mkOption {
      type = lib.types.attrsOf (lib.types.nullOr (lib.types.either lib.types.bool lib.types.int));
      default = {};
      example = {
        context1M = true;
        autoCompactWindow = null;
      };
      description = ''
        Typed Claude Code feature posture forwarded to the wrapper's
        `features` argument: booleans gate features (false bakes the
        feature's CLAUDE_CODE_DISABLE_* env var into both the launch layer
        and the settings env), `autoCompactWindow` is a token count for
        CLAUDE_CODE_AUTO_COMPACT_WINDOW (null bakes nothing). Keys must
        exist in the wrapper's defaultFeatures table.
      '';
    };

    systemTools = lib.mkOption {
      type = lib.types.attrsOf lib.types.bool;
      default = {};
      example = {
        DesignSync = true;
        EnterPlanMode = true;
      };
      description = ''
        Overrides for Claude Code built-in orchestration and hosted-service
        tools. Tool names must be present in the wrapper's defaultSystemTools
        table. True enables the tool; false denies it.
      '';
    };

    addDirs = lib.mkOption {
      type = lib.types.listOf pathLike;
      default = [];
      description = "Directories baked as Claude Code {command}`--add-dir=<dir>` flags.";
    };

    pluginDirs = lib.mkOption {
      type = lib.types.listOf pathLike;
      default = [];
      description = "Directories baked as Claude Code {command}`--plugin-dir=<dir>` flags.";
    };

    primaryCheckouts = lib.mkOption {
      type = lib.types.listOf lib.types.str;
      default = [
        "/home/*/index"
        "/home/*/ix"
      ];
      description = "Shell globs protected by the shared worktree guard hook.";
    };

    kernelOnly = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = ''
        Strict tool policy: deny every built-in tool and every non-index MCP
        server, leaving the index Elixir kernel (`mcp__index__*`) as the
        session's only tool surface.

        The point is the kernel's persistent Elixir workspace. Bindings,
        modules and jobs survive across cells, so work accumulates somewhere a
        later cell can build on, where the same commands through Bash leave
        nothing behind but transcript. Web search and fetch are not lost with
        the exa server; they move in-language to `Web.search/1` and
        `Web.fetch/1` against the same API.

        This is all-or-nothing for the session it applies to: there is no
        per-tool escape hatch, and an explicit `systemTools.<Tool> = true` is
        deliberately overridden rather than honoured. Turn it off to get a
        native tool back.
      '';
    };

    personalStartupContext = lib.mkOption {
      type = lib.types.bool;
      default = false;
      description = "Enable Andrew-only startup context hooks in the rendered Claude Code policy.";
    };

    extraSessionStart = lib.mkOption {
      type = lib.types.listOf (lib.types.submodule {
        options = {
          package = lib.mkOption {
            type = lib.types.package;
            description = "Program run at SessionStart; its stdout is injected as session context.";
          };
          exeName = lib.mkOption {
            type = lib.types.nullOr lib.types.str;
            default = null;
            description = "Executable name inside the package; null uses the package main program.";
          };
          args = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [];
            description = "Arguments passed to the command.";
          };
          timeout = lib.mkOption {
            type = lib.types.ints.positive;
            default = 10;
            description = "Hook timeout in seconds.";
          };
        };
      });
      default = [];
      description = "Extra SessionStart context commands folded into the rendered hook policy (index#3849).";
    };

    defaultMcpServers = lib.mkOption {
      type = lib.types.nullOr jsonFormat.type;
      default = null;
      description = ''
        MCP server JSON to bake into the wrapper's default MCP layer. Null keeps
        the package default; `{ }` intentionally bakes no default MCP config.
        Home Manager's native {option}`programs.claude-code.mcpServers` remains
        the user config layer.
      '';
    };

    housePlugin = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = true;
        description = ''
          Bake the index plugin (the repo skill set, invoked as
          `/index:<skill>`) into the wrapper as a {command}`--plugin-dir`
          layer. Disable to run without the house skills.
        '';
      };
    };

    houseContext = {
      enable = lib.mkOption {
        type = lib.types.bool;
        default = false;
        description = ''
          Write the house context render (the tagged prompt rules minus the
          `system`-only basics, see packages/agent-prompt) to
          {file}`~/.claude/CLAUDE.md` through the native
          {option}`programs.claude-code.context` option, so sessions whose
          runtime keeps its stock system prompt (claude.ai desktop, unwrapped
          CLIs) still ride the house rules. Keep this off when the consuming
          Home Manager configuration already manages {file}`.claude/CLAUDE.md`
          through {option}`home.file`.
        '';
      };

      extraText = lib.mkOption {
        type = lib.types.lines;
        default = "";
        description = ''
          Personal instructions appended after the house rules in the
          rendered context file.
        '';
      };

      omitRules = lib.mkOption {
        type = lib.types.listOf lib.types.str;
        default = [];
        description = ''
          Rule names omitted from the house context render (independent of
          {option}`programs.claude-code.systemPrompt.omitRules`, which governs
          the baked system prompt).
        '';
      };
    };

    systemPrompt = lib.mkOption {
      type = lib.types.submodule {
        options = {
          source = lib.mkOption {
            type = systemPromptSource;
            default = "house";
            description = ''
              Which system prompt the wrapper bakes: `house` renders the
              structured house prompt, `stock` bakes no prompt flag, and `text`
              uses {option}`programs.claude-code.systemPrompt.text`.
            '';
          };

          text = lib.mkOption {
            type = lib.types.nullOr lib.types.lines;
            default = null;
            description = ''
              Replacement Claude Code system prompt when
              {option}`programs.claude-code.systemPrompt.source` is `text`.
            '';
          };

          omitRules = lib.mkOption {
            type = lib.types.listOf lib.types.str;
            default = [];
            description = ''
              Rule names omitted from the generated house system prompt. Only
              valid when {option}`programs.claude-code.systemPrompt.source` is
              `house`.
            '';
          };
        };
      };
      default = {};
      description = ''
        Structured control for the system prompt baked into the Claude Code
        wrapper.
      '';
    };
  };

  config = {
    assertions = [
      {
        assertion = (cfg.systemPrompt.source == "text") == (cfg.systemPrompt.text != null);
        message = "programs.claude-code.systemPrompt: source = \"text\" requires text, and text requires source = \"text\".";
      }
      {
        assertion = cfg.systemPrompt.source == "house" || cfg.systemPrompt.omitRules == [];
        message = "programs.claude-code.systemPrompt.omitRules only applies when source = \"house\".";
      }
      {
        # omitRules reaches the shipped wrapper only through defaultedPackage
        # (basePackage.override packageOverrides); an explicit `package =`
        # discards that override. Left unchecked this shipped a half-applied
        # policy: the explicit package's permissions allowed force-merging
        # while its baked prompt still forbade it (index#3537). The package
        # stays defaulted only while no definition beats the module's own
        # `lib.mkDefault defaultedPackage` (numerically lower highestPrio
        # wins), so compare against that same mkDefault priority.
        assertion =
          cfg.systemPrompt.omitRules
          == []
          || options.programs.claude-code.package.highestPrio >= (lib.mkDefault null).priority;
        message = "programs.claude-code.systemPrompt.omitRules is ignored when package is set explicitly; pass omitRules to that package's override instead (index#3537).";
      }
      {
        # Setting any of these makes the upstream module render settings.json
        # as a read-only store symlink, which Claude Code's own runtime writes
        # then fight (#3180): it rewrites the whole file from memory on a
        # /model or /config toggle, replacing the symlink and stranding the
        # declaration until the next switch. Nix declares policy through the
        # managed layer instead (darwinModules.claude-managed-settings, or
        # environment.etc on NixOS), which outranks every writable scope and
        # needs no file in the user's directory at all (#4312).
        assertion =
          !cfg.enable
          || (
            cfg.settings
            == {}
            && cfg.marketplaces == {}
            && lib.all (server: (server.enabled or null) != false && (server.disabled or false) != true) (
              lib.attrValues cfg.mcpServers
            )
          );
        message = "programs.claude-code: settings.json is app-owned; declare settings/marketplaces/disabled MCP servers through programs.claude-code.defaults, which renders into the package's passthru.settingsPolicy for the managed layer.";
      }
    ];

    programs.claude-code = {
      package = lib.mkDefault defaultedPackage;
      context = lib.mkIf cfg.houseContext.enable (lib.mkDefault houseContextText);
    };

    # Claude Code's stock auto-updater installs native builds under
    # ~/.local/share/claude/versions and re-points ~/.local/bin/claude at
    # them, shadowing the nix wrapper on PATH; experimenting with unwrapped
    # Claude Code strands the same symlink on stale builds. Re-pin it to the
    # wrapper on every switch so a reset never survives an activation.
    # TODO(indexable-inc/index#3671): we should not need this. "Don't
    # uninstall" covers the common case, yet stray installs keep winning
    # PATH; once the updater fight is fixed at the source, remove this.
    home.file.".local/bin/claude" = lib.mkIf cfg.enable {
      source = "${cfg.finalPackage}/bin/claude";
      force = true;
    };
  };
}
