#include "nix/util/finally.hh"
#include "nix/cmd/common-eval-args.hh"
#include "nix/main/common-args.hh"
#include "nix/main/shared.hh"
#include "nix/expr/eval.hh"
#include "nix/expr/eval-inline.hh"
#include "nix/expr/eval-settings.hh"
#include "nix/expr/get-drvs.hh"
#include "nix/util/os-string.hh"
#include "nix/util/signals.hh"
#include "nix/util/mounted-source-accessor.hh"
#include "nix/store/store-open.hh"
#include "nix/store/derivations.hh"
#include "nix/store/outputs-spec.hh"
#include "nix/expr/attr-path.hh"
#include "nix/fetchers/fetch-settings.hh"
#include "nix/fetchers/fetchers.hh"
#include "nix/fetchers/registry.hh"
#include "nix/expr/eval-cache.hh"
#include "nix/cmd/markdown.hh"
#include "nix/util/users.hh"
#include "nix/fetchers/fetch-to-store.hh"
#include "nix/store/local-fs-store.hh"
#include "nix/store/globals.hh"
#include "nix/cmd/rust-eval-session.hh"

#include <filesystem>
#include <nlohmann/json.hpp>
#include <iomanip>

#include "nix/util/strings-inline.hh"

// FIXME is this supposed to be private or not?
#include "flake-command.hh"

using namespace nix;
using namespace nix::flake;
using json = nlohmann::json;

struct CmdFlakeUpdate;

FlakeCommand::FlakeCommand()
{
    expectArgs(
        {.label = "flake-url",
         .optional = true,
         .handler = {&flakeUrl},
         .completer = {[&](AddCompletions & completions, size_t, std::string_view prefix) {
             completeFlakeRef(completions, getStore(), prefix);
         }}});
}

FlakeRef FlakeCommand::getFlakeRef()
{
    return parseFlakeRef(fetchSettings, flakeUrl, std::filesystem::current_path().string()); // FIXME
}

LockedFlake FlakeCommand::lockFlake()
{
    return flake::lockFlake(flakeSettings, *getEvalState(), getFlakeRef(), lockFlags);
}

std::vector<FlakeRef> FlakeCommand::getFlakeRefsForCompletion()
{
    return {// Like getFlakeRef but with expandTilde called first
            parseFlakeRef(fetchSettings, expandTilde(flakeUrl), std::filesystem::current_path().string())};
}

struct CmdFlakeUpdate : FlakeCommand
{
public:

    std::string description() override
    {
        return "update flake lock file";
    }

    CmdFlakeUpdate()
    {
        expectedArgs.clear();
        addFlag({
            .longName = "flake",
            .description = "The flake to operate on. Default is the current directory.",
            .labels = {"flake-url"},
            .handler = {&flakeUrl},
            .completer = {[&](AddCompletions & completions, size_t, std::string_view prefix) {
                completeFlakeRef(completions, getStore(), prefix);
            }},
        });
        expectArgs({
            .label = "inputs",
            .optional = true,
            .handler = {[&](std::vector<std::string> inputsToUpdate) {
                for (const auto & inputToUpdate : inputsToUpdate) {
                    std::optional<NonEmptyInputAttrPath> inputAttrPath;
                    try {
                        inputAttrPath = flake::NonEmptyInputAttrPath::parse(inputToUpdate);
                        if (!inputAttrPath)
                            throw UsageError(
                                "input path to be updated cannot be zero-length; it would refer to the flake itself, not an input");
                    } catch (Error & e) {
                        warn(
                            "Invalid flake input '%s'. To update a specific flake, use 'nix flake update --flake %s' instead.",
                            inputToUpdate,
                            inputToUpdate);
                        throw e;
                    }
                    if (lockFlags.inputUpdates.contains(*inputAttrPath))
                        warn(
                            "Input '%s' was specified multiple times. You may have done this by accident.",
                            printInputAttrPath(*inputAttrPath));
                    lockFlags.inputUpdates.insert(*inputAttrPath);
                }
            }},
            .completer = {[&](AddCompletions & completions, size_t, std::string_view prefix) {
                completeFlakeInputAttrPath(completions, getEvalState(), getFlakeRefsForCompletion(), prefix);
            }},
        });

        /* Remove flags that don't make sense. */
        removeFlag("no-update-lock-file");
        removeFlag("no-write-lock-file");
    }

