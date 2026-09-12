#include "nix/cmd/command.hh"
#include "nix/main/shared.hh"
#include "nix/store/store-api.hh"
#include "optimise-store-paths.hh"

#include <atomic>

using namespace nix;

struct CmdOptimiseStore : StoreCommand
{
    std::vector<std::string> paths;

    CmdOptimiseStore()
    {
        expectArgs({.label = "paths", .optional = true, .handler = {&paths}, .completer = completePath});
    }

    std::string description() override
    {
        return "replace identical files in the store by hard links";
    }

    std::string doc() override
    {
        return
#include "optimise-store.md"
            ;
    }

    void run(ref<Store> store) override
    {
        if (paths.empty())
            store->optimiseStore();
        else
            optimiseStorePaths(store, paths);
    }
};

static auto rCmdOptimiseStore = registerCommand2<CmdOptimiseStore>({"store", "optimise"});
