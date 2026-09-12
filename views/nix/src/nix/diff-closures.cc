#include "nix/cmd/command.hh"
#include "nix/main/shared.hh"
#include "nix/store/store-api.hh"
#include "nix/main/common-args.hh"
using namespace nix;

struct CmdDiffClosures : SourceExprCommand, MixOperateOnOptions
{
    std::string _before, _after;

    CmdDiffClosures()
    {
        expectArg("before", &_before);
        expectArg("after", &_after);
    }

    std::string description() override
    {
        return "show what packages and versions were added and removed between two closures";
    }

    std::string doc() override
    {
        return
#include "diff-closures.md"
            ;
    }

    void run(ref<Store> store) override
    {
        auto before = parseInstallable(store, _before);
        auto beforePath = Installable::toStorePath(getEvalStore(), store, Realise::Outputs, operateOn, before);
        auto after = parseInstallable(store, _after);
        auto afterPath = Installable::toStorePath(getEvalStore(), store, Realise::Outputs, operateOn, after);
        printClosureDiff(store, beforePath, afterPath, "");
    }
};

static auto rCmdDiffClosures = registerCommand2<CmdDiffClosures>({"store", "diff-closures"});
