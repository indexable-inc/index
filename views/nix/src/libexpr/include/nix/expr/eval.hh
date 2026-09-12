#pragma once
///@file

#include "nix/expr/attr-set.hh"
#include "nix/expr/eval-error.hh"
#include "nix/expr/eval-profiler.hh"
#include "nix/util/types.hh"
#include "nix/expr/value.hh"
#include "nix/expr/nixexpr.hh"
#include "nix/expr/symbol-table.hh"
#include "nix/util/configuration.hh"
#include "nix/util/experimental-features.hh"
#include "nix/util/position.hh"
#include "nix/util/pos-table.hh"
#include "nix/util/source-accessor.hh"
#include "nix/store/content-address.hh"
#include "nix/expr/search-path.hh"
#include "nix/util/ref.hh"
#include "nix/expr/counter.hh"

// For `NIX_USE_BOEHMGC`, and if that's set, `GC_THREADS`
#include "nix/expr/config.hh"

#include <boost/unordered/unordered_flat_map.hpp>
#include <boost/unordered/concurrent_flat_map_fwd.hpp>

#include <map>
#include <optional>
#include <functional>

namespace nix {

/**
 * We put a limit on primop arity because it lets us use a fixed size array on
 * the stack. 8 is already an impractical number of arguments. Use an attrset
 * argument for such overly complicated functions.
 */
constexpr size_t maxPrimOpArity = 8;

class Store;

namespace fetchers {
struct Settings;
struct InputCache;
struct Input;
} // namespace fetchers
struct EvalSettings;
class EvalState;
class StorePath;
struct SingleDerivedPath;
enum RepairFlag : bool;
struct MemorySourceAccessor;
struct MountedSourceAccessor;
struct InterruptCallback;

namespace eval_cache {
class EvalCache;
}

class ReadSetTracker;

/**
 * Increments a count on construction and decrements on destruction.
 */
class CallDepth
{
    size_t & count;

public:
    CallDepth(size_t & count)
        : count(count)
    {
        ++count;
    }

    ~CallDepth()
    {
        --count;
    }
};

/**
 * Function that implements a primop.
 */
using PrimOpFun = void(EvalState & state, const PosIdx pos, Value ** args, Value & v);

/**
 * Info about a primitive operation, and its implementation
 */
struct PrimOp
{
    /**
     * Name of the primop. `__` prefix is treated specially.
     */
    std::string name;

    /**
     * Names of the parameters of a primop, for primops that take a
     * fixed number of arguments to be substituted for these parameters.
     */
    std::vector<std::string> args;

    /**
     * Aritiy of the primop.
     *
     * If `args` is not empty, this field will be computed from that
     * field instead, so it doesn't need to be manually set.
     */
    size_t arity = 0;

    /**
     * Optional free-form documentation about the primop.
     */
    std::optional<std::string> doc;

    /**
     * Add a trace item, while calling the `<name>` builtin.
     *
     * This is used to remove the redundant item for `builtins.addErrorContext`.
     */
    bool addTrace = true;

    /**
     * Implementation of the primop.
     */
    fun<PrimOpFun> impl;

    /**
     * Optional experimental for this to be gated on.
     */
    std::optional<ExperimentalFeature> experimentalFeature;

    /**
     * If true, this primop is not exposed to the user.
     */
    bool internal = false;

    /**
     * Validity check to be performed by functions that introduce primops,
     * such as Value::mkPrimOp().
     */
    void check();
};

std::ostream & operator<<(std::ostream & output, const PrimOp & primOp);

struct Env
{
    Env * up;
    Value * values[0];
};

void copyContext(
    const Value & v,
    NixStringContext & context,
    const ExperimentalFeatureSettings & xpSettings = experimentalFeatureSettings);

std::string printValue(EvalState & state, Value & v);
std::ostream & operator<<(std::ostream & os, const ValueType t);

struct StaticEvalSymbols
{
    Symbol with, outPath, drvPath, type, meta, name, value, system, overrides, outputs, outputName, ignoreNulls, file,
        line, column, functor, toString, right, wrong, structuredAttrs, json, allowedReferences, allowedRequisites,
        disallowedReferences, disallowedRequisites, maxSize, maxClosureSize, builder, args, contentAddressed, impure,
        outputHash, outputHashAlgo, outputHashMode, recurseForDerivations, description, self, epsilon, startSet,
        operator_, key, path, prefix, outputSpecified, __meta;

