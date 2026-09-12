#include "flake-command.hh"
#include "nix/fetchers/fetch-to-store.hh"
#include "nix/util/thread-pool.hh"
#include "nix/store/filetransfer.hh"
#include "nix/util/exit.hh"
#include "nix/expr/eval.hh"
#include "nix/store/store-api.hh"
#include "nix/util/mounted-source-accessor.hh"

using namespace nix;
using namespace nix::flake;

struct CmdFlakePrefetchInputs : FlakeCommand
{
    std::string description() override
    {
        return "fetch the inputs of a flake";
    }

    std::string doc() override
    {
        return
#include "flake-prefetch-inputs.md"
            ;
    }

    void run(nix::ref<nix::Store> store) override
    {
        auto flake = lockFlake();

        auto state = getEvalState();

        struct MountedInput
        {
            StorePath expected;
            ref<SourceAccessor> accessor;
        };

        // Resolve mounted accessors before starting workers. Relative inputs
        // have no standalone fetch URL; their mount already fixes their source
        // and store identity. The root must be materialized before its inputs.
        const auto mounted = [&] {
            std::map<NodeId, MountedInput> result;
            for (auto & [node, source] : flake.nodePaths) {
                auto expected = store->toStorePath(source.path.abs()).first;
                auto accessor = state->storeFS->getMount(CanonPath(store->printStorePath(expected)));
                if (!accessor)
                    throw Error("prefetch source '%s' has no mounted input", source);
                result.emplace(node, MountedInput{expected, ref(accessor)});
            }
            return result;
        }();
        auto schedule = flake.lockFile.prefetchSchedule();
        std::atomic<size_t> nrFailed{0};
        std::function<void(NodeId)> fetchNode;
        // Destroy the pool first so no worker outlives the state it captures.
        ThreadPool pool{fileTransferSettings.httpConnections};
        fetchNode = [&](NodeId node) {
            bool succeeded = false;
            try {
                auto locked = flake.lockFile.node(node);
                auto source = mounted.find(node);
                auto accessor = [&]() -> ref<SourceAccessor> {
                    if (source != mounted.end())
                        return source->second.accessor;
                    if (!locked || locked->lockedRef.input.isRelative())
                        throw Error("prefetch input %d is missing its mounted source", node.value);
                    return locked->lockedRef.input.getAccessor(fetchSettings, *store).first;
                }();
                auto name = source != mounted.end() ? std::string(source->second.expected.name())
                                                    : locked->lockedRef.input.getName();
                auto method =
                    accessor->knownTreeRoot ? ContentAddressMethod::Raw::JjTree : ContentAddressMethod::Raw::NixArchive;
                auto copied = fetchToStore(fetchSettings, *store, SourcePath{accessor}, FetchMode::Copy, name, method);
                if (source != mounted.end() && copied != source->second.expected)
                    throw Error(
                        "prefetch source identity changed: expected '%s', copied '%s'",
                        store->printStorePath(source->second.expected),
                        store->printStorePath(copied));
                succeeded = true;
            } catch (Error & error) {
                printError("%s", error.what());
                nrFailed++;
            }
            // Rust releases a shared child exactly once, after every direct
            // parent succeeds. Unrelated branches can continue concurrently.
            for (auto ready : schedule.complete(node, succeeded))
                pool.enqueue([&, ready] { fetchNode(ready); });
        };
        for (auto ready : schedule.ready())
            if (!pool.tryEnqueue([&, ready] { fetchNode(ready); }))
                break;
        pool.process();
        if (!nrFailed)
            schedule.checkComplete();

        throw Exit(nrFailed ? 1 : 0);
    }
};

static auto rCmdFlakePrefetchInputs = registerCommand2<CmdFlakePrefetchInputs>({"flake", "prefetch-inputs"});
