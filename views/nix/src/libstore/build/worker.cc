#include "nix/store/local-store.hh"
#include "nix/store/machines.hh"
#include "nix/store/store-open.hh"
#include "nix/store/build/worker.hh"
#include "nix/store/build/substitution-goal.hh"
#include "nix/store/build/drv-output-substitution-goal.hh"
#include "nix/store/build/derivation-goal.hh"
#include "nix/store/build/derivation-resolution-goal.hh"
#include "nix/store/build/derivation-building-goal.hh"
#include "nix/store/build/derivation-trampoline-goal.hh"
#ifndef _WIN32 // TODO Enable building on Windows
#  include "nix/store/build/hook-instance.hh"
#endif
#include "nix/util/signals.hh"
#include "nix/store/globals.hh"
#include "ixe-build-scheduler.h"

#ifndef _WIN32
#  include <fcntl.h>
#  include <unistd.h>
#endif

namespace nix {

namespace {

template<typename F>
void schedulerCall(F && call)
{
    char * message = nullptr;
    auto status = call(&message);
    std::unique_ptr<char, void (*)(char *)> error(message, ixe_build_scheduler_error_free);
    if (status != 0)
        throw Error("build scheduler: %s", message ? message : "missing failure diagnostic");
}

template<typename T, typename F>
T schedulerResult(F && call)
{
    T result{};
    schedulerCall([&](char ** error) { return call(&result, error); });
    return result;
}

unsigned int schedulerCategory(JobCategory category)
{
    switch (category) {
    case JobCategory::Build:
        return IXE_BUILD_JOB_BUILD;
    case JobCategory::Substitution:
        return IXE_BUILD_JOB_SUBSTITUTION;
    case JobCategory::Administration:
        return IXE_BUILD_JOB_ADMINISTRATION;
    }
    unreachable();
}

} // namespace

Worker::Worker(Store & store, Store & evalStore, BuildFailureMode failureMode)
    : childScheduler(nullptr, ixe_build_scheduler_free)
    , act(*logger, actRealise)
    , actDerivations(*logger, actBuilds)
    , actSubstitutions(*logger, actCopyPaths)
#ifdef _WIN32
    , ioport{CreateIoCompletionPort(INVALID_HANDLE_VALUE, NULL, 0, 0)}
#endif
    , store(store)
    , evalStore(evalStore)
    , settings(nix::settings.getWorkerSettings())
    , keepGoing(failureMode == BuildFailureMode::KeepGoing || settings.keepGoing)
    , independentRequests(failureMode == BuildFailureMode::KeepGoing)
    , getSubstituters{[] {
        return nix::settings.getWorkerSettings().useSubstitutes ? getDefaultSubstituters() : std::list<ref<Store>>{};
    }}
{
#ifdef _WIN32
    if (!ioport)
        throw windows::WinError("CreateIoCompletionPort");
#endif
    IxeBuildSchedulerConfig config{
        .max_builds = settings.maxBuildJobs,
        .max_substitutions = settings.maxSubstitutionJobs,
        .silent_seconds = settings.maxSilentTime,
        .build_seconds = settings.buildTimeout,
        .poll_seconds = settings.pollInterval,
        .monitor_progress = settings.maxNoProgressTime != 0,
    };
    childScheduler.reset(schedulerResult<IxeBuildScheduler *>([&](auto out, char ** error) {
        return ixe_build_scheduler_new(&config, out, error);
    }));

#ifndef _WIN32
    /* See the field doc in worker.hh: level-triggered interrupt wakeup for
       `waitForInput`, so a client disconnect or SIGTERM aborts the goal loop
       even when every builder is silent and no timeout is armed. */
    interruptWakeupPipe = std::make_shared<Pipe>();
    interruptWakeupPipe->create();
    for (auto fd : {interruptWakeupPipe->readSide.get(), interruptWakeupPipe->writeSide.get()}) {
        auto flags = fcntl(fd, F_GETFL);
        if (flags == -1 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) == -1)
            throw SysError("making interrupt wakeup pipe non-blocking");
    }
    interruptWakeupCallback = createInterruptCallback([pipe = interruptWakeupPipe]() {
        /* Non-blocking: if the pipe is full a wakeup is already pending and
           dropping this write is fine. */
        [[maybe_unused]] auto res = ::write(pipe->writeSide.get(), "i", 1);
    });
#endif
}

