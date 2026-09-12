#pragma once
///@file The C++ side of nix-eval-rs's handle API (rust/nix-eval-rs/src/capi.rs).
///
/// The Rust evaluator interface used by command and flake-document consumers.
/// `rust-eval.cc` keeps the one-call string path for nix-instantiate's
/// whole-expression case, and takes its error translation from here so there
/// is one mapping from a Rust status to a cppnix exception rather than two.

#include "nix/expr/eval.hh"
#include "nix/cmd/command.hh"
#include "nix/cmd/rust-eval-perf-scope.hh"
#include "nix/store/path.hh"
#include "nix/store/derived-path.hh"
#include "nix/cmd/installable-value.hh"
#include "nix/flake/flakeref.hh"

#include <optional>

#include <cstddef>
#include <cstdint>
#include <exception>
#include <filesystem>
#include <map>
#include <memory>
#include <string>
#include <string_view>
#include <vector>

#include <nlohmann/json_fwd.hpp>

/// The Rust evaluator's host vtable (`rust/nix-eval-rs/include/ixe.h`) and
/// this file's context object for it.
///
/// Named here rather than included, so a command that only wants
/// `rustEvalRender` does not pull the C ABI in, and declared at global scope
/// because that is where the C ABI declares the first of them. `RustEvalHost`
/// is defined in `rust-eval-session.cc`; nothing outside it needs the
/// contents.
struct IxeHostVtable;
struct IxeEvalCache;
struct IxeFlakeShowDocument;

namespace nix {

struct RustEvalHost;

struct RustEvalCacheStats
{
    uint64_t memoryHits, diskLoads, retainedBytes, entries, evictions;
};

/// Own decoded witness reuse across fresh evaluator sessions. Host and VM
/// lifetimes stay per request; this retains only validated dependency recipes.
class RustEvalCache
{
    IxeEvalCache * cache;
public:
    explicit RustEvalCache(uint64_t maxBytes = 512ULL * 1024 * 1024, size_t maxEntries = 64);
    ~RustEvalCache();
    RustEvalCache(const RustEvalCache &) = delete;
    RustEvalCache & operator=(const RustEvalCache &) = delete;

    IxeEvalCache * get() const
    {
        return cache;
    }

    RustEvalCacheStats stats() const;
};
struct InstallableFlake;

/// Which bytes a command wants out of the value it selected. Mirrors
/// `IXE_RENDER_*`; rendering happens on the Rust side, where all three
/// walkers already exist and are diffed against cppnix by the lang corpus.
enum class RustRender {
    /// `nix-instantiate --eval --strict`: cppnix's `printAmbiguous`.
    Plain,
    /// `nix-instantiate --eval` without `--strict`: served for a value with
    /// no children (the two printings are one answer), refused otherwise --
    /// which children cppnix prints as `<CODE>` is evaluator-internal.
    PlainLazy,
    /// `nix eval` with no output flag: cppnix's `ValuePrinter`, which is a
    /// different function from `printAmbiguous` and does not always agree
    /// with it. The Rust side refuses the cases they disagree about rather
    /// than answering in the other dialect.
    ValuePrinter,
    /// `nix eval --json`. Canonical compact bytes are cached; Rust applies
    /// optional pretty formatting when presenting the answer.
    Json,
    /// `nix eval --raw`.
    Raw,
    /// `nix-instantiate --eval --strict --xml --no-location`:
    /// builtins.toXML's walker, which is the same printValueAsXML cppnix
    /// calls for both once --no-location turns source positions off. The
    /// document already ends in a newline, so the caller prints it without
    /// appending one.
    Xml,
};

/// Everything the Rust evaluator has to be told before one evaluation, and
/// the teardown of the parts that are per-call.
///
/// One object rather than a block copied into each caller: the block had
/// already drifted, and the copy in the handle path was missing the
/// store-copy hook, so `"${./f}"` reported itself unimplemented through
/// `nix eval` while `nix-instantiate` copied the file. Construct one at the
/// top of any function that is about to call into nix-eval-rs.
struct RustEvalSetup
{
    explicit RustEvalSetup(EvalState & state);
    ~RustEvalSetup();

