#include "nix/store/store-api.hh"
#include "nix/store/local-fs-store.hh"
#include "nix/expr/eval.hh"
#include "nix/util/file-system.hh"
#include "nix/util/mounted-source-accessor.hh"
#include "nix/fetchers/fetch-to-store.hh"
#include "nix/fetchers/fetchers.hh"

namespace nix {

SourcePath EvalState::rootPath(CanonPath path)
{
    return {rootFS, std::move(path)};
}

SourcePath EvalState::rootPath(std::string_view path)
{
    return {rootFS, CanonPath(absPath(path).string())};
}

SourcePath EvalState::storePath(const StorePath & path)
{
    return {rootFS, CanonPath{store->printStorePath(path)}};
}

/* The content-addressing method a mounted input is ingested with, decided
   from the accessor alone so that the mount (`mountInput`) and the forced
   copy (`ensureLazyPathCopied`) cannot disagree: the two must land on one
   store path, and the accessor is the only state both sites share.

   An accessor that reads straight out of jj's content-addressed object
   store knows its root's tree id a priori (jj maintained it while
   snapshotting), and `Raw::JjTree` addresses store objects by exactly that
   id: the store path with zero file reads. Everything else is NAR-hashed on
   the walk.

   That includes git, deliberately. A git input is locked by `narHash`
   (`Input::fetchToStore`, the road `nix flake prefetch` and `nix flake
   archive` take, and every lock file in existence), so a mount that
   addressed the same input by its git tree id would give one input two
   store paths depending on the road, and would have to refuse every lock
   it was handed for carrying a hash it cannot check -- a refusal `nix flake
   update` could never clear, since the update writes `narHash` again. One
   identity per input: git's is the NAR hash. Git is the boundary bridge;
   tree-id addressing is jj's. */
static ContentAddressMethod ingestionMethodFor(const SourceAccessor & accessor)
{
    return accessor.knownTreeRoot ? ContentAddressMethod::Raw::JjTree : ContentAddressMethod::Raw::NixArchive;
}

void EvalState::ensureLazyPathCopied(const StorePath & path)
{
    if (settings.readOnlyMode)
        return;

    auto mount = storeFS->getMount(CanonPath(store->printStorePath(path)));
    if (!mount)
        return;

    /* TODO: We could memoise this in-memory if necessary. */
    auto storePath = fetchToStore(
        fetchSettings,
        *store,
        SourcePath{ref(mount)},
        /* Force a copy: mountInput only computed the store path. */
        FetchMode::Copy,
        path.name(),
        ingestionMethodFor(*mount),
        nullptr,
        repair);

    /* This can happen if the source gets modified by another process while we are evaluaing
       from it. Alternatively, the caching might be unsound and fetcher cache is poisoned somehow.
       See https://github.com/NixOS/nix/issues/14317. */
    if (storePath != path) {
        throw Error(
            (unsigned int) 102,
            "store path ('%1%') was hashed to avoid a full copy at first, but upon reading it again, the contents have changed ('%2%'), so we can not proceed. Make sure files do not change during evaluation",
            store->printStorePath(path),
            store->printStorePath(storePath));
    }
}

void EvalState::ensureLazyPathsCopied(const NixStringContext & context)
{
    for (const auto & c : context)
        if (auto * o = std::get_if<NixStringContextElem::Opaque>(&c.raw))
            /* TODO: This could be done in parallel. */
            ensureLazyPathCopied(o->path);
}

StorePath
EvalState::mountInput(fetchers::Input & input, const fetchers::Input & originalInput, ref<SourceAccessor> accessor)
{
    /* Lazy is the only mode: the input's tree is mounted at its store path
       inside the evaluator and materialised only when something forces it
       (`ensureLazyPathCopied`). That is sound only when the tree cannot
       change under the evaluation. An accessor reading a content-addressed
       object cannot change; a live directory on the filesystem can, and a
       writer landing mid-evaluation would put two states into one
       evaluation and then fail the forced copy with a store path mismatch.

       A materialised store object is the one filesystem-backed exception,
       because the store keeps it immutable. Ask the store for its REAL
       directory rather than using `isInStore`, which compares against the
       logical `storeDir`: under a chroot store (`nix --store /x`) the two
       differ, and comparing against the logical one would refuse every
       store object such a store serves.

       Announcing a tree id does not exempt an accessor here. An accessor
       that both hands out a live filesystem path and claims a fixed id is
       the unsound combination, not a safe one: the id would go on
       addressing bytes that can still change.

       This is an INVARIANT, not the user-facing refusal. By the time an
       input reaches this line its fetcher has already had its chance to
       refuse in terms of the input the user actually wrote; reaching here
       means a fetcher served a mutable directory outside the store, which
       is a defect in that fetcher. The remedy is named anyway, because it
       is the same one either way. */
    auto isImmutableStoreObject = [&](const std::filesystem::path & physical) {
        if (auto * fsStore = dynamic_cast<LocalFSStore *>(&*store))
            return isDirOrInDir(physical, fsStore->getRealStoreDir());
        return store->isInStore(physical.string());
    };

    if (auto physical = accessor->getPhysicalPath(CanonPath::root); physical && !isImmutableStoreObject(*physical))
        throw Error(
            "input '%s' was served as a mutable directory (%s) outside the store, which cannot be mounted as a "
            "flake source because its contents could change during the evaluation. "
            "Put it in a Jujutsu repository ('jj init' in that directory) and reference it as 'jj+file://%s'.",
            originalInput.to_string(),
            physical->string(),
            physical->string());

    auto method = ingestionMethodFor(*accessor);

    /* A NAR hash can only be checked by NAR-ingesting the tree, which a
       tree-addressed mount never does; verifying nothing and going on would
       be the silent kind of wrong, so the promise is refused with the fix
       named.

       Only a hand-written promise can get here. The jj scheme rejects the
       `narHash` attribute at parse time, so no `jj` input, locked or not,
       carries one; a git input is NAR-addressed and takes the branch below.
       What remains is a relative input written with a NAR promise inside a
       jj-backed flake (`inputs.sub.url = "path:./sub?narHash=..."`): the
       `path` scheme accepts the attribute, and the subtree it resolves to is
       addressed by its tree id. No lock update can remove that promise,
       because it lives in flake.nix; the message says so. */
    if (method != ContentAddressMethod::Raw::NixArchive && originalInput.getNarHash())
        throw Error(
            "input '%s' promises a NAR hash ('narHash' in its flake.nix attributes), but it is addressed by its "
            "Jujutsu tree id (%s), which a NAR hash cannot be checked against: a tree-addressed mount never "
            "NAR-ingests the tree. Remove 'narHash' from the input in flake.nix; a jj tree is locked by "
            "'treeHash', which the fetcher writes.",
            originalInput.to_string(),
            std::string(method.render()));

    auto [storePath, hash] =
        fetchToStore2(fetchSettings, *store, accessor, FetchMode::DryRun, input.getName(), method);

    mountLazily(storePath, accessor);

    if (method == ContentAddressMethod::Raw::NixArchive) {
        input.attrs.insert_or_assign("narHash", hash.to_string(HashFormat::SRI, true));

        if (originalInput.getNarHash() && hash != *originalInput.getNarHash())
            throw Error(
                (unsigned int) 102,
                "NAR hash mismatch in input '%s', expected '%s' but got '%s'",
                originalInput.to_string(),
                hash.to_string(HashFormat::SRI, true),
                originalInput.getNarHash()->to_string(HashFormat::SRI, true));
    }

    return storePath;
}

void EvalState::mountLazily(const StorePath & storePath, ref<SourceAccessor> accessor)
{
    allowPath(storePath); // FIXME: should just whitelist the entire virtual store
    auto mountPoint = CanonPath(store->printStorePath(storePath));
    if (!storeFS->getMount(mountPoint))
        storeFS->mount(std::move(mountPoint), std::move(accessor));
}

StorePath EvalState::addPathToStore(
    const SourcePath & path,
    std::string_view name,
    ContentAddressMethod method,
    PathFilter * filter,
    const std::optional<Hash> & expectedHash,
    const StorePathSet & refs)
{
    /* A NAR copy re-reads every byte of a tree to find a hash the tree may
       already have. When the directory is, or filters to, a jj tree object,
       its id IS the address: mount the object where the copy would land and
       materialize it only when forced, `mountInput`'s road, taken here for
       every path value under a jj-backed input. A pinned hash names a NAR
       hash, which only the NAR road can check, and references have no
       tree-id form, so both keep it. `--repair` stays on this road (the
       store path must not depend on the flag) but cannot stay lazy: a
       repair means "compare the bytes in the store with the source and
       rewrite them", so the object is materialised here, under `repair`,
       instead of at the first force. */
    if (method == ContentAddressMethod::Raw::NixArchive && !expectedHash && refs.empty()) {
        /* A path under a lazily mounted input names bytes the mount serves:
           ask the mount, not the root filesystem it is composed into. The
           impure root filesystem is a union of the real filesystem over the
           store mounts, and once a mount has been materialized both layers
           have the path, so the union cannot say which tree is meant
           (`UnionSourceAccessor::getSubtree`); the mount can. Access control
           is not skipped by this: every mounted path was allowed when it was
           mounted (`mountLazily`). The filter keeps seeing the coordinates
           it was written for. */
        SourcePath source = path;
        std::optional<PathFilter> rerooted;
        if (store->isInStore(path.path.abs())) {
            auto [storePath, subPath] = store->toStorePath(path.path.abs());
            auto mountPoint = CanonPath(store->printStorePath(storePath));
            if (auto mount = storeFS->getMount(mountPoint)) {
                source = SourcePath{ref(mount), subPath};
                if (filter)
                    rerooted = [&, mountPoint](const std::string & p) { return (*filter)((mountPoint / CanonPath(p)).abs()); };
            }
        }
        if (auto tree = treeObjectAt(source, rerooted ? &*rerooted : filter)) {
            auto [storePath, hash] = fetchToStore2(
                fetchSettings,
                *store,
                SourcePath{ref(tree)},
                repair == Repair ? FetchMode::Copy : FetchMode::DryRun,
                name,
                ContentAddressMethod::Raw::JjTree,
                nullptr,
                repair);
            mountLazily(storePath, ref(tree));
            return storePath;
        }
    }

    std::optional<StorePath> expectedStorePath;
    if (expectedHash)
        expectedStorePath =
            store->makeFixedOutputPathFromCA(name, ContentAddressWithReferences::fromParts(method, *expectedHash, {refs}));

    if (expectedStorePath && store->isValidPath(*expectedStorePath)) {
        allowPath(*expectedStorePath);
        return *expectedStorePath;
    }

    // FIXME: support refs in fetchToStore()?
    auto dstPath = refs.empty() ? fetchToStore(
                                      fetchSettings,
                                      *store,
                                      path,
                                      settings.readOnlyMode ? FetchMode::DryRun : FetchMode::Copy,
                                      name,
                                      method,
                                      filter,
                                      repair)
                                : store->addToStore(
                                      name, path, method, HashAlgorithm::SHA256, refs, filter ? *filter : defaultPathFilter, repair);
    if (expectedStorePath && *expectedStorePath != dstPath)
        throw Error("store path mismatch in (possibly filtered) path added from '%s'", path);
    allowPath(dstPath);
    return dstPath;
}

} // namespace nix