Worker::~Worker()
{
    /* Explicitly get rid of all strong pointers now.  After this all
       goals that refer to this worker should be gone.  (Otherwise we
       are in trouble, since goals may call childTerminated() etc. in
       their destructors). */
    topGoals.clear();

    assert(expectedSubstitutions == 0);
    assert(expectedDownloadSize == 0);
    assert(expectedNarSize == 0);
}

template<class G, typename... Args>
std::shared_ptr<G> Worker::initGoalIfNeeded(std::weak_ptr<G> & goal_weak, Args &&... args)
{
    if (auto goal = goal_weak.lock())
        return goal;

    auto goal = std::make_shared<G>(args...);
    goal_weak = goal;
    wakeUp(goal);
    return goal;
}

std::shared_ptr<DerivationTrampolineGoal> Worker::makeDerivationTrampolineGoal(
    ref<const SingleDerivedPath> drvReq, const OutputsSpec & wantedOutputs, BuildMode buildMode)
{
    return initGoalIfNeeded(
        derivationTrampolineGoals.ensureSlot(*drvReq).value[wantedOutputs], drvReq, wantedOutputs, *this, buildMode);
}

std::shared_ptr<DerivationTrampolineGoal> Worker::makeDerivationTrampolineGoal(
    const StorePath & drvPath, const OutputsSpec & wantedOutputs, const Derivation & drv, BuildMode buildMode)
{
    return initGoalIfNeeded(
        derivationTrampolineGoals.ensureSlot(DerivedPath::Opaque{drvPath}).value[wantedOutputs],
        drvPath,
        wantedOutputs,
        drv,
        *this,
        buildMode);
}

std::shared_ptr<DerivationGoal> Worker::makeDerivationGoal(
    const StorePath & drvPath,
    const Derivation & drv,
    const OutputName & wantedOutput,
    BuildMode buildMode,
    bool storeDerivation)
{
    return initGoalIfNeeded(
        derivationGoals[drvPath][wantedOutput], drvPath, drv, wantedOutput, *this, buildMode, storeDerivation);
}

std::shared_ptr<DerivationResolutionGoal>
Worker::makeDerivationResolutionGoal(const StorePath & drvPath, const Derivation & drv, BuildMode buildMode)
{
    return initGoalIfNeeded(derivationResolutionGoals[drvPath], drvPath, drv, *this, buildMode);
}

std::shared_ptr<DerivationBuildingGoal> Worker::makeDerivationBuildingGoal(
    const StorePath & drvPath, const Derivation & drv, BuildMode buildMode, bool storeDerivation)
{
    return initGoalIfNeeded(derivationBuildingGoals[drvPath], drvPath, drv, *this, buildMode, storeDerivation);
}

std::shared_ptr<PathSubstitutionGoal>
Worker::makePathSubstitutionGoal(const StorePath & path, RepairFlag repair, std::optional<ContentAddress> ca)
{
    return initGoalIfNeeded(substitutionGoals[path], path, *this, repair, ca);
}

std::shared_ptr<DrvOutputSubstitutionGoal> Worker::makeDrvOutputSubstitutionGoal(const DrvOutput & id)
{
    return initGoalIfNeeded(drvOutputSubstitutionGoals[id], id, *this);
}

GoalPtr Worker::makeGoal(const DerivedPath & req, BuildMode buildMode)
{
    return std::visit(
        overloaded{
            [&](const DerivedPath::Built & bfd) -> GoalPtr {
                return makeDerivationTrampolineGoal(bfd.drvPath, bfd.outputs, buildMode);
            },
            [&](const DerivedPath::Opaque & bo) -> GoalPtr {
                return makePathSubstitutionGoal(bo.path, buildMode == bmRepair ? Repair : NoRepair);
            },
        },
        req.raw());
}