    RustEvalSetup(const RustEvalSetup &) = delete;
    RustEvalSetup & operator=(const RustEvalSetup &) = delete;

    /// Who answers this evaluation's questions about the outside world: pass
    /// it to `ixe_session_new` or `ixe_eval_expr`.
    ///
    /// Valid for this object's lifetime and no longer. The evaluator copies
    /// the struct, but everything it points at -- the context object, and the
    /// buffers each answer is written into -- belongs to this `RustEvalSetup`,
    /// so a session must not outlive the setup that built its host.
    ///
    /// A pointer rather than a set of `ixe_set_*` calls because the host is
    /// per session. It used to be process state, so two sessions in one
    /// process shared one host and the second to be created silently
    /// answered out of the first's `EvalState`.
    const IxeHostVtable * host() const;

private:
    RustEvalPerfScope perfScope;
    std::unique_ptr<RustEvalHost> hostState;
};

/// Raise the cppnix exception a nix-eval-rs status stands for.
///
/// Never returns: status 0 is the caller's business and everything else is a
/// failure. Kept in one place because the mapping is a contract -- a thrown
/// error has to arrive as `ThrownError` under the trace note cppnix uses, or
/// the corpus differ reads the class from the wrong exception.
/**
 * Raise the exception a Rust status means, recording the refusal if it is one.
 *
 * `token` names the refusal for the census when `status` is
 * IXE_ERR_UNIMPLEMENTED. It defaults to `unrecorded` because one caller
 * genuinely has no token to give: `ixe_eval_expr` runs without a session, so
 * nothing on that path holds one. Defaulting to a real-looking token would
 * put invented rows in the histogram; defaulting to the sentinel makes the
 * gap countable.
 *
 * `pos` is where in the user's source the failure happened, or null when it
 * happened nowhere the evaluator can name. It becomes the error's own
 * position, which is what makes cppnix print `at /path/file.nix:LINE:COL`
 * and the offending line underneath -- the whole of ENG-12137's user-visible
 * half. Null is a real answer and prints the message alone, exactly as
 * cppnix does for an error it cannot place.
 */
[[noreturn]] void rustEvalThrow(
    EvalState & state,
    int status,
    const std::string & message,
    std::string_view token = "unrecorded",
    std::shared_ptr<const Pos> pos = nullptr);

/// Turn the position nix-eval-rs reported into one cppnix can render.
///
/// `line == 0` is the evaluator saying it has no position and yields null.
/// `file` is the path the failing expression was read from, or null when the
/// source was a string with no file behind it -- in which case `source`
/// becomes the origin, so `--expr` errors print `at «string»:L:C` with the
/// expression quoted underneath, the way cppnix prints them.
///
/// Takes the pieces rather than the ABI's own struct so this header does not
/// have to pull in `ixe.h`.
std::shared_ptr<const Pos>
rustEvalPos(EvalState & state, const std::string & source, const char * file, uint32_t line, uint32_t column);

/// Evaluate `source`, walk `attrPath` through the handle API, and render what
/// is there.
///
/// Selection does not force what it did not select: `hello.meta.description`
/// out of a large set enters `hello`, `meta` and `description`, and nothing
/// else. That is the reason this exists rather than a call that renders the
/// whole expression and picks text out of the result.
///
/// Throws with the marker "rust-eval unimplemented" on anything the backend
/// or this bridge does not cover, naming it.
///
/// `nestedFailureIsUnimplemented` is for `nix eval`'s plain output, which
/// prints `«error: ...»` for a value that fails inside a structure and keeps
/// going, and this printer does not. With it set, a failure raised while
/// rendering -- after the selected value itself forced cleanly, so the
/// failure is necessarily below the root -- is reported as an unimplemented
/// construct rather than as the evaluation error it is, because cppnix would
/// not have failed there at all.
/// `file` is the absolute path `source` was read from, or empty when it was
/// not read from one (`--expr`). It is what `__curPos` reports, and cppnix
/// answers `null` rather than naming a file for the second case, so the two
/// are distinguished rather than defaulted. ENG-12713.
/// The expression a command was pointed at, resolved to the three things
/// nix-eval-rs is handed.
///
/// One function rather than a block in each command, for the reason
/// `RustEvalSetup` is one object: the block was already in `nix eval`, and
/// `nix build` needs exactly the same one, including the refusals. A second
/// copy is a second set of rules about what `--file` accepts, and the two
/// would answer differently the first time either is touched.
struct RustSource
{
    std::string source;
    std::string baseDir;
    /// The absolute path `source` was read from, or empty when it was not
    /// read from one (`--expr`). What `__curPos` reports; the two are
    /// distinguished rather than defaulted, because cppnix answers `null` for
    /// a string origin. ENG-12713.
    std::string file;
    /// Empty for ambient files; otherwise the input's mounted store path.
    std::string root;
};

/// Read `--expr` or `--file`, or nothing when the `--file` argument is not a
/// plain path.
///
std::optional<RustSource> rustReadSource(SourceExprCommand & cmd);

/// Read `--expr` or `--file` for a command the Rust backend is serving, or
/// nothing when the command was given neither and its positional arguments
/// are therefore installables.
///
/// Refuses, by name, every source shape this backend does not read: an
/// expression on stdin and a `--file` that is a flake ref or a lookup path.
/// Also relaxes `pure-eval` for `--file` exactly as `parseInstallables` does,
/// so the two backends agree about what a file argument means -- which is why
/// this must run before any caller builds an `EvalState`, whose constructor
/// captures the restricted accessor.
///
/// `nullopt` is not a refusal. It used to be: with no `--expr` and no
/// `--file` this raised `command-installable`, which is where every flake
/// invocation stopped. Resolving what the positional argument means needs an
/// evaluator, so it moved to `rustEvaluandOf`, one phase later.
std::optional<RustSource> rustSourceOf(SourceExprCommand & cmd);

/// One value the bridge builds and hands to the evaluator.
///
/// Two kinds because `call-flake.nix` takes two kinds: data cppnix computed
/// (the lock file, the overrides set) and one of cppnix's internal primops
/// (`fetchFinalTree`). Both cross through the general handle calls
/// `ixe_alloc_json` and `ixe_internal_primop`; neither the ABI nor the
/// evaluator knows a flake is being built.
struct RustArgument
{
    enum class Kind {
        /// A JSON document in `ixe_alloc_json`'s dialect, which is
        /// `builtins.fromJSON`'s plus a `{"__storePath": "..."}` escape for a
        /// string that must carry a store path as its context.
        Json,
        /// The registered name of one of cppnix's `.internal = true` primops.
        InternalPrimop,
    };

