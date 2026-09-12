#pragma once
///@file

#include "nix/util/canon-path.hh"
#include "nix/util/hash.hh"
#include "nix/util/ref.hh"
#include "nix/util/source-accessor.hh"
#include "nix/util/sync.hh"

#include <atomic>
#include <filesystem>
#include <map>
#include <mutex>
#include <string>
#include <vector>

/* The opaque handle from `jj_tree.h` (`typedef struct JjtRepo JjtRepo;`).
   Forward-declared here so that nothing outside jj-tree.cc needs the ABI
   header or its include path. */
struct JjtRepo;

namespace nix::fetchers {

/**
 * A failure reported by the tree ABI that has no more specific type below.
 *
 * The ABI returns a `JJT_ERR_*` code and, separately, a human-readable
 * message (`jj_tree.h`). The code is the contract and the message is prose
 * the library may reword, so every classification here branches on the
 * code; nothing reads the message except to quote it.
 */
MakeError(JjTreeError, Error);

/**
 * `JJT_ERR_MISSING_OBJECT`: an id is well formed but absent from the LOCAL
 * jj object store. The ABI never fetches, so this is a synchronisation
 * fault, not a network one, and its fix is to bring the object here.
 */
MakeError(JjMissingObject, JjTreeError);

/**
 * `JJT_ERR_UNSUPPORTED_STORE`: the workspace's store is not `ix-local`.
 * A git-backed workspace (`jj git init`, with or without `--colocate`) is
 * read as a `git+file` input instead; an `ix` (RPC-backed) store is refused
 * because its on-disk plane is an accelerator cache that answers "miss" for
 * objects that exist, which would surface here as spurious missing objects.
 */
MakeError(JjUnsupportedStore, JjTreeError);

/**
 * `JJT_ERR_CONFLICT`: a conflicted commit, bookmark target, or root tree.
 * A conflicted tree has no single content, so it is refused as a source
 * rather than rendered with markers.
 */
MakeError(JjConflict, JjTreeError);

/**
 * `JJT_ERR_KIND_MISMATCH`: the object exists but is not of the kind the
 * call requires, including a tree entry this ABI cannot represent (a git
 * submodule, which `jjt_tree_list` refuses by name rather than skipping).
 */
MakeError(JjKindMismatch, JjTreeError);

/**
 * `JJT_ERR_UNTRACKED_PATHS`: jj snapshotted and wrote what `jj status`
 * would have written, but left paths out of the tree by its own rules
 * (over `snapshot.max-new-file-size`, or not matched by
 * `snapshot.auto-track`). `jj status` prints those as a warning and carries
 * on; a build input that silently lacks a file is worse than one that
 * fails, so the ABI withholds the id and this names every path it left out.
 */
MakeError(JjUntrackedPaths, JjTreeError);

/**
 * `JJT_ERR_JJ`: jj itself refused or failed, in its own terms (a stale
 * working copy with `snapshot.auto-update-stale = false`, a concurrent
 * operation, an invalid setting). The ABI's message is what `jj status`
 * would have printed, hints included, so it is quoted verbatim and nothing
 * is added to it: the fix is a jj command, and jj is the one that knows
 * which.
 */
MakeError(JjRefused, JjTreeError);

/**
 * What the ABI reports about a commit, in nix's units.
 */
struct JjCommitMeta
{
    /** Committer timestamp, seconds since the epoch. */
    time_t lastModified;
    /* No `revCount`. The ABI reports jj's generation number (the
       longest-path distance from the root commit), which is NOT the number
       of ancestors that `revCount` means everywhere else in nix. The two
       agree only on a linear history, so publishing one as the other would
       be right until the first merge and silently wrong after it. The field
       is not carried at all, so nothing downstream can reach for it: a jj
       input has no `revCount` (jj.cc). */
    /**
     * Whether the commit's ROOT TREE has more than one term, which is the
     * only shape a conflict takes here: this fork has no per-entry conflict
     * object, so a conflict is a property of the commit, not of a path.
     * When true, `commitTree` on the same commit fails with `JjConflict`.
     */
    bool hasConflict;
};

/**
 * A jj repository opened read-only through the in-process tree ABI
 * (`libjj_tree.a`, header `jj_tree.h`). Local object store only: an object
 * that is not present is an error, never a fetch. Nothing here writes
 * except `snapshotWorkingCopy`, which is jj's own meaning of "the current
 * tree" (jj has no dirty state: the working copy IS a commit once
 * snapshotted), and `filterTree`, which writes the tree objects of a
 * filtered view of a tree (never a blob, a commit or an operation).
 *
 * Every call takes the repository mutex. `jj_tree.h` states that a
 * `JjtRepo` handle is NOT thread-safe and that calls sharing one handle
 * must be serialised, and the evaluator does read inputs from several
 * threads under parallel eval, so this lock is that serialisation.
 */
struct JjTreeRepo : std::enable_shared_from_this<JjTreeRepo>
{
    /**
     * One entry of a tree object's listing: what the tree object records,
     * a kind and an id. No length: a blob's length is not in the tree, and
     * an ABI that supplied one read every file to list its directory
     * (`jj_tree.h`, `JjtEntry`). `Stat::fileSize` is optional for exactly
     * this reason, and `readFile` reports the length with the bytes.
     */
    struct Entry
    {
        /** A `JjtKind` value. */
        uint32_t kind;
        Hash id;
    };

