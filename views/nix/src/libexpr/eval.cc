#include "nix/expr/eval.hh"
#include "nix/expr/eval-error.hh"
#include "nix/expr/eval-perf-census.hh"
#include "nix/expr/eval-readset.hh"
#include "nix/expr/eval-settings.hh"
#include "nix/expr/print-options.hh"
#include "nix/expr/symbol-table.hh"
#include "nix/expr/value.hh"
#include "nix/util/types.hh"
#include "nix/util/util.hh"
#include "nix/util/environment-variables.hh"
#include "nix/store/store-api.hh"
#include "nix/store/derivations.hh"
#include "nix/store/downstream-placeholder.hh"
#include "nix/expr/eval-inline.hh"
#include "nix/store/filetransfer.hh"
#include "nix/expr/function-trace.hh"
#include "nix/store/profiles.hh"
#include "nix/expr/print.hh"
#include "nix/fetchers/filtering-source-accessor.hh"
#include "nix/util/memory-source-accessor.hh"
#include "nix/util/mounted-source-accessor.hh"
#include "nix/expr/gc-small-vector.hh"
#include "nix/expr/rust-eval-refusal.hh"
#include "nix/util/url.hh"
#include "nix/fetchers/fetch-to-store.hh"
#include "nix/fetchers/tarball.hh"
#include "nix/fetchers/input-cache.hh"
#include "nix/util/current-process.hh"
#include "nix/util/experimental-features.hh"
#include "nix/expr/value-wait.hh"

#include <algorithm>
#include <cstddef>
#include <cstdlib>
#include <exception>
#include <iostream>
#include <sstream>
#include <cstring>
#include <optional>
#include <unistd.h>
#include <sys/time.h>
#include <fstream>
#include <functional>
#include <ranges>

#include <nlohmann/json.hpp>
#include <boost/container/small_vector.hpp>
#include <boost/unordered/concurrent_flat_map.hpp>
#include <boost/unordered/unordered_flat_set.hpp>

#ifndef _WIN32 // TODO use portable implementation
#  include <sys/resource.h>
#endif

#include "nix/util/strings-inline.hh"

using json = nlohmann::json;