    static constexpr auto preallocate()
    {
        StaticSymbolTable alloc;

        StaticEvalSymbols staticSymbols = {
            .with = alloc.create("<with>"),
            .outPath = alloc.create("outPath"),
            .drvPath = alloc.create("drvPath"),
            .type = alloc.create("type"),
            .meta = alloc.create("meta"),
            .name = alloc.create("name"),
            .value = alloc.create("value"),
            .system = alloc.create("system"),
            .overrides = alloc.create("__overrides"),
            .outputs = alloc.create("outputs"),
            .outputName = alloc.create("outputName"),
            .ignoreNulls = alloc.create("__ignoreNulls"),
            .file = alloc.create("file"),
            .line = alloc.create("line"),
            .column = alloc.create("column"),
            .functor = alloc.create("__functor"),
            .toString = alloc.create("__toString"),
            .right = alloc.create("right"),
            .wrong = alloc.create("wrong"),
            .structuredAttrs = alloc.create("__structuredAttrs"),
            .json = alloc.create("__json"),
            .allowedReferences = alloc.create("allowedReferences"),
            .allowedRequisites = alloc.create("allowedRequisites"),
            .disallowedReferences = alloc.create("disallowedReferences"),
            .disallowedRequisites = alloc.create("disallowedRequisites"),
            .maxSize = alloc.create("maxSize"),
            .maxClosureSize = alloc.create("maxClosureSize"),
            .builder = alloc.create("builder"),
            .args = alloc.create("args"),
            .contentAddressed = alloc.create("__contentAddressed"),
            .impure = alloc.create("__impure"),
            .outputHash = alloc.create("outputHash"),
            .outputHashAlgo = alloc.create("outputHashAlgo"),
            .outputHashMode = alloc.create("outputHashMode"),
            .recurseForDerivations = alloc.create("recurseForDerivations"),
            .description = alloc.create("description"),
            .self = alloc.create("self"),
            .epsilon = alloc.create(""),
            .startSet = alloc.create("startSet"),
            .operator_ = alloc.create("operator"),
            .key = alloc.create("key"),
            .path = alloc.create("path"),
            .prefix = alloc.create("prefix"),
            .outputSpecified = alloc.create("outputSpecified"),
            .__meta = alloc.create("__meta"),
        };

        return std::pair{staticSymbols, alloc};
    }

    static consteval StaticEvalSymbols create()
    {
        return preallocate().first;
    }

    static constexpr StaticSymbolTable staticSymbolTable()
    {
        return preallocate().second;
    }
};

class EvalMemory
{
public:
    struct Statistics
    {
        Counter nrEnvs;
        Counter nrValuesInEnvs;
        Counter nrValues;
        Counter nrAttrsets;
        Counter nrAttrsInAttrsets;
        Counter nrListElems;
    };

    EvalMemory();

    EvalMemory(const EvalMemory &) = delete;
    EvalMemory(EvalMemory &&) = delete;
    EvalMemory & operator=(const EvalMemory &) = delete;
    EvalMemory & operator=(EvalMemory &&) = delete;

    inline void * allocBytes(size_t n);
    inline Value * allocValue();
    inline Env & allocEnv(size_t size);

    Bindings * allocBindings(size_t capacity);

    BindingsBuilder buildBindings(SymbolTable & symbols, size_t capacity)
    {
        return BindingsBuilder(*this, symbols, allocBindings(capacity), capacity);
    }

    ListBuilder buildList(size_t size)
    {
        stats.nrListElems += size;
        return ListBuilder(*this, size);
    }

    const Statistics & getStats() const &
    {
        return stats;
    }

    /**
     * Storage for the AST nodes
     */

private:
    Statistics stats;
};

class EvalState : public std::enable_shared_from_this<EvalState>
{
public:
    static constexpr StaticEvalSymbols s = StaticEvalSymbols::create();

    const fetchers::Settings & fetchSettings;
    const EvalSettings & settings;

    SymbolTable symbols;
    PosTable positions;

    EvalMemory mem;

    /**
     * If set, force copying files to the Nix store even if they
     * already exist there.
     */
    RepairFlag repair;

    /**
     * The accessor corresponding to `store`.
     */
    const ref<MountedSourceAccessor> storeFS;