    using Listing = std::map<std::string, Entry>;

    /**
     * Open the workspace rooted AT `workspacePath` (the directory holding
     * `.jj`; the ABI does not walk up to an enclosing workspace, so a
     * subdirectory is not silently promoted to its parent). Only jj's
     * native `ix-local` store is accepted.
     *
     * `jjt_repo_open` reports failure as a NULL handle beside a
     * `JJT_ERR_*` code in its out-parameter, so a refused store raises
     * `JjUnsupportedStore` and an absent workspace `JjTreeError`, the same
     * classification every other call gets. Nothing parses the message.
     */
    static ref<JjTreeRepo> open(const std::filesystem::path & workspacePath);

    ~JjTreeRepo();

    JjTreeRepo(const JjTreeRepo &) = delete;
    JjTreeRepo & operator=(const JjTreeRepo &) = delete;

    const std::filesystem::path & getWorkspacePath() const
    {
        return workspacePath;
    }

    /** The commit id `rev` (64 hex chars, blake3) if the commit exists. */
    Hash resolveRev(const Hash & rev);

    /** The commit a local bookmark points at. */
    Hash resolveBookmark(const std::string & name);

    /** Snapshot THIS workspace's working copy and return its `@` commit. */
    Hash snapshotWorkingCopy();

    JjCommitMeta commitMeta(const Hash & commit);

    /** The root tree id of `commit`. */
    Hash commitTree(const Hash & commit);

    /**
     * The listing of the tree object `tree`, from the object store on the
     * first call and from this repository's cache after that.
     *
     * The cache is here, on the repository, and not on an accessor, because
     * it is keyed by tree id and a tree id is a content address: the same
     * id always lists the same way, whichever accessor asked. A subtree
     * accessor (`JjTreeAccessor::getSubtree`) serves objects its parent has
     * already listed on the way down, so a per-accessor cache would list
     * every one of them a second time and, for a `path:./sub` input read
     * beside its parent, pay the same ABI calls twice.
     */
    std::shared_ptr<const Listing> list(const Hash & tree);

    /** (path relative to a tree, link target), one per symlink. */
    using Symlinks = std::vector<std::pair<std::string, std::string>>;

    /**
     * Every symlink under the tree object `tree`, sorted by path: one
     * `jjt_tree_symlinks` call for the whole tree, where `list` is one call
     * per directory (the sealing walk of a 175k-file tree made 14,664 of
     * them, 1.74 s, on every edit). Not cached: the one caller, sealing,
     * asks once per object.
     */
    Symlinks symlinks(const Hash & tree);

    /**
     * The tree keeping exactly the entries of `tree` named by `accepted`
     * (`/`-separated paths relative to `tree`, closed downwards: a directory
     * that is not named prunes its subtree), written to the repository's
     * object store; its id. `builtins.path`'s filter as a Merkle operation:
     * no blob is read, only the directories whose entry set changed are
     * written, an intact subtree keeps its id, and the id depends on the
     * kept content alone (`jjt_tree_filter`). A directory the filter
     * empties is dropped: a jj tree has no empty directories.
     */
    Hash filterTree(const Hash & tree, const std::vector<std::string> & accepted);

    /**
     * How many `jjt_tree_list` and `jjt_tree_symlinks` calls this
     * repository has made and the time spent inside them, over the
     * process. An
     * instrument, not a contract: the sealed-tree walk (`IXE_REPLAY_TRACE`)
     * prints the delta across one walk, which separates the ABI's cost from
     * the walk's own (round 16: a hot run walked the same tree ten times
     * slower than a cold one, and the walk alone could not say where the
     * time went).
     */
    struct AbiStats
    {
        uint64_t listings;
        uint64_t listNs;
        uint64_t walks;
        uint64_t walkNs;
    };

