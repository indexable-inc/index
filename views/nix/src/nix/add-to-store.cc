#include "nix/cmd/command.hh"
#include "nix/main/common-args.hh"
#include "nix/store/store-api.hh"
#include "nix/store/local-fs-store.hh"
#include "nix/util/archive.hh"
#include "nix/util/git.hh"
#include "nix/util/posix-source-accessor.hh"
#include "nix/util/environment-variables.hh"
#include "nix/util/file-system.hh"
#include "nix/cmd/misc-store-flags.hh"

using namespace nix;

struct CmdAddToStore : MixDryRun, StoreCommand
{
    std::filesystem::path path;
    std::optional<std::string> namePart;
    std::optional<std::filesystem::path> outLink;
    ContentAddressMethod caMethod = ContentAddressMethod::Raw::NixArchive;
    HashAlgorithm hashAlgo = HashAlgorithm::SHA256;

    CmdAddToStore()
    {
        // FIXME: completion
        expectArg("path", &path);

        addFlag({
            .longName = "name",
            .shortName = 'n',
            .description = "Override the name component of the store path. It defaults to the base name of *path*.",
            .labels = {"name"},
            .handler = {&namePart},
        });

        addFlag({
            .longName = "out-link",
            .shortName = 'o',
            .description = "Create a symlink at *path* and register it as a garbage collector root before returning.",
            .labels = {"path"},
            .handler = {&outLink},
        });

        addFlag(flag::contentAddressMethod(&caMethod));

        addFlag(flag::hashAlgo(&hashAlgo));
    }

    void run(ref<Store> store) override
    {
        auto localStore = store.dynamic_pointer_cast<LocalFSStore>();
        if (outLink) {
            if (dryRun)
                throw UsageError("--out-link cannot be combined with --dry-run");
            if (outLink->empty())
                throw UsageError("--out-link requires a non-empty path");
            if (!localStore)
                throw UsageError("--out-link requires a store that supports local garbage collector roots");
        }

        if (!namePart)
            namePart = path.filename().string();

        auto sourcePath = PosixSourceAccessor::createAtRoot(makeParentCanonical(path));

        std::optional<Hash> expectedHash;
        if (outLink) {
            auto [expectedPath, hash] = store->computeStorePath(*namePart, sourcePath, caMethod, hashAlgo, {});
            // Root even an already-valid path before addToStoreSlow checks it.
            localStore->addTempRoot(expectedPath);
            expectedHash = hash;
        }

        auto storePath = dryRun ? store->computeStorePath(*namePart, sourcePath, caMethod, hashAlgo, {}).first
                                : store->addToStoreSlow(*namePart, sourcePath, caMethod, hashAlgo, {}, expectedHash).path;

        // Publish the permanent root before releasing the temporary root or
        // reporting success. The expected hash refuses mutation during ingestion.
        if (outLink) {
            // Functional GC handoff test: pause after ingestion, before the
            // permanent root exists. The temporary root must already protect it.
            if (auto sync = getEnv("_NIX_TEST_STORE_ADD_ROOT_SYNC"))
                readFile(*sync);
            localStore->addPermRoot(storePath, absPath(*outLink));
        }

        logger->cout("%s", store->printStorePath(storePath));
    }
};

struct CmdAdd : CmdAddToStore
{
    std::string description() override
    {
        return "Add a file or directory to the Nix store";
    }

    std::string doc() override
    {
        return
#include "add.md"
            ;
    }
};

struct CmdAddFile : CmdAddToStore
{
    CmdAddFile()
    {
        caMethod = ContentAddressMethod::Raw::Flat;
    }

    std::string description() override
    {
        return "Deprecated. Use [`nix store add --mode flat`](@docroot@/command-ref/new-cli/nix3-store-add.md) instead.";
    }
};

struct CmdAddPath : CmdAddToStore
{
    std::string description() override
    {
        return "Deprecated alias to [`nix store add`](@docroot@/command-ref/new-cli/nix3-store-add.md).";
    }
};

static auto rCmdAddFile = registerCommand2<CmdAddFile>({"store", "add-file"});
static auto rCmdAddPath = registerCommand2<CmdAddPath>({"store", "add-path"});
static auto rCmdAdd = registerCommand2<CmdAdd>({"store", "add"});
