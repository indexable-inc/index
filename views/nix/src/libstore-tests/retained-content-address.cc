#include <gtest/gtest.h>

#include "nix/store/local-store.hh"
#include "nix/store/build/worker.hh"
#include "nix/store/derivations.hh"
#include "nix/store/globals.hh"
#include "nix/store/store-open.hh"
#include "nix/util/file-system.hh"
#include "nix/util/archive.hh"
#include "nix/util/posix-source-accessor.hh"

#ifndef _WIN32

#  include "nix/store/build/worker.hh"
#  include "nix/store/globals.hh"
#  include "nix/store/store-open.hh"
#  include "nix/util/file-system.hh"
#  include "nix/util/posix-source-accessor.hh"

namespace nix {

class RetainedContentAddressTest : public ::testing::Test
{
protected:
    std::filesystem::path root;
    std::shared_ptr<LocalStore> store;

    void SetUp() override
    {
        experimentalFeatureSettings.set("extra-experimental-features", "ca-derivations");
        root = createTempDir();
        store = openStore(
                    "local",
                    {{"store", (root / "store").string()},
                     {"state", (root / "state").string()},
                     {"log", (root / "log").string()}})
                    .dynamic_pointer_cast<LocalStore>();
        ASSERT_TRUE(store);
    }

    void TearDown() override
    {
        store.reset();
        std::filesystem::remove_all(root);
    }

    StorePath record(const std::string & contents, bool invalidAddress, bool dangling = false)
    {
        auto temporary = root / "payload";
        if (dangling)
            std::filesystem::create_symlink(contents, temporary);
        else
            writeFile(temporary, contents);
        auto nar =
            hashPath(makeFSSourceAccessor(temporary), FileIngestionMethod::NixArchive, HashAlgorithm::SHA256).first;
        auto address = invalidAddress ? hashString(HashAlgorithm::SHA256, "historical producer identity") : nar;
        auto path = store->makeFixedOutputPath(
            "retained-output", {.method = FileIngestionMethod::NixArchive, .hash = address, .references = {}});
        std::filesystem::rename(temporary, store->toRealPath(path));
        ValidPathInfo info{path, UnkeyedValidPathInfo(*store, nar)};
        info.ca = ContentAddress{.method = ContentAddressMethod::Raw::NixArchive, .hash = address};
        // A former producer admitted its own output with an intact NAR but an
        // incorrect CA. Import through the corrected reader would reject it.
        store->registerValidPaths({{path, info}});
        return path;
    }

    DrvOutput identity()
    {
        return {.drvHash = hashString(HashAlgorithm::SHA256, "retained derivation"), .outputName = "out"};
    }

    void registerOutput(const StorePath & path)
    {
        store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = path}, identity()});
    }

    void proveBuildRepair(BuildMode mode)
    {
        Derivation drv;
        drv.name = "retained-output";
        drv.platform = settings.thisSystem.get();
        drv.builder = "/bin/sh";
        drv.args = {"-c", "printf 'same payload' > \"$out\""};
        drv.env = {{"name", drv.name}, {"out", hashPlaceholder("out")}, {"system", drv.platform}};
        drv.outputs.emplace(
            "out",
            DerivationOutput{DerivationOutput::CAFloating{
                .method = ContentAddressMethod::Raw::NixArchive, .hashAlgo = HashAlgorithm::SHA256}});
        auto drvPath = store->writeDerivation(drv);
        DrvOutput outputId{.drvHash = staticOutputHashes(*store, drv).at("out"), .outputName = "out"};
        auto invalid = record("same payload", true);
        store->registerDrvOutput(Realisation{UnkeyedRealisation{.outPath = invalid}, outputId});
        store->buildPaths(
            {DerivedPath::Built{.drvPath = makeConstantStorePathRef(drvPath), .outputs = OutputsSpec::Names{"out"}}},
            mode);
        auto repaired = store->queryRealisation(outputId);
        ASSERT_TRUE(repaired);
        EXPECT_NE(repaired->outPath, invalid);
        EXPECT_EQ(readFile(store->toRealPath(repaired->outPath)), "same payload");
        EXPECT_EQ(readFile(store->toRealPath(invalid)), "same payload");
        Worker worker{*store, *store};
        EXPECT_EQ(worker.checkPathContents(repaired->outPath), PathContentStatus::Valid);
        EXPECT_EQ(worker.checkPathContents(invalid), PathContentStatus::InvalidContentAddress);
    }
};

TEST_F(RetainedContentAddressTest, NormalBuildRepairsWithoutDeletingHistoricalObject)
{
    proveBuildRepair(bmNormal);
}

TEST_F(RetainedContentAddressTest, RepairBuildRetainsHistoricalObject)
{
    proveBuildRepair(bmRepair);
}

TEST_F(RetainedContentAddressTest, WorkerRejectsIntactNarWithInvalidAddress)
{
    auto invalid = record("same payload", true);
    auto valid = record("same payload", false);
    Worker worker{*store, *store};
    EXPECT_EQ(worker.checkPathContents(invalid), PathContentStatus::InvalidContentAddress);
    EXPECT_EQ(worker.checkPathContents(valid), PathContentStatus::Valid);
}

TEST_F(RetainedContentAddressTest, RepairsOnlyInvalidMappingAndRetainsOriginalObject)
{
    auto invalid = record("same payload", true);
    auto valid = record("same payload", false);
    registerOutput(invalid);
    EXPECT_NO_THROW(registerOutput(valid));
    EXPECT_EQ(store->queryRealisation(identity())->outPath, valid);
    EXPECT_TRUE(store->isValidPath(invalid));
    EXPECT_EQ(readFile(store->toRealPath(invalid)), "same payload");
}

