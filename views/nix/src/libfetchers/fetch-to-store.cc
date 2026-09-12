#include "nix/fetchers/fetch-to-store.hh"
#include "nix/fetchers/fetchers.hh"
#include "nix/fetchers/fetch-settings.hh"
#include "nix/util/environment-variables.hh"

namespace nix {

fetchers::Cache::Key
makeSourcePathToHashCacheKey(std::string_view fingerprint, ContentAddressMethod method, const CanonPath & path)
{
    return fetchers::Cache::Key{
        "sourcePathToHash",
        {{"fingerprint", std::string(fingerprint)}, {"method", std::string{method.render()}}, {"path", path.abs()}}};
}

StorePath fetchToStore(
    const fetchers::Settings & settings,
    Store & store,
    const SourcePath & path,
    FetchMode mode,
    std::string_view name,
    ContentAddressMethod method,
    PathFilter * filter,
    RepairFlag repair)
{
    return fetchToStore2(settings, store, path, mode, name, method, filter, repair).first;
}

std::shared_ptr<SourceAccessor> treeObjectAt(const SourcePath & path, PathFilter * filter)
{
    auto st = path.accessor->maybeLstat(path.path);
    if (!st || st->type != SourceAccessor::tDirectory)
        return nullptr;
    if (!filter && path.path.isRoot())
        return path.accessor->knownTreeRoot ? path.accessor.get_ptr() : nullptr;
    auto tree = filter ? path.accessor->getFilteredTree(path.path, *filter) : path.accessor->getSubtree(path.path);
    /* The contract (`SourceAccessor::getSubtree`): an accessor that names a
       tree object announces its id. One that does not is a defect in that
       accessor, not a licence to address the bytes another way. */
    if (tree && !tree->knownTreeRoot)
        throw Error("accessor for '%s' named a tree object without announcing its id", path);
    return tree;
}

std::pair<StorePath, Hash> fetchToStore2(
    const fetchers::Settings & settings,
    Store & store,
    const SourcePath & path,
    FetchMode mode,
    std::string_view name,
    ContentAddressMethod method,
    PathFilter * filter,
    RepairFlag repair)
{
    std::optional<fetchers::Cache::Key> cacheKey;

    auto [subpath, fingerprint] = filter ? std::pair<CanonPath, std::optional<std::string>>{path.path, std::nullopt}
                                         : path.accessor->getFingerprint(path.path);

    /* An accessor reading out of a content-addressed object store (the jj
       fetcher) knows its trees' ids a priori: the VCS maintained them
       incrementally, Merkle-fashion, while snapshotting. `Raw::JjTree`
       ingests a tree by exactly that id, so the store path follows from it
       with zero file reads -- where the NAR method's flat hash has to
       re-read the whole tree on every content change. A subtree is its own
       object and a filtered view is one too (`treeObjectAt`), each with an
       id of its own; what has no id is a path INTO a tree, which is why the
       object, not the path, is what gets ingested below. */
    std::shared_ptr<SourceAccessor> treeObject;
    std::optional<Hash> knownHash;
    if (method == ContentAddressMethod::Raw::JjTree) {
        treeObject = treeObjectAt(path, filter);
        /* `JjTree` has no other source of a hash: nothing in Nix can compute
           one, so a tree nobody announces cannot be ingested this way.
           Refuse here, naming the gap, rather than in a walk that would fail
           on the first file. */
        if (!treeObject)
            throw TreeIdNotComputable(
                "cannot content-address '%s' by Jujutsu tree id: it is not a directory of a tree that announces "
                "its id",
                path);
        knownHash = treeObject->knownTreeRoot->id;
    }

    std::optional<Hash> trustedHash;
    bool trustedFromCache = false;

    /* The fingerprint-to-hash cache exists to spare the NAR walk; a
       tree-addressed accessor has nothing to spare, so it is neither
       consulted nor seeded for one. */
    if (knownHash) {
        /* nothing to look up */
    } else if (fingerprint) {
        cacheKey = makeSourcePathToHashCacheKey(*fingerprint, method, subpath);
        if (auto res = settings.getCache()->lookup(*cacheKey)) {
            trustedHash = Hash::parseSRI(fetchers::getStrAttr(*res, "hash"));
            trustedFromCache = true;
        }
    } else {
        static auto barf = getEnv("_NIX_TEST_BARF_ON_UNCACHEABLE").value_or("") == "1";
        if (barf && !filter)
            throw Error("source path '%s' is uncacheable (filter=%d)", path, (bool) filter);
        // FIXME: could still provide in-memory caching keyed on `SourcePath`.
        debug("source path '%s' is uncacheable", path);
    }

    if (!trustedHash)
        trustedHash = knownHash;

    if (trustedHash) {
        auto storePath =
            store.makeFixedOutputPathFromCA(name, ContentAddressWithReferences::fromParts(method, *trustedHash, {}));

        /* Add a temproot before the call to isValidPath to prevent accidental GC in case the
           input is cached. Note that this must be done before to avoid races. */
        if (mode != FetchMode::DryRun)
            store.addTempRoot(storePath);

        /* Under `repair` a valid path is not an answer: the bytes must be
           read and compared, which only the copy below does. */
        if (mode == FetchMode::DryRun || (repair == NoRepair && store.isValidPath(storePath))) {
            debug(
                "source path '%s' %s in '%s' (hash '%s')",
                path,
                trustedFromCache ? "cache hit" : "resolved by its announced tree id",
                store.printStorePath(storePath),
                trustedHash->to_string(HashFormat::SRI, true));
            return {storePath, *trustedHash};
        }
        debug("source path '%s' not in store", path);
    }

    /* Forced materialization of a jj tree: write the tree object's bytes
       under the id it announced. The id is the address and is trusted (it
       came from an immutable object store); the store's own record of the
       bytes is the NAR hash libstore computes on write. That costs one read
       of the forced tree, paid only here, never on the evaluation path that
       derived the store path above. The object is dumped, not `path`: for a
       filtered view the two differ, and the id names the object. */
    if (method == ContentAddressMethod::Raw::JjTree) {
        Activity act(*logger, lvlChatty, actUnknown, fmt("copying '%s' to the store", path));
        auto storePath = store.addToStoreWithKnownCA(
            name, SourcePath{ref(treeObject)}, ContentAddress{.method = method, .hash = *knownHash}, repair);
        debug(
            "copied '%s' to '%s' (tree id '%s')",
            path,
            store.printStorePath(storePath),
            knownHash->to_string(HashFormat::SRI, true));
        return {storePath, *knownHash};
    }

    Activity act(
        *logger,
        lvlChatty,
        actUnknown,
        fmt(mode == FetchMode::DryRun ? "hashing '%s'" : "copying '%s' to the store", path));

    auto filter2 = filter ? *filter : defaultPathFilter;

    /* Only NAR (or flat) ingestion reaches here: `JjTree` returned above
       with its id, and no other method knows a hash a priori. */
    auto hashAlgo = HashAlgorithm::SHA256;

    auto [storePath, hash] =
        mode == FetchMode::DryRun
            ? [&]() {
                  auto [storePath, hash] =
                      store.computeStorePath(name, path, method, hashAlgo, {}, filter2);
                  debug(
                      "hashed '%s' to '%s' (hash '%s')",
                      path,
                      store.printStorePath(storePath),
                      hash.to_string(HashFormat::SRI, true));
                  return std::make_pair(storePath, hash);
              }()
            : [&]() {
                  // FIXME: ideally addToStore() would return the hash
                  // right away (like computeStorePath()).
                  auto storePath = store.addToStore(name, path, method, hashAlgo, {}, filter2, repair);
                  auto info = store.queryPathInfo(storePath);
                  assert(info->references.empty());
                  auto hash = method == ContentAddressMethod::Raw::NixArchive ? info->narHash : ({
                      if (!info->ca || info->ca->method != method)
                          throw Error("path '%s' lacks a CA field", store.printStorePath(storePath));
                      info->ca->hash;
                  });
                  debug(
                      "copied '%s' to '%s' (hash '%s')",
                      path,
                      store.printStorePath(storePath),
                      hash.to_string(HashFormat::SRI, true));
                  return std::make_pair(storePath, hash);
              }();

    if (cacheKey)
        settings.getCache()->upsert(*cacheKey, {{"hash", hash.to_string(HashFormat::SRI, true)}});

    return {storePath, hash};
}

} // namespace nix