/**
 * This function is polymorphic (both via type parameters and
 * overloading) and recursive in order to work on a various types of
 * trees
 *
 * @return Whether the tree node we are processing is not empty / should
 * be kept alive. In the case of this overloading the node in question
 * is the leaf, the weak reference itself. If the weak reference points
 * to the goal we are looking for, our caller can delete it. In the
 * inductive case where the node is an interior node, we'll likewise
 * return whether the interior node is non-empty. If it is empty
 * (because we just deleted its last child), then our caller can
 * likewise delete it.
 */
template<typename G>
static bool removeGoal(std::shared_ptr<G> goal, std::weak_ptr<G> & gp)
{
    return gp.lock() != goal;
}

template<typename K, typename G, typename Inner>
static bool removeGoal(std::shared_ptr<G> goal, std::map<K, Inner> & goalMap)
{
    /* !!! inefficient */
    for (auto i = goalMap.begin(); i != goalMap.end();) {
        if (!removeGoal(goal, i->second))
            i = goalMap.erase(i);
        else
            ++i;
    }
    return !goalMap.empty();
}

template<typename G>
static bool
removeGoal(std::shared_ptr<G> goal, typename DerivedPathMap<std::map<OutputsSpec, std::weak_ptr<G>>>::ChildNode & node)
{
    bool valueKeep = removeGoal(goal, node.value);
    bool childMapKeep = removeGoal(goal, node.childMap);
    return valueKeep || childMapKeep;
}

void Worker::removeGoal(GoalPtr goal)
{
    if (auto drvGoal = std::dynamic_pointer_cast<DerivationTrampolineGoal>(goal))
        nix::removeGoal(drvGoal, derivationTrampolineGoals.map);
    else if (auto drvGoal = std::dynamic_pointer_cast<DerivationGoal>(goal))
        nix::removeGoal(drvGoal, derivationGoals);
    else if (auto drvResolutionGoal = std::dynamic_pointer_cast<DerivationResolutionGoal>(goal))
        nix::removeGoal(drvResolutionGoal, derivationResolutionGoals);
    else if (auto drvBuildingGoal = std::dynamic_pointer_cast<DerivationBuildingGoal>(goal))
        nix::removeGoal(drvBuildingGoal, derivationBuildingGoals);
    else if (auto subGoal = std::dynamic_pointer_cast<PathSubstitutionGoal>(goal))
        nix::removeGoal(subGoal, substitutionGoals);
    else if (auto subGoal = std::dynamic_pointer_cast<DrvOutputSubstitutionGoal>(goal))
        nix::removeGoal(subGoal, drvOutputSubstitutionGoals);
    else
        assert(false);

    if (topGoals.find(goal) != topGoals.end()) {
        topGoals.erase(goal);
        /* If a top-level goal failed, then kill all other goals
           (unless keepGoing was set). */
        if (goal->exitCode == Goal::ecFailed && !keepGoing)
            topGoals.clear();
    }

    /* Wake up goals waiting for any goal to finish. */
    for (auto & i : waitingForAnyGoal) {
        GoalPtr goal = i.lock();
        if (goal)
            wakeUp(goal);
    }

    waitingForAnyGoal.clear();
}

void Worker::wakeUp(GoalPtr goal)
{
    goal->trace("woken up");
    addToWeakGoals(awake, goal);
}

size_t Worker::getNrLocalBuilds()
{
    return schedulerResult<uint64_t>([&](auto out, char ** error) {
        return ixe_build_scheduler_running(childScheduler.get(), IXE_BUILD_JOB_BUILD, out, error);
    });
}

size_t Worker::getNrSubstitutions()
{
    return schedulerResult<uint64_t>([&](auto out, char ** error) {
        return ixe_build_scheduler_running(childScheduler.get(), IXE_BUILD_JOB_SUBSTITUTION, out, error);
    });
}

bool Worker::buildSlotAvailable(JobCategory category)
{
    return schedulerResult<unsigned int>([&](auto out, char ** error) {
               return ixe_build_scheduler_slot_available(childScheduler.get(), schedulerCategory(category), out, error);
           })
           != 0;
}