    /**
     * The accessor for the root filesystem.
     */
    const ref<SourceAccessor> rootFS;

    /**
     * The in-memory filesystem for <nix/...> paths.
     */
    const ref<MemorySourceAccessor> corepkgsFS;

    /**
     * Store used to materialise .drv files.
     */
    const ref<Store> store;

    /**
     * Store used to build stuff.
     */
    const ref<Store> buildStore;

    const ref<fetchers::InputCache> inputCache;

    template<class T, typename... Args>
    [[nodiscard, gnu::noinline]]
    EvalErrorBuilder<T> & error(const Args &... args)
    {
        // `EvalErrorBuilder::debugThrow` performs the corresponding `delete`.
        return *new EvalErrorBuilder<T>(*this, args...);
    }

    /**
     * A cache for evaluation caches, so as to reuse the same root value if possible
     */
    std::map<const Hash, ref<eval_cache::EvalCache>> evalCaches;

private:

    /**
     * A cache that maps paths to "resolved" paths for importing Nix
     * expressions, i.e. `/foo` to `/foo/default.nix`.
     */
    const ref<boost::concurrent_flat_map<SourcePath, SourcePath>> importResolutionCache;

    /**
     * Source paths this evaluation has already copied to the store, keyed
     * by the path as it was coerced (before symlink resolution), so a
     * repeat coercion answers from memory instead of from the store.
     *
     * Without it every coercion of the same path reaches `fetchToStore`,
     * which costs two daemon round trips (a temporary root and a validity
     * check) even when the fingerprint cache spares the copy. One
     * home-manager evaluation coerces 170k paths, nearly all of them
     * repeats, and paid 12 s of its wall time in those round trips -- under
     * both evaluators, since both come through here.
     */
    const ref<boost::concurrent_flat_map<SourcePath, StorePath>> srcToStore;

    /**
     * A cache from resolved paths to values.
     */
    const ref<boost::concurrent_flat_map<
        SourcePath,
        Value *,
        std::hash<SourcePath>,
        std::equal_to<SourcePath>,
        traceable_allocator<std::pair<const SourcePath, Value *>>>>
        fileEvalCache;

    LookupPath lookupPath;

    const ref<boost::concurrent_flat_map<std::string, std::optional<SourcePath>, StringViewHash, std::equal_to<>>>
        lookupPathResolved;

public:

    /**
     * @param lookupPath     Only used during construction.
     * @param store          The store to use for instantiation
     * @param fetchSettings  Must outlive the lifetime of this EvalState!
     * @param settings       Must outlive the lifetime of this EvalState!
     * @param buildStore     The store to use for builds ("import from derivation", C API `nix_string_realise`)
     */
    EvalState(
        const LookupPath & lookupPath,
        ref<Store> store,
        const fetchers::Settings & fetchSettings,
        const EvalSettings & settings,
        std::shared_ptr<Store> buildStore = nullptr);
    ~EvalState();

    /**
     * A wrapper around EvalMemory::allocValue() to avoid code churn when it
     * was introduced.
     */
    inline Value * allocValue()
    {
        return mem.allocValue();
    }

    LookupPath getLookupPath()
    {
        return lookupPath;
    }

    /**
     * Return a `SourcePath` that refers to `path` in the root
     * filesystem.
     */
    SourcePath rootPath(CanonPath path);

    /**
     * Variant which accepts relative paths too.
     */
    SourcePath rootPath(std::string_view path);

    /**
     * Return a `SourcePath` that refers to `path` in the store.
     *
     * For now, this has to also be within the root filesystem for
     * backwards compat, but for Windows and maybe also pure eval, we'll
     * probably want to do something different.
     */
    SourcePath storePath(const StorePath & path);

    /**
     * Allow access to a path.
     *
     * Only for restrict eval: pure eval just whitelist store paths,
     * never arbitrary paths.
     */
    void allowPathLegacy(const std::string & path);

    /**
     * Allow access to a store path. Note that this gets remapped to
     * the real store path if `store` is a chroot store.
     */
    void allowPath(const StorePath & storePath);

    /**
     * Allow access to the closure of a store path.
     */
    void allowClosure(const StorePath & storePath);

    /**
     * Allow access to a store path and return it as a string.
     */
    void allowAndSetStorePathString(const StorePath & storePath, Value & v);

    void checkURI(const std::string & uri);