namespace nix {

StringData & StringData::alloc(EvalMemory & mem, size_t size)
{
    void * t = mem.allocBytes(sizeof(StringData) + size + 1);
    if (!t)
        throw std::bad_alloc();
    auto res = new (t) StringData(size);
    return *res;
}

const StringData & StringData::make(EvalMemory & mem, std::string_view s)
{
    if (s.empty())
        return ""_sds;
    auto & res = alloc(mem, s.size());
    std::memcpy(&res.data_, s.data(), s.size());
    res.data_[s.size()] = '\0';
    return res;
}

RootValue allocRootValue(Value * v)
{
    return std::allocate_shared<Value *>(traceable_allocator<Value *>(), v);
}

// Pretty print types for assertion errors
std::ostream & operator<<(std::ostream & os, const ValueType t)
{
    os << showType(t);
    return os;
}

std::string printValue(EvalState & state, Value & v)
{
    std::ostringstream out;
    v.print(state, out);
    return out.str();
}

Value * Value::toPtr(SymbolStr str) noexcept
{
    return const_cast<Value *>(str.valuePtr());
}

void Value::print(EvalState & state, std::ostream & str, PrintOptions options)
{
    printValue(state, str, *this, options);
}

std::string_view showType(ValueType type, bool withArticle)
{
#define WA(a, w) withArticle ? a " " w : w
    switch (type) {
    case nInt:
        return WA("an", "integer");
    case nBool:
        return WA("a", "Boolean");
    case nString:
        return WA("a", "string");
    case nPath:
        return WA("a", "path");
    case nNull:
        return "null";
    case nAttrs:
        return WA("a", "set");
    case nList:
        return WA("a", "list");
    case nFunction:
        return WA("a", "function");
    case nExternal:
        return WA("an", "external value");
    case nFloat:
        return WA("a", "float");
    case nThunk:
        return WA("a", "thunk");
    case nFailed:
        return WA("an", "error");
    }
    unreachable();
}

std::string showType(const Value & v)
{
// Allow selecting a subset of enum values
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wswitch-enum"
    switch (v.getInternalType()) {
    case tString:
        return v.context() ? "a string with context" : "a string";
    case tPrimOp:
        return fmt("the built-in function '%s'", std::string(v.primOp()->name));
    case tPrimOpApp:
        return fmt("the partially applied built-in function '%s'", v.primOpAppPrimOp()->name);
    case tExternal:
        return v.external()->showType();
    case tThunk:
        return v.isBlackhole() ? "a black hole" : "a thunk";
    case tApp:
        return "a function application";
    default:
        return std::string(showType(v.type()));
    }
#pragma GCC diagnostic pop
}

PosIdx Value::determinePos(const PosIdx pos) const
{
// Allow selecting a subset of enum values
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wswitch-enum"
    switch (getInternalType()) {
    case tAttrs:
        return attrs()->pos;
    case tLambda:
        return lambda().fun->pos;
#if 0
    // FIXME: disabled because reading from an app is racy.
    case tApp:
        return app().left->determinePos(pos);
#endif
    default:
        return pos;
    }
#pragma GCC diagnostic pop
}

template<>
bool ValueStorage<sizeof(void *)>::isTrivial() const
{
    auto p1_ = p1; // must acquire before reading p0, since thunks can change
    auto p0_ = p0.load(std::memory_order_acquire);

    auto pd = static_cast<PrimaryDiscriminator>(p0_ & discriminatorMask);

    if (pd == pdThunk || pd == pdPending || pd == pdAwaited) {
        bool isApp = p1_ & discriminatorMask;
        if (isApp)
            return false;
        auto expr = untagPointer<Expr *>(p1_);
        return (dynamic_cast<ExprAttrs *>(expr) && ((ExprAttrs *) expr)->dynamicAttrs->empty())
               || dynamic_cast<ExprLambda *>(expr) || dynamic_cast<ExprList *>(expr);
    }

    else
        return true;
}

static Symbol getName(const AttrName & name, EvalState & state, Env & env)
{
    if (name.symbol) {
        return name.symbol;
    } else {
        Value nameValue;
        name.expr->eval(state, env, nameValue);
        state.forceStringNoCtx(nameValue, name.expr->getPos(), "while evaluating an attribute name");
        return state.symbols.create(nameValue.string_view());
    }
}

EvalMemory::EvalMemory()
{
    assertGCInitialized();
}

EvalState::EvalState(
    const LookupPath & lookupPathFromArguments,
    ref<Store> store,
    const fetchers::Settings & fetchSettings,
    const EvalSettings & settings,
    std::shared_ptr<Store> buildStore)
    : fetchSettings{fetchSettings}
    , settings{settings}
    , symbols(StaticEvalSymbols::staticSymbolTable())
    , repair(NoRepair)
    , storeFS(makeMountedSourceAccessor({
          {CanonPath::root, makeEmptySourceAccessor()},
          /* In the pure eval case, we can simply require
             valid paths. However, in the *impure* eval
             case this gets in the way of the union
             mechanism, because an invalid access in the
             upper layer will *not* be caught by the union
             source accessor, but instead abort the entire
             lookup.

             This happens when the store dir in the
             ambient file system has a path (e.g. because
             another Nix store there), but the relocated
             store does not.

             TODO make the various source accessors doing
             access control all throw the same type of
             exception, and make union source accessor
             catch it, so we don't need to do this hack.
           */
          {CanonPath(store->storeDir), store->getFSAccessor(settings.pureEval)},
      }))
    , rootFS([&] {
        /* In pure eval mode, we provide a filesystem that only
           contains the Nix store.

           Otherwise, use a union accessor to make the augmented store
           available at its logical location while still having the
           underlying directory available. This is necessary for
           instance if we're evaluating a file from the physical
           /nix/store while using a chroot store, and also for lazy
           mounted fetchTree. */
        auto accessor = settings.pureEval ? storeFS.cast<SourceAccessor>()
                                          : makeUnionSourceAccessor({getFSSourceAccessor(), storeFS});

        /* Apply access control if needed. */
        if (settings.restrictEval || settings.pureEval)
            accessor = AllowListSourceAccessor::create(
                accessor, {}, {}, [&settings](const CanonPath & path) -> RestrictedPathError {
                    auto modeInformation = settings.pureEval ? "in pure evaluation mode (use '--impure' to override)"
                                                             : "in restricted mode";
                    throw RestrictedPathError("access to absolute path '%1%' is forbidden %2%", path, modeInformation);
                });

        return accessor;
    }())
    , corepkgsFS(make_ref<MemorySourceAccessor>())
    , store(store)
    , buildStore(buildStore ? buildStore : store)
    , inputCache(fetchers::InputCache::create())
    , importResolutionCache(make_ref<decltype(importResolutionCache)::element_type>())
    , srcToStore(make_ref<decltype(srcToStore)::element_type>())
    , fileEvalCache(make_ref<decltype(fileEvalCache)::element_type>())
    , lookupPathResolved(make_ref<decltype(lookupPathResolved)::element_type>())
    , countCalls(getEnv("NIX_COUNT_CALLS").value_or("0") != "0")
    , primOpCalls(make_ref<decltype(primOpCalls)::element_type>())
    , functionCalls(make_ref<decltype(functionCalls)::element_type>())
    , attrSelects(make_ref<decltype(attrSelects)::element_type>())
    , valueInterruptCallback(registerValueInterruptCallback())
{
    corepkgsFS->setPathDisplay("<nix", ">");

    static_assert(sizeof(Env) <= 16, "environment must be <= 16 bytes");

    /* Construct the Nix expression search path. */
    assert(lookupPath.elements.empty());
    if (!settings.pureEval) {
        for (auto & i : lookupPathFromArguments.elements) {
            lookupPath.elements.emplace_back(LookupPath::Elem{i});
        }
        /* $NIX_PATH overriding regular settings is implemented as a hack in `initGC()` */
        for (auto & i : settings.nixPath.get()) {
            lookupPath.elements.emplace_back(LookupPath::Elem::parse(i));
        }
        if (!settings.restrictEval) {
            for (auto & i : EvalSettings::getDefaultNixPath()) {
                lookupPath.elements.emplace_back(LookupPath::Elem::parse(i));
            }
        }
    }

    /* Allow access to all paths in the search path. */
    if (rootFS.dynamic_pointer_cast<AllowListSourceAccessor>())
        for (auto & i : lookupPath.elements)
            resolveLookupPathPath(i.path, true);

    corepkgsFS->addFile(
        CanonPath("fetchurl.nix"),
#include "fetchurl.nix.gen.hh"
    );

    /* Register function call tracer. */
    if (settings.traceFunctionCalls)
        profiler.addProfiler(make_ref<FunctionCallTrace>());

    switch (settings.evalProfilerMode) {
    case EvalProfilerMode::flamegraph:
        profiler.addProfiler(
            makeSampleStackProfiler(*this, settings.evalProfileFile.get(), settings.evalProfilerFrequency));
        break;
    case EvalProfilerMode::disabled:
        break;
    }

    /* Register read-set instrumentation. Last, so that the trace's start
       time excludes the rest of this constructor. */
    if (!settings.readSetTraceFile.get().empty())
        readSetTracker = std::make_unique<ReadSetTracker>(
            *this, std::optional(std::filesystem::path(settings.readSetTraceFile.get())), settings.readSetHashContents);
}

EvalState::~EvalState() {}

void EvalState::allowPathLegacy(const std::string & path)
{
    if (auto rootFS2 = rootFS.dynamic_pointer_cast<AllowListSourceAccessor>())
        rootFS2->allowPrefix(CanonPath(path));
}

void EvalState::allowPath(const StorePath & storePath)
{
    if (auto rootFS2 = rootFS.dynamic_pointer_cast<AllowListSourceAccessor>())
        rootFS2->allowPrefix(CanonPath(store->printStorePath(storePath)));
}

void EvalState::allowClosure(const StorePath & storePath)
{
    if (!rootFS.dynamic_pointer_cast<AllowListSourceAccessor>())
        return;

    StorePathSet closure;
    store->computeFSClosure(storePath, closure);
    for (auto & p : closure)
        allowPath(p);
}

void EvalState::allowAndSetStorePathString(const StorePath & storePath, Value & v)
{
    allowPath(storePath);

    mkStorePathString(storePath, v);
}

inline static bool isJustSchemePrefix(std::string_view prefix)
{
    return !prefix.empty() && prefix[prefix.size() - 1] == ':'
           && isValidSchemeName(prefix.substr(0, prefix.size() - 1));
}

bool isAllowedURI(std::string_view uri, const Strings & allowedUris)
{
    /* 'uri' should be equal to a prefix, or in a subdirectory of a
       prefix. Thus, the prefix https://github.co does not permit
       access to https://github.com. */
    for (auto & prefix : allowedUris) {
        if (uri == prefix
            // Allow access to subdirectories of the prefix.
            || (uri.size() > prefix.size() && prefix.size() > 0 && hasPrefix(uri, prefix)
                && (
                    // Allow access to subdirectories of the prefix.
                    prefix[prefix.size() - 1] == '/'
                    || uri[prefix.size()] == '/'

                    // Allow access to whole schemes
                    || isJustSchemePrefix(prefix))))
            return true;
    }

    return false;
}

void EvalState::checkURI(const std::string & uri0)
{
    if (!settings.restrictEval)
        return;

    if (isAllowedURI(uri0, settings.allowedUris.get()))
        return;

    /* If the URI is a path, then check it against allowedPaths as
       well. */
    {
        std::filesystem::path path(uri0);
        if (path.is_absolute()) {
            if (auto rootFS2 = rootFS.dynamic_pointer_cast<AllowListSourceAccessor>())
                rootFS2->checkAccess(CanonPath(path.string()));
            return;
        }
    }

    try {
        ParsedURL uri = parseURL(uri0);
        if (uri.scheme == "file") {
            if (auto rootFS2 = rootFS.dynamic_pointer_cast<AllowListSourceAccessor>())
                rootFS2->checkAccess(CanonPath(urlPathToPath(uri.path).string()));
            return;
        }
    } catch (BadURL &) {
    }

    throw RestrictedPathError("access to URI '%s' is forbidden in restricted mode", uri0);
}

void PrimOp::check()
{
    if (arity > maxPrimOpArity) {
        throw Error("primop arity must not exceed %1%", maxPrimOpArity);
    }
}

std::ostream & operator<<(std::ostream & output, const PrimOp & primOp)
{
    output << "primop " << primOp.name;
    return output;
}

const PrimOp * Value::primOpAppPrimOp() const
{
    Value * left = primOpApp().left;
    while (left && !left->isPrimOp()) {
        left = left->primOpApp().left;
    }

    if (!left)
        return nullptr;

    assert(left->isPrimOp());
    return left->primOp();
}

void Value::mkPrimOp(PrimOp * p)
{
    p->check();
    setStorage(p);
}

template<typename... Args>
void EvalState::addErrorTrace(Error & e, const Args &... formatArgs) const
{
    e.addTrace(nullptr, HintFmt(formatArgs...));
}

template<typename... Args>
void EvalState::addErrorTrace(Error & e, const PosIdx pos, const Args &... formatArgs) const
{
    e.addTrace(positions[pos], HintFmt(formatArgs...));
}

void Value::mkString(std::string_view s, EvalMemory & mem)
{
    mkStringNoCopy(StringData::make(mem, s));
}

Value::StringWithContext::Context *
Value::StringWithContext::Context::fromBuilder(const NixStringContext & context, EvalMemory & mem)
{
    if (context.empty())
        return nullptr;

    auto ctx = new (mem.allocBytes(sizeof(Context) + context.size() * sizeof(value_type))) Context(context.size());
    std::ranges::transform(
        context, ctx->elems, [&](const NixStringContextElem & elt) { return &StringData::make(mem, elt.to_string()); });
    return ctx;
}

void Value::mkString(std::string_view s, const NixStringContext & context, EvalMemory & mem)
{
    mkStringNoCopy(StringData::make(mem, s), Value::StringWithContext::Context::fromBuilder(context, mem));
}

void Value::mkStringMove(const StringData & s, const NixStringContext & context, EvalMemory & mem)
{
    mkStringNoCopy(s, Value::StringWithContext::Context::fromBuilder(context, mem));
}

void Value::mkPath(const SourcePath & path, EvalMemory & mem)
{
    mkPath(&*path.accessor, StringData::make(mem, path.path.abs()));
}

[[gnu::always_inline]] inline Value * EvalState::lookupVar(Env * env, const ExprVar & var, bool noEval)
{
    for (auto l = var.level; l; --l, env = env->up)
        ;

    if (!var.fromWith)
        return env->values[var.displ];

    // This early exit defeats the `maybeThunk` optimization for variables from `with`,
    // The added complexity of handling this appears to be similarly in cost, or
    // the cases where applicable were insignificant in the first place.
    if (noEval)
        return nullptr;

    auto * fromWith = var.fromWith;
    while (1) {
        forceAttrs(*env->values[0], fromWith->pos, "while evaluating the first subexpression of a with expression");
        if (auto j = env->values[0]->attrs()->get(var.name)) {
            if (countCalls) [[unlikely]]
                attrSelects->try_emplace_or_visit(j->pos, 1, [](auto & i) { i.second++; });
            return j->value;
        }
        if (!fromWith->parentWith) [[unlikely]]
            error<UndefinedVarError>("undefined variable '%1%'", symbols[var.name]).atPos(var.pos).debugThrow();
        for (size_t l = fromWith->prevWith; l; --l, env = env->up)
            ;
        fromWith = fromWith->parentWith;
    }
}

ListBuilder::ListBuilder(EvalMemory & mem, size_t size)
    : size(size)
    , elems(size <= 2 ? inlineElems : (Value **) mem.allocBytes(size * sizeof(Value *)))
{
}

Value * EvalState::getBool(bool b)
{
    return b ? &Value::vTrue : &Value::vFalse;
}

static Counter nrThunks;

static inline void mkThunk(Value & v, Env & env, Expr * expr)
{
    v.mkThunk(&env, expr);
    nrThunks++;
}

void EvalState::mkStorePathString(const StorePath & p, Value & v)
{
    v.mkString(
        store->printStorePath(p),
        NixStringContext{
            NixStringContextElem::Opaque{.path = p},
        },
        mem);
}

std::string EvalState::mkOutputStringRaw(
    const SingleDerivedPath::Built & b,
    std::optional<StorePath> optStaticOutputPath,
    const ExperimentalFeatureSettings & xpSettings)
{
    /* In practice, this is testing for the case of CA derivations, or
       dynamic derivations. */
    return optStaticOutputPath ? store->printStorePath(std::move(*optStaticOutputPath))
                               /* Downstream we would substitute this for an actual path once
                                  we build the floating CA derivation */
                               : DownstreamPlaceholder::fromSingleDerivedPathBuilt(b, xpSettings).render();
}

void EvalState::mkOutputString(
    Value & value,
    const SingleDerivedPath::Built & b,
    std::optional<StorePath> optStaticOutputPath,
    const ExperimentalFeatureSettings & xpSettings)
{
    value.mkString(mkOutputStringRaw(b, optStaticOutputPath, xpSettings), NixStringContext{b}, mem);
}

std::string EvalState::mkSingleDerivedPathStringRaw(const SingleDerivedPath & p)
{
    return std::visit(
        overloaded{
            [&](const SingleDerivedPath::Opaque & o) { return store->printStorePath(o.path); },
            [&](const SingleDerivedPath::Built & b) {
                auto optStaticOutputPath = std::visit(
                    overloaded{
                        [&](const SingleDerivedPath::Opaque & o) {
                            auto drv = store->readDerivation(o.path);
                            auto i = drv.outputs.find(b.output);
                            if (i == drv.outputs.end())
                                throw Error(
                                    "derivation '%s' does not have output '%s'",
                                    b.drvPath->to_string(*store),
                                    b.output);
                            return i->second.path(*store, drv.name, b.output);
                        },
                        [&](const SingleDerivedPath::Built & o) -> std::optional<StorePath> { return std::nullopt; },
                    },
                    b.drvPath->raw());
                return mkOutputStringRaw(b, optStaticOutputPath);
            }},
        p.raw());
}

void EvalState::mkSingleDerivedPathString(const SingleDerivedPath & p, Value & v)
{
    v.mkString(
        mkSingleDerivedPathStringRaw(p),
        NixStringContext{
            std::visit([](auto && v) -> NixStringContextElem { return v; }, p),
        },
        mem);
}

Value * Expr::maybeThunk(EvalState & state, Env & env)
{
    Value * v = state.allocValue();
    mkThunk(*v, env, this);
    return v;
}

Value * ExprVar::maybeThunk(EvalState & state, Env & env)
{
    Value * v = state.lookupVar(&env, *this, true);
    /* The value might not be initialised in the environment yet.
       In that case, ignore it. */
    if (v) {
        state.nrAvoided++;
        return v;
    }
    return Expr::maybeThunk(state, env);
}

Value * ExprString::maybeThunk(EvalState & state, Env & env)
{
    state.nrAvoided++;
    return &v;
}

Value * ExprInt::maybeThunk(EvalState & state, Env & env)
{
    state.nrAvoided++;
    return &v;
}

Value * ExprFloat::maybeThunk(EvalState & state, Env & env)
{
    state.nrAvoided++;
    return &v;
}

Value * ExprPath::maybeThunk(EvalState & state, Env & env)
{
    state.nrAvoided++;
    return &v;
}

void EvalState::evalFile(const SourcePath &, Value &, bool)
{
    requireBackendCanServe();
}

void EvalState::resetFileCache()
{
    importResolutionCache->clear();
    srcToStore->clear();
    fileEvalCache->clear();
    inputCache->clear();
    positions.clear();
}

size_t EvalState::prepareForNextRequest()
{
    importResolutionCache->clear();
    srcToStore->clear();
    fileEvalCache->clear();
    lookupPathResolved->clear();
    return inputCache->evictUnlocked(fetchSettings);
}

void EvalState::eval(Expr *, Value &)
{
    requireBackendCanServe();
}

void EvalState::requireBackendCanServe()
{
    refuse(refusalTokens::unsupported, RefusingCommand::get());
}

void Expr::eval(EvalState & state, Env &, Value &)
{
    state.requireBackendCanServe();
}

Env * ExprAttrs::buildInheritFromEnv(EvalState & state, Env & up)
{
    Env & inheritEnv = state.mem.allocEnv(inheritFromExprs->size());
    inheritEnv.up = &up;

    Displacement displ = 0;
    for (auto from : *inheritFromExprs)
        inheritEnv.values[displ++] = from->maybeThunk(state, up);

    return &inheritEnv;
}

Value * ExprList::maybeThunk(EvalState & state, Env & env)
{
    if (elems.empty()) {
        return &Value::vEmptyList;
    }
    return Expr::maybeThunk(state, env);
}

Symbol ExprSelect::evalExceptFinalSelect(EvalState & state, Env & env, Value & attrs)
{
    Value vTmp;
    Symbol name = getName(attrPathStart[nAttrPath - 1], state, env);

    if (nAttrPath == 1) {
        e->eval(state, env, vTmp);
    } else {
        ExprSelect init(*this);
        init.nAttrPath--;
        init.eval(state, env, vTmp);
    }
    attrs = vTmp;
    return name;
}

[[gnu::tls_model("initial-exec")]] thread_local size_t EvalState::callDepth = 0;

void EvalState::callFunction(Value & fun, std::span<Value *> args, Value & vRes, const PosIdx pos)
{
    auto _level = addCallDepth(pos);

    auto neededHooks = profiler.getNeededHooks();
    if (neededHooks.test(EvalProfiler::preFunctionCall)) [[unlikely]]
        profiler.preFunctionCallHook(*this, fun, args, pos);

    Finally traceExit_{[&]() {
        if (profiler.getNeededHooks().test(EvalProfiler::postFunctionCall)) [[unlikely]]
            profiler.postFunctionCallHook(*this, fun, args, pos);
    }};

    forceValue(fun, pos);

    Value vCur = fun;

    auto makeAppChain = [&]() {
        for (auto arg : args) {
            auto fun2 = allocValue();
            *fun2 = vCur;
            vCur.reset();
            vCur.mkPrimOpApp(fun2, arg);
        }
        vRes = vCur;
    };

    const Attr * functor;

    while (args.size() > 0) {

        if (vCur.isLambda()) {

            ExprLambda & lambda(*vCur.lambda().fun);

            auto size = (!lambda.arg ? 0 : 1) + (lambda.getFormals() ? lambda.getFormals()->formals.size() : 0);
            Env & env2(mem.allocEnv(size));
            env2.up = vCur.lambda().env;

            Displacement displ = 0;

            if (auto formals = lambda.getFormals()) {
                try {
                    forceAttrs(*args[0], lambda.pos, "while evaluating the value passed for the lambda argument");
                } catch (Error & e) {
                    if (pos)
                        e.addTrace(positions[pos], "from call site");
                    throw;
                }

                if (lambda.arg)
                    env2.values[displ++] = args[0];

                /* For each formal argument, get the actual argument.  If
                   there is no matching actual argument but the formal
                   argument has a default, use the default. */
                size_t attrsUsed = 0;
                for (auto & i : formals->formals) {
                    auto j = args[0]->attrs()->get(i.name);
                    if (!j) {
                        if (!i.def) {
                            error<TypeError>(
                                "function '%1%' called without required argument '%2%'",
                                (lambda.name ? std::string(symbols[lambda.name]) : "anonymous lambda"),
                                symbols[i.name])
                                .atPos(lambda.pos)
                                .withTrace(pos, "from call site")
                                .debugThrow();
                        }
                        env2.values[displ++] = i.def->maybeThunk(*this, env2);
                    } else {
                        attrsUsed++;
                        env2.values[displ++] = j->value;
                    }
                }

                /* Check that each actual argument is listed as a formal
                   argument (unless the attribute match specifies a `...'). */
                if (!formals->ellipsis && attrsUsed != args[0]->attrs()->size()) {
                    /* Nope, so show the first unexpected argument to the
                       user. */
                    for (auto & i : *args[0]->attrs())
                        if (!formals->has(i.name)) {
                            StringSet formalNames;
                            for (auto & formal : formals->formals)
                                formalNames.insert(std::string(symbols[formal.name]));
                            auto suggestions = Suggestions::bestMatches(formalNames, symbols[i.name]);
                            error<TypeError>(
                                "function '%1%' called with unexpected argument '%2%'",
                                (lambda.name ? std::string(symbols[lambda.name]) : "anonymous lambda"),
                                symbols[i.name])
                                .atPos(lambda.pos)
                                .withTrace(pos, "from call site")
                                .withSuggestions(suggestions)
                                .debugThrow();
                        }
                    unreachable();
                }
            } else {
                env2.values[displ++] = args[0];
            }

            nrFunctionCalls++;
            if (countCalls)
                incrFunctionCall(&lambda);

            /* Evaluate the body. */
            try {
                lambda.body->eval(*this, env2, vCur);
            } catch (Error & e) {
                if (loggerSettings.showTrace.get()) {
                    addErrorTrace(
                        e,
                        lambda.pos,
                        "while calling %s",
                        lambda.name ? concatStrings("'", symbols[lambda.name], "'") : "anonymous lambda");
                    if (pos)
                        addErrorTrace(e, pos, "from call site");
                }
                throw;
            }

            args = args.subspan(1);
        }

        else if (vCur.isPrimOp()) {

            size_t argsLeft = vCur.primOp()->arity;

            if (args.size() < argsLeft) {
                /* We don't have enough arguments, so create a tPrimOpApp chain. */
                makeAppChain();
                return;
            } else {
                /* We have all the arguments, so call the primop. */
                auto * fn = vCur.primOp();

                nrPrimOpCalls++;
                if (countCalls)
                    primOpCalls->try_emplace_or_visit(fn->name, 1, [](auto & i) { i.second++; });

                try {
                    auto pos = vCur.determinePos(noPos);
                    vCur.reset();
                    fn->impl(*this, pos, args.data(), vCur);
                } catch (Error & e) {
                    if (fn->addTrace)
                        addErrorTrace(e, pos, "while calling the '%1%' builtin", fn->name);
                    throw;
                }

                args = args.subspan(argsLeft);
            }
        }

        else if (vCur.isPrimOpApp()) {
            /* Figure out the number of arguments still needed. */
            size_t argsDone = 0;
            Value * primOp = &vCur;
            while (primOp->isPrimOpApp()) {
                argsDone++;
                primOp = primOp->primOpApp().left;
            }
            assert(primOp->isPrimOp());
            auto arity = primOp->primOp()->arity;
            auto argsLeft = arity - argsDone;
            assert(argsLeft);

            if (args.size() < argsLeft) {
                /* We still don't have enough arguments, so extend the tPrimOpApp chain. */
                makeAppChain();
                return;
            } else {
                /* We have all the arguments, so call the primop with
                   the previous and new arguments. */

                Value * vArgs[maxPrimOpArity];
                auto n = argsDone;
                for (Value * arg = &vCur; arg->isPrimOpApp(); arg = arg->primOpApp().left)
                    vArgs[--n] = arg->primOpApp().right;

                for (size_t i = 0; i < argsLeft; ++i)
                    vArgs[argsDone + i] = args[i];

                auto fn = primOp->primOp();
                nrPrimOpCalls++;
                if (countCalls)
                    primOpCalls->try_emplace_or_visit(fn->name, 1, [](auto & i) { i.second++; });

                try {
                    // TODO:
                    // 1. Unify this and above code. Heavily redundant.
                    // 2. Create a fake env (arg1, arg2, etc.) and a fake expr (arg1: arg2: etc: builtins.name arg1 arg2
                    // etc)
                    //    so the debugger allows to inspect the wrong parameters passed to the builtin.
                    auto pos = vCur.determinePos(noPos);
                    vCur.reset();
                    fn->impl(*this, pos, vArgs, vCur);
                } catch (Error & e) {
                    if (fn->addTrace)
                        addErrorTrace(e, pos, "while calling the '%1%' builtin", fn->name);
                    throw;
                }

                args = args.subspan(argsLeft);
            }
        }

        else if (vCur.type() == nAttrs && (functor = vCur.attrs()->get(s.functor))) {
            /* 'vCur' may be allocated on the stack of the calling
               function, but for functors we may keep a reference, so
               heap-allocate a copy and use that instead. */
            Value * args2[] = {allocValue(), args[0]};
            *args2[0] = vCur;
            vCur.reset();
            try {
                callFunction(*functor->value, args2, vCur, functor->pos);
            } catch (Error & e) {
                e.addTrace(positions[pos], "while calling a functor (an attribute set with a '__functor' attribute)");
                throw;
            }
            args = args.subspan(1);
        }

        else
            error<TypeError>(
                "attempt to call something which is not a function but %1%: %2%",
                showType(vCur),
                ValuePrinter(*this, vCur, errorPrintOptions))
                .atPos(pos)
                .debugThrow();
    }

    vRes = vCur;
}

// Lifted out of callFunction() because it creates a temporary that
// prevents tail-call optimisation.
void EvalState::incrFunctionCall(ExprLambda * fun)
{
    functionCalls->try_emplace_or_visit(fun, 1, [](auto & i) { i.second++; });
}

void EvalState::autoCallFunction(const Bindings & args, Value & fun, Value & res)
{
    auto pos = fun.determinePos(noPos);

    forceValue(fun, pos);

    if (fun.type() == nAttrs) {
        auto found = fun.attrs()->get(s.functor);
        if (found) {
            Value * v = allocValue();
            callFunction(*found->value, fun, *v, pos);
            forceValue(*v, pos);
            return autoCallFunction(args, *v, res);
        }
    }

    if (!fun.isLambda() || !fun.lambda().fun->getFormals()) {
        res = fun;
        return;
    }
    auto formals = fun.lambda().fun->getFormals();

    auto attrs = buildBindings(std::max(static_cast<uint32_t>(formals->formals.size()), args.size()));

    if (formals->ellipsis) {
        // If the formals have an ellipsis (eg the function accepts extra args) pass
        // all available automatic arguments (which includes arguments specified on
        // the command line via --arg/--argstr)
        for (auto & v : args)
            attrs.insert(v);
    } else {
        // Otherwise, only pass the arguments that the function accepts
        for (auto & i : formals->formals) {
            auto j = args.get(i.name);
            if (j) {
                attrs.insert(*j);
            } else if (!i.def) {
                error<MissingArgumentError>(
                    R"(cannot evaluate a function that has an argument without a value ('%1%')
Nix attempted to evaluate a function as a top level expression; in
this case it must have its arguments supplied either by default
values, or passed explicitly with '--arg' or '--argstr'. See
https://nix.dev/manual/nix/stable/language/syntax.html#functions.)",
                    symbols[i.name])
                    .atPos(i.pos)
                    .debugThrow();
            }
        }
    }

    callFunction(fun, allocValue()->mkAttrs(attrs), res, pos);
}

void EvalState::concatLists(
    Value & v, size_t nrLists, Value * const * lists, const PosIdx pos, std::string_view errorCtx)
{
    nrListConcats++;

    Value * nonEmpty = 0;
    size_t len = 0;
    for (size_t n = 0; n < nrLists; ++n) {
        forceList(*lists[n], pos, errorCtx);
        auto l = lists[n]->listSize();
        len += l;
        if (l)
            nonEmpty = lists[n];
    }

    if (nonEmpty && len == nonEmpty->listSize()) {
        v = *nonEmpty;
        return;
    }

    auto list = buildList(len);
    auto out = list.elems;
    for (size_t n = 0, pos = 0; n < nrLists; ++n) {
        auto listView = lists[n]->listView();
        auto l = listView.size();
        if (l)
            memcpy(out + pos, listView.data(), l * sizeof(Value *));
        pos += l;
    }
    v.mkList(list);
}

void EvalState::tryFixupBlackHolePos(Value & v, PosIdx pos)
{
    if (!v.isBlackhole())
        return;
    auto e = std::current_exception();
    try {
        std::rethrow_exception(e);
    } catch (InfiniteRecursionError & e) {
        if (!e.hasPos())
            e.atPos(positions[pos]);
    } catch (...) {
    }
}

// Kept out of line for the same reason as tryFixupBlackHolePos: forceValue
// is always_inline and must stay small.
[[gnu::noinline]] void
EvalState::handleEvalException(Value & v, const PosIdx pos, Env * env, Expr * expr, Value * left, Value * right)
{
    tryFixupBlackHolePos(v, pos);

    auto ex = std::current_exception();

    /* A RecoverableEvalError must not be cached: the caller may fix
       whatever made it fail (e.g. create the resource a C API primop
       asked for) and force the value again. */
    Value * recovery = nullptr;
    try {
        std::rethrow_exception(ex);
    } catch (const RecoverableEvalError &) {
        if (expr) {
            recovery = allocValue();
            recovery->mkThunk(env, expr);
        } else if (left) {
            recovery = allocValue();
            recovery->mkApp(left, right);
        }
    } catch (...) {
    }

    v.mkFailed(ex, recovery);
}

[[gnu::noinline]] void EvalState::retryFailed(Value & v, const PosIdx pos)
{
    /* Force the recovery value before copying it: copying an unfinished
       value is forbidden (see ValueStorage::operator=). Two threads that
       retry the same failure both force the same recovery value, which is
       itself race-free, and then write the same finished words into `v`. */
    auto * recovery = v.failed()->recoveryValue;
    forceValue(*recovery, pos);
    v = *recovery;
}

void EvalState::forceValueDeep(Value & v)
{
    std::set<const Value *> seen;

    [&, &state(*this)](this const auto & recurse, Value & v) {
        auto _level = state.addCallDepth(v.determinePos(noPos));

        if (!seen.insert(&v).second)
            return;

        state.forceValue(v, v.determinePos(noPos));

        if (v.type() == nAttrs) {
            for (auto & i : *v.attrs())
                try {
                    recurse(*i.value);
                } catch (Error & e) {
                    state.addErrorTrace(e, i.pos, "while evaluating the attribute '%1%'", state.symbols[i.name]);
                    throw;
                }
        }

        else if (v.isList()) {
            size_t index = 0;
            for (auto v2 : v.listView())
                try {
                    recurse(*v2);
                    index++;
                } catch (Error & e) {
                    state.addErrorTrace(e, "while evaluating list element at index %1%", index);
                    throw;
                }
        }
    }(v);
}

NixInt EvalState::forceInt(Value & v, const PosIdx pos, std::string_view errorCtx)
{
    try {
        forceValue(v, pos);
        if (v.type() != nInt)
            error<TypeError>(
                "expected an integer but found %1%: %2%", showType(v), ValuePrinter(*this, v, errorPrintOptions))
                .atPos(pos)
                .debugThrow();
        return v.integer();
    } catch (Error & e) {
        e.addTrace(positions[pos], errorCtx);
        throw;
    }

    return v.integer();
}

NixFloat EvalState::forceFloat(Value & v, const PosIdx pos, std::string_view errorCtx)
{
    try {
        forceValue(v, pos);
        if (v.type() == nInt)
            return v.integer().value;
        else if (v.type() != nFloat)
            error<TypeError>(
                "expected a float but found %1%: %2%", showType(v), ValuePrinter(*this, v, errorPrintOptions))
                .atPos(pos)
                .debugThrow();
        return v.fpoint();
    } catch (Error & e) {
        e.addTrace(positions[pos], errorCtx);
        throw;
    }
}

bool EvalState::forceBool(Value & v, const PosIdx pos, std::string_view errorCtx)
{
    try {
        forceValue(v, pos);
        if (v.type() != nBool)
            error<TypeError>(
                "expected a Boolean but found %1%: %2%", showType(v), ValuePrinter(*this, v, errorPrintOptions))
                .atPos(pos)
                .debugThrow();
        return v.boolean();
    } catch (Error & e) {
        e.addTrace(positions[pos], errorCtx);
        throw;
    }

    return v.boolean();
}

const Attr * EvalState::getAttr(Symbol attrSym, const Bindings * attrSet, std::string_view errorCtx)
{
    auto value = attrSet->get(attrSym);
    if (!value) {
        error<TypeError>("attribute '%s' missing", symbols[attrSym]).withTrace(noPos, errorCtx).debugThrow();
    }
    return value;
}

bool EvalState::isFunctor(const Value & fun) const
{
    return fun.type() == nAttrs && fun.attrs()->get(s.functor);
}

void EvalState::forceFunction(Value & v, const PosIdx pos, std::string_view errorCtx)
{
    try {
        forceValue(v, pos);
        if (v.type() != nFunction && !isFunctor(v))
            error<TypeError>(
                "expected a function but found %1%: %2%", showType(v), ValuePrinter(*this, v, errorPrintOptions))
                .atPos(pos)
                .debugThrow();
    } catch (Error & e) {
        e.addTrace(positions[pos], errorCtx);
        throw;
    }
}

std::string_view EvalState::forceString(Value & v, const PosIdx pos, std::string_view errorCtx)
{
    try {
        forceValue(v, pos);
        if (v.type() != nString)
            error<TypeError>(
                "expected a string but found %1%: %2%", showType(v), ValuePrinter(*this, v, errorPrintOptions))
                .atPos(pos)
                .debugThrow();
        if (readSetTracker) [[unlikely]]
            readSetTracker->provConsume(v);
        return v.string_view();
    } catch (Error & e) {
        e.addTrace(positions[pos], errorCtx);
        throw;
    }
}

void copyContext(const Value & v, NixStringContext & context, const ExperimentalFeatureSettings & xpSettings)
{
    if (auto * ctx = v.context())
        for (auto * elem : *ctx)
            context.insert(NixStringContextElem::parse(elem->view(), xpSettings));
}

std::string_view EvalState::forceString(
    Value & v,
    NixStringContext & context,
    const PosIdx pos,
    std::string_view errorCtx,
    const ExperimentalFeatureSettings & xpSettings)
{
    auto s = forceString(v, pos, errorCtx);
    copyContext(v, context, xpSettings);
    return s;
}

std::string_view EvalState::forceStringNoCtx(Value & v, const PosIdx pos, std::string_view errorCtx)
{
    auto s = forceString(v, pos, errorCtx);
    if (v.context()) {
        auto ctxElem = NixStringContextElem::parse((*v.context()->begin())->view());
        error<EvalError>(
            "the string '%1%' is not allowed to refer to a store path (such as '%2%')",
            v.string_view(),
            ctxElem.display(*store))
            .withTrace(pos, errorCtx)
            .debugThrow();
    }
    return s;
}

bool EvalState::isDerivation(Value & v)
{
    if (v.type() != nAttrs)
        return false;
    auto i = v.attrs()->get(s.type);
    if (!i)
        return false;
    forceValue(*i->value, i->pos);
    if (i->value->type() != nString)
        return false;
    return i->value->string_view().compare("derivation") == 0;
}

std::optional<std::string>
EvalState::tryAttrsToString(const PosIdx pos, Value & v, NixStringContext & context, bool coerceMore, bool copyToStore)
{
    auto i = v.attrs()->get(s.toString);
    if (i) {
        Value v1;
        callFunction(*i->value, v, v1, pos);
        return coerceToString(
                   pos,
                   v1,
                   context,
                   "while evaluating the result of the `__toString` attribute",
                   coerceMore,
                   copyToStore)
            .toOwned();
    }

    return {};
}

BackedStringView EvalState::coerceToString(
    const PosIdx pos,
    Value & v,
    NixStringContext & context,
    std::string_view errorCtx,
    bool coerceMore,
    bool copyToStore,
    bool canonicalizePath)
{
    auto _level = addCallDepth(pos);

    forceValue(v, pos);

    if (v.type() == nString) {
        if (readSetTracker) [[unlikely]]
            readSetTracker->provConsume(v);
        copyContext(v, context);
        return v.string_view();
    }

    if (v.type() == nPath) {
        // FIXME: instead of copying the path to the store, we could
        // return a virtual store path that lazily copies the path to
        // the store in devirtualize().
        if (!canonicalizePath && !copyToStore) {
            // FIXME: hack to preserve path literals that end in a
            // slash, as in /foo/${x}.
            return v.pathStrView();
        } else if (copyToStore) {
            return store->printStorePath(copyPathToStore(context, v.path()));
        } else {
            return std::string(v.path().path.abs());
        }
    }

    if (v.type() == nAttrs) {
        auto maybeString = tryAttrsToString(pos, v, context, coerceMore, copyToStore);
        if (maybeString)
            return std::move(*maybeString);
        auto i = v.attrs()->get(s.outPath);
        if (!i) {
            error<TypeError>(
                "cannot coerce %1% to a string: %2%", showType(v), ValuePrinter(*this, v, errorPrintOptions))
                .withTrace(pos, errorCtx)
                .debugThrow();
        }
        return coerceToString(pos, *i->value, context, errorCtx, coerceMore, copyToStore, canonicalizePath);
    }

    if (v.type() == nExternal) {
        try {
            return v.external()->coerceToString(*this, pos, context, coerceMore, copyToStore);
        } catch (Error & e) {
            e.addTrace(nullptr, errorCtx);
            throw;
        }
    }

    if (coerceMore) {
        /* Note that `false' is represented as an empty string for
           shell scripting convenience, just like `null'. */
        if (v.type() == nBool && v.boolean())
            return "1";
        if (v.type() == nBool && !v.boolean())
            return "";
        if (v.type() == nInt)
            return std::to_string(v.integer().value);
        if (v.type() == nFloat)
            return std::to_string(v.fpoint());
        if (v.type() == nNull)
            return "";

        if (v.isList()) {
            std::string result;
            auto listView = v.listView();
            for (auto [n, v2] : enumerate(listView)) {
                try {
                    result += *coerceToString(
                        pos,
                        *v2,
                        context,
                        "while evaluating one element of the list",
                        coerceMore,
                        copyToStore,
                        canonicalizePath);
                } catch (Error & e) {
                    e.addTrace(positions[pos], errorCtx);
                    throw;
                }
                if (n < v.listSize() - 1
                    /* !!! not quite correct */
                    && (!v2->isList() || v2->listSize() != 0))
                    result += " ";
            }
            return result;
        }
    }

    error<TypeError>("cannot coerce %1% to a string: %2%", showType(v), ValuePrinter(*this, v, errorPrintOptions))
        .withTrace(pos, errorCtx)
        .debugThrow();
}

StorePath EvalState::copyPathToStore(NixStringContext & context, const SourcePath & path)
{
    if (nix::isDerivation(path.path.abs()))
        error<EvalError>("file names are not allowed to end in '%1%'", drvExtension).debugThrow();

    /* One copy per path per evaluation. The first call did the store work
       and the allow-list entry; a repeat owes only the context element. */
    if (auto cached = getConcurrent(*srcToStore, path)) {
        context.insert(NixStringContextElem::Opaque{.path = *cached});
        return *cached;
    }

    auto dstPath = copyPathToStoreUncached(path);
    srcToStore->emplace(path, dstPath);
    context.insert(NixStringContextElem::Opaque{.path = dstPath});
    return dstPath;
}

StorePath EvalState::copyPathToStoreUncached(const SourcePath & path)
{
    auto resolved = path.resolveSymlinks(SymlinkResolution::Ancestors);

    /* A path value naming the root of a mounted input (`./.` in a flake,
       `self.outPath` as a path) is that input's store object, already
       addressed: forcing the mount is the copy, and the store path is the
       one every other spelling of the input has (`ensureLazyPathCopied`,
       the road a build takes). Copying it again under the literal's base
       name would give one tree two store paths, `<h2>-<h1>-source` beside
       `<h1>-source`, the double-named path NixOS/nix#10627 records as
       unwanted -- and for a tree addressed by its jj id, a NAR walk of a
       tree whose id is already known, ingested under the wrong method. */
    if (auto storePath = store->maybeParseStorePath(resolved.path.abs()))
        if (storeFS->getMount(resolved.path)) {
            ensureLazyPathCopied(*storePath);
            allowPath(*storePath);
            return *storePath;
        }

    return addPathToStore(resolved, path.baseName(), ContentAddressMethod::Raw::NixArchive, nullptr, std::nullopt, {});
}

SourcePath EvalState::coerceToPath(const PosIdx pos, Value & v, NixStringContext & context, std::string_view errorCtx)
{
    try {
        forceValue(v, pos);
    } catch (Error & e) {
        e.addTrace(positions[pos], errorCtx);
        throw;
    }

    /* Handle path values directly, without coercing to a string. */
    if (v.type() == nPath)
        return v.path();

    /* Similarly, handle __toString where the result may be a path
       value. */
    if (v.type() == nAttrs) {
        auto i = v.attrs()->get(s.toString);
        if (i) {
            Value v1;
            callFunction(*i->value, v, v1, pos);
            return coerceToPath(pos, v1, context, errorCtx);
        }
    }

    /* Any other value should be coercible to a string, interpreted
       relative to the root filesystem. */
    auto path = coerceToString(pos, v, context, errorCtx, false, false, true).toOwned();
    if (path == "" || path[0] != '/')
        error<EvalError>("string '%1%' doesn't represent an absolute path", path).withTrace(pos, errorCtx).debugThrow();
    return rootPath(path);
}

StorePath
EvalState::coerceToStorePath(const PosIdx pos, Value & v, NixStringContext & context, std::string_view errorCtx)
{
    auto path = coerceToString(pos, v, context, errorCtx, false, false, true).toOwned();
    if (auto storePath = store->maybeParseStorePath(path))
        return *storePath;
    error<EvalError>("cannot coerce '%s' to a store path because it is not a subpath of the Nix store", path)
        .withTrace(pos, errorCtx)
        .debugThrow();
}

std::pair<SingleDerivedPath, std::string_view> EvalState::coerceToSingleDerivedPathUnchecked(
    const PosIdx pos, Value & v, std::string_view errorCtx, const ExperimentalFeatureSettings & xpSettings)
{
    NixStringContext context;
    auto s = forceString(v, context, pos, errorCtx, xpSettings);
    auto csize = context.size();
    if (csize != 1)
        error<EvalError>("string '%s' has %d entries in its context. It should only have exactly one entry", s, csize)
            .withTrace(pos, errorCtx)
            .debugThrow();
    auto derivedPath = std::visit(
        overloaded{
            [&](NixStringContextElem::Opaque && o) -> SingleDerivedPath { return std::move(o); },
            [&](NixStringContextElem::DrvDeep &&) -> SingleDerivedPath {
                error<EvalError>(
                    "string '%s' has a context which refers to a complete source and binary closure. This is not supported at this time",
                    s)
                    .withTrace(pos, errorCtx)
                    .debugThrow();
            },
            [&](NixStringContextElem::Built && b) -> SingleDerivedPath { return std::move(b); },
        },
        ((NixStringContextElem &&) *context.begin()).raw);
    return {
        std::move(derivedPath),
        std::move(s),
    };
}

SingleDerivedPath EvalState::coerceToSingleDerivedPath(const PosIdx pos, Value & v, std::string_view errorCtx)
{
    auto [derivedPath, s_] = coerceToSingleDerivedPathUnchecked(pos, v, errorCtx);
    auto s = s_;
    auto sExpected = mkSingleDerivedPathStringRaw(derivedPath);
    if (s != sExpected) {
        /* `std::visit` is used here just to provide a more precise
           error message. */
        std::visit(
            overloaded{
                [&](const SingleDerivedPath::Opaque & o) {
                    error<EvalError>("path string '%s' has context with the different path '%s'", s, sExpected)
                        .withTrace(pos, errorCtx)
                        .debugThrow();
                },
                [&](const SingleDerivedPath::Built & b) {
                    error<EvalError>(
                        "string '%s' has context with the output '%s' from derivation '%s', but the string is not the right placeholder for this derivation output. It should be '%s'",
                        s,
                        b.output,
                        b.drvPath->to_string(*store),
                        sExpected)
                        .withTrace(pos, errorCtx)
                        .debugThrow();
                }},
            derivedPath.raw());
    }
    return derivedPath;
}

// NOTE: This implementation must match eqValues!
// We accept this burden because informative error messages for
// `assert a == b; x` are critical for our users' testing UX.
void EvalState::assertEqValues(Value & v1, Value & v2, const PosIdx pos, std::string_view errorCtx)
{
    auto _level = addCallDepth(pos);

    // This implementation must match eqValues.
    forceValue(v1, pos);
    forceValue(v2, pos);

    if (&v1 == &v2)
        return;

    // Special case type-compatibility between float and int
    if ((v1.type() == nInt || v1.type() == nFloat) && (v2.type() == nInt || v2.type() == nFloat)) {
        if (eqValues(v1, v2, pos, errorCtx)) {
            return;
        } else {
            error<AssertionError>(
                "%s with value '%s' is not equal to %s with value '%s'",
                showType(v1),
                ValuePrinter(*this, v1, errorPrintOptions),
                showType(v2),
                ValuePrinter(*this, v2, errorPrintOptions))
                .debugThrow();
        }
    }

    if (v1.type() != v2.type()) {
        error<AssertionError>(
            "%s of value '%s' is not equal to %s of value '%s'",
            showType(v1),
            ValuePrinter(*this, v1, errorPrintOptions),
            showType(v2),
            ValuePrinter(*this, v2, errorPrintOptions))
            .debugThrow();
    }

    switch (v1.type()) {
    case nInt:
        if (v1.integer() != v2.integer()) {
            error<AssertionError>("integer '%d' is not equal to integer '%d'", v1.integer(), v2.integer()).debugThrow();
        }
        return;

    case nBool:
        if (v1.boolean() != v2.boolean()) {
            error<AssertionError>(
                "boolean '%s' is not equal to boolean '%s'",
                ValuePrinter(*this, v1, errorPrintOptions),
                ValuePrinter(*this, v2, errorPrintOptions))
                .debugThrow();
        }
        return;

    case nString:
        if (v1.string_view() != v2.string_view()) {
            error<AssertionError>(
                "string '%s' is not equal to string '%s'",
                ValuePrinter(*this, v1, errorPrintOptions),
                ValuePrinter(*this, v2, errorPrintOptions))
                .debugThrow();
        }
        return;

    case nPath:
        if (v1.pathAccessor() != v2.pathAccessor()) {
            error<AssertionError>(
                "path '%s' is not equal to path '%s' because their accessors are different",
                ValuePrinter(*this, v1, errorPrintOptions),
                ValuePrinter(*this, v2, errorPrintOptions))
                .debugThrow();
        }
        if (v1.pathStrView() != v2.pathStrView()) {
            error<AssertionError>(
                "path '%s' is not equal to path '%s'",
                ValuePrinter(*this, v1, errorPrintOptions),
                ValuePrinter(*this, v2, errorPrintOptions))
                .debugThrow();
        }
        return;

    case nNull:
        return;

    case nList:
        if (v1.listSize() != v2.listSize()) {
            error<AssertionError>(
                "list of size '%d' is not equal to list of size '%d', left hand side is '%s', right hand side is '%s'",
                v1.listSize(),
                v2.listSize(),
                ValuePrinter(*this, v1, errorPrintOptions),
                ValuePrinter(*this, v2, errorPrintOptions))
                .debugThrow();
        }
        for (size_t n = 0; n < v1.listSize(); ++n) {
            try {
                assertEqValues(*v1.listView()[n], *v2.listView()[n], pos, errorCtx);
            } catch (Error & e) {
                e.addTrace(positions[pos], "while comparing list element %d", n);
                throw;
            }
        }
        return;

    case nAttrs: {
        if (isDerivation(v1) && isDerivation(v2)) {
            auto i = v1.attrs()->get(s.outPath);
            auto j = v2.attrs()->get(s.outPath);
            if (i && j) {
                try {
                    assertEqValues(*i->value, *j->value, pos, errorCtx);
                    return;
                } catch (Error & e) {
                    e.addTrace(positions[pos], "while comparing a derivation by its '%s' attribute", "outPath");
                    throw;
                }
                assert(false);
            }
        }

        if (v1.attrs()->size() != v2.attrs()->size()) {
            error<AssertionError>(
                "attribute names of attribute set '%s' differs from attribute set '%s'",
                ValuePrinter(*this, v1, errorPrintOptions),
                ValuePrinter(*this, v2, errorPrintOptions))
                .debugThrow();
        }

        // Like normal comparison, we compare the attributes in non-deterministic Symbol index order.
        // This function is called when eqValues has found a difference, so to reliably
        // report about its result, we should follow in its literal footsteps and not
        // try anything fancy that could lead to an error.
        Bindings::const_iterator i, j;
        for (i = v1.attrs()->begin(), j = v2.attrs()->begin(); i != v1.attrs()->end(); ++i, ++j) {
            if (i->name != j->name) {
                // A difference in a sorted list means that one attribute is not contained in the other, but we don't
                // know which. Let's find out. Could use <, but this is more clear.
                if (!v2.attrs()->get(i->name)) {
                    error<AssertionError>(
                        "attribute name '%s' is contained in '%s', but not in '%s'",
                        symbols[i->name],
                        ValuePrinter(*this, v1, errorPrintOptions),
                        ValuePrinter(*this, v2, errorPrintOptions))
                        .debugThrow();
                }
                if (!v1.attrs()->get(j->name)) {
                    error<AssertionError>(
                        "attribute name '%s' is missing in '%s', but is contained in '%s'",
                        symbols[j->name],
                        ValuePrinter(*this, v1, errorPrintOptions),
                        ValuePrinter(*this, v2, errorPrintOptions))
                        .debugThrow();
                }
                assert(false);
            }
            try {
                assertEqValues(*i->value, *j->value, pos, errorCtx);
            } catch (Error & e) {
                // The order of traces is reversed, so this presents as
                //  where left hand side is
                //    at <pos>
                //  where right hand side is
                //    at <pos>
                //  while comparing attribute '<name>'
                if (j->pos != noPos)
                    e.addTrace(positions[j->pos], "where right hand side is");
                if (i->pos != noPos)
                    e.addTrace(positions[i->pos], "where left hand side is");
                e.addTrace(positions[pos], "while comparing attribute '%s'", symbols[i->name]);
                throw;
            }
        }
        return;
    }

    case nFunction:
        error<AssertionError>("distinct functions and immediate comparisons of identical functions compare as unequal")
            .debugThrow();

    case nExternal:
        if (!(*v1.external() == *v2.external())) {
            error<AssertionError>(
                "external value '%s' is not equal to external value '%s'",
                ValuePrinter(*this, v1, errorPrintOptions),
                ValuePrinter(*this, v2, errorPrintOptions))
                .debugThrow();
        }
        return;

    case nFloat:
        // !!!
        if (!(v1.fpoint() == v2.fpoint())) {
            error<AssertionError>("float '%f' is not equal to float '%f'", v1.fpoint(), v2.fpoint()).debugThrow();
        }
        return;

    // Cannot be returned by forceValue().
    case nThunk:
    case nFailed:
        unreachable();

    default: // Note that we pass compiler flags that should make `default:` unreachable.
        // Also note that this probably ran after `eqValues`, which implements
        // the same logic more efficiently (without having to unwind stacks),
        // so maybe `assertEqValues` and `eqValues` are out of sync. Check it for solutions.
        error<EvalError>("assertEqValues: cannot compare %1% with %2%", showType(v1), showType(v2))
            .withTrace(pos, errorCtx)
            .panic();
    }
}

// This implementation must match assertEqValues
bool EvalState::eqValues(Value & v1, Value & v2, const PosIdx pos, std::string_view errorCtx)
{
    auto _level = addCallDepth(pos);

    forceValue(v1, pos);
    forceValue(v2, pos);

    /* !!! Hack to support some old broken code that relies on pointer
       equality tests between sets.  (Specifically, builderDefs calls
       uniqList on a list of sets.)  Will remove this eventually. */
    if (&v1 == &v2)
        return true;

    // Special case type-compatibility between float and int
    if (v1.type() == nInt && v2.type() == nFloat)
        return v1.integer().value == v2.fpoint();
    if (v1.type() == nFloat && v2.type() == nInt)
        return v1.fpoint() == v2.integer().value;

    // All other types are not compatible with each other.
    if (v1.type() != v2.type())
        return false;

    switch (v1.type()) {
    case nInt:
        return v1.integer() == v2.integer();

    case nBool:
        return v1.boolean() == v2.boolean();

    case nString:
        return v1.string_view() == v2.string_view();

    case nPath:
        return
            // FIXME: compare accessors by their fingerprint.
            v1.pathAccessor() == v2.pathAccessor() && v1.pathStrView() == v2.pathStrView();

    case nNull:
        return true;

    case nList:
        if (v1.listSize() != v2.listSize())
            return false;
        for (size_t n = 0; n < v1.listSize(); ++n)
            if (!eqValues(*v1.listView()[n], *v2.listView()[n], pos, errorCtx))
                return false;
        return true;

    case nAttrs: {
        /* If both sets denote a derivation (type = "derivation"),
           then compare their outPaths. */
        if (isDerivation(v1) && isDerivation(v2)) {
            auto i = v1.attrs()->get(s.outPath);
            auto j = v2.attrs()->get(s.outPath);
            if (i && j)
                return eqValues(*i->value, *j->value, pos, errorCtx);
        }

        if (v1.attrs()->size() != v2.attrs()->size())
            return false;

        /* Otherwise, compare the attributes one by one. */
        Bindings::const_iterator i, j;
        for (i = v1.attrs()->begin(), j = v2.attrs()->begin(); i != v1.attrs()->end(); ++i, ++j)
            if (i->name != j->name || !eqValues(*i->value, *j->value, pos, errorCtx))
                return false;

        return true;
    }

    /* Functions are incomparable. */
    case nFunction:
        return false;

    case nExternal:
        return *v1.external() == *v2.external();

    case nFloat:
        // !!!
        return v1.fpoint() == v2.fpoint();

    // Cannot be returned by forceValue().
    case nThunk:
    case nFailed:
        unreachable();

    default: // Note that we pass compiler flags that should make `default:` unreachable.
        error<EvalError>("eqValues: cannot compare %1% with %2%", showType(v1), showType(v2))
            .withTrace(pos, errorCtx)
            .panic();
    }
}

bool EvalState::fullGC()
{
#if NIX_USE_BOEHMGC
    GC_gcollect();
    // Check that it ran. We might replace this with a version that uses more
    // of the boehm API to get this reliably, at a maintenance cost.
    // We use a 1K margin because technically this has a race condition, but we
    // probably won't encounter it in practice, because the CLI isn't concurrent
    // like that.
    return GC_get_bytes_since_gc() < 1024;
#else
    return false;
#endif
}

bool Counter::enabled = getEnv("NIX_SHOW_STATS").value_or("0") != "0";

std::optional<std::filesystem::path> evalStatsPath;

void EvalState::maybePrintStats()
{
    if (Counter::enabled && !statsPrinted.exchange(true)) {
        // Make the final heap size more deterministic.
#if NIX_USE_BOEHMGC
        if (!fullGC()) {
            warn("failed to perform a full GC before reporting stats");
        }
#endif
        printStatistics();
    }
}

/// Whether `s` is a string the JSON writer will accept.
///
/// Asked by round-tripping it through the writer rather than by a UTF-8
/// validator of our own, on purpose: the question is not "is this valid
/// UTF-8" in the abstract, it is "will `dump` throw on this", and a second
/// implementation of that predicate could disagree with the first. Only ever
/// called on the failure path, so the cost does not matter.
static bool serialisesAsJsonString(const std::string & s)
{
    try {
        (void) json(s).dump();
        return true;
    } catch (const json::exception &) {
        return false;
    }
}

/// Find invalid UTF-8 fields so statistics retain an explicit damage report.
static void collectUnserialisableStrings(const json & doc, const std::string & at, std::vector<std::string> & out)
{
    if (doc.is_string()) {
        if (!serialisesAsJsonString(doc.get<std::string>()))
            out.push_back(at.empty() ? "/" : at);
    } else if (doc.is_object()) {
        for (const auto & [key, value] : doc.items())
            collectUnserialisableStrings(value, at + "/" + key, out);
    } else if (doc.is_array()) {
        for (size_t i = 0; i < doc.size(); ++i)
            collectUnserialisableStrings(doc[i], at + "/" + std::to_string(i), out);
    }
}

/// Preserve numeric statistics even if a diagnostic contains invalid UTF-8.
static std::string renderStatistics(json & topObj)
{
    try {
        return topObj.dump(2);
    } catch (const json::exception & e) {
        std::vector<std::string> damaged;
        collectUnserialisableStrings(topObj, "", damaged);
        auto fields = json::array();
        for (const auto & f : damaged)
            fields.push_back(f);
        topObj["serialisationDamage"] = json::object({
            {"replaced", "invalid-utf8"},
            {"fields", fields},
            {"writerError", e.what()},
        });
        printError(
            "eval statistics: %d field(s) were not valid UTF-8 and have been written with replacement characters; "
            "every other field is intact. Fields: %s. Writer said: %s",
            damaged.size(),
            damaged.empty() ? "<none found on rescan>" : concatStringsSep(", ", damaged),
            e.what());
        /* `replace` substitutes U+FFFD for anything it cannot encode, so this
           call cannot throw for the reason the one above did. */
        return topObj.dump(2, ' ', false, json::error_handler_t::replace);
    }
}

void EvalState::printStatistics()
{
    std::chrono::microseconds cpuTimeDuration = getCpuUserTime();
    float cpuTime = std::chrono::duration_cast<std::chrono::duration<float>>(cpuTimeDuration).count();

    auto & memstats = mem.getStats();

    uint64_t bEnvs = memstats.nrEnvs * sizeof(Env) + memstats.nrValuesInEnvs * sizeof(Value *);
    uint64_t bLists = memstats.nrListElems * sizeof(Value *);
    uint64_t bValues = memstats.nrValues * sizeof(Value);
    uint64_t bAttrsets = memstats.nrAttrsets * sizeof(Bindings) + memstats.nrAttrsInAttrsets * sizeof(Attr);

#if NIX_USE_BOEHMGC
    GC_word heapSize, totalBytes;
    GC_get_heap_usage_safe(&heapSize, 0, 0, 0, &totalBytes);
    double gcFullOnlyTime = ({
        auto ms = GC_get_full_gc_total_time();
        ms * 0.001;
    });
    auto gcCycles = getGCCycles();
#endif

    auto outPath = evalStatsPath ? evalStatsPath->string() : getEnv("NIX_SHOW_STATS_PATH").value_or("-");
    std::fstream fs;
    if (outPath != "-")
        fs.open(outPath, std::fstream::out);
    json topObj = json::object();
    auto nRust = nrRustEvals.load();
    topObj["evaluator"] = nRust ? "rust" : "none";
    topObj["evaluatorCalls"] = {{"rust", nRust}};
    if (EvalPerfCensus::recorded()) {
        auto perf = json::object();
        for (const auto & [key, n] : EvalPerfCensus::snapshot())
            perf[key] = n;
        topObj["rustEvalPerf"] = perf;
    }

    {
        auto refusals = json::object();
        const auto & vocabulary = RefusalCensus::vocabulary();
        for (const auto & token : vocabulary)
            refusals[token] = 0;
        for (const auto & [token, n] : RefusalCensus::snapshot())
            refusals[token] = n;
        topObj["refusals"] = {
            {"total", RefusalCensus::total()},
            {"byToken", refusals},
            /* Says which of the two claims the rows above are making: every
               token at zero where none occurred, or only the tokens that did.
               A build without the Rust evaluator registers nothing, and
               reading its handful of rows as a complete census is how a
               refusal rate of zero gets reported by a binary that cannot
               refuse. */
            {"vocabulary", vocabulary.empty() ? "observed-only" : "complete"},
        };
    }

    topObj["cpuTime"] = cpuTime;
    topObj["time"] = {
        {"cpu", cpuTime},
#if NIX_USE_BOEHMGC
        {GC_is_incremental_mode() ? "gcNonIncremental" : "gc", gcFullOnlyTime},
        {GC_is_incremental_mode() ? "gcNonIncrementalFraction" : "gcFraction", gcFullOnlyTime / cpuTime},
#endif
    };
    topObj["envs"] = {
        {"number", memstats.nrEnvs.load()},
        {"elements", memstats.nrValuesInEnvs.load()},
        {"bytes", bEnvs},
    };
    topObj["nrExprs"] = Expr::nrExprs.load();
    topObj["list"] = {
        {"elements", memstats.nrListElems.load()},
        {"bytes", bLists},
        {"concats", nrListConcats.load()},
    };
    topObj["values"] = {
        {"number", memstats.nrValues.load()},
        {"bytes", bValues},
    };
    topObj["symbols"] = {
        {"number", symbols.size()},
        {"bytes", symbols.totalSize()},
    };
    topObj["sets"] = {
        {"number", memstats.nrAttrsets.load()},
        {"bytes", bAttrsets},
        {"elements", memstats.nrAttrsInAttrsets.load()},
    };
    topObj["sizes"] = {
        {"Env", sizeof(Env)},
        {"Value", sizeof(Value)},
        {"Bindings", sizeof(Bindings)},
        {"Attr", sizeof(Attr)},
    };
    topObj["nrOpUpdates"] = nrOpUpdates.load();
    topObj["nrOpUpdateValuesCopied"] = nrOpUpdateValuesCopied.load();
    topObj["nrThunks"] = nrThunks.load();
    topObj["nrThunksAwaited"] = nrThunksAwaited.load();
    topObj["nrThunksAwaitedSlow"] = nrThunksAwaitedSlow.load();
    topObj["nrSpuriousWakeups"] = nrSpuriousWakeups.load();
    topObj["maxWaiting"] = maxWaiting.load();
    topObj["waitingTime"] = microsecondsWaiting / (double) 1000000;
    topObj["nrAvoided"] = nrAvoided.load();
    topObj["nrLookups"] = nrLookups.load();
    topObj["nrPrimOpCalls"] = nrPrimOpCalls.load();
    topObj["nrFunctionCalls"] = nrFunctionCalls.load();
#if NIX_USE_BOEHMGC
    topObj["gc"] = {
        {"heapSize", heapSize},
        {"totalBytes", totalBytes},
        {"cycles", gcCycles},
    };
#endif

    if (countCalls) {
        {
            auto & obj = topObj["primops"];
            obj = json::object();
            primOpCalls->visit_all([&](auto & i) { obj[i.first] = i.second; });
        }
        {
            auto & list = topObj["functions"];
            list = json::array();
            functionCalls->visit_all([&](auto & i) {
                auto & [fun, count] = i;
                json obj = json::object();
                if (fun->name)
                    obj["name"] = (std::string_view) symbols[fun->name];
                else
                    obj["name"] = nullptr;
                if (auto pos = positions[fun->pos]) {
                    if (auto path = std::get_if<SourcePath>(&pos.origin))
                        obj["file"] = path->to_string();
                    obj["line"] = pos.line;
                    obj["column"] = pos.column;
                }
                obj["count"] = count;
                list.push_back(obj);
            });
        }
        {
            auto & list = topObj["attributes"];
            list = json::array();
            attrSelects->visit_all([&](auto & i) {
                json obj = json::object();
                if (auto pos = positions[i.first]) {
                    if (auto path = std::get_if<SourcePath>(&pos.origin))
                        obj["file"] = path->to_string();
                    obj["line"] = pos.line;
                    obj["column"] = pos.column;
                }
                obj["count"] = i.second;
                list.push_back(obj);
            });
        }
    }

    if (getEnv("NIX_SHOW_SYMBOLS").value_or("0") != "0") {
        auto list = json::array();
        symbols.dump([&](std::string_view s) { list.emplace_back(std::string(s)); });
        // XXX: overrides earlier assignment
        topObj["symbols"] = std::move(list);
    }
    /* Rendered before it is written, and rendered by something that cannot
       fail. The old shape was `fs << topObj.dump(2)`, with `fs` already open
       in truncating mode from further up: a throw out of `dump` therefore
       left a file that had been emptied and never refilled. */
    auto rendered = renderStatistics(topObj);
    if (outPath == "-") {
        std::cerr << rendered << std::endl;
    } else {
        fs << rendered << std::endl;
    }
}

SourcePath resolveExprPath(SourcePath path, bool addDefaultNix)
{
    unsigned int followCount = 0, maxFollow = 1024;

    /* If `path' is a symlink, follow it.  This is so that relative
       path references work. */
    while (!path.path.isRoot()) {
        // Basic cycle/depth limit to avoid infinite loops.
        if (++followCount >= maxFollow)
            throw Error("too many symbolic links encountered while traversing the path '%s'", path);
        auto p = path.parent().resolveSymlinks() / path.baseName();
        if (p.lstat().type != SourceAccessor::tSymlink)
            break;
        path = {path.accessor, CanonPath(p.readLink(), path.path.parent().value_or(CanonPath::root))};
    }

    /* If `path' refers to a directory, append `/default.nix'. */
    if (addDefaultNix && path.resolveSymlinks().lstat().type == SourceAccessor::tDirectory)
        return path / "default.nix";

    return path;
}

SourcePath EvalState::findFile(const std::string_view path)
{
    return findFile(lookupPath, path);
}

SourcePath EvalState::findFile(const LookupPath & lookupPath, const std::string_view path, const PosIdx pos)
{
    for (auto & i : lookupPath.elements) {
        auto suffixOpt = i.prefix.suffixIfPotentialMatch(path);

        if (!suffixOpt)
            continue;
        auto suffix = *suffixOpt;

        auto rOpt = resolveLookupPathPath(i.path);
        if (!rOpt)
            continue;
        auto r = *rOpt;

        auto res = (r / CanonPath(suffix)).resolveSymlinks();
        if (res.pathExists())
            return res;

        // Backward compatibility hack: throw an exception if access
        // to this path is not allowed.
        if (auto accessor = res.accessor.dynamic_pointer_cast<FilteringSourceAccessor>())
            accessor->checkAccess(res.path);
    }

    if (hasPrefix(path, "nix/"))
        return {corepkgsFS, CanonPath(path.substr(3))};

    error<ThrownError>(
        settings.pureEval ? "cannot look up '<%s>' in pure evaluation mode (use '--impure' to override)"
                          : "file '%s' was not found in the Nix search path (add it using $NIX_PATH or -I)",
        path)
        .atPos(pos)
        .debugThrow();
}

std::optional<SourcePath> EvalState::resolveLookupPathPath(const LookupPath::Path & value0, bool initAccessControl)
{
    auto & value = value0.s;
    if (auto cached = getConcurrent(*lookupPathResolved, value))
        return *cached;

    auto finish = [&](std::optional<SourcePath> res) {
        if (res)
            debug("resolved search path element '%s' to '%s'", value, *res);
        else
            debug("failed to resolve search path element '%s'", value);
        lookupPathResolved->emplace(std::string(value), res);
        return res;
    };

    if (EvalSettings::isPseudoUrl(value)) {
        try {
            auto accessor = fetchers::downloadTarball(*store, fetchSettings, EvalSettings::resolvePseudoUrl(value));
            auto storePath = fetchToStore(fetchSettings, *store, SourcePath(accessor), FetchMode::Copy);
            return finish(this->storePath(storePath));
        } catch (Error & e) {
            logWarning({.msg = HintFmt("Nix search path entry '%1%' cannot be downloaded, ignoring", value)});
        }
    }

    if (auto colPos = value.find(':'); colPos != value.npos) {
        auto scheme = value.substr(0, colPos);
        auto rest = value.substr(colPos + 1);
        if (auto * hook = get(settings.lookupPathHooks, scheme)) {
            auto res = (*hook)(*this, rest);
            if (res)
                return finish(std::move(*res));
        }
    }

    {
        auto path = rootPath(value);

        /* Allow access to paths in the search path. */
        if (initAccessControl) {
            allowPathLegacy(path.path.abs());
            if (store->isInStore(path.path.abs())) {
                try {
                    allowClosure(store->toStorePath(path.path.abs()).first);
                } catch (InvalidPath &) {
                }
            }
        }

        if (path.resolveSymlinks().pathExists())
            return finish(std::move(path));
        else {
            // Backward compatibility hack: throw an exception if access
            // to this path is not allowed.
            if (auto accessor = path.accessor.dynamic_pointer_cast<FilteringSourceAccessor>())
                accessor->checkAccess(path.path);

            logWarning({.msg = HintFmt("Nix search path entry '%1%' does not exist, ignoring", value)});
        }
    }

    return finish(std::nullopt);
}

std::string ExternalValueBase::coerceToString(
    EvalState & state, const PosIdx & pos, NixStringContext & context, bool copyMore, bool copyToStore) const
{
    state.error<TypeError>("cannot coerce %1% to a string: %2%", showType(), *this).atPos(pos).debugThrow();
}

bool ExternalValueBase::operator==(const ExternalValueBase & b) const noexcept
{
    return false;
}

std::ostream & operator<<(std::ostream & str, const ExternalValueBase & v)
{
    return v.print(str);
}

void forceNoNullByte(std::string_view s, std::function<Pos()> pos)
{
    if (s.find('\0') != s.npos) {
        using namespace std::string_view_literals;
        auto str = replaceStrings(std::string(s), "\0"sv, "␀"sv);
        Error error("input string '%s' cannot be represented as Nix string because it contains null bytes", str);
        if (pos) {
            error.atPos(pos());
        }
        throw error;
    }
}

} // namespace nix