uint64_t Worker::schedulerTime(steady_time_point now) const
{
    auto milliseconds = std::chrono::duration_cast<std::chrono::milliseconds>(now - schedulerEpoch).count();
    if (milliseconds < 0)
        throw Error("build scheduler monotonic clock precedes worker creation");
    return static_cast<uint64_t>(milliseconds);
}

void Worker::childStarted(
    GoalPtr goal, const std::set<MuxablePipePollState::CommChannel> & channels, bool inBuildSlot, bool respectTimeouts)
{
    if (std::ranges::any_of(children, [&](const Child & child) { return child.identity == goal.get(); }))
        throw Error("build goal already has a registered child");
    Child child{
        .goal = goal,
        .identity = goal.get(),
        .schedulerId = 0,
        .channels = channels,
    };
    child.schedulerId = schedulerResult<uint64_t>([&](auto out, char ** error) {
        return ixe_build_scheduler_start(
            childScheduler.get(),
            schedulerCategory(goal->jobCategory()),
            inBuildSlot,
            respectTimeouts,
            schedulerTime(steady_time_point::clock::now()),
            out,
            error);
    });
    try {
        children.emplace_back(std::move(child));
    } catch (...) {
        schedulerResult<unsigned int>([&](auto out, char ** error) {
            return ixe_build_scheduler_stop(childScheduler.get(), child.schedulerId, false, out, error);
        });
        throw;
    }
}

void Worker::childTerminated(Goal * goal, bool wakeSleepers)
{
    auto i =
        std::find_if(children.begin(), children.end(), [&](const Child & child) { return child.identity == goal; });
    if (i == children.end())
        return;

    auto wake = schedulerResult<unsigned int>([&](auto out, char ** error) {
        return ixe_build_scheduler_stop(childScheduler.get(), i->schedulerId, wakeSleepers, out, error);
    });
    children.erase(i);

    if (wake) {

        /* Wake up goals waiting for a build slot. */
        for (auto & j : wantingToBuild) {
            GoalPtr goal = j.lock();
            if (goal)
                wakeUp(goal);
        }

        wantingToBuild.clear();
    }
}

void Worker::waitForBuildSlot(GoalPtr goal)
{
    goal->trace("wait for build slot");
    if (buildSlotAvailable(goal->jobCategory()))
        wakeUp(goal); /* we can do it right away */
    else
        addToWeakGoals(wantingToBuild, goal);
}

void Worker::waitForAnyGoal(GoalPtr goal)
{
    debug("wait for any goal");
    addToWeakGoals(waitingForAnyGoal, goal);
}

void Worker::waitForAWhile(GoalPtr goal)
{
    debug("wait for a while");
    addToWeakGoals(waitingForAWhile, goal);
}