    /**
     * Mount an input on the Nix store.
     */
    StorePath mountInput(fetchers::Input & input, const fetchers::Input & originalInput, ref<SourceAccessor> accessor);

    /**
     * Serve `accessor` at `storePath` inside the evaluator without copying
     * it: the store path was derived from the tree's id, and the bytes are
     * written only when something forces them (`ensureLazyPathCopied`).
     * The path is allowed under restricted evaluation. A second mount at a
     * path already served keeps the first: one id, one object, so the two
     * accessors serve the same bytes.
     */
    void mountLazily(const StorePath & storePath, ref<SourceAccessor> accessor);

    /**
     * The store object for `path` as `builtins.path` denotes it: `name`d,
     * ingested by `method` under `filter`, carrying `refs`, checked against
     * `expectedHash` when one is given, allowed under restricted evaluation.
     * Every road from a path value into the store (`builtins.path`,
     * `builtins.filterSource`, a path coerced to a string, the Rust
     * evaluator's host question) is this one call.
     *
     * A directory served from a jj object store is addressed by the id of
     * the tree `filter` leaves (`SourceAccessor::getFilteredTree`) and
     * mounted lazily at that store path, as a flake input is: no file is
     * read or copied until a consumer forces it (`ensureLazyPathCopied`: a
     * build input, an evaluation result carrying its context, `nix flake
     * archive`); under `--repair` it is materialised at once, so that the
     * bytes are compared and rewritten, at the same store path. A pinned
     * `expectedHash` names a NAR hash, which only the NAR road can check,
     * and `refs` have no tree-id form, so those keep the NAR road.
     */
    StorePath addPathToStore(
        const SourcePath & path,
        std::string_view name,
        ContentAddressMethod method,
        PathFilter * filter,
        const std::optional<Hash> & expectedHash,
        const StorePathSet & refs);

    /**
     * Evaluate an expression read from the given file to normal
     * form. Optionally enforce that the top-level expression is
     * trivial (i.e. doesn't require arbitrary computation).
     */
    void evalFile(const SourcePath & path, Value & v, bool mustBeTrivial = false);

    void resetFileCache();

    /// Start a new request without retaining answers keyed by mutable paths.
    /// Content-addressed input accessors remain cached. Returns entries evicted.
    size_t prepareForNextRequest();

    /**
     * Look up a file in the search path.
     */
    SourcePath findFile(const std::string_view path);
    SourcePath findFile(const LookupPath & lookupPath, const std::string_view path, const PosIdx pos = noPos);

    /**
     * Try to resolve a search path value (not the optional key part).
     *
     * If the specified search path element is a URI, download it.
     *
     * If it is not found, return `std::nullopt`.
     */
    std::optional<SourcePath> resolveLookupPathPath(const LookupPath::Path & elem, bool initAccessControl = false);

    /**
     * Evaluate an expression to normal form
     *
     * @param [out] v The resulting is stored here.
     */
    void eval(Expr * e, Value & v);

    /// Reject commands that still require C++ expression values.
    [[noreturn]] void requireBackendCanServe();

    /// Count completed Rust questions, including memoized answers.
    std::atomic<uint64_t> nrRustEvals{0};
    /// Successful commands that exec flush stats before replacing the
    /// process. If exec then fails, the destructor must not write them twice.
    std::atomic<bool> statsPrinted{false};

    /**
     * Record that the Rust backend is about to serve one evaluation.
     *
     * Called from `RustEvalSetup`'s constructor, which is the single crossing
     * into nix-eval-rs, so a failed Rust evaluation counts too: the question
     * the field answers is which evaluator ran, not which one succeeded.
     */
    void countRustEval()
    {
        nrRustEvals++;
    }

    /**
     * Evaluation the expression, then verify that it has the expected
     * type.
     */
    inline bool evalBool(Env & env, Expr * e);
    inline bool evalBool(Env & env, Expr * e, const PosIdx pos, std::string_view errorCtx);
    inline void evalAttrs(Env & env, Expr * e, Value & v, const PosIdx pos, std::string_view errorCtx);

    /**
     * If `v` is a thunk, enter it and overwrite `v` with the result
     * of the evaluation of the thunk.  If `v` is a delayed function
     * application, call the function and overwrite `v` with the
     * result.  Otherwise, this is a no-op.
     */
    inline void forceValue(Value & v, const PosIdx pos)
    {
        v.force(*this, pos);
    }

