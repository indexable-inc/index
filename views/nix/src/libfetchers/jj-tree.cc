#include "nix/fetchers/jj-tree.hh"

#include <chrono>
#include "nix/util/fmt.hh"
#include "nix/util/serialise.hh"

#include <jj_tree.h>

#include <algorithm>
#include <cstring>

namespace nix::fetchers {

/* The ABI records one message per thread and hands it over exactly once
   (a second read clears the slot and returns 0), so every failing call
   reads it immediately, on the thread that made the call. The return is the
   number of bytes COPIED, so it is already bounded by the buffer; the clamp
   stays anyway because it is the difference between a truncated message and
   a read past the end of `buf`, and it costs nothing. An empty message is
   reported as such rather than invented: the status code already says the
   call failed. */
static std::string takeLastError()
{
    uint8_t buf[4096];
    auto n = jjt_last_error(buf, sizeof buf);
    if (n == 0)
        return "(no error message recorded)";
    return std::string(reinterpret_cast<const char *>(buf), std::min(n, sizeof buf));
}

/* Turn a `JJT_ERR_*` status into the exception that names the fault.

   The status code is the ABI's contract (`jj_tree.h`); the message beside
   it is prose the library may reword at any time. So the classification
   branches on the code only, and the message is quoted, never matched.
   `context` says what the caller was attempting, already formatted.

   Codes with no arm of their own (INVALID_ARGUMENT, IO, INTERNAL, and any
   code a later library adds) fall to `JjTreeError` carrying the number, so
   an unclassified failure is still legible and still fails. */
[[noreturn]] static void throwJjError(int code, const std::string & context)
{
    auto msg = takeLastError();
    switch (code) {

    case JJT_ERR_MISSING_OBJECT:
        throw JjMissingObject(
            "%s: %s. The object is absent from that repository's local store and this fetcher never fetches "
            "one; bring it there (for example with 'jj git fetch' in that workspace) and retry.",
            context,
            msg);

    case JJT_ERR_UNSUPPORTED_STORE:
        throw JjUnsupportedStore(
            "%s: %s. Only jj's native 'ix-local' store is read in-process: a git-backed workspace (including "
            "one made by 'jj git init --colocate') is a 'git+file' input instead, and an 'ix' (RPC-backed) "
            "store has no local plane that is a source of truth.",
            context,
            msg);

    case JJT_ERR_CONFLICT:
        throw JjConflict(
            "%s: %s. A conflicted tree has no single content, so it is refused as a build input rather than "
            "rendered with conflict markers; resolve the conflict and retry.",
            context,
            msg);

    case JJT_ERR_KIND_MISMATCH:
        throw JjKindMismatch(
            "%s: %s. A Git submodule reports itself this way: an entry this fetcher cannot represent is "
            "refused by name rather than dropped from the tree.",
            context,
            msg);

    case JJT_ERR_UNTRACKED_PATHS:
        throw JjUntrackedPaths(
            "%s: %s. jj wrote the snapshot, but the tree it wrote is missing those paths, so it is not the "
            "tree you are looking at; track them ('snapshot.auto-track') or raise "
            "'snapshot.max-new-file-size' and retry.",
            context,
            msg);

    /* jj's own words, hints included, with nothing appended: the ABI's
       message is what `jj status` would have printed, and the fix it names
       (`jj workspace update-stale`, `jj op integrate`) is more specific
       than anything this fetcher could add. */
    case JJT_ERR_JJ:
        throw JjRefused("%s: %s", context, msg);

    /* The message is already the whole answer: some name the caller gave
       does not resolve. */
    case JJT_ERR_NOT_FOUND:
        throw JjTreeError("%s: %s", context, msg);

    default:
        throw JjTreeError("%s: %s (Jujutsu tree ABI status %d)", context, msg, code);
    }
}

static Hash idToHash(const JjtId & id)
{
    /* `nativeIdXpSettings`: a jj id is minted by the repository's backend,
       so it must not be gated on the user's `blake3-hashes` choice. */
    Hash h(HashAlgorithm::BLAKE3, nativeIdXpSettings());
    static_assert(sizeof id.bytes == regularHashSize(HashAlgorithm::BLAKE3));
    std::memcpy(h.hash, id.bytes, sizeof id.bytes);
    return h;
}

/* A SHA-1 here means a caller handed a Git commit id to a native jj
   repository: the two id spaces never overlap, so the ABI could only report
   a missing object and the user would read "no such revision" for what is
   really the wrong repository kind. Name the real mistake instead.

   Checked in one place because two callers need it at different stages:
   `hashToId`, which is about to narrow a `Hash` to 32 raw bytes, and
   `resolveRev`, which passes the id as hex and never goes through
   `hashToId` at all (a 40-hex rev reaches it from `applyOverrides`). */
static void requireNativeId(const Hash & h)
{
    if (h.algo != HashAlgorithm::BLAKE3)
        throw JjTreeError(
            "'%s' is not a jj native (blake3) id; a git-backed jj repository is read through 'git+file'",
            h.to_string(HashFormat::Base16, true));
}

static JjtId hashToId(const Hash & h)
{
    requireNativeId(h);
    JjtId id;
    std::memcpy(id.bytes, h.hash, sizeof id.bytes);
    return id;
}

JjTreeRepo::JjTreeRepo(JjtRepo * repo, std::filesystem::path workspacePath)
    : repo(repo)
    , workspacePath(std::move(workspacePath))
{
}

JjTreeRepo::~JjTreeRepo()
{
    jjt_repo_free(repo);
}

ref<JjTreeRepo> JjTreeRepo::open(const std::filesystem::path & workspacePath)
{
    /* `jjt_repo_open` writes its status on EVERY return (`jj_tree.h`), so
       a refused store is classified by CODE here exactly as it is for
       every other call: `JJT_ERR_UNSUPPORTED_STORE` becomes
       `JjUnsupportedStore` and carries the `git+file` route, and the
       message is quoted, never matched. Initialised to a nonzero code so
       that a library that broke its contract and left it untouched still
       fails, rather than reporting success beside a NULL handle. */
    int code = JJT_ERR_INTERNAL;
    auto * repo = jjt_repo_open(workspacePath.string().c_str(), &code);
    if (!repo)
        throwJjError(code, fmt("cannot open the Jujutsu repository at '%s'", workspacePath.string()));
    /* `ref` wants a shared_ptr; the constructor is private, so no make_ref. */
    return ref<JjTreeRepo>(std::shared_ptr<JjTreeRepo>(new JjTreeRepo(repo, workspacePath)));
}

Hash JjTreeRepo::resolveRev(const Hash & rev)
{
    requireNativeId(rev);
    std::lock_guard lock(mutex);
    JjtId commit;
    if (auto rc = jjt_resolve_rev(repo, rev.gitRev().c_str(), &commit); rc != JJT_OK)
        throwJjError(
            rc, fmt("cannot resolve revision '%s' in the Jujutsu repository at '%s'", rev.gitRev(), workspacePath.string()));
    return idToHash(commit);
}

Hash JjTreeRepo::resolveBookmark(const std::string & name)
{
    std::lock_guard lock(mutex);
    JjtId commit;
    if (auto rc = jjt_resolve_bookmark(repo, name.c_str(), &commit); rc != JJT_OK)
        throwJjError(
            rc, fmt("cannot resolve bookmark '%s' in the Jujutsu repository at '%s'", name, workspacePath.string()));
    return idToHash(commit);
}

Hash JjTreeRepo::snapshotWorkingCopy()
{
    std::lock_guard lock(mutex);
    JjtId commit;
    if (auto rc = jjt_snapshot_working_copy(repo, &commit); rc != JJT_OK)
        throwJjError(rc, fmt("cannot snapshot the Jujutsu working copy at '%s'", workspacePath.string()));
    return idToHash(commit);
}

JjCommitMeta JjTreeRepo::commitMeta(const Hash & commit)
{
    std::lock_guard lock(mutex);
    auto id = hashToId(commit);
    JjtCommitMeta meta;
    if (auto rc = jjt_commit_meta(repo, &id, &meta); rc != JJT_OK)
        throwJjError(
            rc, fmt("cannot read commit '%s' in the Jujutsu repository at '%s'", commit.gitRev(), workspacePath.string()));
    /* jj stamps milliseconds; nix's `lastModified` is seconds. Floor, so
       that a stamp inside the same second renders the same value both here
       and in `jj log`. C++ integer division rounds toward zero, which is
       not the floor for a pre-epoch stamp -- and `committer_millis` is
       signed precisely because those exist (`jj_tree.h`), so the negative
       remainder is corrected rather than assumed away. */
    auto seconds = meta.committer_millis / 1000;
    if (meta.committer_millis % 1000 != 0 && meta.committer_millis < 0)
        --seconds;
    return JjCommitMeta{
        .lastModified = static_cast<time_t>(seconds),
        .hasConflict = meta.has_conflict,
    };
}

Hash JjTreeRepo::commitTree(const Hash & commit)
{
    std::lock_guard lock(mutex);
    auto id = hashToId(commit);
    JjtId tree;
    /* `JJT_ERR_CONFLICT` here means a multi-term root tree, the same state
       `commitMeta().hasConflict` reports. The fetcher reads the metadata
       first and refuses there, with the repository named, so this arm is
       for any other caller of this library type rather than for that
       path. */
    if (auto rc = jjt_commit_tree(repo, &id, &tree); rc != JJT_OK)
        throwJjError(
            rc,
            fmt("cannot read the tree of commit '%s' in the Jujutsu repository at '%s'",
                commit.gitRev(),
                workspacePath.string()));
    return idToHash(tree);
}

ref<SourceAccessor> JjTreeRepo::getAccessor(const Hash & tree, std::string displayPrefix)
{
    auto accessor = make_ref<JjTreeAccessor>(ref<JjTreeRepo>(shared_from_this()), tree);
    accessor->setPathDisplay(std::move(displayPrefix));
    return accessor;
}

JjTreeAccessor::JjTreeAccessor(ref<JjTreeRepo> repo, const Hash & root)
    : repo(std::move(repo))
    , root(root)
{
    knownTreeRoot = KnownTreeRoot{.id = root};
    /* Every accessor carries the fingerprint of the tree it serves, root
       and subtree alike: `SourceAccessor::getSubtree` requires the result
       to be a complete tree in its own right, and a subtree accessor never
       passes through a fetcher that could stamp one on afterwards. Same
       string `JjInputScheme::getFingerprint` builds, so the two agree on
       the root accessor rather than racing to set it. A tree id is a
       content address, so it is a sound cache key on its own. */
    fingerprint = "jj-tree:" + root.gitRev();
}

std::shared_ptr<const JjTreeRepo::Listing> JjTreeRepo::list(const Hash & tree)
{
    {
        auto cache(listings.lock());
        if (auto it = cache->find(tree); it != cache->end())
            return it->second;
    }

    JjtEntry * entries = nullptr;
    size_t count = 0;
    {
        std::lock_guard lock(mutex);
        auto id = hashToId(tree);
        auto started = std::chrono::steady_clock::now();
        auto rc = jjt_tree_list(repo, &id, &entries, &count);
        auto elapsed = std::chrono::steady_clock::now() - started;
        abiListNs += std::chrono::duration_cast<std::chrono::nanoseconds>(elapsed).count();
        abiListings += 1;
        if (rc != JJT_OK)
            throwJjError(
                rc,
                fmt("cannot list the Jujutsu tree %s in the repository at '%s'",
                    tree.gitRev(),
                    workspacePath.string()));
    }

    /* The array and its names are ours from here (`jj_tree.h`), so free
       them on every exit, including a throwing `emplace`. An empty tree
       reports count 0 with a non-NULL, non-dereferenceable pointer, which
       `jjt_entries_free` accepts. */
    struct FreeEntries
    {
        JjtEntry * entries;
        size_t count;

        ~FreeEntries()
        {
            if (entries)
                jjt_entries_free(entries, count);
        }
    } freeEntries{entries, count};

    auto listing = std::make_shared<Listing>();
    for (size_t i = 0; i < count; ++i) {
        auto & e = entries[i];
        listing->emplace(
            std::string(e.name, e.name_len),
            Entry{
                .kind = e.kind,
                .id = idToHash(e.id),
            });
    }

    /* Two threads may list the same tree concurrently; `emplace` keeps the
       first listing and the loser's is dropped, which is fine because both
       are the same content (that is what a tree id promises). */
    auto cache(listings.lock());
    return cache->emplace(tree, std::move(listing)).first->second;
}

JjTreeRepo::Symlinks JjTreeRepo::symlinks(const Hash & tree)
{
    JjtSymlink * links = nullptr;
    size_t count = 0;
    {
        std::lock_guard lock(mutex);
        auto id = hashToId(tree);
        auto started = std::chrono::steady_clock::now();
        auto rc = jjt_tree_symlinks(repo, &id, &links, &count);
        auto elapsed = std::chrono::steady_clock::now() - started;
        abiWalkNs += std::chrono::duration_cast<std::chrono::nanoseconds>(elapsed).count();
        abiWalks += 1;
        if (rc != JJT_OK)
            throwJjError(
                rc,
                fmt("cannot list the symlinks under the Jujutsu tree %s in the repository at '%s'",
                    tree.gitRev(),
                    workspacePath.string()));
    }

    /* As in `list`: the array and its strings are ours from here, freed on
       every exit. An empty answer is count 0 with a non-NULL pointer, which
       `jjt_symlinks_free` accepts. */
    struct FreeLinks
    {
        JjtSymlink * links;
        size_t count;

        ~FreeLinks()
        {
            if (links)
                jjt_symlinks_free(links, count);
        }
    } freeLinks{links, count};

    Symlinks res;
    res.reserve(count);
    for (size_t i = 0; i < count; ++i)
        res.emplace_back(
            std::string(links[i].path, links[i].path_len), std::string(links[i].target, links[i].target_len));
    return res;
}

Hash JjTreeRepo::filterTree(const Hash & tree, const std::vector<std::string> & accepted)
{
    std::vector<const char *> names;
    names.reserve(accepted.size());
    for (auto & path : accepted)
        names.push_back(path.c_str());

    std::lock_guard lock(mutex);
    auto id = hashToId(tree);
    JjtId filtered;
    /* `accepted` may be NULL when `count` is 0 (`jj_tree.h`); an empty
       vector's `data()` may be NULL too, so the two agree without a branch. */
    auto rc = jjt_tree_filter(repo, &id, names.data(), names.size(), &filtered);
    if (rc != JJT_OK)
        throwJjError(
            rc,
            fmt("cannot filter the Jujutsu tree %s in the repository at '%s'", tree.gitRev(), workspacePath.string()));
    return idToHash(filtered);
}

std::optional<JjTreeRepo::Entry> JjTreeAccessor::lookup(const CanonPath & path)
{
    std::optional<JjTreeRepo::Entry> current;
    Hash tree = root;
    for (auto component : path) {
        if (current) {
            /* Descending through something that is not a directory: absent,
               the same answer `lstat` gives for `file/x`. */
            if (current->kind != JJT_TREE)
                return std::nullopt;
            tree = current->id;
        }
        auto listing = repo->list(tree);
        auto it = listing->find(std::string(component));
        if (it == listing->end())
            return std::nullopt;
        current = it->second;
    }
    return current;
}

void JjTreeAccessor::readFile(const CanonPath & path, Sink & sink, fun<void(uint64_t)> sizeCallback)
{
    /* The root exists and is a directory; `lookup` has no entry for it, so
       without this it would read as absent. */
    if (path.isRoot())
        throw NotARegularFile("path '%s' is not a regular file", showPath(path));

    auto entry = lookup(path);
    if (!entry)
        throw FileNotFound("path '%s' does not exist", showPath(path));
    if (entry->kind != JJT_FILE && entry->kind != JJT_EXEC)
        throw NotARegularFile("path '%s' is not a regular file", showPath(path));

    uint8_t * data = nullptr;
    size_t len = 0;
    {
        std::lock_guard lock(repo->mutex);
        auto id = hashToId(entry->id);
        if (auto rc = jjt_blob_read(repo->repo, &id, &data, &len); rc != JJT_OK)
            throwJjError(rc, fmt("cannot read '%s' (blob %s)", showPath(path), entry->id.gitRev()));
    }
    /* Freed on every exit below, including a throwing sink. */
    struct Free
    {
        uint8_t * data;
        size_t len;

        ~Free()
        {
            if (data)
                jjt_bytes_free(data, len);
        }
    } freeBytes{data, len};

    sizeCallback(len);
    StringSource source{std::string_view(reinterpret_cast<const char *>(data), len)};
    source.drainInto(sink);
}

std::optional<SourceAccessor::Stat> JjTreeAccessor::maybeLstat(const CanonPath & path)
{
    if (path.isRoot())
        return Stat{.type = tDirectory};

    auto entry = lookup(path);
    if (!entry)
        return std::nullopt;

    /* No `fileSize`: the listing carries none (`Entry`), and the field is
       optional by contract ("not all accessors return this since it may be
       too expensive to compute"). Every reader of the bytes gets the length
       from `readFile`'s size callback, and a symlink's from `readLink`. */
    switch (entry->kind) {
    case JJT_FILE:
        return Stat{.type = tRegular};
    case JJT_EXEC:
        return Stat{.type = tRegular, .isExecutable = true};
    case JJT_SYMLINK:
        return Stat{.type = tSymlink};
    case JJT_TREE:
        return Stat{.type = tDirectory};
    default:
        throw JjTreeError("'%s' has unknown Jujutsu entry kind %d", showPath(path), entry->kind);
    }
}

SourceAccessor::DirEntries JjTreeAccessor::readDirectory(const CanonPath & path)
{
    Hash tree = root;
    if (!path.isRoot()) {
        auto entry = lookup(path);
        if (!entry)
            throw FileNotFound("path '%s' does not exist", showPath(path));
        if (entry->kind != JJT_TREE)
            throw NotADirectory("path '%s' is not a directory", showPath(path));
        tree = entry->id;
    }

    DirEntries res;
    for (auto & [name, entry] : *repo->list(tree)) {
        std::optional<Type> type;
        switch (entry.kind) {
        case JJT_FILE:
        case JJT_EXEC:
            type = tRegular;
            break;
        case JJT_SYMLINK:
            type = tSymlink;
            break;
        case JJT_TREE:
            type = tDirectory;
            break;
        default:
            throw JjTreeError("'%s' has unknown Jujutsu entry kind %d", showPath(path / name), entry.kind);
        }
        res.emplace(name, type);
    }
    return res;
}

std::string JjTreeAccessor::readLink(const CanonPath & path)
{
    /* As in `readFile`: the root is a directory, not an absent path. */
    if (path.isRoot())
        throw NotASymlink("path '%s' is not a symlink", showPath(path));

    auto entry = lookup(path);
    if (!entry)
        throw FileNotFound("path '%s' does not exist", showPath(path));
    if (entry->kind != JJT_SYMLINK)
        throw NotASymlink("path '%s' is not a symlink", showPath(path));

    uint8_t * data = nullptr;
    size_t len = 0;
    std::lock_guard lock(repo->mutex);
    auto id = hashToId(entry->id);
    if (auto rc = jjt_symlink_target(repo->repo, &id, &data, &len); rc != JJT_OK)
        throwJjError(rc, fmt("cannot read the target of symlink '%s'", showPath(path)));
    /* Freed on every exit, including a throwing string construction; the
       target is bytes, not a C string, so `len` is the whole answer. */
    struct FreeBytes
    {
        uint8_t * data;
        size_t len;

        ~FreeBytes()
        {
            if (data)
                jjt_bytes_free(data, len);
        }
    } freeBytes{data, len};

    return std::string(reinterpret_cast<const char *>(data), len);
}

std::shared_ptr<SourceAccessor> JjTreeAccessor::getSubtree(const CanonPath & path)
{
    if (path.isRoot())
        return shared_from_this();
    auto entry = lookup(path);
    if (!entry)
        throw FileNotFound("path '%s' does not exist", showPath(path));
    if (entry->kind != JJT_TREE)
        throw NotADirectory("path '%s' is not a directory", showPath(path));
    /* The subtree's id is already in the parent's listing: no ABI round
       trip, and the very object the parent tree names. `ref` locally
       because this result is never null; the null the contract reserves is
       for trees that have no subtree objects at all, which a jj tree
       always has. */
    auto accessor = make_ref<JjTreeAccessor>(repo, entry->id);
    accessor->setPathDisplay(displayPrefix + "/" + std::string(path.rel()), displaySuffix);
    return accessor.get_ptr();
}

std::shared_ptr<SourceAccessor> JjTreeAccessor::getFilteredTree(const CanonPath & path, PathFilter & filter)
{
    Hash tree = root;
    if (!path.isRoot()) {
        auto entry = lookup(path);
        if (!entry)
            throw FileNotFound("path '%s' does not exist", showPath(path));
        if (entry->kind != JJT_TREE)
            throw NotADirectory("path '%s' is not a directory", showPath(path));
        tree = entry->id;
    }

    /* The filter's verdicts, asked in this accessor's coordinates exactly as
       `dumpPath` would ask them, collected as paths relative to `path` for
       the ABI. A refused directory is not descended into, as `dumpPath`
       never descends into one, so the set is closed downwards by
       construction. Every directory comes from the repository's listing
       cache; no blob is read. */
    std::vector<std::string> accepted;
    bool rejected = false;
    auto walk = [&](auto & self, const Hash & dir, const CanonPath & at, const std::string & rel) -> void {
        for (auto & [name, entry] : *repo->list(dir)) {
            auto child = at / name;
            if (!filter(child.abs())) {
                rejected = true;
                continue;
            }
            auto childRel = rel.empty() ? name : rel + "/" + name;
            accepted.push_back(childRel);
            if (entry.kind == JJT_TREE)
                self(self, entry.id, child, childRel);
        }
    };
    walk(walk, tree, path, "");

    /* A filter that refused nothing leaves the tree it was given: its id is
       the answer, and the ABI's second walk of the same objects is not
       paid. */
    auto accessor = make_ref<JjTreeAccessor>(repo, rejected ? repo->filterTree(tree, accepted) : tree);
    accessor->setPathDisplay(
        displayPrefix + (path.isRoot() ? std::string() : "/" + std::string(path.rel())),
        displaySuffix + (rejected ? " (filtered)" : ""));
    return accessor.get_ptr();
}

} // namespace nix::fetchers
