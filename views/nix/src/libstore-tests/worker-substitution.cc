#include <gtest/gtest.h>
#include <nlohmann/json.hpp>

#include "nix/store/build/worker.hh"
#include "nix/store/build/substitution-goal.hh"
#include "nix/store/derivations.hh"
#include "nix/store/dummy-store-impl.hh"
#include "nix/store/globals.hh"
#include "nix/util/memory-source-accessor.hh"

#include "nix/store/tests/libstore.hh"
#include "nix/util/tests/json-characterization.hh"

namespace nix {

class WorkerSubstitutionTest : public LibStoreTest, public JsonCharacterizationTest<ref<DummyStore>>
{
    std::filesystem::path unitTestData = getUnitTestData() / "worker-substitution";

protected:
    ref<DummyStore> dummyStore;
    ref<DummyStore> substituter;

    WorkerSubstitutionTest()
        : LibStoreTest([] {
            auto config = make_ref<DummyStoreConfig>(DummyStoreConfig::Params{});
            config->readOnly = false;
            return config->openDummyStore();
        }())
        , dummyStore(store.dynamic_pointer_cast<DummyStore>())
        , substituter([] {
            auto config = make_ref<DummyStoreConfig>(DummyStoreConfig::Params{});
            config->readOnly = false;
            config->isTrusted = true;
            return config->openDummyStore();
        }())
    {
    }

public:
    std::filesystem::path goldenMaster(std::string_view testStem) const override
    {
        return unitTestData / testStem;
    }

    static void SetUpTestSuite()
    {
        initLibStore(false);
    }
};

TEST_F(WorkerSubstitutionTest, singleStoreObject)
{
    // Add a store path to the substituter
    auto pathInSubstituter = substituter->addToStore(
        "hello",
        SourcePath{
            [] {
                auto sc = make_ref<MemorySourceAccessor>();
                sc->root = MemorySourceAccessor::File{MemorySourceAccessor::File::Regular{
                    .executable = false,
                    .contents = "Hello, world!",
                }};
                return sc;
            }(),
        },
        ContentAddressMethod::Raw::NixArchive,
        HashAlgorithm::SHA256);

    // Snapshot the substituter (has one store object)
    checkpointJson("single/substituter", substituter);

    // Snapshot the destination store before (should be empty)
    checkpointJson("../dummy-store/empty", dummyStore);

    // The path should not exist in the destination store yet
    ASSERT_FALSE(dummyStore->isValidPath(pathInSubstituter));

    // Create a worker with our custom substituter
    Worker worker{*dummyStore, *dummyStore};

    // Override the substituters to use our dummy store substituter
    ref<Store> substituerAsStore = substituter;
    worker.getSubstituters = [substituerAsStore]() -> std::list<ref<Store>> { return {substituerAsStore}; };

    // Create a substitution goal for the path
    auto goal = worker.makePathSubstitutionGoal(pathInSubstituter);

    // Run the worker with -j0 semantics (no local builds, only substitution)
    // The worker.run() takes a set of goals
    Goals goals;
    goals.insert(upcast_goal(goal));
    worker.run(goals);

    // Snapshot the destination store after (should match the substituter)
    checkpointJson("single/substituter", dummyStore);

    // The path should now exist in the destination store
    ASSERT_TRUE(dummyStore->isValidPath(pathInSubstituter));

    // Verify the goal succeeded
    ASSERT_EQ(upcast_goal(goal)->exitCode, Goal::ecSuccess);
}

TEST_F(WorkerSubstitutionTest, missingOutputDoesNotCancelIndependentRoot)
{
    auto source = make_ref<MemorySourceAccessor>();
    source->root = MemorySourceAccessor::File{MemorySourceAccessor::File::Regular{
        .executable = false,
        .contents = "already built",
    }};
    auto output = dummyStore->addToStore(
        "independent-output", SourcePath{source}, ContentAddressMethod::Raw::NixArchive, HashAlgorithm::SHA256);
    Derivation drv;
    drv.name = "independent-root";
    drv.outputs.emplace("out", DerivationOutput{DerivationOutput::InputAddressed{.path = output}});
    auto drvPath = dummyStore->writeDerivation(drv);
    DerivedPath bad = DerivedPath::Built{
        .drvPath = makeConstantStorePathRef(drvPath),
        .outputs = OutputsSpec::Names{"missing"},
    };
    DerivedPath good = DerivedPath::Built{
        .drvPath = makeConstantStorePathRef(drvPath),
        .outputs = OutputsSpec::Names{"out"},
    };
    auto configuredKeepGoing = settings.getWorkerSettings().keepGoing.get();

    // Default callers retain their throwing behavior. The independent mode
    // must attribute the same exception to only the bad requested output.
    EXPECT_THROW(dummyStore->buildPathsWithResults({bad, good}), Error);
    auto results = dummyStore->buildPathsWithResults({bad, good}, bmNormal, nullptr, BuildFailureMode::KeepGoing);
    ASSERT_EQ(results.size(), 2);
    EXPECT_EQ(results[0].path.raw(), bad.raw());
    auto failure = results[0].tryGetFailure();
    ASSERT_NE(failure, nullptr);
    EXPECT_NE(failure->message().find("missing"), std::string::npos);
    EXPECT_EQ(results[1].path.raw(), good.raw());
    ASSERT_NE(results[1].tryGetSuccess(), nullptr);
    EXPECT_TRUE(dummyStore->isValidPath(output));
    EXPECT_EQ(settings.getWorkerSettings().keepGoing.get(), configuredKeepGoing);
}

namespace {

class NestedErrorGoal : public PathSubstitutionGoal
{
    GoalPtr dependency;
    bool fail;
    bool & continued;

