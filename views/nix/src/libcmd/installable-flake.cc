#include "nix/store/globals.hh"
#include "nix/cmd/installable-flake.hh"
#include "nix/cmd/installable-derived-path.hh"
#include "nix/store/outputs-spec.hh"
#include "nix/util/util.hh"
#include "nix/cmd/command.hh"
#include "nix/expr/attr-path.hh"
#include "nix/cmd/common-eval-args.hh"
#include "nix/store/derivations.hh"
#include "nix/expr/eval-inline.hh"
#include "nix/expr/eval.hh"
#include "nix/expr/get-drvs.hh"
#include "nix/store/store-api.hh"
#include "nix/main/shared.hh"
#include "nix/flake/flake.hh"
#include "nix/expr/eval-cache.hh"
#include "nix/util/url.hh"
#include "nix/fetchers/registry.hh"
#include "nix/store/build-result.hh"

#include <regex>
#include <queue>
#include <set>

#include <nlohmann/json.hpp>

namespace nix {

std::vector<std::string> InstallableFlake::getActualAttrPaths()
{
    std::vector<std::string> res;
    if (attrPaths.size() == 1 && attrPaths.front().starts_with(".")) {
        attrPaths.front().erase(0, 1);
        res.push_back(attrPaths.front());
        return res;
    }

    for (auto & prefix : prefixes)
        res.push_back(prefix + *attrPaths.begin());

    for (auto & s : attrPaths)
        res.push_back(s);

    return res;
}

static std::string showAttrPaths(const std::vector<std::string> & paths)
{
    std::string s;
    for (const auto & [n, i] : enumerate(paths)) {
        if (n > 0)
            s += n + 1 == paths.size() ? " or " : ", ";
        s += '\'';
        s += i;
        s += '\'';
    }
    return s;
}

InstallableFlake::InstallableFlake(
    SourceExprCommand * cmd,
    ref<EvalState> state,
    FlakeRef && flakeRef,
    std::string_view fragment,
    ExtendedOutputsSpec extendedOutputsSpec,
    Strings attrPaths,
    Strings prefixes,
    const flake::LockFlags & lockFlags)
    : InstallableValue(state)
    , flakeRef(flakeRef)
    , attrPaths(fragment == "" ? attrPaths : Strings{(std::string) fragment})
    , prefixes(fragment == "" ? Strings{} : prefixes)
    , extendedOutputsSpec(std::move(extendedOutputsSpec))
    , lockFlags(lockFlags)
{
    if (cmd && cmd->getAutoArgs(*state)->size())
        throw UsageError("'--arg' and '--argstr' are incompatible with flakes");
}

DerivedPathsWithInfo InstallableFlake::toDerivedPaths()
{
    Activity act(*logger, lvlTalkative, actUnknown, fmt("evaluating derivation '%s'", what()));

    auto attr = getCursor(*state);

    auto attrPath = attr->getAttrPathStr();

    if (!attr->isDerivation()) {

        // FIXME: use eval cache?
        auto v = attr->forceValue();

        if (std::optional derivedPathWithInfo = trySinglePathToDerivedPaths(
                v, noPos, fmt("while evaluating the flake output attribute '%s'", attrPath))) {
            return {*derivedPathWithInfo};
        } else {
            throw Error(
                "expected flake output attribute '%s' to be a derivation or path but found %s: %s",
                attrPath,
                showType(v),
                ValuePrinter(*this->state, v, errorPrintOptions));
        }
    }

    auto drvPath = attr->forceDerivation();

    std::optional<NixInt::Inner> priority;

    if (attr->maybeGetAttr(state->s.outputSpecified)) {
    } else if (auto aMeta = attr->maybeGetAttr(state->s.meta)) {
        if (auto aPriority = aMeta->maybeGetAttr("priority"))
            priority = aPriority->getInt().value;
    }

    return {{
        .path =
            DerivedPath::Built{
                .drvPath = makeConstantStorePathRef(std::move(drvPath)),
                .outputs = std::visit(
                    overloaded{
                        [&](const ExtendedOutputsSpec::Default & d) -> OutputsSpec {
                            StringSet outputsToInstall;
                            if (auto aOutputSpecified = attr->maybeGetAttr(state->s.outputSpecified)) {
                                if (aOutputSpecified->getBool()) {
                                    if (auto aOutputName = attr->maybeGetAttr("outputName"))
                                        outputsToInstall = {aOutputName->getString()};
                                }
                            } else if (auto aMeta = attr->maybeGetAttr(state->s.meta)) {
                                if (auto aOutputsToInstall = aMeta->maybeGetAttr("outputsToInstall"))
                                    for (auto & s : aOutputsToInstall->getListOfStrings())
                                        outputsToInstall.insert(s);
                            }

                            if (outputsToInstall.empty())
                                outputsToInstall.insert("out");

                            return OutputsSpec::Names{std::move(outputsToInstall)};
                        },
                        [&](const ExtendedOutputsSpec::Explicit & e) -> OutputsSpec { return e; },
                    },
                    extendedOutputsSpec.raw),
            },
        .info = make_ref<ExtraPathInfoFlake>(
            ExtraPathInfoValue::Value{
                .priority = priority,
                .attrPath = attrPath,
                .extendedOutputsSpec = extendedOutputsSpec,
            },
            ExtraPathInfoFlake::Flake{
                .originalRef = flakeRef,
                .lockedRef = getLockedFlake()->flake.lockedRef,
            }),
    }};
}

std::pair<Value *, PosIdx> InstallableFlake::toValue(EvalState & state)
{
    return {&getCursor(state)->forceValue(), noPos};
}

std::vector<ref<eval_cache::AttrCursor>> InstallableFlake::getCursors(EvalState & state)
{
    auto evalCache = openEvalCache(state, getLockedFlake());

    auto root = evalCache->getRoot();

    std::vector<ref<eval_cache::AttrCursor>> res;

    Suggestions suggestions;
    auto attrPaths = getActualAttrPaths();

    for (auto & attrPath : attrPaths) {
        debug("trying flake output attribute '%s'", attrPath);

        auto attr = root->findAlongAttrPath(AttrPath::parse(state, attrPath));
        if (attr) {
            res.push_back(ref(*attr));
        } else {
            suggestions += attr.getSuggestions();
        }
    }

    if (res.size() == 0)
        throw Error(suggestions, "flake '%s' does not provide attribute %s", flakeRef, showAttrPaths(attrPaths));

    return res;
}

ref<flake::LockedFlake> InstallableFlake::getLockedFlake() const
{
    if (!_lockedFlake) {
        flake::LockFlags lockFlagsApplyConfig = lockFlags;
        // FIXME why this side effect?
        lockFlagsApplyConfig.applyNixConfig = true;
        _lockedFlake = make_ref<flake::LockedFlake>(lockFlake(flakeSettings, *state, flakeRef, lockFlagsApplyConfig));
    }
    // _lockedFlake is now non-null but still just a shared_ptr
    return ref<flake::LockedFlake>(_lockedFlake);
}

/* The flake reference a locked node can be fetched by on its own.

   A relative-path input (`path:./nixpkgs`) has no flake reference of its
   own: its lock entry is the literal path plus the node it is relative to,
   and it is a directory of that node's tree. Fetched on its own the path
   resolves to nothing (`Input::isRelative`, "resolves only as a flake
   input"), so it is named the one way a directory of a tree can be named
   from outside: the tree's own reference plus `dir`. Nested relative inputs
   compose the same way up to the first node that has a reference of its
   own; the root flake is the last resort. */
static FlakeRef standaloneFlakeRef(
    const flake::LockedFlake & lockedFlake,
    flake::NodeId nodeId,
    std::set<flake::NodeId> & visited)
{
    const auto * lockedNode = lockedFlake.lockFile.node(nodeId);
    if (!lockedNode)
        return lockedFlake.flake.lockedRef;
    const auto & node = *lockedNode;
    /* A lock file is data: a `parent` chain that loops would loop here. */
    if (!visited.insert(nodeId).second)
        throw Error(
            "cycle in lock file: relative input '%s' is (transitively) its own parent",
            node.lockedRef.to_string());
    auto relative = node.lockedRef.input.isRelative();
    if (!relative)
        return node.lockedRef;
    auto parent = [&]() -> FlakeRef {
        if (node.parentInputAttrPath && !node.parentInputAttrPath->empty())
            if (auto parentNode = lockedFlake.lockFile.findInput(*node.parentInputAttrPath))
                return standaloneFlakeRef(lockedFlake, *parentNode, visited);
        return lockedFlake.flake.lockedRef;
    }();
    auto dir = CanonPath(relative->string(), CanonPath(parent.subdir));
    return FlakeRef(fetchers::Input(parent.input), dir.isRoot() ? "" : std::string(dir.rel()));
}

FlakeRef InstallableFlake::nixpkgsFlakeRef() const
{
    auto lockedFlake = getLockedFlake();

    if (auto nixpkgsInput = lockedFlake->lockFile.findInput({"nixpkgs"})) {
        if (auto lockedNode = lockedFlake->lockFile.node(*nixpkgsInput)) {
            if (lockedNode->isFlake) {
                std::set<flake::NodeId> visited;
                auto ref = standaloneFlakeRef(*lockedFlake, *nixpkgsInput, visited);
                debug("using nixpkgs flake '%s'", ref);
                return ref;
            }
        }
    }

    return defaultNixpkgsFlakeRef();
}

} // namespace nix
