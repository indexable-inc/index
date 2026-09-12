#include <nlohmann/json.hpp>
#include <assert.h>
#include <stdint.h>
#include <boost/container/detail/std_fwd.hpp>
#include <boost/core/pointer_traits.hpp>
#include <boost/unordered/detail/foa/table.hpp>
#include <algorithm>
#include <filesystem>
#include <functional>
#include <map>
#include <memory>
#include <optional>
#include <set>
#include <span>
#include <string>
#include <tuple>
#include <utility>
#include <variant>
#include <vector>
#include <format>

#include "nix/util/terminal.hh"
#include "nix/util/ref.hh"
#include "nix/util/environment-variables.hh"
#include "nix/flake/flake.hh"
#include "nix/flake/flake-document.hh"

#include "nix/expr/eval.hh"
#include "nix/expr/eval-cache.hh"
#include "nix/expr/eval-settings.hh"
#include "nix/flake/lockfile.hh"
#include "nix/expr/eval-inline.hh"
#include "nix/store/store-api.hh"
#include "nix/fetchers/fetchers.hh"
#include "nix/util/finally.hh"
#include "nix/fetchers/fetch-settings.hh"
#include "nix/flake/settings.hh"
#include "nix/expr/value-to-json.hh"
#include "nix/fetchers/fetch-to-store.hh"
#include "nix/util/memory-source-accessor.hh"
#include "nix/util/mounted-source-accessor.hh"
#include "nix/fetchers/input-cache.hh"
#include "nix/expr/attr-set.hh"
#include "nix/expr/eval-error.hh"
#include "nix/expr/nixexpr.hh"
#include "nix/expr/symbol-table.hh"
#include "nix/expr/value.hh"
#include "nix/expr/value/context.hh"
#include "nix/fetchers/attrs.hh"
#include "nix/fetchers/registry.hh"
#include "nix/flake/flakeref.hh"
#include "nix/store/path.hh"
#include "nix/util/canon-path.hh"
#include "nix/util/configuration.hh"
#include "nix/util/error.hh"
#include "nix/util/experimental-features.hh"
#include "nix/util/file-system.hh"
#include "nix/util/fmt.hh"
#include "nix/util/hash.hh"
#include "nix/util/logging.hh"
#include "nix/util/pos-idx.hh"
#include "nix/util/pos-table.hh"
#include "nix/util/position.hh"
#include "nix/util/source-path.hh"
#include "nix/util/types.hh"
#include "nix/util/util.hh"

