#include "user-env.hh"
#include "nix/store/store-api.hh"
#include "nix/main/shared.hh"
#include "nix/expr/eval.hh"

namespace nix {

PackageInfos queryInstalled(EvalState & state, const std::filesystem::path & userEnv)
{
    PackageInfos elems;
    if (pathExists(userEnv / "manifest.json"))
        throw Error("profile %s is incompatible with 'nix-env'; please use 'nix profile' instead", PathFmt(userEnv));
    auto manifestFile = userEnv / "manifest.nix";
    if (pathExists(manifestFile)) {
        Value v;
        state.evalFile(state.rootPath(CanonPath(manifestFile.string())).resolveSymlinks(), v);
        Bindings & bindings = Bindings::emptyBindings;
        getDerivations(state, v, "", bindings, elems, false);
    }
    return elems;
}

bool createUserEnv(
    EvalState & state,
    PackageInfos & elems,
    const std::filesystem::path & profile,
    bool keepDerivations,
    const std::string & lockToken)
{
    state.requireBackendCanServe();
}

} // namespace nix
