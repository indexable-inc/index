#include "nix/cmd/common-eval-args.hh"
#include "nix/expr/eval.hh"
#include "nix/util/fs-sink.hh"
#include "nix/util/memory-source-accessor.hh"

#include <optional>
#include <string>
#include <string_view>

using namespace nix;

namespace {

const std::string mountPoint = "/nix/store/00000000000000000000000000000000-pathroot-lookup";
const std::string incompleteMountPoint = mountPoint + "/nested";

ref<SourceAccessor> fixtureAccessor()
{
    static auto accessor = [] {
        auto files = make_ref<MemorySourceAccessor>();
        MemorySink sink{*files};
        sink.createDirectory(CanonPath::root);
        sink.createRegularFile(CanonPath("/main.nix"), [](CreateRegularFileSink & file) {
            file("import ./dep.nix");
        });
        sink.createRegularFile(CanonPath("/dep.nix"), [](CreateRegularFileSink & file) { file("42"); });
        return files;
    }();
    return accessor;
}

struct RegisterLookupHook
{
    RegisterLookupHook()
    {
        evalSettings.lookupPathHooks.emplace(
            "pathroot-test",
            [](EvalState & state, std::string_view) -> std::optional<SourcePath> {
                auto accessor = fixtureAccessor();
                state.storeFS->mount(CanonPath(mountPoint), accessor);
                return SourcePath{accessor, CanonPath::root};
            });
        evalSettings.lookupPathHooks.emplace(
            "pathroot-incomplete",
            [](EvalState & state, std::string_view) -> std::optional<SourcePath> {
                auto accessor = fixtureAccessor();
                state.storeFS->mount(CanonPath(incompleteMountPoint), accessor);
                return SourcePath{accessor, CanonPath::root};
            });
    }
};

RegisterLookupHook registerLookupHook;

} // namespace