    std::string doc() override
    {
        return
#include "flake-update.md"
            ;
    }

    void run(nix::ref<nix::Store> store) override
    {
        fetchSettings.tarballTtl = 0;
        auto updateAll = lockFlags.inputUpdates.empty();

        lockFlags.recreateLockFile = updateAll;
        lockFlags.writeLockFile = true;
        lockFlags.applyNixConfig = true;

        lockFlake();
    }
};

struct CmdFlakeLock : FlakeCommand
{
    std::string description() override
    {
        return "create missing lock file entries";
    }

    CmdFlakeLock()
    {
        /* Remove flags that don't make sense. */
        removeFlag("no-write-lock-file");
    }

    std::string doc() override
    {
        return
#include "flake-lock.md"
            ;
    }

    void run(nix::ref<nix::Store> store) override
    {
        fetchSettings.tarballTtl = 0;

        lockFlags.writeLockFile = true;
        lockFlags.failOnUnlocked = true;
        lockFlags.applyNixConfig = true;

        lockFlake();
    }
};

struct CmdFlakeMetadata : FlakeCommand, MixJSON
{
    std::string description() override
    {
        return "show flake metadata";
    }

    std::string doc() override
    {
        return
#include "flake-metadata.md"
            ;
    }

    void run(nix::ref<nix::Store> store) override
    {
        auto lockedFlake = lockFlake();
        auto & flake = lockedFlake.flake;

        /* Flakes do not get copied to the store, but are instead mounted at
           their expected store paths in storeFS. Querying metadata does not
           force copying to the store, as one would expect. */
        auto storePath = store->toStorePath(flake.path.path.abs()).first;

        if (json) {
            nlohmann::json j;
            if (flake.description)
                j["description"] = *flake.description;
            j["originalUrl"] = flake.originalRef.to_string();
            j["original"] = fetchers::attrsToJSON(flake.originalRef.toAttrs());
            j["resolvedUrl"] = flake.resolvedRef.to_string();
            j["resolved"] = fetchers::attrsToJSON(flake.resolvedRef.toAttrs());
            j["url"] = flake.lockedRef.to_string(); // FIXME: rename to lockedUrl
            // "locked" is a misnomer - this is the result of the
            // attempt to lock.
            j["locked"] = fetchers::attrsToJSON(flake.lockedRef.toAttrs());
            if (auto rev = flake.lockedRef.input.getRev())
                j["revision"] = rev->to_string(HashFormat::Base16, false);
            if (auto revCount = flake.lockedRef.input.getRevCount())
                j["revCount"] = *revCount;
            if (auto lastModified = flake.lockedRef.input.getLastModified())
                j["lastModified"] = *lastModified;
            j["path"] = store->printStorePath(storePath);
            j["locks"] = lockedFlake.lockFile.toJSON().first;
            if (auto fingerprint = lockedFlake.getFingerprint(*store, fetchSettings))
                j["fingerprint"] = fingerprint->to_string(HashFormat::Base16, false);
            printJSON(j);
        } else {
            logger->cout(ANSI_BOLD "Resolved URL:" ANSI_NORMAL "  %s", flake.resolvedRef.to_string());
            if (flake.lockedRef.input.isLocked(fetchSettings))
                logger->cout(ANSI_BOLD "Locked URL:" ANSI_NORMAL "    %s", flake.lockedRef.to_string());
            if (flake.description)
                logger->cout(ANSI_BOLD "Description:" ANSI_NORMAL "   %s", *flake.description);
            logger->cout(ANSI_BOLD "Path:" ANSI_NORMAL "          %s", store->printStorePath(storePath));
            if (auto rev = flake.lockedRef.input.getRev())
                logger->cout(ANSI_BOLD "Revision:" ANSI_NORMAL "      %s", rev->to_string(HashFormat::Base16, false));
            if (auto revCount = flake.lockedRef.input.getRevCount())
                logger->cout(ANSI_BOLD "Revisions:" ANSI_NORMAL "     %s", *revCount);
            if (auto lastModified = flake.lockedRef.input.getLastModified())
                logger->cout(
                    ANSI_BOLD "Last modified:" ANSI_NORMAL " %s",
                    std::put_time(std::localtime(&*lastModified), "%F %T"));
            if (auto fingerprint = lockedFlake.getFingerprint(*store, fetchSettings))
                logger->cout(
                    ANSI_BOLD "Fingerprint:" ANSI_NORMAL "   %s", fingerprint->to_string(HashFormat::Base16, false));

            if (!lockedFlake.lockFile.inputs(lockedFlake.lockFile.root).empty())
                logger->cout(ANSI_BOLD "Inputs:" ANSI_NORMAL);

            std::set<NodeId> visited{lockedFlake.lockFile.root};

            [&](this const auto & recurse, NodeId node, const std::string & prefix) -> void {
                const auto inputs = lockedFlake.lockFile.inputs(node);
                for (const auto & [i, input] : enumerate(inputs)) {
                    bool last = i + 1 == inputs.size();

                    if (auto inputNode = std::get_if<0>(&input.second)) {
                        const auto * lockedNode = lockedFlake.lockFile.node(*inputNode);
                        std::string lastModifiedStr = "";
                        if (auto lastModified = lockedNode->lockedRef.input.getLastModified())
                            lastModifiedStr = fmt(" (%s)", std::put_time(std::gmtime(&*lastModified), "%F %T"));
                        logger->cout(
                            "%s" ANSI_BOLD "%s" ANSI_NORMAL ": %s%s",
                            prefix + (last ? treeLast : treeConn),
                            input.first,
                            lockedNode->lockedRef,
                            lastModifiedStr);

                        bool firstVisit = visited.insert(*inputNode).second;

                        if (firstVisit)
                            recurse(*inputNode, prefix + (last ? treeNull : treeLine));
                    } else if (auto follows = std::get_if<1>(&input.second)) {
                        logger->cout(
                            "%s" ANSI_BOLD "%s" ANSI_NORMAL " follows input '%s'",
                            prefix + (last ? treeLast : treeConn),
                            input.first,
                            printInputAttrPath(*follows));
                    }
                }
            }(lockedFlake.lockFile.root, "");
        }
    }
};

