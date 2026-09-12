#include "nix/cmd/installables.hh"
#include "nix/cmd/installable-derived-path.hh"
#include "nix/cmd/installable-value.hh"
#include "nix/store/store-api.hh"
#include "nix/expr/eval-inline.hh"
#include "nix/expr/eval-cache.hh"
#include "nix/store/names.hh"
#include "nix/cmd/command.hh"
#include "nix/store/derivations.hh"
#include "nix/store/downstream-placeholder.hh"

namespace nix {

/**
 * Return the rewrites that are needed to resolve a string whose context is
 * included in `dependencies`.
 */
StringPairs resolveRewrites(Store & store, const std::vector<BuiltPathWithResult> & dependencies)
{
    StringPairs res;
    if (!experimentalFeatureSettings.isEnabled(Xp::CaDerivations)) {
        return res;
    }
    for (auto & dep : dependencies) {
        auto drvDep = std::get_if<BuiltPathBuilt>(&dep.path);
        if (!drvDep) {
            continue;
        }

        for (const auto & [outputName, outputPath] : drvDep->outputs) {
            res.emplace(
                DownstreamPlaceholder::fromSingleDerivedPathBuilt(
                    SingleDerivedPath::Built{
                        .drvPath = make_ref<SingleDerivedPath>(drvDep->drvPath->discardOutputPath()),
                        .output = outputName,
                    })
                    .render(),
                store.printStorePath(outputPath));
        }
    }
    return res;
}

/**
 * Resolve the given string assuming the given context.
 */
std::string
resolveString(Store & store, const std::string & toResolve, const std::vector<BuiltPathWithResult> & dependencies)
{
    auto rewrites = resolveRewrites(store, dependencies);
    return rewriteStrings(toResolve, rewrites);
}

std::string_view expectedAppType(std::string_view firstAttr)
{
    return firstAttr == "apps" || firstAttr == "defaultApp" ? "app" : "derivation";
}

UnresolvedApp unresolvedAppOf(const AppFields & fields)
{
    return std::visit(
        overloaded{
            [](const AppFields::Program & program) {
                std::vector<DerivedPath> context;
                for (auto & c : program.context) {
                    context.emplace_back(
                        std::visit(
                            overloaded{
                                [&](const NixStringContextElem::DrvDeep & d) -> DerivedPath {
                                    /* We want all outputs of the drv */
                                    return DerivedPath::Built{
                                        .drvPath = makeConstantStorePathRef(d.drvPath),
                                        .outputs = OutputsSpec::All{},
                                    };
                                },
                                [&](const NixStringContextElem::Built & b) -> DerivedPath {
                                    return DerivedPath::Built{
                                        .drvPath = b.drvPath,
                                        .outputs = OutputsSpec::Names{b.output},
                                    };
                                },
                                [&](const NixStringContextElem::Opaque & o) -> DerivedPath {
                                    return DerivedPath::Opaque{
                                        .path = o.path,
                                    };
                                },
                            },
                            c.raw));
                }
                return UnresolvedApp{App{
                    .context = std::move(context),
                    .program = program.program,
                }};
            },
            [](const AppFields::Derivation & drv) {
                auto mainProgram = drv.mainProgram ? *drv.mainProgram : drv.pname ? *drv.pname : DrvName(drv.name).name;
                return UnresolvedApp{App{
                    .context = {DerivedPath::Built{
                        .drvPath = makeConstantStorePathRef(drv.drvPath),
                        .outputs = OutputsSpec::Names{drv.outputName},
                    }},
                    .program = drv.outPath + "/bin/" + mainProgram,
                }};
            },
        },
        fields.raw);
}

UnresolvedApp materialiseApp(EvalState & state, UnresolvedApp app)
{
    NixStringContext opaque;
    for (auto & path : app.unresolved.context)
        if (auto * o = std::get_if<DerivedPath::Opaque>(&path.raw()))
            opaque.insert(NixStringContextElem::Opaque{.path = o->path});
    state.ensureLazyPathsCopied(opaque);
    return app;
}

UnresolvedApp InstallableValue::toApp(EvalState & state)
{
    auto cursor = getCursor(state);
    auto attrPath = cursor->getAttrPath();

    auto type = cursor->getAttr("type")->getString();

    auto expectedType = expectedAppType(attrPath.empty() ? "" : std::string_view(state.symbols[attrPath[0]]));
    if (type != expectedType)
        throw Error("attribute '%s' should have type '%s'", cursor->getAttrPathStr(), expectedType);

    AppFields fields;
    if (type == "app") {
        auto [program, context] = cursor->getAttr("program")->getStringWithContext();
        fields.raw = AppFields::Program{.program = std::move(program), .context = std::move(context)};
    } else if (type == "derivation") {
        auto aPname = cursor->maybeGetAttr("pname");
        auto aMeta = cursor->maybeGetAttr(state.s.meta);
        auto aMainProgram = aMeta ? aMeta->maybeGetAttr("mainProgram") : nullptr;
        fields.raw = AppFields::Derivation{
            .drvPath = cursor->forceDerivation(),
            .outPath = cursor->getAttr(state.s.outPath)->getString(),
            .outputName = cursor->getAttr(state.s.outputName)->getString(),
            .name = cursor->getAttr(state.s.name)->getString(),
            .pname = aPname ? std::optional(aPname->getString()) : std::nullopt,
            .mainProgram = aMainProgram ? std::optional(aMainProgram->getString()) : std::nullopt,
        };
    } else
        throw Error("attribute '%s' has unsupported type '%s'", cursor->getAttrPathStr(), type);

    return materialiseApp(state, unresolvedAppOf(fields));
}

std::vector<BuiltPathWithResult> UnresolvedApp::build(ref<Store> evalStore, ref<Store> store)
{
    Installables installableContext;

    for (auto & ctxElt : unresolved.context)
        installableContext.push_back(make_ref<InstallableDerivedPath>(store, DerivedPath{ctxElt}));

    return Installable::build(evalStore, store, Realise::Outputs, installableContext);
}

App UnresolvedApp::resolve(ref<Store> evalStore, ref<Store> store)
{
    auto res = unresolved;

    auto builtContext = build(evalStore, store);
    res.program = resolveString(*store, unresolved.program.string(), builtContext);
    if (!store->isInStore(res.program.string()))
        throw Error("app program '%s' is not in the Nix store", res.program.string());

    return res;
}

} // namespace nix