    Co nested()
    {
        if (dependency)
            co_await await(Goals{dependency});
        if (fail)
            throw Error("nested request failure");
        co_return Return{};
    }

    Co run()
    {
        co_await nested();
        continued = true;
        co_return doneSuccess(BuildResult::Success{.status = BuildResult::Success::AlreadyValid});
    }

public:
    NestedErrorGoal(
        Worker & worker, const StorePath & path, std::string name, GoalPtr dependency, bool fail, bool & continued)
        : PathSubstitutionGoal(path, worker)
        , dependency(std::move(dependency))
        , fail(fail)
        , continued(continued)
    {
        this->name = std::move(name);
        // Worker removal deliberately recognizes concrete goal types. Keep
        // its substitution lifecycle, replacing only the test's coroutine.
        top_co.emplace(run());
        top_co->handle.promise().goal = this;
    }

    std::string key() override
    {
        return name;
    }

    JobCategory jobCategory() const override
    {
        return JobCategory::Administration;
    }
};

StorePath nestedGoalPath(DummyStore & store)
{
    auto source = make_ref<MemorySourceAccessor>();
    source->root = MemorySourceAccessor::File{MemorySourceAccessor::File::Regular{
        .executable = false,
        .contents = "already valid",
    }};
    return store.addToStore(
        "nested-goal", SourcePath{source}, ContentAddressMethod::Raw::NixArchive, HashAlgorithm::SHA256);
}

}

TEST_F(WorkerSubstitutionTest, nestedErrorStopsContinuationAndPreservesSharedDependencyPeer)
{
    Worker worker{*dummyStore, *dummyStore, BuildFailureMode::KeepGoing};
    auto path = nestedGoalPath(*dummyStore);
    bool dependencyContinued = false, badContinued = false, goodContinued = false;
    auto dependency = std::make_shared<NestedErrorGoal>(worker, path, "2-dependency", nullptr, false, dependencyContinued);
    auto bad = std::make_shared<NestedErrorGoal>(worker, path, "0-bad", dependency, true, badContinued);
    auto good = std::make_shared<NestedErrorGoal>(worker, path, "1-good", dependency, false, goodContinued);
    worker.wakeUp(bad);
    worker.wakeUp(good);
    worker.wakeUp(dependency);
    worker.run(Goals{bad, good});

    EXPECT_EQ(bad->exitCode, Goal::ecFailed);
    ASSERT_NE(bad->buildResult.tryGetFailure(), nullptr);
    EXPECT_NE(bad->buildResult.tryGetFailure()->message().find("nested request failure"), std::string::npos);
    EXPECT_FALSE(badContinued);
    EXPECT_EQ(good->exitCode, Goal::ecSuccess);
    EXPECT_TRUE(goodContinued);
    EXPECT_EQ(dependency->exitCode, Goal::ecSuccess);
    EXPECT_TRUE(dependencyContinued);
    EXPECT_TRUE(dependency->waiters.empty());
}

TEST_F(WorkerSubstitutionTest, nestedErrorStillThrowsForConfiguredWorker)
{
    Worker worker{*dummyStore, *dummyStore};
    auto path = nestedGoalPath(*dummyStore);
    bool continued = false;
    auto bad = std::make_shared<NestedErrorGoal>(worker, path, "bad", nullptr, true, continued);
    worker.wakeUp(bad);
    EXPECT_THROW(worker.run(Goals{bad}), Error);
    EXPECT_FALSE(continued);
}

TEST_F(WorkerSubstitutionTest, singleRootStoreObjectWithSingleDepStoreObject)
{
    // First, add a dependency store path to the substituter
    auto dependencyPath = substituter->addToStore(
        "dependency",
        SourcePath{
            [] {
                auto sc = make_ref<MemorySourceAccessor>();
                sc->root = MemorySourceAccessor::File{MemorySourceAccessor::File::Regular{
                    .executable = false,
                    .contents = "I am a dependency",
                }};
                return sc;
            }(),
        },
        ContentAddressMethod::Raw::NixArchive,
        HashAlgorithm::SHA256);

    // Now add a store path that references the dependency
    auto mainPath = substituter->addToStore(
        "main",
        SourcePath{
            [&] {
                auto sc = make_ref<MemorySourceAccessor>();
                // Include a reference to the dependency path in the contents
                sc->root = MemorySourceAccessor::File{MemorySourceAccessor::File::Regular{
                    .executable = false,
                    .contents = "I depend on " + substituter->printStorePath(dependencyPath),
                }};
                return sc;
            }(),
        },
        ContentAddressMethod::Raw::NixArchive,
        HashAlgorithm::SHA256,
        StorePathSet{dependencyPath});

    // Snapshot the substituter (has two store objects)
    checkpointJson("with-dep/substituter", substituter);

    // Snapshot the destination store before (should be empty)
    checkpointJson("../dummy-store/empty", dummyStore);

    // Neither path should exist in the destination store yet
    ASSERT_FALSE(dummyStore->isValidPath(dependencyPath));
    ASSERT_FALSE(dummyStore->isValidPath(mainPath));

    // Create a worker with our custom substituter
    Worker worker{*dummyStore, *dummyStore};

    // Override the substituters to use our dummy store substituter
    ref<Store> substituterAsStore = substituter;
    worker.getSubstituters = [substituterAsStore]() -> std::list<ref<Store>> { return {substituterAsStore}; };

    // Create a substitution goal for the main path only
    // The worker should automatically substitute the dependency as well
    auto goal = worker.makePathSubstitutionGoal(mainPath);

    // Run the worker
    Goals goals;
    goals.insert(upcast_goal(goal));
    worker.run(goals);

    // Snapshot the destination store after (should match the substituter)
    checkpointJson("with-dep/substituter", dummyStore);

    // Both paths should now exist in the destination store
    ASSERT_TRUE(dummyStore->isValidPath(dependencyPath));
    ASSERT_TRUE(dummyStore->isValidPath(mainPath));

    // Verify the goal succeeded
    ASSERT_EQ(upcast_goal(goal)->exitCode, Goal::ecSuccess);
}

TEST_F(WorkerSubstitutionTest, floatingDerivationOutput)
{
    // Enable CA derivations experimental feature
    experimentalFeatureSettings.set("extra-experimental-features", "ca-derivations");

    // Create a CA floating output derivation
    Derivation drv;
    drv.name = "test-ca-drv";
    drv.outputs = {
        {
            "out",
            DerivationOutput{DerivationOutput::CAFloating{
                .method = ContentAddressMethod::Raw::NixArchive,
                .hashAlgo = HashAlgorithm::SHA256,
            }},
        },
    };

    // Write the derivation to the destination store
    auto drvPath = dummyStore->writeDerivation(drv);

    // Snapshot the destination store before
    checkpointJson("ca-drv/store-before", dummyStore);

    // Compute the hash modulo of the derivation
    // For CA floating derivations, the kind is Deferred since outputs aren't known until build
    auto hashModulo = hashDerivationModulo(*dummyStore, drv, true);
    ASSERT_EQ(hashModulo.kind, DrvHash::Kind::Deferred);
    auto drvHash = hashModulo.hashes.at("out");

    // Create the output store object
    auto outputPath = substituter->addToStore(
        "test-ca-drv-out",
        SourcePath{
            [] {
                auto sc = make_ref<MemorySourceAccessor>();
                sc->root = MemorySourceAccessor::File{MemorySourceAccessor::File::Regular{
                    .executable = false,
                    .contents = "I am the output of a CA derivation",
                }};
                return sc;
            }(),
        },
        ContentAddressMethod::Raw::NixArchive,
        HashAlgorithm::SHA256);

    // Add the realisation (build trace) to the substituter
    substituter->buildTrace.insert_or_assign(
        drvHash,
        std::map<std::string, UnkeyedRealisation>{
            {
                "out",
                UnkeyedRealisation{
                    .outPath = outputPath,
                },
            },
        });

    // Snapshot the substituter
    checkpointJson("ca-drv/substituter", substituter);

    // The realisation should not exist in the destination store yet
    DrvOutput drvOutput{drvHash, "out"};
    ASSERT_FALSE(dummyStore->queryRealisation(drvOutput));

    // Create a worker with our custom substituter
    Worker worker{*dummyStore, *dummyStore};

    // Override the substituters to use our dummy store substituter
    ref<Store> substituterAsStore = substituter;
    worker.getSubstituters = [substituterAsStore]() -> std::list<ref<Store>> { return {substituterAsStore}; };

    // Create a derivation goal for the CA derivation output
    // The worker should substitute the output rather than building
    auto goal = worker.makeDerivationGoal(drvPath, drv, "out", bmNormal, true);

    // Run the worker
    Goals goals;
    goals.insert(upcast_goal(goal));
    worker.run(goals);

    // Snapshot the destination store after
    checkpointJson("ca-drv/store-after", dummyStore);

    // The output path should now exist in the destination store
    ASSERT_TRUE(dummyStore->isValidPath(outputPath));

    // The realisation should now exist in the destination store
    auto realisation = dummyStore->queryRealisation(drvOutput);
    ASSERT_TRUE(realisation);
    ASSERT_EQ(realisation->outPath, outputPath);

    // Verify the goal succeeded
    ASSERT_EQ(upcast_goal(goal)->exitCode, Goal::ecSuccess);

    // Disable CA derivations experimental feature
    experimentalFeatureSettings.set("extra-experimental-features", "");
}

/**
 * Test for issue #11928: substituting a CA derivation output should not
 * require fetching the output of an input derivation when that output
 * is not referenced.
 */
TEST_F(WorkerSubstitutionTest, floatingDerivationOutputWithDepDrv)
{
    // Enable CA derivations experimental feature
    experimentalFeatureSettings.set("extra-experimental-features", "ca-derivations");

    // Create the dependency CA floating derivation
    Derivation depDrv;
    depDrv.name = "dep-drv";
    depDrv.outputs = {
        {
            "out",
            DerivationOutput{DerivationOutput::CAFloating{
                .method = ContentAddressMethod::Raw::NixArchive,
                .hashAlgo = HashAlgorithm::SHA256,
            }},
        },
    };

    // Write the dependency derivation to the destination store
    auto depDrvPath = dummyStore->writeDerivation(depDrv);

    // Compute the hash modulo for the dependency derivation
    auto depHashModulo = hashDerivationModulo(*dummyStore, depDrv, true);
    ASSERT_EQ(depHashModulo.kind, DrvHash::Kind::Deferred);
    auto depDrvHash = depHashModulo.hashes.at("out");

    // Create the output store object for the dependency in the substituter
    auto depOutputPath = substituter->addToStore(
        "dep-drv-out",
        SourcePath{
            [] {
                auto sc = make_ref<MemorySourceAccessor>();
                sc->root = MemorySourceAccessor::File{MemorySourceAccessor::File::Regular{
                    .executable = false,
                    .contents = "I am the dependency output",
                }};
                return sc;
            }(),
        },
        ContentAddressMethod::Raw::NixArchive,
        HashAlgorithm::SHA256);

    // Add the realisation for the dependency to the substituter
    substituter->buildTrace.insert_or_assign(
        depDrvHash,
        std::map<std::string, UnkeyedRealisation>{
            {
                "out",
                UnkeyedRealisation{
                    .outPath = depOutputPath,
                },
            },
        });

    // Create the root CA floating derivation that depends on depDrv
    Derivation rootDrv;
    rootDrv.name = "root-drv";
    rootDrv.outputs = {
        {
            "out",
            DerivationOutput{DerivationOutput::CAFloating{
                .method = ContentAddressMethod::Raw::NixArchive,
                .hashAlgo = HashAlgorithm::SHA256,
            }},
        },
    };
    // Add the dependency derivation as an input
    rootDrv.inputDrvs = {.map = {{depDrvPath, {.value = {"out"}}}}};

    // Write the root derivation to the destination store
    auto rootDrvPath = dummyStore->writeDerivation(rootDrv);

    // Snapshot the destination store before
    checkpointJson("issue-11928/store-before", dummyStore);

    // Compute the hash modulo for the root derivation
    auto rootHashModulo = hashDerivationModulo(*dummyStore, rootDrv, true);
    ASSERT_EQ(rootHashModulo.kind, DrvHash::Kind::Deferred);
    auto rootDrvHash = rootHashModulo.hashes.at("out");

    // Create the output store object for the root derivation
    // Note: it does NOT reference the dependency's output
    auto rootOutputPath = substituter->addToStore(
        "root-drv-out",
        SourcePath{
            [] {
                auto sc = make_ref<MemorySourceAccessor>();
                sc->root = MemorySourceAccessor::File{MemorySourceAccessor::File::Regular{
                    .executable = false,
                    .contents =
                        "I am the root output. "
                        "I don't reference anything because the other derivation's output is just needed at build time.",
                }};
                return sc;
            }(),
        },
        ContentAddressMethod::Raw::NixArchive,
        HashAlgorithm::SHA256);

    // The DrvOutputs for both derivations
    DrvOutput depDrvOutput{depDrvHash, "out"};
    DrvOutput rootDrvOutput{rootDrvHash, "out"};

    // Add the realisation for the root derivation to the substituter
    substituter->buildTrace.insert_or_assign(
        rootDrvHash,
        std::map<std::string, UnkeyedRealisation>{
            {
                "out",
                UnkeyedRealisation{
                    .outPath = rootOutputPath,
                },
            },
        });

    // Snapshot the substituter
    // Note: it has realisations for both drvs, but only the root's output store object
    checkpointJson("issue-11928/substituter", substituter);

    // The realisations should not exist in the destination store yet
    ASSERT_FALSE(dummyStore->queryRealisation(depDrvOutput));
    ASSERT_FALSE(dummyStore->queryRealisation(rootDrvOutput));

    // Create a worker with our custom substituter
    Worker worker{*dummyStore, *dummyStore};

    // Override the substituters to use our dummy store substituter
    ref<Store> substituterAsStore = substituter;
    worker.getSubstituters = [substituterAsStore]() -> std::list<ref<Store>> { return {substituterAsStore}; };

    // Create a derivation goal for the root derivation output
    // The worker should substitute the output rather than building
    auto goal = worker.makeDerivationGoal(rootDrvPath, rootDrv, "out", bmNormal, false);

    // Run the worker
    Goals goals;
    goals.insert(upcast_goal(goal));
    worker.run(goals);

    // Snapshot the destination store after
    checkpointJson("issue-11928/store-after", dummyStore);

    // The root output path should now exist in the destination store
    ASSERT_TRUE(dummyStore->isValidPath(rootOutputPath));

    // The root realisation should now exist in the destination store
    auto rootRealisation = dummyStore->queryRealisation(rootDrvOutput);
    ASSERT_TRUE(rootRealisation);
    ASSERT_EQ(rootRealisation->outPath, rootOutputPath);

    // #11928: The dependency's REALISATION should be fetched, because
    // it is needed to resolve the underlying derivation. Currently the
    // realisation is not fetched (bug). Once fixed: Change
    // depRealisation ASSERT_FALSE to ASSERT_TRUE and uncomment the
    // ASSERT_EQ
    auto depRealisation = dummyStore->queryRealisation(depDrvOutput);
    ASSERT_FALSE(depRealisation);
    // ASSERT_EQ(depRealisation->outPath, depOutputPath);

    // The dependency's OUTPUT is correctly not fetched (not referenced by root output)
    ASSERT_FALSE(dummyStore->isValidPath(depOutputPath));

    // Verify the goal succeeded
    ASSERT_EQ(upcast_goal(goal)->exitCode, Goal::ecSuccess);

    // Disable CA derivations experimental feature
    experimentalFeatureSettings.set("extra-experimental-features", "");
}

} // namespace nix