    AbiStats abiStats() const
    {
        return {
            abiListings.load(),
            abiListNs.load(),
            abiWalks.load(),
            abiWalkNs.load()};
    }

    /**
     * An accessor serving the tree object `tree`, announcing it as
     * `KnownTreeRoot{tree}`.
     */
    ref<SourceAccessor> getAccessor(const Hash & tree, std::string displayPrefix);

private:
    friend struct JjTreeAccessor;

    JjTreeRepo(JjtRepo * repo, std::filesystem::path workspacePath);

    std::mutex mutex;
    JjtRepo * repo;
    std::filesystem::path workspacePath;

    /* Listings by tree id. A `Hash` is not hashable by std, and the map is
       small (one entry per directory visited in this process), so an ordered
       map suffices. Its own lock, not `mutex`: a cache hit must not wait on
       an ABI call another thread is making. */
    Sync<std::map<Hash, std::shared_ptr<const Listing>>> listings;

    /* `AbiStats`; atomics because `list` is called from every eval thread. */
    std::atomic<uint64_t> abiListings{0};
    std::atomic<uint64_t> abiListNs{0};
    std::atomic<uint64_t> abiWalks{0};
    std::atomic<uint64_t> abiWalkNs{0};
};

/**
 * A read-only view of one jj tree object, resolved lazily: a path lookup
 * lists the tree objects along it, and blob contents are read only when a
 * file is read. Listings come from the repository's cache
 * (`JjTreeRepo::list`), shared by every accessor on that repository.
 *
 * `knownTreeRoot` is the root tree id on every instance, including one made
 * by `getSubtree()`, which carries the SUBTREE's id: a
 * subtree of a jj tree is itself a tree object with its own address, and
 * that is what lets a `path:./sub` input inside a jj-backed flake be
 * identified without a hash of its own.
 *
 * Nothing here looks for conflicted entries, because this fork has no
 * per-entry conflict object: a conflict is a multi-term ROOT TREE on the
 * commit (`jj_tree.h`), so it is caught once, at `commitMeta`/`commitTree`,
 * before any accessor exists. Every tree id that reaches this accessor is
 * therefore a single resolved tree.
 */
struct JjTreeAccessor : SourceAccessor
{
    JjTreeAccessor(ref<JjTreeRepo> repo, const Hash & root);

    void readFile(const CanonPath & path, Sink & sink, fun<void(uint64_t)> sizeCallback) override;

    std::optional<Stat> maybeLstat(const CanonPath & path) override;

    DirEntries readDirectory(const CanonPath & path) override;

    std::string readLink(const CanonPath & path) override;

    std::string_view identityClass(const CanonPath & path) override
    {
        return "jj-tree";
    }

    /** The repository's `JjTreeRepo::abiStats`, for the sealing trace. */
    JjTreeRepo::AbiStats abiStats() const
    {
        return repo->abiStats();
    }

    /**
     * Every symlink under this accessor's root, relative to it
     * (`JjTreeRepo::symlinks` on `knownTreeRoot`): what a `readDirectory`
     * walk would find, in one ABI call. A subtree accessor answers for its
     * subtree alone.
     */
    JjTreeRepo::Symlinks symlinks() const
    {
        return repo->symlinks(root);
    }

    /**
     * An accessor for the directory at `path`, rooted there, carrying that
     * subtree's own id as its `knownTreeRoot`.
     *
     * A jj tree always has subtree objects, so this never returns the
     * `nullptr` that `SourceAccessor::getSubtree` reserves for trees that
     * cannot name their subtrees: an absent path throws `FileNotFound` and
     * a non-directory throws `NotADirectory`, per that contract.
     */
    std::shared_ptr<SourceAccessor> getSubtree(const CanonPath & path) override;

    /**
     * An accessor for the tree at `path` with `filter` applied, rooted
     * there, carrying the FILTERED tree's id as its `knownTreeRoot`
     * (`JjTreeRepo::filterTree`). The filter's verdicts are collected from
     * the repository's cached listings, so no blob is read. Never
     * `nullptr`, for the reason `getSubtree` gives.
     */
    std::shared_ptr<SourceAccessor> getFilteredTree(const CanonPath & path, PathFilter & filter) override;

private:
    ref<JjTreeRepo> repo;
    Hash root;

    /**
     * The entry at `path`, or nullopt if any component is absent or an
     * intermediate component is not a directory. The root has no entry.
     */
    std::optional<JjTreeRepo::Entry> lookup(const CanonPath & path);
};

} // namespace nix::fetchers
