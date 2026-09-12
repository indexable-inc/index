#pragma once
///@file

#include "nix/expr/diagnose.hh"
#include "nix/expr/eval-profiler-settings.hh"
#include "nix/util/configuration.hh"
#include "nix/util/source-path.hh"

namespace nix {

class EvalState;

/**
 * A deprecated bool setting that migrates to a `Setting<Diagnose>`.
 * When set to true, it emits a deprecation warning and sets the target
 * `Setting<Diagnose>` setting to `Warn`.
 */
class DeprecatedWarnSetting : public BaseSetting<bool>
{
    Setting<Diagnose> & target;
    const char * targetName;

public:
    DeprecatedWarnSetting(
        Config * options,
        Setting<Diagnose> & target,
        const char * targetName,
        const std::string & name,
        const std::string & description,
        const StringSet & aliases = {})
        : BaseSetting<bool>(false, true, name, description, aliases, std::nullopt)
        , target(target)
        , targetName(targetName)
    {
        options->addSetting(this);
    }

    void assign(const bool & v) override;
    void appendOrSet(bool newValue, bool append) override;
    void override(const bool & v) override;
};

struct EvalSettings : Config
{
    /**
     * Function used to interpret look path entries of a given scheme.
     *
     * The argument is the non-scheme part of the lookup path entry (see
     * `LookupPathHooks` below).
     *
     * The return value is (a) whether the entry was valid, and, if so,
     * what does it map to.
     */
    using LookupPathHook = std::optional<SourcePath>(EvalState & state, std::string_view);

    /**
     * Map from "scheme" to a `LookupPathHook`.
     *
     * Given a lookup path value (i.e. either the whole thing, or after
     * the `<key>=`) in the form of:
     *
     * ```
     * <scheme>:<arbitrary string>
     * ```
     *
     * if `<scheme>` is a key in this map, then `<arbitrary string>` is
     * passed to the hook that is the value in this map.
     */
    using LookupPathHooks = std::map<std::string, fun<LookupPathHook>>;

    EvalSettings(bool & readOnlyMode, LookupPathHooks lookupPathHooks = {});

    bool & readOnlyMode;

    /** Read-only evaluation is a ceiling that flake configuration cannot lift. */
    bool isImportFromDerivationAllowed() const
    {
        return !readOnlyMode && enableImportFromDerivation;
    }

    static Strings getDefaultNixPath();

    static bool isPseudoUrl(std::string_view s);

    static Strings parseNixPath(const std::string & s);

    static std::string resolvePseudoUrl(std::string_view url);

    LookupPathHooks lookupPathHooks;