    Kind kind;
    /// The document, or the primop's name.
    std::string text;
};

/// Flake policy that commands still need after Rust has selected the value.
///
/// The selected value alone cannot recover this host context. `nix develop` uses
/// the source path for phases and resolves `bashInteractive` through the
/// original flake's nixpkgs input, just as the C++ installable does.
struct RustFlakeContext
{
    FlakeRef nixpkgsFlakeRef;
    std::optional<std::filesystem::path> sourcePath;
    /// Logical store root of this input, before any flake subdirectory.
    /// Host checkout mapping uses it after evaluation; it is not memoized.
    std::string sourceStorePath;
    /// Only an unpinned workspace reference may edit its current checkout.
    bool canEditCheckout = false;
};

/// One `--arg name expr` or `--argstr name string`, as the bytes it is.
///
/// `--arg-from-file` and `--arg-from-stdin` are strings by the time cppnix
/// binds them (`MixEvalArgs::getAutoArgs` reads the file), and are strings
/// here, so what the memo key holds is what the function is applied to.
struct RustAutoArg
{
    enum class Kind {
        /// An expression, parsed under the working directory.
        Expr,
        /// A string with no context.
        String,
    };

    Kind kind;
    std::string name;
    std::string text;
};

/// The `--arg`/`--argstr` a command was given, in the ABI's terms.
std::vector<RustAutoArg> rustAutoArgsOf(const MixEvalArgs & args);

/// Everything one installable resolves to for the Rust backend: what to
/// evaluate, what to apply it to, and where to look in the result.
///
/// One type for all three source kinds, because after this point they are the
/// same job. `--expr` and `--file` carry no arguments and exactly one
/// attribute path; a flake carries `call-flake.nix`'s three arguments and the
/// candidate list `InstallableFlake::getActualAttrPaths` produced.
struct RustEvaluand
{
    RustSource src;