struct CmdFlakeInfo : CmdFlakeMetadata
{
    void run(nix::ref<nix::Store> store) override
    {
        warn("'nix flake info' is a deprecated alias for 'nix flake metadata'");
        CmdFlakeMetadata::run(store);
    }
};

struct CmdFlakeCheck : FlakeCommand
{
    bool build = true;
    bool checkAllSystems = false;

    CmdFlakeCheck()
    {
        addFlag({
            .longName = "no-build",
            .description = "Do not build checks.",
            .handler = {&build, false},
        });
        addFlag({
            .longName = "all-systems",
            .description = "Check the outputs for all systems.",
            .handler = {&checkAllSystems, true},
        });
    }

    std::string description() override
    {
        return "check whether the flake evaluates and run its tests";
    }

    std::string doc() override
    {
        return
#include "flake-check.md"
            ;
    }

    void run(nix::ref<nix::Store> store) override
    {
        const auto previousReadOnly = settings.readOnlyMode;
        const auto previousIFD = evalSettings.enableImportFromDerivation.get();
        Finally restore([&] {
            settings.readOnlyMode = previousReadOnly;
            evalSettings.enableImportFromDerivation = previousIFD;
        });
        if (!build)
            settings.readOnlyMode = true;
        evalSettings.enableImportFromDerivation = build && previousIFD;
        auto state = getEvalState();
        lockFlags.applyNixConfig = true;
        auto flake = lockFlake();
        auto evaluand = rustEvaluandOfLockedFlake(*state, flake);
        const bool keepGoing = settings.getWorkerSettings().keepGoing;
        bool hasErrors = false;
        StringSet omittedSystems;
        std::map<DerivedPath, std::vector<std::string>> names;
        auto reportError = [&](const Error & error) {
            checkInterrupt();
            if (!keepGoing)
                throw error;
            logError(error.info());
            hasErrors = true;
        };
        // Each phase constructs a fresh VM with its own captured/keyed policy.
        // No live VM changes policy, and Hydra never inherits forced IFD values.
        for (bool hydra : {true, false}) {
            evalSettings.enableImportFromDerivation = !hydra && build && previousIFD;
            auto report = rustEvalFlakeCheck(*state, evaluand, hydra, checkAllSystems, keepGoing, !build);
            for (auto & error : report.errors)
                reportError(Error("%s", error));
            omittedSystems.insert(report.omittedSystems.begin(), report.omittedSystems.end());
            for (auto & derivation : report.derivations) {
                if (!derivation.build)
                    continue;
                DerivedPath path = DerivedPath::Built{
                    .drvPath = makeConstantStorePathRef(derivation.drvPath), .outputs = OutputsSpec::All{}};
                names[path].push_back(std::move(derivation.attributePath));
            }
        }
        if (build && !names.empty()) {
            std::vector<DerivedPath> paths;
            for (auto & [path, _] : names)
                paths.push_back(path);
            Activity activity(*logger, lvlInfo, actUnknown, fmt("running %d flake checks", paths.size()));
            // A cached validation report never substitutes for checking builds.
            for (auto & result : store->buildPathsWithResults(paths)) {
                if (auto * failure = result.tryGetFailure()) {
                    auto found = names.find(result.path);
                    if (found == names.end())
                        reportError(
                            Error("build of '%s' failed: %s", result.path.to_string(*store), failure->message()));
                    else
                        for (auto & name : found->second)
                            reportError(Error("failed to build attribute %s: %s", name, failure->message()));
                }
            }
        }
        if (hasErrors)
            throw Error("flake checks failed");
        if (!omittedSystems.empty())
            warn(
                "omitted incompatible systems: %s; use --all-systems to validate them",
                concatStringsSep(", ", omittedSystems));
        logger->log(lvlInfo, "all checks passed");
    }
};

