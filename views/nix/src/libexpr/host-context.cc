#include "nix/expr/eval.hh"
#include "nix/expr/eval-inline.hh"
#include "nix/expr/eval-readset.hh"
#include "nix/expr/eval-settings.hh"
#include "nix/store/derivations.hh"
#include "nix/store/downstream-placeholder.hh"
#include "nix/store/globals.hh"
#include "nix/store/store-api.hh"
#include "nix/util/environment-variables.hh"
#include "nix/util/mounted-source-accessor.hh"
#include "nix/util/util.hh"

#include <chrono>

namespace nix {

std::string EvalState::realiseString(Value & s, StorePathSet * storePathsOutMaybe, bool isIFD, const PosIdx pos)
{
    nix::NixStringContext stringContext;
    auto rawStr = coerceToString(pos, s, stringContext, "while realising a string").toOwned();
    auto rewrites = realiseContext(stringContext, storePathsOutMaybe, isIFD);
    ensureLazyPathsCopied(stringContext);
    return nix::rewriteStrings(rawStr, rewrites);
}

std::vector<DerivedPath::Built>
EvalState::realiseContextCheck(const NixStringContext & context, StorePathSet * maybePathsOut, bool isIFD)
{
    std::vector<DerivedPath::Built> drvs;

    for (auto & c : context) {
        auto ensureValid = [&](const StorePath & p) {
            /* A store query is an input: the answer depends on what the store
               holds. It is a cheap one to track, because a store path is
               already content addressed. */
            if (readSetTracker) [[unlikely]]
                readSetTracker->recordStoreQuery(store->printStorePath(p));
            if (!store->isValidPath(p))
                error<InvalidPathError>(p).debugThrow();
        };
        std::visit(
            overloaded{
                [&](const NixStringContextElem::Built & b) {
                    drvs.push_back(
                        DerivedPath::Built{
                            .drvPath = b.drvPath,
                            .outputs = OutputsSpec::Names{b.output},
                        });
                    ensureValid(b.drvPath->getBaseStorePath());
                },
                [&](const NixStringContextElem::Opaque & o) {
                    /* If the path happens to be mounted on the storeFS, that means it's lazy path string and would get
                       copied to the store on-demand (when referenced in a derivation). The string is equal to final
                       store path where the store object would end up (the path is hashed before mounting). */
                    if (!storeFS->getMount(CanonPath(store->printStorePath(o.path))))
                        ensureValid(o.path);
                    if (maybePathsOut)
                        maybePathsOut->emplace(o.path);
                },
                [&](const NixStringContextElem::DrvDeep & d) {
                    /* Treat same as Opaque */
                    ensureValid(d.drvPath);
                    if (maybePathsOut)
                        maybePathsOut->emplace(d.drvPath);
                },
            },
            c.raw);
    }

    if (drvs.empty())
        return drvs;

    if (isIFD) {
        if (!settings.isImportFromDerivationAllowed())
            error<IFDError>(
                "cannot build '%1%' during evaluation because the option 'allow-import-from-derivation' is disabled",
                drvs.begin()->to_string(*store))
                .debugThrow();

        if (settings.traceImportFromDerivation)
            warn("built '%1%' during evaluation due to an import from derivation", drvs.begin()->to_string(*store));
    }

    return drvs;
}

StringMap EvalState::realiseContextBuild(
    const std::vector<DerivedPath::Built> & drvs, StorePathSet * maybePathsOut, StorePathSet & outputsToAllow)
{
    /* The build thread's own phases, beside the Rust side's `ixe slow:
       Realise work_ns=`: `slow.Realise_ns` is begin-to-collect wall and
       once read 17 already-built contexts as 130 ms of `buildPaths` each;
       skipping the build changed nothing (bed l3ba1), the time was here in
       `resolveDerivedPath` (l2cb1: 1.83 s of 1.83 s worked). */
    static const bool trace = getEnv("IXE_REPLAY_TRACE").has_value();
    using Clock = std::chrono::steady_clock;
    auto nsSince = [](Clock::time_point from) {
        return std::chrono::duration_cast<std::chrono::nanoseconds>(Clock::now() - from).count();
    };

    /* Build/substitute the context. */
    std::vector<DerivedPath> buildReqs;
    buildReqs.reserve(drvs.size());
    for (auto & d : drvs)
        buildReqs.emplace_back(DerivedPath{d});
    auto buildStarted = Clock::now();
    buildStore->buildPaths(buildReqs, bmNormal, store);
    auto buildNs = nsSince(buildStarted);
    auto resolveStarted = Clock::now();
    auto res = realiseContextOutputs(drvs, maybePathsOut, outputsToAllow);
    if (trace)
        std::cerr << "ixe realise-build: drvs=" << drvs.size() << " build_ns=" << buildNs
                  << " resolve_ns=" << nsSince(resolveStarted) << "\n";
    return res;
}

StringMap EvalState::realiseContextOutputs(
    const std::vector<DerivedPath::Built> & drvs, StorePathSet * maybePathsOut, StorePathSet & outputsToAllow)
{
    StringMap res;

    for (auto & drv : drvs) {
        auto outputs = resolveDerivedPath(*buildStore, drv, &*store);
        for (auto & [outputName, outputPath] : outputs) {
            outputsToAllow.insert(outputPath);
            if (maybePathsOut)
                maybePathsOut->emplace(outputPath);

            /* Get all the output paths corresponding to the placeholders we had */
            if (experimentalFeatureSettings.isEnabled(Xp::CaDerivations)) {
                res.insert_or_assign(
                    DownstreamPlaceholder::fromSingleDerivedPathBuilt(
                        SingleDerivedPath::Built{
                            .drvPath = drv.drvPath,
                            .output = outputName,
                        })
                        .render(),
                    buildStore->printStorePath(outputPath));
            }
        }
    }
    if (store != buildStore)
        copyClosure(*buildStore, *store, outputsToAllow);

    return res;
}

StringMap EvalState::realiseContext(const NixStringContext & context, StorePathSet * maybePathsOut, bool isIFD)
{
    auto drvs = realiseContextCheck(context, maybePathsOut, isIFD);

    if (drvs.empty())
        return {};

    StorePathSet outputsToCopyAndAllow;
    auto res = realiseContextBuild(drvs, maybePathsOut, outputsToCopyAndAllow);

    if (isIFD) {
        /* Allow access to the output closures of this derivation. */
        for (auto & outputPath : outputsToCopyAndAllow)
            allowClosure(outputPath);
    }

    return res;
}

SourcePath EvalState::realisePath(
    const PosIdx pos, Value & v, std::optional<SymlinkResolution> resolveSymlinks, CopyLazyPaths copyLazyPaths)
{
    NixStringContext context;

    auto path = coerceToPath(noPos, v, context, "while realising the context of a path");

    try {
        if (!context.empty() && path.accessor == rootFS) {
            auto rewrites = realiseContext(context);
            if (copyLazyPaths == CopyLazyPaths::Copy)
                ensureLazyPathsCopied(context);
            path = {path.accessor, CanonPath(rewriteStrings(path.path.abs(), rewrites))};
        }
        return resolveSymlinks ? path.resolveSymlinks(*resolveSymlinks) : path;
    } catch (Error & e) {
        e.addTrace(positions[pos], "while realising the context of path '%s'", path);
        throw;
    }
}

} // namespace nix