    /// Applied to `src`'s value in order, before selection. Empty for
    /// `--expr` and `--file`.
    std::vector<RustArgument> args;

    /// Attribute paths to try in order; the first that resolves is the one
    /// selected, which is `InstallableFlake::getCursor` taking `.at(0)` of
    /// the cursors that exist. Exactly one entry for `--expr` and `--file`.
    Strings attrPaths;

    /// Whether an all-digit path component indexes a list.
    ///
    /// True for `--expr`/`--file`, where cppnix walks with
    /// `findAlongAttrPath` (`attr-path.cc`), which does. False for a flake,
    /// where it walks with `AttrCursor::findAlongAttrPath`
    /// (`eval-cache.cc:514`), which only ever calls `maybeGetAttr` -- so
    /// `<flake>#xs.0` is a missing *attribute* named `0` in cppnix, and
    /// indexing here would answer where cppnix reports nothing found.
    bool indexLists = true;

    /// `nix eval --apply`: applied to the selected value before the answer,
    /// by the evaluator, through `ixe_question_apply`. In the memo key.
    std::optional<std::string> apply;

    /// `--arg`/`--argstr`, as cppnix's `autoArgs`: what a function met on
    /// the walk is applied to, by the evaluator, through `ixe_auto_call`
    /// wherever cppnix auto-calls. In the memo key.
    std::vector<RustAutoArg> autoArgs;

    /// Whether the selected value is auto-called once more at the end, as
    /// `nix-instantiate --eval` does when it has arguments (`processExpr`)
    /// and `nix eval` never does. In the memo key.
    bool autoCall = false;

    /// The flake reference, for the "does not provide attribute" message,
    /// and absent for the other two source kinds -- whose single missing
    /// attribute keeps `AttrPathNotFound` and its suggestions.
    std::optional<std::string> flakeRef;

    /// Present only when this evaluand came from a locked flake.
    std::optional<RustFlakeContext> flakeContext;
};

/// Resolve one raw installable into what the Rust backend evaluates.
///
/// The second phase of source resolution. `rustSourceOf` runs before there is
/// an `EvalState`, because it moves `pure-eval`; this runs after, because a
/// flake reference has to be locked and locking needs an evaluator, a store
/// and the registry.
///
/// `source` is what `rustSourceOf` returned. When it holds a value the
/// positional argument is an attribute path into it and nothing is fetched.
/// When it is empty the positional argument is an installable, and this
/// refuses a store path by name and locks a flake reference through
/// cppnix's own `lockFlake` -- which stays cpp, as it must: it walks the
/// input graph, hits the registry and writes `flake.lock`, all of which is IO
/// and policy the evaluator does not decide.
///
/// What crosses afterwards is `call-flake.nix` (ordinary Nix, 105 lines) and
/// its three arguments. No `LockedFlake` reaches the VM.
RustEvaluand rustEvaluandOf(
    SourceExprCommand & cmd, ref<EvalState> state, const std::optional<RustSource> & source, std::string_view prefix);

/// A derivation the Rust evaluator selects for a positional argument.
///
/// Nothing is asked until `toDerivedPaths` is: a command that needs the
/// installable's *value* refuses at `InstallableValue::require` before any
/// question reaches the evaluator, so `nix search <flake>` reports that
/// `nix search` is unsupported and not that the flake has no default package
/// (the route asking eagerly at parse time did exactly that, 2026-09-04).
/// cppnix has the same order: `InstallableFlake` locks and evaluates in
/// `getCursors`, never in its constructor. From the answer on the work is
/// libstore's: a `DerivedPath::Built` with the outputs
/// `meta.outputsToInstall` selected, or the explicit `^dev`/`^*`. `what()`
/// is the argument as the user spelled it, so a message about the
/// installable names `nixpkgs#hello` and not the `.drv` path it reduced to,
/// and `InstallableValue::require` can tell a derivation the route produced
/// from a store path the user typed.
struct InstallableRustDerivation : Installable
{
    SourceExprCommand & cmd;
    ref<EvalState> state;
    /// What `rustSourceOf` returned: the `--expr`/`--file` that `prefix` is
    /// an attribute path into, or empty when `prefix` is a flake reference.
    std::optional<RustSource> source;
    std::string prefix;
    ExtendedOutputsSpec extendedOutputsSpec;
    std::string spelling;