    void tryFixupBlackHolePos(Value & v, PosIdx pos);

    /**
     * Record the in-flight exception in `v`, which this thread holds
     * pending. `env`/`expr` (for a thunk) or `left`/`right` (for a
     * function application) describe what failed, and are null if this
     * thread does not own it; a `RecoverableEvalError` keeps them as a
     * recovery value so `retryFailed()` can evaluate it again.
     */
    void handleEvalException(Value & v, PosIdx pos, Env * env, Expr * expr, Value * left, Value * right);

    /**
     * Retry a failed value that carries a recovery value, and overwrite
     * it with the result.
     */
    void retryFailed(Value & v, PosIdx pos);

public:

    /**
     * Force a value, then recursively force list elements and
     * attributes.
     */
    void forceValueDeep(Value & v);

    /**
     * Force `v`, and then verify that it has the expected type.
     */
    NixInt forceInt(Value & v, const PosIdx pos, std::string_view errorCtx);
    NixFloat forceFloat(Value & v, const PosIdx pos, std::string_view errorCtx);
    bool forceBool(Value & v, const PosIdx pos, std::string_view errorCtx);

    void forceAttrs(Value & v, const PosIdx pos, std::string_view errorCtx);

    template<typename Callable>
    inline void forceAttrs(Value & v, Callable getPos, std::string_view errorCtx);

    inline void forceList(Value & v, const PosIdx pos, std::string_view errorCtx);
    /**
     * @param v either lambda or primop
     */
    void forceFunction(Value & v, const PosIdx pos, std::string_view errorCtx);
    std::string_view forceString(Value & v, const PosIdx pos, std::string_view errorCtx);
    std::string_view forceString(
        Value & v,
        NixStringContext & context,
        const PosIdx pos,
        std::string_view errorCtx,
        const ExperimentalFeatureSettings & xpSettings = experimentalFeatureSettings);
    std::string_view forceStringNoCtx(Value & v, const PosIdx pos, std::string_view errorCtx);

    /**
     * Get attribute from an attribute set and throw an error if it doesn't exist.
     */
    const Attr * getAttr(Symbol attrSym, const Bindings * attrSet, std::string_view errorCtx);

    template<typename... Args>
    [[gnu::noinline]]
    void addErrorTrace(Error & e, const Args &... formatArgs) const;
    template<typename... Args>
    [[gnu::noinline]]
    void addErrorTrace(Error & e, const PosIdx pos, const Args &... formatArgs) const;

public:
    /**
     * @return true iff the value `v` denotes a derivation (i.e. a
     * set with attribute `type = "derivation"`).
     */
    bool isDerivation(Value & v);

    std::optional<std::string> tryAttrsToString(
        const PosIdx pos, Value & v, NixStringContext & context, bool coerceMore = false, bool copyToStore = true);

    enum class CopyLazyPaths : bool {
        PreserveLazy = false,
        Copy = true,
    };

    /**
     * For efficiency reasons, some store paths (as seen by the evaluator) in
     * the storeFS at their content-addressed locations don't get copied to the
     * store eagerly. This saves on needless I/O and possibly IPC if all the
     * evaluator does is just evaluate nix expressions from those locations.
     * This function copies such store objects to the store if they aren't already valid.
     */
    void ensureLazyPathCopied(const StorePath & path);

    /**
     * Ensure that all NixStringContextElem::Opaque context elements get fetched
     * to the store.
     */
    void ensureLazyPathsCopied(const NixStringContext & context);

    /**
     * String coercion.
     *
     * Converts strings, paths and derivations to a
     * string.  If `coerceMore` is set, also converts nulls, integers,
     * booleans and lists to a string.  If `copyToStore` is set,
     * referenced paths are copied to the Nix store as a side effect.
     */
    BackedStringView coerceToString(
        const PosIdx pos,
        Value & v,
        NixStringContext & context,
        std::string_view errorCtx,
        bool coerceMore = false,
        bool copyToStore = true,
        bool canonicalizePath = true);

    StorePath copyPathToStore(NixStringContext & context, const SourcePath & path);

private:
    /**
     * The store work behind `copyPathToStore`, done once per path per
     * evaluation; the public method keeps the answer in `srcToStore`.
     */
    StorePath copyPathToStoreUncached(const SourcePath & path);

public:

    /**
     * Path coercion.
     *
     * Converts strings, paths and derivations to a
     * path.  The result is guaranteed to be a canonicalised, absolute
     * path.  Nothing is copied to the store.
     */
    SourcePath coerceToPath(const PosIdx pos, Value & v, NixStringContext & context, std::string_view errorCtx);

    /**
     * Like coerceToPath, but the result must be a store path.
     */
    StorePath coerceToStorePath(const PosIdx pos, Value & v, NixStringContext & context, std::string_view errorCtx);

    /**
     * Part of `coerceToSingleDerivedPath()` without any store IO which is exposed for unit testing only.
     */
    std::pair<SingleDerivedPath, std::string_view> coerceToSingleDerivedPathUnchecked(
        const PosIdx pos,
        Value & v,
        std::string_view errorCtx,
        const ExperimentalFeatureSettings & xpSettings = experimentalFeatureSettings);

    /**
     * Coerce to `SingleDerivedPath`.
     *
     * Must be a string which is either a literal store path or a
     * "placeholder (see `DownstreamPlaceholder`).
     *
     * Even more importantly, the string context must be exactly one
     * element, which is either a `NixStringContextElem::Opaque` or
     * `NixStringContextElem::Built`. (`NixStringContextEleme::DrvDeep`
     * is not permitted).
     *
     * The string is parsed based on the context --- the context is the
     * source of truth, and ultimately tells us what we want, and then
     * we ensure the string corresponds to it.
     */
    SingleDerivedPath coerceToSingleDerivedPath(const PosIdx pos, Value & v, std::string_view errorCtx);

private:

    inline Value * lookupVar(Env * env, const ExprVar & var, bool noEval);

    friend struct ExprVar;
    friend struct ExprAttrs;
    friend struct ExprLet;

    /**
     * Current Nix call stack depth, used with `max-call-depth`
     * setting to throw stack overflow hopefully before we run out of
     * system stack.
     */
    [[gnu::tls_model("initial-exec")]] thread_local static size_t callDepth;

public:

    /**
     * Check that the call depth is within limits, and increment it, until the returned object is destroyed.
     */
    inline CallDepth addCallDepth(const PosIdx pos);

    /**
     * Do a deep equality test between two values.  That is, list
     * elements and attributes are compared recursively.
     */
    bool eqValues(Value & v1, Value & v2, const PosIdx pos, std::string_view errorCtx);

    /**
     * Like `eqValues`, but throws an `AssertionError` if not equal.
     *
     * WARNING:
     * Callers should call `eqValues` first and report if `assertEqValues` behaves
     * incorrectly. (e.g. if it doesn't throw if eqValues returns false or vice versa)
     */
    void assertEqValues(Value & v1, Value & v2, const PosIdx pos, std::string_view errorCtx);

    bool isFunctor(const Value & fun) const;

    void callFunction(Value & fun, std::span<Value *> args, Value & vRes, const PosIdx pos);

    void callFunction(Value & fun, Value & arg, Value & vRes, const PosIdx pos)
    {
        Value * args[] = {&arg};
        callFunction(fun, args, vRes, pos);
    }

    /**
     * Automatically call a function for which each argument has a
     * default value or has a binding in the `args` map.
     */
    void autoCallFunction(const Bindings & args, Value & fun, Value & res);

    BindingsBuilder buildBindings(size_t capacity)
    {
        return mem.buildBindings(symbols, capacity);
    }

    ListBuilder buildList(size_t size)
    {
        return mem.buildList(size);
    }

    /**
     * Return a boolean `Value *` without allocating.
     */
    Value * getBool(bool b);

    /**
     * Create a string representing a store path.
     *
     * The string is the printed store path with a context containing a
     * single `NixStringContextElem::Opaque` element of that store path.
     */
    void mkStorePathString(const StorePath & storePath, Value & v);