#ifndef _WIN32
namespace nix {
namespace {

struct ReadyBatchObservation
{
    unsigned int childEvents = 0;
    unsigned int selfWakeResumptions = 0;
};

enum class ReadyBatchRole { First, Child, SelfWake };

class ReadyBatchGoal : public PathSubstitutionGoal
{
    ReadyBatchRole role;
    ReadyBatchObservation & observation;

    Co run()
    {
        if (role == ReadyBatchRole::Child) {
            auto event = co_await WaitForChildEvent{};
            EXPECT_TRUE(std::holds_alternative<ChildOutput>(event));
            ++observation.childEvents;
            worker.childTerminated(this);
        } else if (role == ReadyBatchRole::SelfWake) {
            worker.wakeUp(shared_from_this());
            co_await Suspend{};
            ++observation.selfWakeResumptions;
        }
        co_return doneSuccess(BuildResult::Success{.status = BuildResult::Success::AlreadyValid});
    }

public:
    ReadyBatchGoal(Worker & worker, const StorePath & path, ReadyBatchRole role, ReadyBatchObservation & observation)
        : PathSubstitutionGoal(path, worker), role(role), observation(observation)
    {
        top_co.emplace(run());
        top_co->handle.promise().goal = this;
    }

    std::string key() override
    {
        switch (role) {
        case ReadyBatchRole::First: return "a-first";
        case ReadyBatchRole::Child: return "b-child";
        case ReadyBatchRole::SelfWake: return "c-self-wake";
        }
        unreachable();
    }