    InstallableRustDerivation(
        SourceExprCommand & cmd,
        ref<EvalState> state,
        std::optional<RustSource> source,
        std::string prefix,
        ExtendedOutputsSpec extendedOutputsSpec,
        std::string spelling)
        : cmd(cmd)
        , state(state)
        , source(std::move(source))
        , prefix(std::move(prefix))
        , extendedOutputsSpec(std::move(extendedOutputsSpec))
        , spelling(std::move(spelling))
    {
    }

    std::string what() const override
    {
        return spelling;
    }

    DerivedPathsWithInfo toDerivedPaths() override;

private:
    /// The answer, once asked. A failed question leaves it empty and is
    /// asked again, as `InstallableFlake` re-evaluates.
    std::optional<DerivedPath> derivedPath;
};

/// The Rust backend's installable parsing: what `SourceExprCommand::
/// parseInstallables` returns.
///
/// A store path is parsed as cppnix parses it and evaluates nothing. Every
/// other positional argument -- an attribute path into `--expr`/`--file`, or a
/// flake reference -- is asked of the Rust evaluator for the one derivation it
/// selects (`rustEvalDerivations`) when its `toDerivedPaths` is called, and
/// comes back as an `InstallableRustDerivation`: from there on everything is
/// store work, which is libstore's on every backend. Every command that consumes installables as
/// derivations is served by this one function; a command that needs the
/// installable's *value* refuses at `InstallableValue::require`.
Installables rustParseInstallables(SourceExprCommand & cmd, ref<Store> store, std::vector<std::string> ss);

/// Build an evaluand from an installable whose flake and selection policy are
/// already known. This is the served path for develop's bashInteractive
/// lookup; a refusal is user-visible.
RustEvaluand rustEvaluandOfInstallable(EvalState & state, InstallableFlake & installable);

/// Build the Rust evaluand for the outputs of an already locked flake.
/// Used by `nix flake show` after locking once.
RustEvaluand rustEvaluandOfLockedFlake(EvalState & state, const flake::LockedFlake & lockedFlake);

/// One derivation the Rust backend selected, reduced to what a build needs.
///
/// This is deliberately not a `Value`: the Rust VM's values live in its own
/// handle table and never become cppnix `Value`s, so what crosses is the
/// answer rather than the object. `nix build` needs a drvPath and the set of
/// outputs to install, and those are the whole of it.
struct RustDerivation
{
    StorePath drvPath;
    /// The outputs to install, already reduced by `meta.outputsToInstall`.
    /// Never empty: cppnix defaults to `out`.
    StringSet outputs;
};

/// One derivation `nix-build` builds: cppnix's `PackageInfo::requireDrvPath`
/// and `queryOutputName`, which is the derivation's own `outputName`
/// attribute (not `meta.outputsToInstall`, which is `nix build`'s rule).
struct RustBuiltDerivation
{
    StorePath drvPath;
    std::string outputName;
};

/// Evaluate `source` once, walk EVERY attribute path of the evaluand (a list
/// to visit, as `nix-build -A a -A b` is, not a ladder of candidates), and
/// collect from each every derivation cppnix's `getDerivations`
/// (`get-drvs.cc`) reaches: auto-called at every level under the evaluand's
/// arguments, attributes in name order, sets entered under
/// `recurseForDerivations = true` or a parent's `_combineChannels`, every
/// list element, each derivation once by identity. The walk is the
/// evaluator's (`ixe_derivation_set`); this side parses the store paths it
/// reports and files the answer.
std::vector<RustBuiltDerivation> rustEvalDerivationSet(EvalState & state, const RustEvaluand & evaluand);

/// Evaluate `source`, walk `attrPath`, and report the derivation found there.
///
/// The same session and the same handle walk `rustEvalRender` performs -- one
/// evaluation pipeline, not two -- with a different question asked at the
/// end: instead of rendering the value, this reads the handful of attributes
/// cppnix's `PackageInfo` reads (`get-drvs.cc`), and answers with the
/// derivation they name.
///
/// Refuses, by name, everything in that shape it does not cover: a value that
/// is not a derivation, an `outputs` list this cannot read, and any
/// `outputsToInstall` shape whose reduction is not a plain subset. Never a
/// silent fallback and never a guess: a wrong output set is a wrong build.
RustDerivation rustEvalDerivations(EvalState & state, const RustEvaluand & evaluand);

/// Evaluate an installable only far enough to obtain its derivation path.
/// `nix develop` does not select outputs; asking the broader derivation
/// question would make `meta.outputsToInstall` affect a command that never
/// reads it.
StorePath rustEvalDerivationPath(EvalState & state, const RustEvaluand & evaluand);

struct RustFlakeCheckDerivation
{
    std::string attributePath;
    StorePath drvPath;
    bool build;
};

struct RustFlakeCheckReport
{
    std::vector<RustFlakeCheckDerivation> derivations;
    std::vector<std::string> errors;
    StringSet omittedSystems;
};

// A scope creates its own Rust session; configure IFD before calling.
RustFlakeCheckReport rustEvalFlakeCheck(
    EvalState & state, const RustEvaluand & evaluand, bool hydra, bool allSystems, bool keepGoing, bool evaluateOnly);

/// The unresolved application answer consumed by cppnix's existing store
/// realisation and placeholder-rewrite code.
UnresolvedApp rustEvalApp(EvalState & state, const RustEvaluand & evaluand);

struct RustSourcePosition
{
    std::string file;
    uint32_t line;
};

/// Select the package, then read its authoritative metadata location in Rust.
RustSourcePosition rustEvalSourcePosition(EvalState & state, const RustEvaluand & evaluand);

/// Rust owns the typed document, codec and render policy. These are only
/// ownership and byte-transfer adapters for the traversal and logger.
struct RustFlakeShowRenderReport
{
    std::string output;
    std::vector<std::string> warnings;
};

class RustFlakeShowDocument
{
    struct Deleter
    {
        void operator()(IxeFlakeShowDocument * document) const;
    };