void Worker::run(const Goals & _topGoals)
{
    std::vector<nix::DerivedPath> topPaths;

    for (auto & i : _topGoals) {
        topGoals.insert(i);
        if (auto goal = dynamic_cast<DerivationTrampolineGoal *>(i.get())) {
            topPaths.push_back(
                DerivedPath::Built{
                    .drvPath = goal->drvReq,
                    .outputs = goal->wantedOutputs,
                });
        } else if (auto goal = dynamic_cast<PathSubstitutionGoal *>(i.get())) {
            topPaths.push_back(DerivedPath::Opaque{goal->storePath});
        }
    }

    /* Call queryMissing() to efficiently query substitutes. */
    // Independent requests must report malformed derivations against their
    // own goals. This batch prefetch can throw before any goal runs; the
    // goals perform the same lookups themselves when it is omitted.
    if (!independentRequests)
        store.queryMissing(topPaths);

    debug("entered goal loop");

    while (1) {

        checkInterrupt();

        // TODO GC interface?
        if (auto localStore = dynamic_cast<LocalStore *>(&store))
            localStore->autoGC(false);

        /* Call every wake goal (in the ordering established by
           CompareGoalPtrs). */
        while (!awake.empty() && !topGoals.empty()) {
            Goals awake2;
            for (auto & i : awake) {
                GoalPtr goal = i.lock();
                if (goal)
                    awake2.insert(goal);
            }
            awake.clear();
            for (auto & goal : awake2) {
                checkInterrupt();
                // Polling between steps can wake a goal that is already in
                // this batch. This step consumes that wake too; keep only
                // wakes produced during or after work for the next batch.
                awake.erase(goal);
                goal->work();
                // A new hook can negotiate synchronously. Drain accepted hooks
                // between goal steps so their diagnostics cannot block uploads
                // while the remaining ready goals compete for remote slots.
                if (!topGoals.empty() && !children.empty())
                    waitForInput(false);
                if (topGoals.empty())
                    break; // stuff may have been cancelled
            }
        }

        if (topGoals.empty())
            break;

        /* Wait for input. */
        if (!children.empty() || !waitingForAWhile.empty())
            waitForInput();
        else if (awake.empty() && 0U == settings.maxBuildJobs) {
            if (Machine::parseConfig({nix::settings.thisSystem}, nix::settings.getWorkerSettings().builders).empty())
                throw Error(
                    "Unable to start any build; either increase '--max-jobs' or enable remote builds.\n"
                    "\n"
                    "For more information run 'man nix.conf' and search for '/machines'.");
            else
                throw Error(
                    "Unable to start any build; remote machines may not have all required system features.\n"
                    "\n"
                    "For more information run 'man nix.conf' and search for '/machines'.");
        } else
            assert(!awake.empty());
    }

    /* If --keep-going is not set, it's possible that the main goal
       exited while some of its subgoals were still active.  But if
       --keep-going *is* set, then they must all be finished now. */
    assert(!keepGoing || awake.empty());
    assert(!keepGoing || wantingToBuild.empty());
    assert(!keepGoing || children.empty());
}

void Worker::waitForInput(bool block)
{
    printMsg(lvlVomit, "waiting for children");

    /* Process output from the file descriptors attached to the
       children, namely log output and output path creation commands.
       We also use this to detect child termination: if we get EOF on
       the logger pipe of a build, we assume that the builder has
       terminated. */

    auto before = steady_time_point::clock::now();
    auto localStore = dynamic_cast<LocalStore *>(&store);
    auto plan = schedulerResult<IxeBuildWaitPlan>([&](auto out, char ** error) {
        return ixe_build_scheduler_wait_plan(
            childScheduler.get(),
            schedulerTime(before),
            !waitingForAWhile.empty(),
            localStore && localStore->config->getLocalSettings().getGCSettings().minFree.get() != 0,
            out,
            error);
    });

    if (plan.has_timeout)
        vomit("sleeping %d milliseconds", plan.timeout_ms);

    MuxablePipePollState state;

#ifndef _WIN32
    /* Use select() to wait for the input side of any logger pipe to
       become `available'.  Note that `available' (i.e., non-blocking)
       includes EOF. */
    for (auto & i : children) {
        for (auto & j : i.channels) {
            state.pollStatus.push_back((struct pollfd) {.fd = j, .events = POLLIN});
            state.fdToPollStatus[j] = state.pollStatus.size() - 1;
        }
    }

    {
        /* Register the interrupt wakeup pipe so an interrupt reliably wakes
           this poll (see the field doc in worker.hh). */
        auto wakeupFd = interruptWakeupPipe->readSide.get();
        state.pollStatus.push_back((struct pollfd) {.fd = wakeupFd, .events = POLLIN});
        state.fdToPollStatus[wakeupFd] = state.pollStatus.size() - 1;
    }
#endif

    state.poll(
#ifdef _WIN32
        ioport.get(),
#endif
        !block || plan.has_timeout ? std::optional{block ? plan.timeout_ms : 0U} : std::nullopt);

#ifndef _WIN32
    /* Drain pending wakeup bytes and act on the interrupt now: with no
       children (e.g. only goals polling for a lock) none of the loops below
       would call checkInterrupt before going back to sleep. */
    {
        char buf[64];
        while (::read(interruptWakeupPipe->readSide.get(), buf, sizeof(buf)) > 0)
            ;
    }
    checkInterrupt();
#endif

    auto after = steady_time_point::clock::now();
    auto afterMs = schedulerTime(after);

    /* Process all available file descriptors. FIXME: this is
       O(children * fds). */
    decltype(children)::iterator i;
    for (auto j = children.begin(); j != children.end(); j = i) {
        i = std::next(j);

        checkInterrupt();

        GoalPtr goal = j->goal.lock();
        assert(goal);

        state.iterate(
            j->channels,
            [&](Descriptor k, std::string_view data) {
                printMsg(lvlVomit, "%1%: read %2% bytes", goal->getName(), data.size());
                schedulerCall([&](char ** error) {
                    return ixe_build_scheduler_note_output(childScheduler.get(), j->schedulerId, afterMs, error);
                });
                goal->handleChildOutput(k, data);
            },
            [&](Descriptor k) {
                debug("%1%: got EOF", goal->getName());
                goal->handleEOF(k);
            });

        auto monitorProgress = schedulerResult<unsigned int>([&](auto out, char ** error) {
            return ixe_build_scheduler_monitors_progress(childScheduler.get(), j->schedulerId, out, error);
        });
        auto noProgressTimeout = monitorProgress ? goal->noProgressTimeout(after) : std::nullopt;
        auto decision = schedulerResult<IxeBuildChildDecision>([&](auto out, char ** error) {
            return ixe_build_scheduler_inspect_child(
                childScheduler.get(),
                j->schedulerId,
                afterMs,
                goal->exitCode == Goal::ecBusy,
                noProgressTimeout.value_or(0),
                out,
                error);
        });
        if (decision.timeout_kind != IXE_BUILD_TIMEOUT_NONE)
            goal->timedOut(TimedOut(static_cast<time_t>(decision.timeout_seconds)));
    }

    auto wakeLocks = schedulerResult<unsigned int>([&](auto out, char ** error) {
        return ixe_build_scheduler_finish_poll(childScheduler.get(), afterMs, !waitingForAWhile.empty(), out, error);
    });
    if (wakeLocks) {
        for (auto & i : waitingForAWhile) {
            GoalPtr goal = i.lock();
            if (goal)
                wakeUp(goal);
        }
        waitingForAWhile.clear();
    }
}