static Strings defaultTemplateAttrPathsPrefixes{"templates."};
static Strings defaultTemplateAttrPaths = {"templates.default", "defaultTemplate"};

struct CmdFlakeInitCommon : virtual Args, EvalCommand
{
    std::string templateUrl = "templates";
    std::filesystem::path destDir;

    const LockFlags lockFlags{.writeLockFile = false};

    CmdFlakeInitCommon()
    {
        addFlag({
            .longName = "template",
            .shortName = 't',
            .description = "The template to use.",
            .labels = {"template"},
            .handler = {&templateUrl},
            .completer = {[&](AddCompletions & completions, size_t, std::string_view prefix) {
                completeFlakeRefWithFragment(
                    completions,
                    getEvalState(),
                    lockFlags,
                    defaultTemplateAttrPathsPrefixes,
                    defaultTemplateAttrPaths,
                    prefix);
            }},
        });
    }

    void run(nix::ref<nix::Store> store) override
    {
        auto flakeDir = absPath(destDir);

        auto evalState = getEvalState();

        auto [templateFlakeRef, templateName] =
            parseFlakeRefWithFragment(fetchSettings, templateUrl, std::filesystem::current_path().string());

        auto installable = InstallableFlake(
            nullptr,
            evalState,
            std::move(templateFlakeRef),
            templateName,
            ExtendedOutputsSpec::Default(),
            defaultTemplateAttrPaths,
            defaultTemplateAttrPathsPrefixes,
            lockFlags);

        auto cursor = installable.getCursor(*evalState);

        auto templateDirAttr = cursor->getAttr("path")->forceValue();
        NixStringContext context;
        auto templateDir = evalState->coerceToPath(noPos, templateDirAttr, context, "");

        std::vector<std::filesystem::path> changedFiles;
        std::vector<std::filesystem::path> conflictedFiles;

        [&](this const auto & copyDir, const SourcePath & from, const std::filesystem::path & to) -> void {
            createDirs(to);

            for (auto & [name, entry] : from.readDirectory()) {
                checkInterrupt();
                auto from2 = from / name;
                auto to2 = to / name;
                auto st = from2.lstat();
                auto to_st = std::filesystem::symlink_status(to2);
                if (st.type == SourceAccessor::tDirectory)
                    copyDir(from2, to2);
                else if (st.type == SourceAccessor::tRegular) {
                    auto contents = from2.readFile();
                    if (std::filesystem::exists(to_st)) {
                        auto contents2 = readFile(to2);
                        if (contents != contents2) {
                            printError(
                                "refusing to overwrite existing file %s\n please merge it manually with '%s'",
                                PathFmt(to2),
                                from2);
                            conflictedFiles.push_back(to2);
                        } else {
                            notice("skipping identical file: %s", from2);
                        }
                        continue;
                    } else
                        writeFile(to2, contents);
                } else if (st.type == SourceAccessor::tSymlink) {
                    auto target = from2.readLink();
                    if (std::filesystem::exists(to_st)) {
                        if (std::filesystem::read_symlink(to2) != target) {
                            printError(
                                "refusing to overwrite existing file %s\n please merge it manually with '%s'",
                                PathFmt(to2),
                                from2);
                            conflictedFiles.push_back(to2);
                        } else {
                            notice("skipping identical file: %s", from2);
                        }
                        continue;
                    } else
                        createSymlink(target, to2);
                } else
                    throw Error(
                        "path '%s' needs to be a symlink, file, or directory but instead is a %s",
                        from2,
                        st.typeString());
                changedFiles.push_back(to2);
                notice("wrote: %s", PathFmt(to2));
            }
        }(templateDir, flakeDir);

        auto hasGit = std::filesystem::exists(std::filesystem::path{flakeDir} / ".git");
        auto hasJj = std::filesystem::exists(std::filesystem::path{flakeDir} / ".jj");

        if (!changedFiles.empty() && hasGit) {
            OsStrings args = {
                OS_STR("-C"),
                flakeDir.native(),
                OS_STR("add"),
                OS_STR("--intent-to-add"),
                OS_STR("--force"),
                OS_STR("--"),
            };
            for (auto & s : changedFiles)
                args.emplace_back(s.native());
            runProgram("git", true, args);
        }

        /* Writing the template does not make the directory buildable: a flake
           source has to have an identity to be fetched at all
           (`libfetchers/path.cc`), and neither an empty directory nor a Git
           tree holding files that are only `--intent-to-add`ed has one. Say
           that here, where the files were just written, rather than leaving it
           to the next command the user runs and a message about a directory
           they did not know they had to version.

           Creating the repository for them is deliberately not done: which
           version control a project uses is the project's decision, not a side
           effect of writing a template into it. */
        if (!changedFiles.empty()) {
            if (hasGit)
                notice(
                    "commit these files before building: until they are in a commit, %s has no revision for a lock "
                    "file to name",
                    PathFmt(flakeDir));
            else if (!hasJj)
                notice(
                    "%s is not under version control, so nothing can build it as a flake yet. Run 'jj init' in it, "
                    "or 'git init' and commit.",
                    PathFmt(flakeDir));
        }

        if (auto welcomeText = cursor->maybeGetAttr("welcomeText")) {
            notice("\n");
            notice(renderMarkdownToTerminal(welcomeText->getString()));
        }

        if (!conflictedFiles.empty())
            throw Error("encountered %d conflicts - see above", conflictedFiles.size());
    }
};