    std::unique_ptr<IxeFlakeShowDocument, Deleter> document;
    explicit RustFlakeShowDocument(IxeFlakeShowDocument * document);

public:
    RustFlakeShowDocument();
    uint64_t addNode(
        unsigned int kind,
        std::string_view name,
        const std::optional<std::string> & description,
        const std::map<std::string, uint64_t> & children);
    void finish(uint64_t root);
    std::string encode() const;
    static RustFlakeShowDocument decode(const std::string & encoded);
    RustFlakeShowRenderReport render(std::string_view rootLabel, bool json) const;
};

/// Lazily walk a flake output tree with the same decisions as
/// AttrCursor::maybeGetAttr and retain presentation-relevant node states.
RustFlakeShowDocument rustEvalFlakeShow(
    EvalState & state,
    const RustEvaluand & evaluand,
    bool showLegacy,
    bool showAllSystems,
    bool withDescriptions,
    const std::string & localSystem);

/// Evaluate `evaluand`, apply its arguments, select, and render.
///
/// The whole served pipeline for a command that prints a value. `nix build`
/// takes the same pipeline as far as selection and then asks
/// `rustEvalDerivations` a different question of the value it reached.
std::string rustEvalRender(
    EvalState & state, const RustEvaluand & evaluand, RustRender render, bool nestedFailureIsUnimplemented = false);

/// Describe the Rust language catalogue without creating an evaluator or store.
std::string rustLanguageDocs();

/// Identify the immutable host build before using persistent evaluation caches.
void rustSetHostBuildIdentity(std::string_view identity);

} // namespace nix
