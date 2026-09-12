#include "nix/cmd/command.hh"
#include "nix/cmd/rust-persistent.hh"
#include "nix/main/common-args.hh"
#include "nix/main/shared.hh"

#include <iostream>

using namespace nix;

struct CmdRustEvalPersistent : SourceExprCommand, MixReadOnlyOption
{
    std::vector<std::string> installables;
    bool interactive = false;
    std::optional<std::filesystem::path> requestFile;
    uint64_t memoryCacheBytes = 512ULL * 1024 * 1024;

    CmdRustEvalPersistent()
    {
        expectArgs({
            .label = "installables",
            .handler = {&installables},
            .completer = getCompleteInstallable(),
        });
        addFlag({
            .longName = "memory-cache-size",
            .description =
                "Maximum accounted bytes of decoded dependency witnesses retained between requests (0 disables retention).",
            .labels = {"bytes"},
            .handler = {&memoryCacheBytes},
        });
        addFlag({
            .longName = "request-file",
            .description = "Evaluate a bounded versioned JSON request file with explicit request IDs and apply expressions.",
            .labels = {"path"},
            .handler = {&requestFile},
        });
        addFlag({
            .longName = "interactive",
            .description = "Read one installable per line from standard input, evaluating each as it arrives.",
            .handler = {&interactive, true},
        });
    }

    std::string description() override
    {
        return "evaluate requests with the Rust incremental cache in one process";
    }

    std::string doc() override
    {
        return
#include "eval-persistent.md"
            ;
    }

    Category category() override
    {
        return catSecondary;
    }

    void run(ref<Store>) override
    {
        if (requestFile && (interactive || !installables.empty()))
            throw UsageError("--request-file cannot be combined with positional requests or --interactive");
        if (requestFile) {
            // Parse every request before any source is resolved or evaluated.
            RustPersistentRequests requests(*requestFile);
            RustEvalCache cache(memoryCacheBytes);
            while (auto request = requests.next()) {
                std::string report;
                try {
                    report = rustEvalPersistentRequest(*this, request->installable, cache, request->apply);
                } catch (Error & error) {
                    auto failed = requests.complete(false, error.what());
                    auto suspension = logger->suspend();
                    writeFull(getStandardOutput(), failed);
                    throw;
                }
                auto framed = requests.complete(true, report);
                auto suspension = logger->suspend();
                writeFull(getStandardOutput(), framed);
            }
            return;
        }
        if (installables.empty() && !interactive)
            throw UsageError("provide an installable or use --interactive");
        RustEvalCache cache(memoryCacheBytes);
        auto evaluate = [&](const std::string & request) {
            auto report = rustEvalPersistentRequest(*this, request, cache);
            auto suspension = logger->suspend();
            writeFull(getStandardOutput(), report);
        };
        for (const auto & request : installables)
            evaluate(request);
        if (interactive) {
            std::string request;
            while (std::getline(std::cin, request))
                if (!request.empty())
                    evaluate(request);
            if (std::cin.bad())
                throw Error("cannot read persistent evaluation requests from standard input");
        }
    }
};

static auto rCmdRustEvalPersistent = registerCommand<CmdRustEvalPersistent>("eval-persistent");