struct CmdFlakeInit : CmdFlakeInitCommon
{
    std::string description() override
    {
        return "create a flake in the current directory from a template";
    }

    std::string doc() override
    {
        return
#include "flake-init.md"
            ;
    }

    CmdFlakeInit()
    {
        destDir = ".";
    }
};

struct CmdFlakeNew : CmdFlakeInitCommon
{
    std::string description() override
    {
        return "create a flake in the specified directory from a template";
    }

    std::string doc() override
    {
        return
#include "flake-new.md"
            ;
    }

    CmdFlakeNew()
    {
        expectArgs({.label = "dest-dir", .handler = {&destDir}, .completer = completePath});
    }
};

struct CmdFlakeClone : FlakeCommand
{
    std::filesystem::path destDir;

    std::string description() override
    {
        return "clone flake repository";
    }

    std::string doc() override
    {
        return
#include "flake-clone.md"
            ;
    }

    CmdFlakeClone()
    {
        addFlag({
            .longName = "dest",
            .shortName = 'f',
            .description = "Clone the flake to path *dest*.",
            .labels = {"path"},
            .handler = {&destDir},
        });
    }

    void run(nix::ref<nix::Store> store) override
    {
        if (destDir.empty())
            throw Error("missing flag '--dest'");

        getFlakeRef().resolve(fetchSettings, *store).input.clone(fetchSettings, *store, destDir);
    }
};