    /**
     * Create a string representing a `SingleDerivedPath::Built`.
     *
     * The string is the printed store path with a context containing a
     * single `NixStringContextElem::Built` element of the drv path and
     * output name.
     *
     * @param value Value we are settings
     *
     * @param b the drv whose output we are making a string for, and the
     * output
     *
     * @param optStaticOutputPath Optional output path for that string.
     * Must be passed if and only if output store object is
     * input-addressed or fixed output. Will be printed to form string
     * if passed, otherwise a placeholder will be used (see
     * `DownstreamPlaceholder`).
     *
     * @param xpSettings Stop-gap to avoid globals during unit tests.
     */
    void mkOutputString(
        Value & value,
        const SingleDerivedPath::Built & b,
        std::optional<StorePath> optStaticOutputPath,
        const ExperimentalFeatureSettings & xpSettings = experimentalFeatureSettings);

    /**
     * Create a string representing a `SingleDerivedPath`.
     *
     * A combination of `mkStorePathString` and `mkOutputString`.
     */
    void mkSingleDerivedPathString(const SingleDerivedPath & p, Value & v);

    void concatLists(Value & v, size_t nrLists, Value * const * lists, const PosIdx pos, std::string_view errorCtx);

    /**
     * Print statistics, if enabled.
     *
     * Performs a full memory GC before printing the statistics, so that the
     * GC statistics are more accurate.
     */
    void maybePrintStats();

    /**
     * Print statistics, unconditionally, cheaply, without performing a GC first.
     */
    void printStatistics();

    /**
     * Perform a full memory garbage collection - not incremental.
     *
     * @return true if Nix was built with GC and a GC was performed, false if not.
     *              The return value is currently not thread safe - just the return value.
     */
    bool fullGC();

    /**
     * Realise the given context
     * @param[in] context the context to realise
     * @param[out] maybePaths if not nullptr, all built or referenced store paths will be added to this set
     * @return a mapping from the placeholders used to construct the associated value to their final store path.
     */
    [[nodiscard]] StringMap
    realiseContext(const NixStringContext & context, StorePathSet * maybePaths = nullptr, bool isIFD = true);

    /**
     * The three phases of `realiseContext`, split so the rust-eval bridge can
     * run the middle one on a worker thread while the evaluation carries on
     * (ENG-13150). `realiseContext` composes them; nothing else should call
     * the pieces without a reason as good as that one.
     *
     * Phase 1 -- everything that touches this `EvalState` on the way in: the
     * `isValidPath` check on each element (recorded into `readSetTracker`,
     * which is single-threaded), the `allow-import-from-derivation` refusal
     * and the `trace-import-from-derivation` warning. Must run on the
     * evaluation thread. Returns the build requests, empty when there is
     * nothing to build.
     */
    [[nodiscard]] std::vector<DerivedPath::Built>
    realiseContextCheck(const NixStringContext & context, StorePathSet * maybePaths = nullptr, bool isIFD = true);

    /**
     * Phase 2 -- the build. Touches `store` and `buildStore` only, both of
     * which serve concurrent callers (the daemon protocol takes a connection
     * per call; `LocalStore` holds per-path file locks), and no other member
     * of this `EvalState`: no `readSetTracker`, no allow list, no symbol or
     * position table. That is the property that lets the rust-eval bridge
     * call it from a worker thread, and the property to preserve when
     * editing it. The outputs land in `outputsToAllow` for phase 3, which is
     * `allowClosure` on each of them -- run at answer-delivery time on the
     * evaluation thread, because the allow list is a plain set with no lock
     * and the evaluation thread reads it on every file access.
     */
    [[nodiscard]] StringMap realiseContextBuild(
        const std::vector<DerivedPath::Built> & drvs, StorePathSet * maybePaths, StorePathSet & outputsToAllow);

    /** Resolve and copy outputs after a successful build, without starting another worker. */
    [[nodiscard]] StringMap realiseContextOutputs(
        const std::vector<DerivedPath::Built> & drvs, StorePathSet * maybePaths, StorePathSet & outputsToAllow);

    /**
     * Coerce `v` to a path and realise it, i.e. build anything in the value's string context using `realiseContext()`.
     * @param copyLazyPaths When encountering a lazy path (i.e. a string with Opaque context that's also "mounted" on
     * the storeFS), fetch the store path to the store.
     */
    SourcePath realisePath(
        const PosIdx pos,
        Value & v,
        std::optional<SymlinkResolution> resolveSymlinks = SymlinkResolution::Full,
        CopyLazyPaths copyLazyPaths = CopyLazyPaths::PreserveLazy);

