#include "nix/util/current-process.hh"
#include "nix/cmd/rust-eval-session.hh"
#include "nix/main/shared.hh"
#include "nix/expr/eval.hh"
#include "nix/cmd/editor-for.hh"

#include <unistd.h>

using namespace nix;

struct CmdEdit : RawInstallableCommand
{
    std::string description() override
    {
        return "open the Nix expression of a Nix package in $EDITOR";
    }

    std::string doc() override
    {
        return
#include "edit.md"
            ;
    }

    Category category() override
    {
        return catSecondary;
    }

    void run(ref<Store> store) override
    {
        auto prefix = ExtendedOutputsSpec::parse(rawInstallable()).first;
        auto source = rustSourceOf(*this);
        auto state = getEvalState();
        auto evaluand = rustEvaluandOf(*this, state, source, prefix);
        auto position = rustEvalSourcePosition(*state, evaluand);
        CanonPath target(position.file);
        // The metadata question is settled before resolving the editor's host
        // target. Ambient files are opened by the editor, not read by evaluation.
        // Store targets retain lazy-input materialization and chroot mapping.
        auto file = state->store->isInStore(target.abs()) ? state->rootPath(target.abs())
                                                          : SourcePath{getFSSourceAccessor(), target};
        if (evaluand.flakeContext && evaluand.flakeContext->canEditCheckout && evaluand.flakeContext->sourcePath
            && !state->store->isInStore(evaluand.flakeContext->sourcePath->string())) {
            auto & context = *evaluand.flakeContext;
            CanonPath root(context.sourceStorePath);
            if (file.path.isWithin(root)) {
                // The answer names immutable source. Only the root input maps
                // to this invocation's checkout, after the memo is settled.
                auto local = CanonPath(context.sourcePath->string()) / file.path.removePrefix(root);
                file = SourcePath{getFSSourceAccessor(), std::move(local)};
            }
        }
        auto args = editorFor(*state, file, position.line);
        state->maybePrintStats();

        logger->stop();

        restoreProcessContext();

        execvp(args.front().c_str(), stringsToCharPtrs(args).data());

        std::string command;
        for (const auto & arg : args)
            command += " '" + arg + "'";
        throw SysError("cannot run command%s", command);
    }
};

static auto rCmdEdit = registerCommand<CmdEdit>("edit");