    JobCategory jobCategory() const override { return JobCategory::Administration; }
};

}

TEST_F(WorkerSubstitutionTest, childEventCoalescesWithReadyBatchButPreservesSelfWake)
{
    Worker worker{*dummyStore, *dummyStore, BuildFailureMode::KeepGoing};
    auto path = nestedGoalPath(*dummyStore);
    ReadyBatchObservation observation;
    auto first = std::make_shared<ReadyBatchGoal>(worker, path, ReadyBatchRole::First, observation);
    auto child = std::make_shared<ReadyBatchGoal>(worker, path, ReadyBatchRole::Child, observation);
    auto selfWake = std::make_shared<ReadyBatchGoal>(worker, path, ReadyBatchRole::SelfWake, observation);
    Pipe output;
    output.create();
    writeFull(output.writeSide.get(), "ready");
    worker.childStarted(child, {output.readSide.get()}, false, false);
    worker.wakeUp(first);
    worker.wakeUp(child);
    worker.wakeUp(selfWake);
    // First completes, then the real child poll requeues Child before its
    // existing batch turn. Retain every goal so a stale wake cannot expire.
    worker.run(Goals{first, child, selfWake});
    EXPECT_EQ(observation.childEvents, 1U);
    EXPECT_EQ(observation.selfWakeResumptions, 1U);
    EXPECT_EQ(child->exitCode, Goal::ecSuccess);
    EXPECT_EQ(selfWake->exitCode, Goal::ecSuccess);
}
}
#endif