TEST_F(RetainedContentAddressTest, ValidDifferentOutputStillRefuses)
{
    auto first = record("first valid payload", false);
    auto second = record("different valid payload", false);
    registerOutput(first);
    EXPECT_THROW(registerOutput(second), Error);
    EXPECT_EQ(store->queryRealisation(identity())->outPath, first);
}

TEST_F(RetainedContentAddressTest, InvalidReplacementStillRefuses)
{
    auto first = record("first valid payload", false);
    auto invalid = record("incorrect replacement", true);
    registerOutput(first);
    EXPECT_THROW(registerOutput(invalid), Error);
    EXPECT_EQ(store->queryRealisation(identity())->outPath, first);
}

TEST_F(RetainedContentAddressTest, AvailabilityIgnoresWarmMetadataAndRecoversWithoutInvalidatingIt)
{
    auto path = record("available bytes", false);
    Store & interface = *store;
    EXPECT_TRUE(interface.isValidPath(path));
    auto info = interface.queryPathInfo(path);
    auto realPath = store->toRealPath(path);
    std::filesystem::remove(realPath);
    EXPECT_FALSE(interface.isValidPath(path));
    EXPECT_EQ(interface.queryPathInfo(path)->narHash, info->narHash);
    writeFile(realPath, "available bytes");
    EXPECT_TRUE(interface.isValidPath(path));
    EXPECT_EQ(interface.queryPathInfo(path)->narHash, info->narHash);
}

TEST_F(RetainedContentAddressTest, AvailabilityRequiresRegistrationButPreservesDanglingSymlink)
{
    auto path = record("absent-target", false, true);
    Store & interface = *store;
    EXPECT_TRUE(interface.isValidPath(path));
    EXPECT_EQ(interface.queryPathInfo(path)->path, path);
    EXPECT_TRUE(interface.isValidPath(path));
    auto unregistered = store->makeFixedOutputPath(
        "unregistered",
        {.method = FileIngestionMethod::NixArchive,
         .hash = hashString(HashAlgorithm::SHA256, "unregistered"),
         .references = {}});
    std::filesystem::create_directory(store->toRealPath(unregistered));
    EXPECT_FALSE(interface.isValidPath(unregistered));
}

TEST_F(RetainedContentAddressTest, ImportRestoresRegisteredMissingEntry)
{
    auto path = record("same payload", false);
    StringSink nar;
    dumpPath(store->toRealPath(path), nar);
    auto info = *store->queryPathInfo(path);
    info.narSize = nar.s.size();
    std::filesystem::remove(store->toRealPath(path));
    ASSERT_FALSE(store->isValidPath(path));
    ASSERT_EQ(store->queryPathInfo(path)->path, path);
    StringSource source{nar.s};
    store->addToStore(info, source, NoRepair, NoCheckSigs);
    EXPECT_EQ(readFile(store->toRealPath(path)), "same payload");
    Worker worker{*store, *store};
    EXPECT_EQ(worker.checkPathContents(path), PathContentStatus::Valid);
}

TEST_F(RetainedContentAddressTest, MissingEntryRejectsBadNarAndRemovesRejectedBytes)
{
    auto path = record("same payload", false);
    StringSink nar;
    dumpPath(store->toRealPath(path), nar);
    auto info = *store->queryPathInfo(path);
    info.narSize = nar.s.size();
    std::filesystem::remove(store->toRealPath(path));
    auto offset = nar.s.find("same payload");
    ASSERT_NE(offset, std::string::npos);
    nar.s[offset] = 'X';
    StringSource source{nar.s};
    EXPECT_THROW(store->addToStore(info, source, NoRepair, NoCheckSigs), Error);
    EXPECT_FALSE(pathExists(store->toRealPath(path)));
    EXPECT_FALSE(store->isValidPath(path));
    EXPECT_EQ(store->queryPathInfo(path)->path, path);
}

TEST_F(RetainedContentAddressTest, MissingEntryRejectsBadContentAddressAndRemovesRejectedBytes)
{
    auto path = record("same payload", false);
    StringSink nar;
    dumpPath(store->toRealPath(path), nar);
    auto info = *store->queryPathInfo(path);
    info.narSize = nar.s.size();
    info.ca->hash = hashString(HashAlgorithm::SHA256, "incorrect content address");
    std::filesystem::remove(store->toRealPath(path));
    StringSource source{nar.s};
    EXPECT_THROW(store->addToStore(info, source, NoRepair, NoCheckSigs), Error);
    EXPECT_FALSE(pathExists(store->toRealPath(path)));
    EXPECT_FALSE(store->isValidPath(path));
    EXPECT_EQ(store->queryPathInfo(path)->path, path);
}

TEST_F(RetainedContentAddressTest, ImportKeepsRegisteredDanglingSymlink)
{
    auto path = record("missing-target", false, true);
    auto before = lstat(store->toRealPath(path));
    StringSink nar;
    dumpPath(store->toRealPath(path), nar);
    auto info = *store->queryPathInfo(path);
    info.narSize = nar.s.size();
    StringSource source{nar.s};
    store->addToStore(info, source, NoRepair, NoCheckSigs);
    EXPECT_EQ(lstat(store->toRealPath(path)).st_ino, before.st_ino);
    EXPECT_EQ(std::filesystem::read_symlink(store->toRealPath(path)), "missing-target");
}

} // namespace nix
#endif
