#include "nix/cmd/command.hh"
#include "nix/cmd/rust-command.hh"
#include "nix/main/common-args.hh"
#include "nix/main/shared.hh"
#include "nix/store/store-api.hh"

using namespace nix;

/// Argument registration and host access. Rust owns the command request,
/// selection, evaluation, rendering, and final output bytes.
struct CmdRustEval : MixJSON, RawInstallableCommand, MixReadOnlyOption
{
    bool raw = false;
    std::optional<std::string> apply;
    std::optional<std::filesystem::path> writeTo;

    CmdRustEval()
    {
        addFlag({
            .longName = "raw",
            .description = "Print strings without quotes or escaping.",
            .handler = {&raw, true},
        });
        addFlag({
            .longName = "apply",
            .description = "Apply the function *expr* to the selected value.",
            .labels = {"expr"},
            .handler = {&apply},
        });
        addFlag({
            .longName = "write-to",
            .description = "Write a string or attrset of strings to *path*.",
            .labels = {"path"},
            .handler = {&writeTo},
        });
    }

    std::string description() override
    {
        return "evaluate a Nix expression";
    }

    std::string doc() override
    {
        return
#include "eval.md"
            ;
    }

    Category category() override
    {
        return catSecondary;
    }

    void run(ref<Store>) override
    {
        RustEvalCommandOptions options{
            .raw = raw,
            .json = json,
            .pretty = outputPretty,
            .file = file.has_value(),
            .expr = expr.has_value(),
            .writeTo = writeTo.has_value(),
            .installable = rawInstallable(),
        };
        rustValidateEvalCommand(options);
        auto source = rustSourceOf(*this);
        auto state = getEvalState();
        auto evaluand = rustEvaluandOf(*this, state, source, options.installable);
        evaluand.apply = apply;
        auto output = rustEvalCommand(*state, evaluand, options);
        auto suspension = logger->suspend();
        writeFull(getStandardOutput(), output);
    }
};

static auto rCmdRustEval = registerCommand<CmdRustEval>("eval");