PathContentStatus Worker::checkPathContents(const StorePath & path)
{
    auto i = pathContentsCache.find(path);
    if (i != pathContentsCache.end())
        return i->second;
    printInfo("checking path '%s'...", store.printStorePath(path));
    auto info = store.queryPathInfo(path);
    auto res = PathContentStatus::InvalidArchive;
    if (auto accessor = store.getFSAccessor(path, /*requireValidPath=*/false)) {
        std::optional<ContentAddressHashResult> contentHash;
        if (info->ca && info->ca->method.raw != ContentAddressMethod::Raw::JjTree)
            contentHash = hashContentAddress(
                {ref{accessor}},
                {.method = info->ca->method,
                 .algorithm = info->ca->hash.algo,
                 .selfReference = std::string{path.hashPart()}});
        auto current = contentHash && contentHash->narHashAndSize
                           ? contentHash->narHashAndSize->hash
                           : hashPath({ref{accessor}}, FileIngestionMethod::NixArchive, info->narHash.algo).first;
        Hash nullHash(HashAlgorithm::SHA256);
        res = info->narHash != nullHash && info->narHash != current ? PathContentStatus::InvalidArchive
              : contentHash && contentHash->hash != info->ca->hash  ? PathContentStatus::InvalidContentAddress
                                                                    : PathContentStatus::Valid;
    }
    pathContentsCache.insert_or_assign(path, res);
    if (res != PathContentStatus::Valid)
        printError("path '%s' is corrupted or missing!", store.printStorePath(path));
    return res;
}

void Worker::markContentsGood(const StorePath & path)
{
    pathContentsCache.insert_or_assign(path, PathContentStatus::Valid);
}

GoalPtr upcast_goal(std::shared_ptr<PathSubstitutionGoal> subGoal)
{
    return subGoal;
}

GoalPtr upcast_goal(std::shared_ptr<DrvOutputSubstitutionGoal> subGoal)
{
    return subGoal;
}

GoalPtr upcast_goal(std::shared_ptr<DerivationGoal> subGoal)
{
    return subGoal;
}

} // namespace nix
