#include "nix/cmd/command.hh"
#include "nix/cmd/rust-eval-session.hh"
#include "nix/expr/eval.hh"
#include "nix/util/environment-variables.hh"
#include "nix/store/globals.hh"

#include "run.hh"

using namespace nix;

struct CmdFormatter : NixMultiCommand
{
    CmdFormatter()
        : NixMultiCommand("formatter", RegisterCommand::getCommandsFor({"formatter"}))
    {
    }

    std::string description() override
    {
        return "build or run the formatter";
    }

    Category category() override
    {
        return catSecondary;
    }
};

static auto rCmdFormatter = registerCommand<CmdFormatter>("formatter");

/** Common implementation bits for the `nix formatter` subcommands. */
struct MixFormatter : SourceExprCommand
{
    Strings getDefaultFlakeAttrPaths() override
    {
        return Strings{"formatter." + settings.thisSystem.get()};
    }

    Strings getDefaultFlakeAttrPathPrefixes() override
    {
        return Strings{};
    }
};

struct CmdFormatterRun : MixFormatter, MixJSON
{
    std::vector<std::string> args;

    CmdFormatterRun()
    {
        expectArgs({.label = "args", .handler = {&args}});
    }

    std::string description() override
    {
        return "reformat your code in the standard style";
    }

    std::string doc() override
    {
        return
#include "formatter-run.md"
            ;
    }

    Category category() override
    {
        return catSecondary;
    }

    void run(ref<Store> store) override
    {
        auto source = rustSourceOf(*this);
        auto evalState = getEvalState();
        auto evaluand = rustEvaluandOf(*this, evalState, source, ".");
        if (!evaluand.flakeContext || !evaluand.flakeContext->sourcePath)
            throw Error("formatter run requires a local flake source directory");
        auto flakeDir = *evaluand.flakeContext->sourcePath;
        auto app = rustEvalApp(*evalState, evaluand).resolve(getEvalStore(), store);

        Strings programArgs{app.program.string()};

        // Propagate arguments from the CLI
        for (auto & i : args) {
            programArgs.push_back(i);
        }

        // Add the path to the flake as an environment variable. This enables formatters to format the entire flake even
        // if run from a subdirectory.
        StringMap env = getEnv();
        env["PRJ_ROOT"] = flakeDir.string();

        // exec does not run the evaluator's destructor.
        evalState->maybePrintStats();

        execProgramInStore(
            store,
            UseLookupPath::DontUse,
            app.program.string(),
            programArgs,
            std::nullopt, // Use default system
            env);
    };
};

static auto rFormatterRun = registerCommand2<CmdFormatterRun>({"formatter", "run"});

struct CmdFormatterBuild : MixFormatter, MixOutLinkByDefault
{
    CmdFormatterBuild() {}

    std::string description() override
    {
        return "build the current flake's formatter";
    }

    std::string doc() override
    {
        return
#include "formatter-build.md"
            ;
    }

    Category category() override
    {
        return catSecondary;
    }

    void run(ref<Store> store) override
    {
        auto source = rustSourceOf(*this);
        auto evalState = getEvalState();
        auto evalStore = getEvalStore();
        auto unresolvedApp = rustEvalApp(*evalState, rustEvaluandOf(*this, evalState, source, "."));
        auto app = unresolvedApp.resolve(evalStore, store);
        auto buildables = unresolvedApp.build(evalStore, store);
        createOutLinksMaybe(buildables, store);

        logger->cout("%s", app.program.string());
    };
};

static auto rFormatterBuild = registerCommand2<CmdFormatterBuild>({"formatter", "build"});

struct CmdFmt : CmdFormatterRun
{
    void run(ref<Store> store) override
    {
        CmdFormatterRun::run(store);
    }
};

static auto rFmt = registerCommand<CmdFmt>("fmt");