namespace nix {
struct SourceAccessor;

using namespace flake;

namespace flake {

// Rust evaluates a flake document; this library consumes its typed JSON.
FlakeDocumentReader & flakeDocumentReader()
{
    static FlakeDocumentReader reader;
    return reader;
}

static nlohmann::json flakeDocument(EvalState & state, const SourcePath & flakePath)
{
    auto & reader = flakeDocumentReader();
    if (!reader)
        throw Error("no Rust flake document reader is installed for '%s'", flakePath);
    return reader(state, flakePath);
}

// Decode Rust's normalized schema into host fetch/store objects. Declaration
// validation, defaults, implicit inputs and follows parsing belong to Rust.
static fetchers::Attrs fetchAttrsFromDocument(const nlohmann::json & document)
{
    fetchers::Attrs attrs;
    for (auto & [name, field] : document.items()) {
        auto kind = field.at("kind").get<std::string>();
        auto & value = field.at("value");
        if (kind == "string")
            attrs.emplace(name, value.get<std::string>());
        else if (kind == "bool")
            attrs.emplace(name, Explicit<bool>{value.get<bool>()});
        else if (kind == "uint")
            attrs.emplace(name, value.get<uint64_t>());
        else
            throw Error("invalid Rust flake fetch-attribute tag '%s'", kind);
    }
    return attrs;
}

static std::map<FlakeId, FlakeInput>
inputsFromDocument(EvalState & state, const nlohmann::json & inputs, const InputAttrPath & lockRootAttrPath)
{
    std::map<FlakeId, FlakeInput> result;
    for (auto & [name, document] : inputs.items()) {
        FlakeInput input;
        input.isFlake = document.at("is_flake").get<bool>();
        input.overrides = inputsFromDocument(state, document.at("overrides"), lockRootAttrPath);
        auto & follows = document.at("follows");
        if (!follows.is_null()) {
            input.follows = lockRootAttrPath;
            for (auto & component : follows)
                input.follows->push_back(component.get<std::string>());
        }
        auto & reference = document.at("reference");
        if (!reference.is_null()) {
            auto kind = reference.at("kind").get<std::string>();
            auto & value = reference.at("value");
            if (kind == "attrs")
                input.ref = FlakeRef::fromAttrs(state.fetchSettings, fetchAttrsFromDocument(value));
            else if (kind == "url")
                input.ref = parseFlakeRef(state.fetchSettings, value.get<std::string>(), {}, true, input.isFlake, true);
            else if (kind == "implicit")
                input.ref = parseFlakeRef(state.fetchSettings, value.get<std::string>());
            else
                throw Error("invalid Rust flake reference tag '%s'", kind);
        }
        result.emplace(name, std::move(input));
    }
    return result;
}

static ConfigFile configFromDocument(EvalState & state, const nlohmann::json & config, const SourcePath & flakeDir)
{
    ConfigFile result;
    for (auto & [name, field] : config.items()) {
        auto kind = field.at("kind").get<std::string>();
        auto & value = field.at("value");
        if (kind == "string")
            result.settings.emplace(name, value.get<std::string>());
        else if (kind == "path") {
            SourcePath source{flakeDir.accessor, CanonPath(value.get<std::string>(), flakeDir.path)};
            auto path = fetchToStore(state.fetchSettings, *state.store, source, FetchMode::Copy);
            result.settings.emplace(name, state.store->printStorePath(path));
        } else if (kind == "int")
            result.settings.emplace(name, value.get<int64_t>());
        else if (kind == "bool")
            result.settings.emplace(name, Explicit<bool>{value.get<bool>()});
        else if (kind == "strings")
            result.settings.emplace(name, value.get<std::vector<std::string>>());
        else
            throw Error("invalid Rust flake configuration tag '%s'", kind);
    }
    return result;
}

static Flake flakeFromDocument(
    EvalState & state,
    const nlohmann::json & document,
    const FlakeRef & originalRef,
    const FlakeRef & resolvedRef,
    const FlakeRef & lockedRef,
    const SourcePath & flakePath,
    const InputAttrPath & lockRootAttrPath)
{
    Flake flake{
        .originalRef = originalRef,
        .resolvedRef = resolvedRef,
        .lockedRef = lockedRef,
        .path = flakePath,
    };
    auto & description = document.at("description");
    if (!description.is_null())
        flake.description = description.get<std::string>();
    flake.inputs = inputsFromDocument(state, document.at("inputs"), lockRootAttrPath);
    flake.selfAttrs = fetchAttrsFromDocument(document.at("self_attrs"));
    flake.config = configFromDocument(state, document.at("config"), flakePath.parent());
    return flake;
}

static Flake readFlake(
    EvalState & state,
    const FlakeRef & originalRef,
    const FlakeRef & resolvedRef,
    const FlakeRef & lockedRef,
    const SourcePath & rootDir,
    const InputAttrPath & lockRootAttrPath)
{
    auto flakeDir = rootDir / CanonPath(resolvedRef.subdir);
    auto flakePath = flakeDir / "flake.nix";
    return flakeFromDocument(
        state, flakeDocument(state, flakePath), originalRef, resolvedRef, lockedRef, flakePath, lockRootAttrPath);
}

static FlakeRef applySelfAttrs(const FlakeRef & ref, const Flake & flake)
{
    auto newRef(ref);

    for (auto & attr : flake.selfAttrs)
        newRef.input.attrs.insert_or_assign(attr.first, attr.second);

    return newRef;
}

static Flake getFlake(
    EvalState & state,
    const FlakeRef & originalRef,
    fetchers::UseRegistries useRegistries,
    const InputAttrPath & lockRootAttrPath)
{
    // Fetch a lazy tree first.
    auto cachedInput =
        state.inputCache->getAccessor(state.fetchSettings, *state.store, originalRef.input, useRegistries);

    auto subdir = fetchers::maybeGetStrAttr(cachedInput.extraAttrs, "dir").value_or(originalRef.subdir);
    auto resolvedRef = FlakeRef(std::move(cachedInput.resolvedInput), subdir);
    auto lockedRef = FlakeRef(std::move(cachedInput.lockedInput), subdir);

    // Mount before either read so Rust's source and host questions name the
    // same immutable input. Mounting keeps the fetched tree lazy.
    auto flake = readFlake(
        state,
        originalRef,
        resolvedRef,
        lockedRef,
        state.storePath(state.mountInput(lockedRef.input, originalRef.input, cachedInput.accessor)),
        lockRootAttrPath);

    // Re-fetch the tree if necessary.
    auto newLockedRef = applySelfAttrs(lockedRef, flake);

    if (lockedRef != newLockedRef) {
        debug("refetching input '%s' due to self attribute", newLockedRef);
        // FIXME: need to remove attrs that are invalidated by the changed input attrs, such as 'narHash'.
        newLockedRef.input.attrs.erase("narHash");
        newLockedRef.input.attrs.erase("treeHash");
        /* `__final` is likewise invalidated: it asserted that fetching
           would reproduce the previous attribute set exactly, but the
           self attributes just changed the input, possibly to the point
           of being served by another scheme (forge inputs requesting
           submodules are fetched as `git` inputs), and checkLocks()
           pins every attribute of a final input, `type` included. */
        newLockedRef.input.attrs.erase("__final");
        auto cachedInput2 = state.inputCache->getAccessor(
            state.fetchSettings, *state.store, newLockedRef.input, fetchers::UseRegistries::No);
        cachedInput.accessor = cachedInput2.accessor;
        lockedRef = FlakeRef(std::move(cachedInput2.lockedInput), newLockedRef.subdir);
    }

    // Re-parse flake.nix from the store.
    return readFlake(
        state,
        originalRef,
        resolvedRef,
        lockedRef,
        state.storePath(state.mountInput(lockedRef.input, originalRef.input, cachedInput.accessor)),
        lockRootAttrPath);
}

Flake getFlake(EvalState & state, const FlakeRef & originalRef, fetchers::UseRegistries useRegistries)
{
    return getFlake(state, originalRef, useRegistries, {});
}

static LockFile readLockFile(const fetchers::Settings & fetchSettings, const SourcePath & lockFilePath)
{
    return lockFilePath.pathExists() ? LockFile(fetchSettings, lockFilePath.readFile(), fmt("%s", lockFilePath))
                                     : LockFile();
}

/* Compute an in-memory lock file for the specified top-level flake,
   and optionally write it to file, if the flake is writable. */
LockedFlake
lockFlake(const Settings & settings, EvalState & state, const FlakeRef & topRef, const LockFlags & lockFlags)
{
    experimentalFeatureSettings.require(Xp::Flakes);

    auto useRegistries = lockFlags.useRegistries.value_or(settings.useRegistries);
    auto useRegistriesTop = useRegistries ? fetchers::UseRegistries::All : fetchers::UseRegistries::No;
    auto useRegistriesInputs = useRegistries ? fetchers::UseRegistries::Limited : fetchers::UseRegistries::No;

    auto flake = getFlake(state, topRef, useRegistriesTop, {});

    if (lockFlags.applyNixConfig) {
        flake.config.apply(settings);
        state.store->setOptions();
    }

    try {
        if (!state.fetchSettings.allowDirty && lockFlags.referenceLockFilePath) {
            throw Error("reference lock file was provided, but the `allow-dirty` setting is set to false");
        }

        auto oldLockFile =
            readLockFile(state.fetchSettings, lockFlags.referenceLockFilePath.value_or(flake.lockFilePath()));

        debug("old lock file: %s", oldLockFile);

        struct OverrideTarget
        {
            FlakeInput input;
            /* Where the overriding flake sits in its tree (`treePos` in
               `computeLocks`): a relative override is resolved against it. */
            CanonPath treePos;
            std::optional<InputAttrPath> parentInputAttrPath; // FIXME: rename to inputAttrPathPrefix?
        };

        std::map<NonEmptyInputAttrPath, OverrideTarget> overrides;
        std::set<NonEmptyInputAttrPath> explicitCliOverrides;
        std::set<NonEmptyInputAttrPath> overridesUsed;
        std::set<InputAttrPath> updatesUsed;
        std::map<NodeId, SourcePath> nodePaths;

        for (auto & i : lockFlags.inputOverrides) {
            overrides.emplace(
                i.first,
                OverrideTarget{
                    .input = FlakeInput{.ref = i.second},
                    /* Note: any relative overrides
                       (e.g. `--override-input B/C "path:./foo/bar"`)
                       are interpreted relative to the top-level
                       flake. */
                    .treePos = flake.path.path.parent().value(),
                });
            explicitCliOverrides.insert(i.first);
        }

        LockFile newLockFile;

        std::vector<FlakeRef> parents;

        struct OldNode
        {
            const LockFile * graph;
            NodeId id;
        };

        std::function<void(
            const FlakeInputs & flakeInputs,
            NodeId node,
            const InputAttrPath & inputAttrPathPrefix,
            std::optional<OldNode> oldNode,
            const InputAttrPath & followsPrefix,
            const CanonPath & treePos,
            const FlakeRef & parentFlakeRef,
            bool trustLock)>
            computeLocks;

        computeLocks = [&](
                           /* The inputs of this node, either from flake.nix or
                              flake.lock. */
                           const FlakeInputs & flakeInputs,
                           /* The node whose locks are to be updated.*/
                           NodeId node,
                           /* The path to this node in the lock file graph. */
                           const InputAttrPath & inputAttrPathPrefix,
                           /* The old node, if any, from which locks can be
                              copied. */
                           std::optional<OldNode> oldNode,
                           /* The prefix relative to which 'follows' should be
                              interpreted. When a node is initially locked, it's
                              relative to the node's flake; when it's already locked,
                              it's relative to the root of the lock file. */
                           const InputAttrPath & followsPrefix,
                           /* This node's flake directory as a path in the
                              evaluator's mount table (`storeFS`): the store
                              path the enclosing tree is mounted at, plus the
                              directory within it. Relative inputs are
                              resolved against it, so it has to be the
                              directory's position in the tree it LIVES in.
                              For a flake with an identity of its own that is
                              its mount and its subdir. For a relative-path
                              flake it is the directory it was found at inside
                              its ancestor's mount -- not the store path its
                              subtree object is mounted at for evaluation.
                              That object is a root: mounted on its own it has
                              no parent, and `../x` composed from there leaves
                              the store path (landing on `<store>/x`) instead
                              of naming the sibling directory in the same
                              tree. */
                           const CanonPath & treePos,
                           /* The locked ref of the tree `treePos` is in (for
                              a relative path flake, its nearest non-relative
                              ancestor). For messages only: the tree itself is
                              asked through the mount table. */
                           const FlakeRef & parentFlakeRef,
                           bool trustLock) {
            debug("computing lock file node '%s'", printInputAttrPath(inputAttrPathPrefix));

            // One adjacency snapshot for this node, shared by all input lookups.
            auto oldInputs = oldNode ? oldNode->graph->inputs(oldNode->id) : std::map<FlakeId, Edge>{};

            /* Get the overrides (i.e. attributes of the form
               'inputs.nixops.inputs.nixpkgs.url = ...'). */
            auto addOverrides =
                [&](this const auto & addOverrides, const FlakeInput & input, const InputAttrPath & prefix) -> void {
                for (auto & [idOverride, inputOverride] : input.overrides) {
                    auto inputAttrPath = NonEmptyInputAttrPath::append(prefix, idOverride);
                    if (inputOverride.ref || inputOverride.follows)
                        overrides.emplace(
                            inputAttrPath,
                            OverrideTarget{
                                .input = inputOverride,
                                .treePos = treePos,
                                .parentInputAttrPath = inputAttrPathPrefix});
                    addOverrides(inputOverride, inputAttrPath);
                }
            };

            for (auto & [id, input] : flakeInputs) {
                auto inputAttrPath(inputAttrPathPrefix);
                inputAttrPath.push_back(id);
                addOverrides(input, inputAttrPath);
            }

            /* Check whether this input has overrides for a
               non-existent input. */
            for (auto [inputAttrPath, inputOverride] : overrides) {
                auto follow = inputAttrPath.inputName();
                auto inputAttrPath2 = inputAttrPath.parent();
                if (inputAttrPath2 == inputAttrPathPrefix && !flakeInputs.count(follow))
                    warn(
                        "input '%s' has an override for a non-existent input '%s'",
                        printInputAttrPath(inputAttrPathPrefix),
                        follow);
            }

            /* Go over the flake inputs, resolve/fetch them if
               necessary (i.e. if they're new or the flakeref changed
               from what's in the lock file). */
            for (auto & [id, input2] : flakeInputs) {
                auto nonEmptyInputAttrPath = NonEmptyInputAttrPath::append(inputAttrPathPrefix, id);
                auto inputAttrPath = nonEmptyInputAttrPath.get();
                auto inputAttrPathS = printInputAttrPath(inputAttrPath);
                debug("computing input '%s'", inputAttrPathS);

                try {

                    /* Do we have an override for this input from one of the
                       ancestors? */
                    auto i = overrides.find(nonEmptyInputAttrPath);
                    bool hasOverride = i != overrides.end();
                    bool hasCliOverride = explicitCliOverrides.contains(nonEmptyInputAttrPath);
                    if (hasOverride)
                        overridesUsed.insert(nonEmptyInputAttrPath);
                    auto input = hasOverride ? i->second.input : input2;

                    /* Resolve relative 'path:' inputs against the position
                       of the overrider. */
                    auto overriddenTreePos = hasOverride ? i->second.treePos : treePos;

                    /* Respect the "flakeness" of the input even if we
                       override it. */
                    if (hasOverride)
                        input.isFlake = input2.isFlake;

                    /* Resolve 'follows' later (since it may refer to an input
                       path we haven't processed yet. */
                    if (input.follows) {
                        InputAttrPath target;

                        target.insert(target.end(), input.follows->begin(), input.follows->end());

                        debug("input '%s' follows '%s'", inputAttrPathS, printInputAttrPath(target));
                        newLockFile.setInput(node, id, target);
                        continue;
                    }

                    if (!input.ref)
                        input.ref =
                            FlakeRef::fromAttrs(state.fetchSettings, {{"type", "indirect"}, {"id", std::string(id)}});

                    auto overriddenParentPath =
                        input.ref->input.isRelative()
                            ? std::optional<InputAttrPath>(
                                  hasOverride ? i->second.parentInputAttrPath : inputAttrPathPrefix)
                            : std::nullopt;

                    /* A relative input is the directory it names inside the
                       declaring flake's tree, read through that tree's
                       accessor: nothing is fetched, and the lock entry is
                       the literal path plus `parent`, with no hash of its
                       own, because the parent's identity already fixes
                       every byte under it (a stale child lock cannot exist:
                       when the parent moves, the child moves with it).

                       When the parent's tree comes from a Merkle object
                       store, the directory is an object in its own right
                       with an id of its own (`getSubtree`), and that id is
                       the same one the directory has committed at the root
                       of any other repository. Mount it as its own store
                       object: the child then evaluates at the store path its
                       content has everywhere, and forcing it materializes
                       the subtree, not the whole parent. The Input handed to
                       `mountInput` is a copy: whatever identity attribute the
                       mount records belongs to that store object, not to the
                       lock, which keeps only the parent-relative path. A
                       parent without subtree objects (a plain directory, a
                       NAR) keeps the child at a subpath of its own store
                       object.

                       The subtree is asked of the evaluator's own mount table
                       (`storeFS`), never of `rootFS`, and that is load-bearing.
                       Outside pure mode `rootFS` is a union of the real
                       filesystem over `storeFS`, answered by the first layer
                       that has the path. The parent's store path is a lazy
                       mount: it exists on disk only once something forced its
                       copy. Before that, the store layer answers and names the
                       subtree object; after it, the real filesystem answers
                       and names nothing (a directory on disk has no subtree
                       objects), and the child would silently drop to a
                       subpath of the parent, at a different outPath from the
                       one the same flake had one evaluation earlier, with
                       rc=0 both times. `storeFS` holds exactly the mounts this
                       evaluation made, keyed by store path, and resolves the
                       nearest one (`MountedSourceAccessor::resolve`), which is
                       the parent input's accessor whatever subdirectory the
                       parent flake sits in; its answer is a function of the
                       input, not of the store's state. (`UnionSourceAccessor`
                       also refuses to name a subtree present in two layers,
                       so no other caller can get a state-dependent answer
                       through `rootFS` either.) */
                    struct ResolvedRelative
                    {
                        /* What the input evaluates as: the subtree object's
                           own mount, or a subpath of the parent's. */
                        SourcePath path;
                        /* Where the directory is in the enclosing tree, in
                           mount-table coordinates: the base for the
                           directory's own relative inputs and for its
                           metadata. */
                        CanonPath treePos;
                    };

                    auto resolveRelativePath = [&]() -> std::optional<ResolvedRelative> {
                        auto relativePath = input.ref->input.isRelative();
                        if (!relativePath)
                            return std::nullopt;

                        /* The mount the declaring flake's directory lies in.
                           `storeFS` has a root mount (an empty accessor), so
                           the search stops short of it: a directory outside
                           every input mount has no tree to compose in. */
                        auto mountRoot = overriddenTreePos;
                        std::shared_ptr<SourceAccessor> mount;
                        while (!mountRoot.isRoot()) {
                            if ((mount = state.storeFS->getMount(mountRoot)))
                                break;
                            mountRoot.pop();
                        }
                        if (!mount)
                            throw Error(
                                "bug in Nix: the directory of flake '%s' (%s) is not inside any mounted source tree",
                                parentFlakeRef,
                                overriddenTreePos);

                        /* Compose lexically, but refuse to leave the tree
                           instead of letting `CanonPath` normalise `..` past
                           the mount point. Above the mount the composed path
                           names another store path, and above `/` it names
                           the store root; either would then be read from
                           the real filesystem with rc=0, or refused for the
                           wrong reason (pure evaluation) with the flake never
                           named. */
                        auto components = [](const CanonPath & path) {
                            long n = 0;
                            for (auto component : path) {
                                (void) component;
                                ++n;
                            }
                            return n;
                        };
                        auto depth = components(overriddenTreePos) - components(mountRoot);
                        /* `const auto &`: libc++'s path iterator yields a
                           `path` by value, libstdc++'s a `const path &`. */
                        for (const auto & component : *relativePath) {
                            if (component == "..") {
                                if (depth == 0)
                                    throw Error(
                                        "relative path input '%s' of flake '%s' names '%s', which is outside the "
                                        "tree of that flake: '%s' climbs above the tree's root. A relative path is "
                                        "a directory of the flake's own tree, locked by the flake; a directory "
                                        "outside it is a tree of its own and is named as one (a 'jj+file' or "
                                        "'git+file' input).",
                                        inputAttrPathS,
                                        parentFlakeRef,
                                        relativePath->string(),
                                        relativePath->string());
                                --depth;
                            } else if (component != "." && !component.empty())
                                ++depth;
                        }
                        auto composed = CanonPath(relativePath->string(), overriddenTreePos);

                        /* Only a DIRECTORY is a subtree. A relative input may
                           also name a file (`E.url = "./foo.nix"` with
                           `flake = false`): a file is not a tree, has no
                           object of its own on any road, and is read as a
                           path of the parent's tree, whose identity already
                           fixes its bytes -- asking `getSubtree` for it would
                           be refused as "not a directory" instead of being
                           served. A missing path falls through the same way
                           and surfaces when it is read, with its name in the
                           error. */
                        auto stat = state.storeFS->maybeLstat(composed);
                        if (stat && stat->type == SourceAccessor::tDirectory) {
                            if (auto subtree = state.storeFS->getSubtree(composed)) {
                                auto mounted = input.ref->input;
                                return ResolvedRelative{
                                    .path = state.storePath(state.mountInput(mounted, input.ref->input, ref(subtree))),
                                    .treePos = composed,
                                };
                            }

                            /* No subtree object: the licence to address the
                               directory as `<parent>/sub` (`SourceAccessor::
                               getSubtree`). It is only a licence for a parent
                               whose store path is a hash of its own bytes. A
                               parent mounted under a tree id has subtree
                               objects in the repository it came from; a mount
                               that announces the id and still names no
                               subtree is a flattened store object standing in
                               for that repository (jj.cc, `storeObjectFor`),
                               and composing here would give the directory a
                               second identity beside the one its own id gives
                               it everywhere else -- a different outPath for
                               the same flake depending on which source served
                               the parent, with rc=0 both ways. Refuse, naming
                               what is missing. */
                            if (mount->knownTreeRoot)
                                throw Error(
                                    "cannot resolve relative input '%s' of flake '%s': the flake is served from "
                                    "store object '%s', which is addressed by Jujutsu tree id %s but, being a "
                                    "flattened copy, cannot name the id of the directory '%s' inside it, and that "
                                    "id is the input's identity. The repository the flake was locked from is "
                                    "required to read it; bring it back where the lock names it (%s).",
                                    inputAttrPathS,
                                    parentFlakeRef,
                                    mountRoot,
                                    mount->knownTreeRoot->id.to_string(HashFormat::SRI, true),
                                    relativePath->string(),
                                    parentFlakeRef.input.to_string());
                        }
                        return ResolvedRelative{.path = state.rootPath(composed), .treePos = composed};
                    };

                    /* Resolved once: the same directory is read as the
                       input's flake and stamped with the tree's metadata. A
                       relative input never takes the kept-lock branch below
                       (`deferToChildLock`), so resolving before that branch
                       resolves nothing that would not have been resolved. */
                    auto resolved = resolveRelativePath();

                    /* A relative path input has no timestamp of its own,
                       but the parent's tree knows one: the submodule commit
                       time when the path is a submodule mount, else the
                       enclosing tree's own last-modified. Stamp it on the
                       locked ref so it reaches the lock file and the
                       input's sourceInfo.lastModified like any other
                       input's (indexable-inc/index#3737). */
                    auto stampRelativeTreeMetadata = [&](FlakeRef & lockedRef, const LockedNode * oldLock) {
                        if (!resolved)
                            return;
                        assert(lockedRef.input.isRelative());
                        /* The tree is asked through the mount table at the
                           position the resolution used, so an override
                           (resolved against the overrider's position) and an
                           input declared in place get the same answer from
                           the same accessor. The flake's own source path
                           would be useless here: it may be a flattened store
                           copy in root-filesystem coordinates that lost the
                           fetcher's tree metadata (submodule mounts and
                           their commit info). */
                        auto & treePath = resolved->treePos;
                        /* A position at the root of a mount (`..` climbing
                           back to the tree the flake lives in, or `./.`) is
                           the parent input's own tree: its rev IS the
                           parent's lock identity, and copying it onto the
                           child's entry would duplicate that identity and
                           rewrite the lock whenever the parent moves --
                           measured as `path:../?lastModified=...&rev=...`
                           churning between two evaluations of an unchanged
                           tree. */
                        if (state.storeFS->getMount(treePath))
                            return;
                        /* Only a path that is itself a pinned tree in the
                           parent (a submodule mount root, marked by having a
                           rev) gets stamped: its metadata changes only when
                           the parent moves the gitlink, so locks stay
                           byte-stable across unrelated parent commits. A
                           plain subdirectory shares the parent's history and
                           stamping its time would churn the lock on every
                           parent commit for a value derivable from the
                           parent itself. When the parent's tree came from a
                           flattened store copy that knows no mounts, fall
                           back to the previous lock's values so stamps never
                           flap between fetch modes. */
                        if (auto rev = state.storeFS->getRev(treePath)) {
                            lockedRef.input.attrs.insert_or_assign("rev", rev->gitRev());
                            auto lastModified = state.storeFS->getLastModified(treePath);
                            /* 0 is the fetchers' "unknown" value, not a real
                               time; an epoch-0 stamp is worse than none. */
                            if (lastModified && *lastModified > 0)
                                lockedRef.input.attrs.insert_or_assign("lastModified", uint64_t(*lastModified));
                        } else if (oldLock && oldLock->originalRef.canonicalize() == input.ref->canonicalize()) {
                            if (auto prev = fetchers::maybeGetStrAttr(oldLock->lockedRef.input.attrs, "rev"))
                                lockedRef.input.attrs.insert_or_assign("rev", *prev);
                            if (auto prev = fetchers::maybeGetIntAttr(oldLock->lockedRef.input.attrs, "lastModified"))
                                lockedRef.input.attrs.insert_or_assign("lastModified", *prev);
                        }
                    };

                    /* Get the input flake, resolve 'path:./...'
                       flakerefs relative to the parent flake. */
                    auto getInputFlake = [&](const FlakeRef & ref, const fetchers::UseRegistries useRegistries) {
                        if (resolved) {
                            return readFlake(state, ref, ref, ref, resolved->path, inputAttrPath);
                        } else {
                            return getFlake(state, ref, useRegistries, inputAttrPath);
                        }
                    };

                    /* The position a child flake's own inputs are resolved
                       against: a relative child stays in the enclosing tree
                       at the directory it was found at (plus its subdir); a
                       child with an identity of its own starts at its mount. */
                    auto childTreePos = [&](const Flake & inputFlake) {
                        if (resolved)
                            return resolved->treePos / CanonPath(inputFlake.lockedRef.subdir);
                        return inputFlake.path.path.parent().value();
                    };

                    /* Do we have an entry in the existing lock file?
                       And the input is not in updateInputs? */
                    const LockedNode * oldLock = nullptr;
                    std::optional<OldNode> oldLockNode;

                    updatesUsed.insert(inputAttrPath);

                    if (oldNode && !lockFlags.inputUpdates.count(nonEmptyInputAttrPath)) {
                        if (auto oldEdge = get(oldInputs, id))
                            if (auto oldId = std::get_if<NodeId>(&*oldEdge)) {
                                oldLock = oldNode->graph->node(*oldId);
                                oldLockNode = OldNode{oldNode->graph, *oldId};
                            }
                    }

                    /* A relative path input, flake or not, is never kept
                       from the old lock; it is re-resolved on every
                       operation. Two reasons, and either alone would do.

                       Sparse lock semantics (NixOS/nix#7730): such an input
                       lives inside the parent's own source tree, so the
                       parent's pin already fixes its content and resolving
                       it is free (it is the parent's subtree). The copy of a
                       relative FLAKE's transitive locks in our lock file is
                       therefore not authoritative; the child's own flake.nix
                       and flake.lock are, so they are re-read.

                       Identity: `callFlake` takes a relative node's tree from
                       `nodePaths`, which only the fresh branch below records.
                       The kept branch records nothing, and call-flake.nix
                       would then have to invent the tree as
                       `<parent>/<path>` -- a subpath of the parent's store
                       object, where the fresh branch mounted the subtree
                       object at its own store path. One lock file, two
                       outPaths for the same input, alternating with whether
                       the lock existed when the evaluation started. So a
                       relative non-flake input takes the fresh branch too,
                       and call-flake.nix refuses a relative node it was
                       handed no tree for.

                       If the child is unchanged this reproduces the exact
                       same nodes, so in-sync lock files stay byte-identical. */
                    auto deferToChildLock = (bool) input.ref->input.isRelative();

                    if (oldLock && !deferToChildLock && oldLock->originalRef.canonicalize() == input.ref->canonicalize()
                        && oldLock->parentInputAttrPath == overriddenParentPath && !hasCliOverride) {
                        debug("keeping existing input '%s'", inputAttrPathS);

                        /* Copy the input from the old lock since its flakeref
                           didn't change and there is no override from a
                           higher level flake. */
                        auto childNode = newLockFile.addNode(
                            oldLock->lockedRef, oldLock->originalRef, oldLock->isFlake, oldLock->parentInputAttrPath);

                        newLockFile.setInput(node, id, childNode);

                        /* If we have this input in updateInputs, then we
                           must fetch the flake to update it. */
                        auto lb = lockFlags.inputUpdates.lower_bound(nonEmptyInputAttrPath);

                        auto mustRefetch = lb != lockFlags.inputUpdates.end() && lb->get().size() > inputAttrPath.size()
                                           && std::equal(inputAttrPath.begin(), inputAttrPath.end(), lb->get().begin());

                        FlakeInputs fakeInputs;

                        if (!mustRefetch) {
                            /* No need to fetch this flake, we can be
                               lazy. However there may be new overrides on the
                               inputs of this flake, so we need to check
                               those. */
                            for (auto & i : oldLockNode->graph->inputs(oldLockNode->id)) {
                                if (auto lockedId = std::get_if<NodeId>(&i.second)) {
                                    auto lockedNode = oldLockNode->graph->node(*lockedId);
                                    /* A kept flake whose lock subtree contains
                                       a relative path input, flake or not,
                                       cannot be handled lazily: resolving that
                                       input needs the kept flake's real source
                                       tree rather than the parent's
                                       (NixOS/nix#14762) -- the fake-input
                                       recursion below composes against
                                       `treePos`, the PARENT's position, so a
                                       relative path resolved there would
                                       name a directory of the wrong flake --
                                       and the child-lock deferral above needs
                                       the child's actual flake.nix and
                                       flake.lock. Refetch the kept flake; its
                                       lockedRef is pinned, so this is cached
                                       and deterministic. */
                                    if (lockedNode->lockedRef.input.isRelative()) {
                                        mustRefetch = true;
                                        break;
                                    }
                                    fakeInputs.emplace(
                                        i.first,
                                        FlakeInput{
                                            .ref = lockedNode->originalRef,
                                            .isFlake = lockedNode->isFlake,
                                        });
                                } else if (auto follows = std::get_if<1>(&i.second)) {
                                    if (!trustLock) {
                                        // It is possible that the flake has changed,
                                        // so we must confirm all the follows that are in the lock file are also in the
                                        // flake.
                                        auto overridePath =
                                            NonEmptyInputAttrPath::append(nonEmptyInputAttrPath, i.first);
                                        auto o = overrides.find(overridePath);
                                        // If the override disappeared, we have to refetch the flake,
                                        // since some of the inputs may not be present in the lock file.
                                        if (o == overrides.end()) {
                                            mustRefetch = true;
                                            // There's no point populating the rest of the fake inputs,
                                            // since we'll refetch the flake anyways.
                                            break;
                                        }
                                    }
                                    auto absoluteFollows(followsPrefix);
                                    absoluteFollows.insert(absoluteFollows.end(), follows->begin(), follows->end());
                                    fakeInputs.emplace(
                                        i.first,
                                        FlakeInput{
                                            .follows = absoluteFollows,
                                        });
                                }
                            }
                        }

                        if (mustRefetch) {
                            auto inputFlake = getInputFlake(oldLock->lockedRef, useRegistriesInputs);
                            nodePaths.emplace(childNode, inputFlake.path.parent());
                            computeLocks(
                                inputFlake.inputs,
                                childNode,
                                inputAttrPath,
                                oldLockNode,
                                followsPrefix,
                                childTreePos(inputFlake),
                                resolved ? parentFlakeRef : inputFlake.lockedRef,
                                false);
                        } else {
                            computeLocks(
                                fakeInputs,
                                childNode,
                                inputAttrPath,
                                oldLockNode,
                                followsPrefix,
                                treePos,
                                parentFlakeRef,
                                true);
                        }

                    } else {
                        /* We need to create a new lock file entry. So fetch
                           this input. */
                        debug("creating new input '%s'", inputAttrPathS);

                        if (!lockFlags.allowUnlocked && !input.ref->input.isLocked(state.fetchSettings)
                            && !input.ref->input.isRelative())
                            throw Error("cannot update unlocked flake input '%s' in pure mode", inputAttrPathS);

                        /* Note: in case of an --override-input, we use
                            the *original* ref (input2.ref) for the
                            "original" field, rather than the
                            override. This ensures that the override isn't
                            nuked the next time we update the lock
                            file. That is, overrides are sticky unless you
                            use --no-write-lock-file. */
                        auto inputIsOverride = explicitCliOverrides.contains(nonEmptyInputAttrPath);
                        auto ref = (input2.ref && inputIsOverride) ? *input2.ref : *input.ref;

                        if (input.isFlake) {
                            auto inputFlake = getInputFlake(
                                *input.ref, inputIsOverride ? fetchers::UseRegistries::All : useRegistriesInputs);

                            stampRelativeTreeMetadata(inputFlake.lockedRef, oldLock);

                            auto childNode = newLockFile.addNode(inputFlake.lockedRef, ref, true, overriddenParentPath);

                            newLockFile.setInput(node, id, childNode);

                            /* Guard against circular flake imports. */
                            for (auto & parent : parents)
                                if (parent == *input.ref)
                                    throw Error("found circular import of flake '%s'", parent);
                            parents.push_back(*input.ref);
                            Finally cleanup([&]() { parents.pop_back(); });

                            /* Recursively process the inputs of this
                               flake, using its own lock file. */
                            nodePaths.emplace(childNode, inputFlake.path.parent());
                            auto childLocks = readLockFile(state.fetchSettings, inputFlake.lockFilePath());
                            computeLocks(
                                inputFlake.inputs,
                                childNode,
                                inputAttrPath,
                                OldNode{&childLocks, childLocks.root},
                                inputAttrPath,
                                childTreePos(inputFlake),
                                resolved ? parentFlakeRef : inputFlake.lockedRef,
                                false);
                        }

                        else {
                            auto [path, lockedRef] = [&]() -> std::tuple<SourcePath, FlakeRef> {
                                // Handle non-flake 'path:./...' inputs.
                                if (resolved) {
                                    return {resolved->path, *input.ref};
                                } else {
                                    auto cachedInput = state.inputCache->getAccessor(
                                        state.fetchSettings, *state.store, input.ref->input, useRegistriesInputs);

                                    auto lockedRef = FlakeRef(std::move(cachedInput.lockedInput), input.ref->subdir);

                                    return {
                                        state.storePath(
                                            state.mountInput(lockedRef.input, input.ref->input, cachedInput.accessor)),
                                        lockedRef};
                                }
                            }();

                            stampRelativeTreeMetadata(lockedRef, oldLock);

                            auto childNode = newLockFile.addNode(lockedRef, ref, false, overriddenParentPath);

                            nodePaths.emplace(childNode, path);

                            newLockFile.setInput(node, id, childNode);
                        }
                    }

                } catch (Error & e) {
                    e.addTrace({}, "while updating the flake input '%s'", inputAttrPathS);
                    throw;
                }
            }
        };

        nodePaths.emplace(newLockFile.root, flake.path.parent());

        computeLocks(
            flake.inputs,
            newLockFile.root,
            {},
            lockFlags.recreateLockFile ? std::nullopt : std::optional(OldNode{&oldLockFile, oldLockFile.root}),
            {},
            flake.path.path.parent().value(),
            flake.lockedRef,
            false);

        for (auto & i : lockFlags.inputOverrides)
            if (!overridesUsed.count(i.first))
                warn(
                    "the flag '--override-input %s %s' does not match any input",
                    printInputAttrPath(i.first),
                    i.second);

        for (auto & i : lockFlags.inputUpdates)
            if (!updatesUsed.count(i))
                warn("'%s' does not match any input of this flake", printInputAttrPath(i));

        /* Check 'follows' inputs. */
        newLockFile.check();

        debug("new lock file: %s", newLockFile);

        auto sourcePath = topRef.input.getSourcePath();

        /* Check whether we need to / can write the new lock file. */
        if (newLockFile != oldLockFile || lockFlags.outputLockFilePath) {

            auto diff = LockFile::diff(oldLockFile, newLockFile);

            if (lockFlags.writeLockFile) {
                if (sourcePath || lockFlags.outputLockFilePath) {
                    if (auto unlockedInput = newLockFile.isUnlocked(state.fetchSettings)) {
                        if (lockFlags.failOnUnlocked)
                            throw Error(
                                "Not writing lock file of flake '%s' because it has an unlocked input ('%s'). "
                                "Use '--allow-dirty-locks' to allow this anyway.",
                                topRef,
                                *unlockedInput);
                        if (state.fetchSettings.warnDirty)
                            warn(
                                "not writing lock file of flake '%s' because it has an unlocked input ('%s')",
                                topRef,
                                *unlockedInput);
                    } else {
                        if (!lockFlags.updateLockFile)
                            throw Error(
                                "flake '%s' requires lock file changes but they're not allowed due to '--no-update-lock-file'",
                                topRef);

                        auto newLockFileS = fmt("%s\n", newLockFile);

                        if (lockFlags.outputLockFilePath) {
                            if (lockFlags.commitLockFile)
                                throw Error("'--commit-lock-file' and '--output-lock-file' are incompatible");
                            writeFile(*lockFlags.outputLockFilePath, newLockFileS);
                        } else {
                            /* Writing the lock file into the source mutates
                               it. When the source is identified by a commit,
                               that mutation leaves nothing to lock to: the
                               write would succeed, and the re-read further
                               down would then refuse the flake it had just
                               been asked to lock. Ask for the commit up
                               front rather than failing halfway through. */
                            if (topRef.input.putFileRequiresCommit() && !lockFlags.commitLockFile)
                                throw Error(
                                    "refusing to write the lock file of flake '%s' into its source, because that "
                                    "source is identified by a commit: writing into it would leave it with "
                                    "uncommitted changes and no revision to lock to.\n"
                                    "Pass '--commit-lock-file' to commit the lock file as part of this update, "
                                    "or '--no-write-lock-file' to evaluate without writing it, "
                                    "or keep the flake in a jj workspace, where the snapshot is the commit and "
                                    "neither flag is needed.",
                                    topRef);

                            auto relPath = (topRef.subdir == "" ? "" : topRef.subdir + "/") + "flake.lock";
                            auto outputLockFilePath = *sourcePath / relPath;

                            bool lockFileExists = pathExists(outputLockFilePath);

                            auto s = chomp(diff);
                            if (lockFileExists) {
                                if (s.empty())
                                    warn("updating lock file %s", PathFmt(outputLockFilePath));
                                else
                                    warn("updating lock file %s:\n%s", PathFmt(outputLockFilePath), s);
                            } else
                                warn("creating lock file %s: \n%s", PathFmt(outputLockFilePath), s);

                            std::optional<std::string> commitMessage = std::nullopt;

                            if (lockFlags.commitLockFile) {
                                std::string cm;

                                cm = settings.commitLockFileSummary.get();

                                if (cm == "") {
                                    cm = fmt("%s: %s", relPath, lockFileExists ? "Update" : "Add");
                                }

                                cm += "\n\nFlake lock file updates:\n\n";
                                cm += filterANSIEscapes(diff, true);
                                commitMessage = cm;
                            }

                            topRef.input.putFile(
                                CanonPath((topRef.subdir == "" ? "" : topRef.subdir + "/") + "flake.lock"),
                                newLockFileS,
                                commitMessage);

                            flake.lockFilePath().invalidateCache();
                        }

                        /* Rewriting the lockfile changed the top-level
                           repo, so we should re-read it. FIXME: we could
                           also just clear the 'rev' field... */
                        auto prevLockedRef = flake.lockedRef;
                        flake = getFlake(state, topRef, useRegistriesTop);

                        if (lockFlags.commitLockFile && flake.lockedRef.input.getRev()
                            && prevLockedRef.input.getRev() != flake.lockedRef.input.getRev())
                            warn("committed new revision '%s'", flake.lockedRef.input.getRev()->gitRev());
                    }
                } else
                    throw Error(
                        "cannot write modified lock file of flake '%s' (use '--no-write-lock-file' to ignore)", topRef);
            } else {
                warn("not writing modified lock file of flake '%s':\n%s", topRef, chomp(diff));
                flake.forceDirty = true;
            }
        }

        return LockedFlake{
            .flake = std::move(flake), .lockFile = std::move(newLockFile), .nodePaths = std::move(nodePaths)};

    } catch (Error & e) {
        e.addTrace({}, "while updating the lock file of flake '%s'", flake.lockedRef.to_string());
        throw;
    }
}

std::string_view callFlakeSource()
{
    static const std::string source =
#include "call-flake.nix.gen.hh" // IWYU pragma: keep
        ;
    return source;
}

std::optional<Fingerprint> LockedFlake::getFingerprint(Store & store, const fetchers::Settings & fetchSettings) const
{
    if (lockFile.isUnlocked(fetchSettings))
        return std::nullopt;

    auto fingerprint = flake.lockedRef.input.getFingerprint(store);
    if (!fingerprint)
        return std::nullopt;

    *fingerprint += fmt(";%s;%s", flake.lockedRef.subdir, lockFile);

    /* Include revCount and lastModified because they're not
       necessarily implied by the content fingerprint (e.g. for
       tarball flakes) but can influence the evaluation result. */
    if (auto revCount = flake.lockedRef.input.getRevCount())
        *fingerprint += fmt(";revCount=%d", *revCount);
    if (auto lastModified = flake.lockedRef.input.getLastModified())
        *fingerprint += fmt(";lastModified=%d", *lastModified);

    // FIXME: as an optimization, if the flake contains a lock file
    // and we haven't changed it, then it's sufficient to use
    // flake.sourceInfo.storePath for the fingerprint.
    return hashString(HashAlgorithm::SHA256, *fingerprint);
}

Flake::~Flake() {}

ref<eval_cache::EvalCache> openEvalCache(EvalState & state, ref<const LockedFlake> lockedFlake)
{
    state.requireBackendCanServe();
}

} // namespace flake

} // namespace nix
