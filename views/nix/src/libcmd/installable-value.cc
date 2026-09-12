#include "nix/cmd/installable-value.hh"
#include "nix/cmd/rust-eval-session.hh"
#include "nix/expr/rust-eval-refusal.hh"
#include "nix/expr/eval-cache.hh"
#include "nix/fetchers/fetch-to-store.hh"

namespace nix {

std::vector<ref<eval_cache::AttrCursor>> InstallableValue::getCursors(EvalState & state)
{
    auto evalCache =
        std::make_shared<nix::eval_cache::EvalCache>(std::nullopt, state, [&]() { return toValue(state).first; });
    return {evalCache->getRoot()};
}

ref<eval_cache::AttrCursor> InstallableValue::getCursor(EvalState & state)
{
    /* Although getCursors should return at least one element, in case it doesn't,
       bound check to avoid an undefined behavior for vector[0] */
    return getCursors(state).at(0);
}

[[noreturn]] static void nonValueInstallable(Installable & installable)
{
    // Value-inspection commands need a Rust value question of their own.
    if (dynamic_cast<InstallableRustDerivation *>(&installable))
        refuseWithAdvice(
            refusalTokens::unsupported,
            RefusingCommand::get(),
            "This command needs a value handle, but received derivations: "
            "value inspection is not implemented for this command.");
    throw UsageError("installable '%s' does not correspond to a Nix language value", installable.what());
}

InstallableValue & InstallableValue::require(Installable & installable)
{
    auto * castedInstallable = dynamic_cast<InstallableValue *>(&installable);
    if (!castedInstallable)
        nonValueInstallable(installable);
    return *castedInstallable;
}

ref<InstallableValue> InstallableValue::require(ref<Installable> installable)
{
    auto castedInstallable = installable.dynamic_pointer_cast<InstallableValue>();
    if (!castedInstallable)
        nonValueInstallable(*installable);
    return ref{castedInstallable};
}

std::optional<DerivedPathWithInfo>
InstallableValue::trySinglePathToDerivedPaths(Value & v, const PosIdx pos, std::string_view errorCtx)
{
    if (v.type() == nPath) {
        auto storePath = fetchToStore(state->fetchSettings, *state->store, v.path(), FetchMode::Copy);
        return {{
            .path =
                DerivedPath::Opaque{
                    .path = std::move(storePath),
                },
            .info = make_ref<ExtraPathInfo>(),
        }};
    }

    else if (v.type() == nString) {
        auto path = state->coerceToSingleDerivedPath(pos, v, errorCtx);
        if (auto o = std::get_if<SingleDerivedPath::Opaque>(&path.raw()))
            state->ensureLazyPathCopied(o->path);
        return {{
            .path = DerivedPath::fromSingle(path),
            .info = make_ref<ExtraPathInfo>(),
        }};
    }

    else
        return std::nullopt;
}

} // namespace nix