struct CmdFlakeArchive : FlakeCommand, MixJSON, MixDryRun, MixNoCheckSigs
{
    std::optional<StoreReference> dstUri;

    SubstituteFlag substitute = NoSubstitute;

    CmdFlakeArchive()
    {
        addFlag({
            .longName = "to",
            .description = "URI of the destination Nix store",
            .labels = {"store-uri"},
            .handler = {[this](std::string s) { dstUri = StoreReference::parse(s); }},
        });
    }

    std::string description() override
    {
        return "copy a flake and all its inputs to a store";
    }

    std::string doc() override
    {
        return
#include "flake-archive.md"
            ;
    }

    void run(nix::ref<nix::Store> store) override
    {
        auto flake = lockFlake();

        StorePathSet sources;

        auto storePath = store->toStorePath(flake.flake.path.path.abs()).first;

        /* The flake's own tree is a lazy mount: `lockFlake` derived its store
           path and copied nothing (the inputs below go through `fetchToStore`,
           which does copy). Copying a path that was never materialized would
           fail as an invalid source path, so force it through the same road a
           build takes for a source it reads. */
        if (!dryRun)
            getEvalState()->ensureLazyPathCopied(storePath);

        sources.insert(storePath);

        // Rust validates that direct input edges are acyclic.
        std::function<nlohmann::json(NodeId node)> traverse;
        traverse = [&](NodeId node) {
            nlohmann::json jsonObj2 = json ? json::object() : nlohmann::json(nullptr);
            for (auto & [inputName, input] : flake.lockFile.inputs(node)) {
                if (auto inputNode = std::get_if<0>(&input)) {
                    const auto * lockedNode = flake.lockFile.node(*inputNode);
                    std::optional<StorePath> storePath;
                    if (!lockedNode->lockedRef.input.isRelative()) {
                        storePath = dryRun ? lockedNode->lockedRef.input.computeStorePath(*store)
                                           : lockedNode->lockedRef.input.fetchToStore(fetchSettings, *store).first;
                        sources.insert(*storePath);
                    }
                    if (json) {
                        auto & jsonObj3 = jsonObj2[inputName];
                        if (storePath)
                            jsonObj3["path"] = store->printStorePath(*storePath);
                        jsonObj3["inputs"] = traverse(*inputNode);
                    } else
                        traverse(*inputNode);
                }
            }
            return jsonObj2;
        };

        if (json) {
            nlohmann::json jsonRoot = {
                {"path", store->printStorePath(storePath)},
                {"inputs", traverse(flake.lockFile.root)},
            };
            printJSON(jsonRoot);
        } else {
            traverse(flake.lockFile.root);
        }

        if (!dryRun && dstUri) {
            ref<Store> dstStore = openStore(StoreReference{*dstUri});

            copyPaths(*store, *dstStore, sources, NoRepair, checkSigs, substitute);
        }
    }
};

struct CmdFlakeShow : FlakeCommand, MixJSON
{
    bool showLegacy = false;
    bool showAllSystems = false;

    CmdFlakeShow()
    {
        addFlag({
            .longName = "legacy",
            .description = "Show the contents of the `legacyPackages` output.",
            .handler = {&showLegacy, true},
        });
        addFlag({
            .longName = "all-systems",
            .description = "Show the contents of outputs for all systems.",
            .handler = {&showAllSystems, true},
        });
    }

    std::string description() override
    {
        return "show the outputs provided by a flake";
    }

    std::string doc() override
    {
        return
#include "flake-show.md"
            ;
    }

    void run(nix::ref<nix::Store> store) override
    {
        evalSettings.enableImportFromDerivation.setDefault(false);
        auto state = getEvalState();
        auto locked = lockFlake();
        auto document = rustEvalFlakeShow(
            *state,
            rustEvaluandOfLockedFlake(*state, locked),
            showLegacy,
            showAllSystems,
            json,
            std::string(settings.thisSystem.get()));
        auto report = document.render(locked.flake.lockedRef.to_string(), json);
        for (auto & warning : report.warnings)
            logger->warn(warning);
        logger->cout("%s", report.output);
    }
};