    /**
     * Realise the given string with context, and return the string with outputs instead of downstream output
     * placeholders.
     * @param[in] str the string to realise
     * @param[out] paths all referenced store paths will be added to this set
     * @return the realised string
     * @throw EvalError if the value is not a string, path or derivation (see `coerceToString`)
     */
    std::string
    realiseString(Value & str, StorePathSet * storePathsOutMaybe, bool isIFD = true, const PosIdx pos = noPos);

private:

    /**
     * Like `mkOutputString` but just creates a raw string, not an
     * string Value, which would also have a string context.
     */
    std::string mkOutputStringRaw(
        const SingleDerivedPath::Built & b,
        std::optional<StorePath> optStaticOutputPath,
        const ExperimentalFeatureSettings & xpSettings = experimentalFeatureSettings);

    /**
     * Like `mkSingleDerivedPathStringRaw` but just creates a raw string
     * Value, which would also have a string context.
     */
    std::string mkSingleDerivedPathStringRaw(const SingleDerivedPath & p);

    Counter nrLookups;
    Counter nrAvoided;
    Counter nrOpUpdates;
    Counter nrOpUpdateValuesCopied;
    Counter nrListConcats;
    Counter nrPrimOpCalls;
    Counter nrFunctionCalls;

public:
    Counter nrThunksAwaited;
    Counter nrThunksAwaitedSlow;
    Counter microsecondsWaiting;
    Counter currentlyWaiting;
    Counter maxWaiting;
    Counter nrSpuriousWakeups;

private:
    const bool countCalls;

    typedef boost::concurrent_flat_map<std::string, size_t, StringViewHash, std::equal_to<>> PrimOpCalls;
    const ref<PrimOpCalls> primOpCalls;

    typedef boost::concurrent_flat_map<ExprLambda *, size_t> FunctionCalls;
    const ref<FunctionCalls> functionCalls;

    /** Evaluation/call profiler. */
    MultiEvalProfiler profiler;

public:
    /**
     * Read-set instrumentation. Null unless `read-set-trace-file` is set.
     * Held by pointer rather than by value so that the common case costs one
     * null pointer in `EvalState` and nothing else. Public because the
     * boundaries that push tracked entries are primops.
     */
    std::unique_ptr<ReadSetTracker> readSetTracker;

    /**
     * The string literal values of every file parsed while a tracker was
     * active, per file. The parse cache outlives a request but a tracker
     * does not, so each new tracker replays these registrations; without
     * the replay a request that parses nothing (every file cached) leaves
     * a graph blind to literal value flows. Empty in ordinary evaluation.
     */
    std::vector<std::pair<SourcePath, std::vector<Value *>>> parsedStringLiterals;

private:

    void incrFunctionCall(ExprLambda * fun);

    typedef boost::concurrent_flat_map<PosIdx, size_t, std::hash<PosIdx>> AttrSelects;
    const ref<AttrSelects> attrSelects;

    friend struct ExprOpUpdate;
    friend struct ExprOpConcatLists;
    friend struct ExprVar;
    friend struct ExprString;
    friend struct ExprInt;
    friend struct ExprFloat;
    friend struct ExprPath;
    friend struct ExprSelect;

    friend struct Value;
    friend class ListBuilder;

public:

    /** Unregister value wait notifications before destroying host state. */
    std::unique_ptr<InterruptCallback> valueInterruptCallback;
};

/**
 * @return A string representing the type of the value `v`.
 *
 * @param withArticle Whether to begin with an english article, e.g. "an
 * integer" vs "integer".
 */
std::string_view showType(ValueType type, bool withArticle = true);
std::string showType(const Value & v);

/**
 * If `path` refers to a directory, then append "/default.nix".
 *
 * @param addDefaultNix Whether to append "/default.nix" after resolving symlinks.
 */
SourcePath resolveExprPath(SourcePath path, bool addDefaultNix = true);

/**
 * Whether a URI is allowed, assuming restrictEval is enabled
 */
bool isAllowedURI(std::string_view uri, const Strings & allowedPaths);

/**
 * Where `printStatistics` writes, overriding `NIX_SHOW_STATS_PATH`. Set by a
 * caller that wants the statistics in a specific file rather than wherever the
 * user's environment happens to point, such as the invocation recorder. The
 * environment variable is not usable for that, because it would also be
 * inherited by every child `nix` process.
 */
extern std::optional<std::filesystem::path> evalStatsPath;

} // namespace nix

#include "nix/expr/eval-inline.hh"