    Setting<bool> enableNativeCode{this, false, "allow-unsafe-native-code-during-evaluation", R"(
        Enable built-in functions that allow executing native code.

        In particular, this adds:
        - `builtins.importNative` *path* *symbol*

          Opens dynamic shared object (DSO) at *path*, loads the function with the symbol name *symbol* from it and runs it.
          The loaded function must have the following signature:
          ```cpp
          extern "C" typedef void (*ValueInitialiser) (EvalState & state, Value & v);
          ```

          The [Nix C++ API documentation](@docroot@/development/documentation.md#api-documentation) has more details on evaluator internals.

        - `builtins.exec` *arguments*

          Execute a program, where *arguments* are specified as a list of strings, and parse its output as a Nix expression.
    )"};

    Setting<Strings> nixPath{
        this,
        {},
        "nix-path",
        R"(
          List of search paths to use for [lookup path](@docroot@/language/constructs/lookup-path.md) resolution.
          This setting determines the value of
          [`builtins.nixPath`](@docroot@/language/builtins.md#builtins-nixPath) and can be used with [`builtins.findFile`](@docroot@/language/builtins.md#builtins-findFile).

          - The configuration setting is overridden by the [`NIX_PATH`](@docroot@/command-ref/env-common.md#env-NIX_PATH)
          environment variable.
          - `NIX_PATH` is overridden by [specifying the setting as the command line flag](@docroot@/command-ref/conf-file.md#command-line-flags) `--nix-path`.
          - Any current value is extended by the [`-I` option](@docroot@/command-ref/opt-common.md#opt-I) or `--extra-nix-path`.

          If the respective paths are accessible, the default values are:

          - `$HOME/.nix-defexpr/channels`

            The [user channel link](@docroot@/command-ref/files/default-nix-expression.md#user-channel-link), pointing to the current state of [channels](@docroot@/command-ref/files/channels.md) for the current user.

          - `nixpkgs=$NIX_STATE_DIR/profiles/per-user/root/channels/nixpkgs`

            The current state of the `nixpkgs` channel for the `root` user.

          - `$NIX_STATE_DIR/profiles/per-user/root/channels`

            The current state of all channels for the `root` user.

          These files are set up by the [Nix installer](@docroot@/installation/installing-binary.md).
          See [`NIX_STATE_DIR`](@docroot@/command-ref/env-common.md#env-NIX_STATE_DIR) for details on the environment variable.

          > **Note**
          >
          > If [restricted evaluation](@docroot@/command-ref/conf-file.md#conf-restrict-eval) is enabled, the default value is empty.
          >
          > If [pure evaluation](#conf-pure-eval) is enabled, `builtins.nixPath` *always* evaluates to the empty list `[ ]`.
        )",
        {},
        false};

    Setting<std::string> currentSystem{
        this,
        "",
        "eval-system",
        R"(
          This option defines
          [`builtins.currentSystem`](@docroot@/language/builtins.md#builtins-currentSystem)
          in the Nix language if it is set as a non-empty string.
          Otherwise, if it is defined as the empty string (the default), the value of the
          [`system` ](#conf-system)
          configuration setting is used instead.

          Unlike `system`, this setting does not change what kind of derivations can be built locally.
          This is useful for evaluating Nix code on one system to produce derivations to be built on another type of system.
        )"};

    /**
     * Implements the `eval-system` vs `system` defaulting logic
     * described for `eval-system`.
     */
    const std::string & getCurrentSystem() const;

    Setting<bool> restrictEval{
        this,
        false,
        "restrict-eval",
        R"(
          If set to `true`, the Nix evaluator doesn't allow access to any
          files outside of
          [`builtins.nixPath`](@docroot@/language/builtins.md#builtins-nixPath),
          or to URIs outside of
          [`allowed-uris`](@docroot@/command-ref/conf-file.md#conf-allowed-uris).
        )"};

    Setting<bool> pureEval{
        this,
        false,
        "pure-eval",
        R"(
          Pure evaluation mode ensures that the result of Nix expressions is fully determined by explicitly declared inputs, and not influenced by external state:

          - Restrict file system and network access to files specified by cryptographic hash
          - Disable impure constants:
            - [`builtins.currentSystem`](@docroot@/language/builtins.md#builtins-currentSystem)
            - [`builtins.currentTime`](@docroot@/language/builtins.md#builtins-currentTime)
            - [`builtins.nixPath`](@docroot@/language/builtins.md#builtins-nixPath)
            - [`builtins.storePath`](@docroot@/language/builtins.md#builtins-storePath)
        )"};

    Setting<bool> traceImportFromDerivation{
        this,
        false,
        "trace-import-from-derivation",
        R"(
          By default, Nix allows [Import from Derivation](@docroot@/language/import-from-derivation.md).

          When this setting is `true`, Nix logs a warning indicating that it performed such an import.
          This option has no effect if `allow-import-from-derivation` is disabled.
        )"};

    Setting<bool> enableImportFromDerivation{
        this,
        true,
        "allow-import-from-derivation",
        R"(
          By default, Nix allows [Import from Derivation](@docroot@/language/import-from-derivation.md).

          With this option set to `false`, Nix throws an error when evaluating an expression that uses this feature,
          even when the required store object is readily available.
          This ensures that evaluation doesn't require any builds to take place,
          regardless of the state of the store.
        )"};

    Setting<Strings> allowedUris{
        this,
        {},
        "allowed-uris",
        R"(
          A list of URI prefixes to which access is allowed in restricted
          evaluation mode. For example, when set to
          `https://github.com/NixOS`, builtin functions such as `fetchGit` are
          allowed to access `https://github.com/NixOS/patchelf.git`.

          Access is granted when
          - the URI is equal to the prefix,
          - or the URI is a subpath of the prefix,
          - or the prefix is a URI scheme ended by a colon `:` and the URI has the same scheme.
        )"};

    Setting<bool> traceFunctionCalls{
        this,
        false,
        "trace-function-calls",
        R"(
          If set to `true`, the Nix evaluator traces every function call.
          Nix prints a log message at the "vomit" level for every function
          entrance and function exit.

              function-trace entered undefined position at 1565795816999559622
              function-trace exited undefined position at 1565795816999581277
              function-trace entered /nix/store/.../example.nix:226:41 at 1565795253249935150
              function-trace exited /nix/store/.../example.nix:226:41 at 1565795253249941684

          The `undefined position` means the function call is a builtin.

          Use the `contrib/stack-collapse.py` script distributed with the Nix
          source code to convert the trace logs in to a format suitable for
          `flamegraph.pl`.
        )"};

    Setting<EvalProfilerMode> evalProfilerMode{
        this,
        EvalProfilerMode::disabled,
        "eval-profiler",
        R"(
          Enables evaluation profiling. The following modes are supported:

          * `flamegraph` stack sampling profiler. Outputs folded format, one line per stack (suitable for `flamegraph.pl` and compatible tools).

          Use [`eval-profile-file`](#conf-eval-profile-file) to specify where the profile is saved.

          See [Using the `eval-profiler`](@docroot@/advanced-topics/eval-profiler.md).
        )"};

    Setting<std::filesystem::path> evalProfileFile{
        this,
        "nix.profile",
        "eval-profile-file",
        R"(
          Specifies the file where [evaluation profile](#conf-eval-profiler) is saved.
        )"};

    Setting<uint32_t> evalProfilerFrequency{
        this,
        99,
        "eval-profiler-frequency",
        R"(
          Specifies the sampling rate in hertz for sampling evaluation profilers.
          Use `0` to sample the stack after each function call.
          See [`eval-profiler`](#conf-eval-profiler).
        )"};

    Setting<std::string> readSetTraceFile{
        this,
        "",
        "read-set-trace-file",
        R"(
          If set to a path, write a read-set trace there: one JSON object per
          line describing each tracked evaluation boundary, the inputs it
          read, and the time and value allocations attributable to it.

          A tracked boundary is the result of an `import`, a
          `builtins.derivationStrict` call, and, if
          [`read-set-track-options`](#conf-read-set-track-options) is set,
          an option value in the NixOS module fixpoint. Empty, the default,
          means no instrumentation and no cost.

          This records what evaluation read. It does not cache anything and
          does not change what evaluation produces.
        )"};

    Setting<bool> readSetHashContents{
        this,
        true,
        "read-set-hash-contents",
        R"(
          Whether a read-set trace records a content hash for every file
          read. Without it a trace records only which paths were read, so
          comparing two traces cannot tell that a file's contents changed
          unless its path did too.

          Has no effect unless
          [`read-set-trace-file`](#conf-read-set-trace-file) is set.
        )"};

    Setting<bool> readSetTrackOptions{
        this,
        false,
        "read-set-track-options",
        R"(
          Whether a read-set trace records one entry per NixOS option value.

          Off by default because it costs evaluation work that a normal run
          does not do: option values are recognised by the error context the
          module system attaches to them, and finding that context means
          forcing the context message of every `builtins.addErrorContext`
          call, which is otherwise only forced when an error is thrown.

          Has no effect unless
          [`read-set-trace-file`](#conf-read-set-trace-file) is set.
        )"};

    Setting<bool> readSetTrackPositions{
        this,
        true,
        "read-set-track-positions",
        R"(
          Whether a read-set trace records source positions as inputs.

          Positions are inputs because `builtins.unsafeGetAttrPos` observes
          them, so an edit that shifts a line changes them. Recording them
          is what makes a trace usable for validating a cache; turning them
          off measures how much reuse they cost.

          Has no effect unless
          [`read-set-trace-file`](#conf-read-set-trace-file) is set.
        )"};

    Setting<bool> useEvalCache{
        this,
        true,
        "eval-cache",
        R"(
            Whether to use the flake evaluation cache.
            Certain commands won't have to evaluate when invoked for the second time with a particular version of a flake.
            Intermediate results are not cached.
        )"};

    Setting<bool> traceVerbose{
        this,
        false,
        "trace-verbose",
        "Whether `builtins.traceVerbose` should trace its first argument when evaluated."};

    Setting<unsigned int> maxCallDepth{
        this, 10000, "max-call-depth", "The maximum function call depth to allow before erroring."};

    Setting<std::string> evalCacheDir{
        this,
        "",
        "eval-cache-dir",
        R"(
          Directory holding the Rust evaluator's on-disk cache of compiled
          modules and evaluation results. Empty (the default) keeps those
          caches in memory, so every invocation starts cold.

          Entries are addressed by the content they
          were produced from, so an edit changes the address and misses rather
          than needing to be noticed: there is no invalidation pass and nothing
          to clear after changing a file. Removing the directory is always
          safe, and only ever costs recomputation.

          Bounded by [`eval-cache-max-bytes`](#conf-eval-cache-max-bytes).
        )"};

    Setting<uint64_t> evalCacheMaxBytes{
        this,
        4ULL << 30,
        "eval-cache-max-bytes",
        R"(
          Byte cap on [`eval-cache-dir`](#conf-eval-cache-dir). After each
          evaluation result is published the cache is swept, least recently
          used entries first, until it fits. `0` never sweeps.

          The sweep reads directory metadata and one small sidecar per
          witness, never a witness body, so its cost is the number of entries
          in the cache and not their size. Eviction can only make a later
          evaluation a miss, never change what one returns.
        )"};

    Setting<unsigned int> evalCacheVerifyRate{
        this,
        0,
        "eval-cache-verify-rate",
        R"(
          How often the Rust evaluator's on-disk cache checks itself, as one
          occasion in this many. `0`, the default, never checks. `1` checks
          every occasion. `20` checks one in twenty.

          A cache cannot be checked by reading its answers, because its
          answers are by construction whatever it was told to say. A wrong row
          is served silently and looks exactly like a right one; the only run
          that can tell the difference is one that does the work anyway and
          compares. ENG-12541 -- a memo key blind to the store directory, so a
          cache shared between two stores served paths for the wrong one --
          was found by reading the code, and would have been found in
          production by a one-in-twenty check, on the first machine that
          pointed one cache at two stores.

          Two different occasions are sampled at this rate, because the cache
          has two failure shapes and neither check can see the other's. One
          hit in this many is re-evaluated and compared against what was
          served, which catches a wrong answer; the served answer is still
          what the command prints, so whether the sampler fired cannot change
          output. One record in this many is looked up again in the same
          process, which catches a cache that writes rows it will never serve
          -- correct and useless -- the shape a verifier built only from the
          hit side reports perfect health through.

          The cost is exactly what it sounds like: a sampled hit pays a full
          evaluation, so at `1` the cache saves nothing and is only being
          audited. `20` is the recommendation for a dogfood machine -- a few
          percent of the saving spent on the only evidence that the rest of it
          is honest -- and `0` is right where speed is the whole point.

          Verification runs only when [`eval-cache-dir`](#conf-eval-cache-dir) names a
          directory: with no cache on disk there is nothing to check. A
          disagreement is reported on stderr and names the memo key, so the
          row can be found and the inputs that differ identified.
        )"};

    Setting<bool> builtinsAbortOnWarn{
        this,
        false,
        "abort-on-warn",
        R"(
          If set to true, [`builtins.warn`](@docroot@/language/builtins.md#builtins-warn) throws an error when logging a warning.

          This will give you a stack trace that leads to the location of the warning.

          This is useful for finding the origin of warnings in third-party Nix code, including when Nix is called from a non-interactive script.

          This option can be enabled by setting `NIX_ABORT_ON_WARN=1` in the environment.
        )"};

    Setting<Diagnose> lintShortPathLiterals{
        this,
        Diagnose::Ignore,
        "lint-short-path-literals",
        R"(
          Controls handling of relative path literals that don't start with `./` or `../`.

          - `ignore`: Ignore without warning (default)
          - `warn`: Emit a warning suggesting to use `./` prefix
          - `fatal`: Treat as a parse error

          For example, with this setting set to `warn` or `fatal`, `foo/bar` would
          suggest using `./foo/bar` instead.

          This is useful for improving code readability and making path literals
          more explicit.
        )",
    };

    DeprecatedWarnSetting warnShortPathLiterals{
        this,
        lintShortPathLiterals,
        "lint-short-path-literals",
        "warn-short-path-literals",
        R"(
          Deprecated. Use [`lint-short-path-literals`](#conf-lint-short-path-literals)` = warn` instead.
        )",
    };

    Setting<Diagnose> lintAbsolutePathLiterals{
        this,
        Diagnose::Ignore,
        "lint-absolute-path-literals",
        R"(
          Controls handling of absolute path literals (paths starting with `/`) and home path literals (paths starting with `~/`).

          - `ignore`: Ignore without warning (default)
          - `warn`: Emit a warning about non-portability
          - `fatal`: Treat as a parse error

          It is true that some files are more difficult to reference with relative paths,
          because they would require lots of `../../..` upward traversing to reach them.
          But firstly, it is probably not a good idea to reference these files ---
          such paths often make Nix expressions less portable and reproducible,
          as they depend on the file system layout of the machine evaluating the expression.

          Secondly, with [pure evaluation mode](#conf-pure-eval), most such files are prohibited to access anyway,
          whether by absolute or relative paths.
          In that case, enabling this lint in fatal mode is less disruptive,
          because the paths pure eval allows are usually not the ones that would be ergonomically expressed with absolute paths anyway.
        )",
    };

    Setting<Diagnose> lintUrlLiterals{
        this,
        Diagnose::Ignore,
        "lint-url-literals",
        R"(
          Controls handling of unquoted URLs as part of the Nix language syntax.
          The Nix language allows for URL literals, like so:

          ```
          $ nix repl
          nix-repl> http://foo
          "http://foo"
          ```

          Setting this to `warn` or `fatal` will cause the Nix parser to
          warn or throw an error when encountering a URL literal:

          ```
          $ nix repl --lint-url-literals fatal
          nix-repl> http://foo
          error: URL literal 'http://foo' is deprecated
                 at «string»:1:1:

                      1| http://foo
                       | ^
          ```

          Unquoted URLs are being deprecated and their usage is discouraged.

          The reason is that, as opposed to path literals, URLs have no
          special properties that distinguish them from regular strings, URLs
          containing query parameters have to be quoted anyway, and unquoted URLs
          may confuse external tooling.
        )",
    };

    Setting<unsigned> bindingsUpdateLayerRhsSizeThreshold{
        this,
        sizeof(void *) == 4 ? 8192 : 16,
        "eval-attrset-update-layer-rhs-threshold",
        R"(
          Tunes the maximum size of an attribute set that, when used
          as a right operand in an [attribute set update expression](@docroot@/language/operators.md#update),
          uses a more space-efficient linked-list representation of attribute sets.

          Setting this to larger values generally leads to less memory allocations,
          but may lead to worse evaluation performance.

          A value of `0` disables this optimization completely.

          This is an advanced performance tuning option and typically should not be changed.
          The default value is chosen to balance performance and memory usage. On 32 bit systems
          where memory is scarce, the default is a large value to reduce the amount of allocations.
    )"};
};

/**
 * Stack size for a thread that evaluates Nix expressions. The main thread and
 * every parallel evaluator worker must agree, or an expression that recurses
 * to just under the limit on one would overflow on the other.
 */
constexpr size_t evalStackSize = 60 * 1024 * 1024;

/**
 * Conventionally part of the default nix path in impure mode.
 */
std::filesystem::path getNixDefExpr();

} // namespace nix