struct CmdFlakePrefetch : FlakeCommand, MixJSON
{
    std::optional<std::filesystem::path> outLink;

    CmdFlakePrefetch()
    {
        addFlag({
            .longName = "out-link",
            .shortName = 'o',
            .description = "Create symlink named *path* to the resulting store path.",
            .labels = {"path"},
            .handler = {&outLink},
            .completer = completePath,
        });
    }

    std::string description() override
    {
        return "download the source tree denoted by a flake reference into the Nix store";
    }

    std::string doc() override
    {
        return
#include "flake-prefetch.md"
            ;
    }

    void run(ref<Store> store) override
    {
        auto originalRef = getFlakeRef();
        auto resolvedRef = originalRef.resolve(fetchSettings, *store);
        /* The road an archived input takes (`Input::fetchToStore`): ingestion
           by the accessor's announced tree id when there is one. A plain
           `fetchToStore` of the accessor NAR-ingested a jj tree under a
           second, NAR-named store path, distinct from the path the lazy
           mount derives and `nix flake archive` copies -- two store paths
           for one tree, measured as a prefetched flake missing from the
           store an archive of the same flake had just populated. The store
           object's own record of the bytes is its NAR hash either way,
           which is what the output below reports. */
        auto [storePath, lockedInput] = resolvedRef.input.fetchToStore(fetchSettings, *store);
        auto lockedRef = FlakeRef(std::move(lockedInput), resolvedRef.subdir);
        auto hash = store->queryPathInfo(storePath)->narHash;

        if (json) {
            auto res = nlohmann::json::object();
            res["storePath"] = store->printStorePath(storePath);
            res["hash"] = hash.to_string(HashFormat::SRI, true);
            res["original"] = fetchers::attrsToJSON(resolvedRef.toAttrs());
            res["locked"] = fetchers::attrsToJSON(lockedRef.toAttrs());
            res["locked"].erase("__final"); // internal for now
            printJSON(res);
        } else {
            notice(
                "Downloaded '%s' to '%s' (hash '%s').",
                lockedRef.to_string(),
                store->printStorePath(storePath),
                hash.to_string(HashFormat::SRI, true));
        }

        if (outLink) {
            if (auto store2 = store.dynamic_pointer_cast<LocalFSStore>())
                createOutLinks(*outLink, {BuiltPath::Opaque{storePath}}, *store2);
            else
                throw Error("'--out-link' is not supported for this Nix store");
        }
    }
};

struct CmdFlake : NixMultiCommand
{
    CmdFlake()
        : NixMultiCommand("flake", RegisterCommand::getCommandsFor({"flake"}))
    {
    }

    std::string description() override
    {
        return "manage Nix flakes";
    }

    std::string doc() override
    {
        return
#include "flake.md"
            ;
    }

    void run() override
    {
        experimentalFeatureSettings.require(Xp::Flakes);
        NixMultiCommand::run();
    }
};

static auto rCmdFlake = registerCommand<CmdFlake>("flake");
static auto rCmdFlakeArchive = registerCommand2<CmdFlakeArchive>({"flake", "archive"});
static auto rCmdFlakeCheck = registerCommand2<CmdFlakeCheck>({"flake", "check"});
static auto rCmdFlakeClone = registerCommand2<CmdFlakeClone>({"flake", "clone"});
static auto rCmdFlakeInfo = registerCommand2<CmdFlakeInfo>({"flake", "info"});
static auto rCmdFlakeInit = registerCommand2<CmdFlakeInit>({"flake", "init"});
static auto rCmdFlakeLock = registerCommand2<CmdFlakeLock>({"flake", "lock"});
static auto rCmdFlakeMetadata = registerCommand2<CmdFlakeMetadata>({"flake", "metadata"});
static auto rCmdFlakeNew = registerCommand2<CmdFlakeNew>({"flake", "new"});
static auto rCmdFlakePrefetch = registerCommand2<CmdFlakePrefetch>({"flake", "prefetch"});
static auto rCmdFlakeShow = registerCommand2<CmdFlakeShow>({"flake", "show"});
static auto rCmdFlakeUpdate = registerCommand2<CmdFlakeUpdate>({"flake", "update"});
