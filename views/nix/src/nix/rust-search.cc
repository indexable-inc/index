#include "nix/cmd/command.hh"
#include "nix/cmd/rust-search.hh"
#include "nix/main/shared.hh"
#include "nix/store/globals.hh"
#include "nix/store/store-api.hh"

using namespace nix;

struct CmdRustSearch : RawInstallableCommand, MixJSON
{
    std::vector<std::string> include;
    std::vector<std::string> exclude;
    bool defaultRoots = false;

    CmdRustSearch()
    {
        expectArgs("regex", &include);
        addFlag({
            .longName = "exclude",
            .shortName = 'e',
            .description = "Hide packages whose attribute path, name or description match *regex*.",
            .labels = {"regex"},
            .handler = {[this](std::string pattern) { exclude.push_back(std::move(pattern)); }},
        });
    }

    std::string description() override
    {
        return "search for packages";
    }

    std::string doc() override
    {
        return
#include "search.md"
            ;
    }

    Strings getDefaultFlakeAttrPaths() override
    {
        return {"packages." + settings.thisSystem.get(), "legacyPackages." + settings.thisSystem.get()};
    }

    Strings getDefaultFlakeAttrPathPrefixes() override
    {
        return defaultRoots ? Strings{} : SourceExprCommand::getDefaultFlakeAttrPathPrefixes();
    }

    void run(ref<Store>) override
    {
        RustSearchPlan plan(include, exclude, json);
        auto source = rustSourceOf(*this);
        auto state = getEvalState();
        if (!source) {
            auto [reference, fragment] =
                parseFlakeRefWithFragment(fetchSettings, rawInstallable(), absPath(getCommandBaseDir()));
            defaultRoots = fragment.empty();
        }
        auto evaluand = rustEvaluandOf(*this, state, source, rawInstallable());
        auto output = plan.render(rustEvalSearchCatalogue(*state, evaluand, defaultRoots));
        auto suspension = logger->suspend();
        writeFull(getStandardOutput(), output);
    }
};

static auto rCmdRustSearch = registerCommand<CmdRustSearch>("search");
