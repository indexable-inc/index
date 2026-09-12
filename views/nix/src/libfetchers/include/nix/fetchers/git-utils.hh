#pragma once

#include "nix/fetchers/filtering-source-accessor.hh"
#include "nix/util/fs-sink.hh"

namespace nix {

namespace fetchers {
struct PublicKey;
struct Settings;
} // namespace fetchers

/**
 * A sink that writes into a Git repository. Note that nothing may be written
 * until `flush()` is called.
 */
struct GitFileSystemObjectSink : ExtendedFileSystemObjectSink
{
    /**
     * Flush builder and return a final Git hash.
     */
    virtual Hash flush() = 0;
};

struct GitAccessorOptions
{
    bool exportIgnore = false;
    bool smudgeLfs = false;
    /* Hide submodule entries instead of rendering them as empty directories. */
    bool omitGitlinks = false;
};

struct GitRepo
{
    virtual ~GitRepo() {}

    struct Options
    {
        bool create = false;
        bool bare = false;
        bool packfilesOnly = false;
    };

    static ref<GitRepo> openRepo(const std::filesystem::path & path, Options options);

    virtual uint64_t getRevCount(const Hash & rev) = 0;

    /**
     * Return the commit history reachable from `rev` as a JSON array
     * (rendered to a string), newest first.
     *
     * The result is a pure function of (`rev`, `depth`,
     * `includePaths`): commits are emitted in a deterministic
     * topological order (every commit precedes its parents; among the
     * candidates whose emitted-set children are all emitted, the
     * numerically smallest commit hash is emitted first), and each
     * entry contains only data stored in the commit objects
     * themselves: revision, parent revisions, author/committer (name,
     * email, time, timezone offset), the full commit message, and --
     * if `includePaths` is set -- the paths touched relative to the
     * first parent (relative to the empty tree for root commits), with
     * no rename detection. Path extraction is a tree diff per commit
     * and dominates the export cost on repositories with large trees,
     * which is why it can be switched off.
     *
     * `depth` bounds the emitted set to at most that many commits,
     * chosen level by level outward from the tip (each level holds the
     * commits at that minimal parent-edge distance from `rev`); when
     * the cap lands inside a level, the numerically smallest commit
     * hashes win, keeping the set deterministic. `depth` 0 means
     * unlimited. `parents` always lists all parents, including ones
     * outside the emitted set.
     */
    virtual std::string getHistoryJson(const Hash & rev, uint64_t depth, bool includePaths) = 0;

    virtual uint64_t getLastModified(const Hash & rev) = 0;

    /**
     * Return the root tree of the commit `rev`.
     */
    virtual Hash getTreeHash(const Hash & rev) = 0;

    /**
     * Whether the commit object `rev` carries an extra header field
     * (e.g. Jujutsu's legacy "jj:trees" conflict header).
     */
    virtual bool hasCommitExtraHeader(const Hash & rev, const std::string & field) = 0;

    virtual bool isShallow() = 0;

    /* Return the commit hash to which a ref points. */
    virtual Hash resolveRef(std::string ref) = 0;

    virtual void setRemote(const std::string & name, const std::string & url) = 0;

    /**
     * Info about a submodule.
     */
    struct Submodule
    {
        CanonPath path;
        std::string url;
        std::string branch;
    };

    struct WorkdirInfo
    {
        bool isDirty = false;

        /* The checked out commit, or nullopt if there are no commits
           in the repo yet. */
        std::optional<Hash> headRev;

        /* All modified or added tracked files. Untracked files are not
           listed: they belong to no commit, so they are neither a change
           to the checked-out tree nor visible in what the fetcher
           serves. */
        std::set<CanonPath> dirtyFiles;

        /* The deleted files. */
        std::set<CanonPath> deletedFiles;

        /* The submodules listed in .gitmodules of this workdir. */
        std::vector<Submodule> submodules;
    };

    virtual WorkdirInfo getWorkdirInfo() = 0;

    static WorkdirInfo getCachedWorkdirInfo(const std::filesystem::path & path);

    /**
     * Forget every cached working directory state.
     *
     * `getCachedWorkdirInfo` is keyed on the repository path alone and lives
     * as long as the process, which is right for a process that evaluates one
     * thing: the working tree cannot change underneath it, and asking git
     * twice would only cost time. A process that serves several evaluations
     * has the opposite problem. The tree is edited between them and the path
     * does not change, so the second evaluation reads the first one's state,
     * decides the tree is still clean at the old revision, and answers with
     * the pre-edit derivation in milliseconds. Any caller that outlives a
     * single evaluation has to call this between them.
     */
    static void clearCachedWorkdirInfo();

    /* Get the ref that HEAD points to. */
    virtual std::optional<std::string> getWorkdirRef() = 0;

    /**
     * Return the submodules of this repo at the indicated revision,
     * along with the revision of each submodule.
     */
    virtual std::vector<std::tuple<Submodule, Hash>> getSubmodules(const Hash & rev, bool exportIgnore) = 0;

    virtual std::string resolveSubmoduleUrl(const std::string & url) = 0;

    virtual bool hasObject(const Hash & oid) = 0;

    /**
     * Serve the tree of the object `rev` (a commit or a tree) out of the
     * object store. This is the only accessor over a Git repository: a
     * working directory is not served, because it has no identity to
     * address the result by (`git.cc`, `pinCheckedOutCommit`).
     */
    virtual ref<SourceAccessor>
    getAccessor(const Hash & rev, const GitAccessorOptions & options, std::string displayPrefix) = 0;

    virtual ref<GitFileSystemObjectSink> getFileSystemObjectSink() = 0;

    virtual void flush() = 0;

    /**
     * Fetch `refspec` from `url` by running the `git` executable.
     *
     * `packfilesOnly` keeps the fetched objects in packfiles regardless
     * of their number (`fetch.unpackLimit=1`) and suppresses automatic
     * maintenance, for repositories like the tarball cache whose
     * invariant is to contain only packfiles and whose objects are
     * typically unreferenced (an auto-gc would prune them).
     */
    virtual void
    fetch(const std::string & url, const std::string & refspec, bool shallow, bool packfilesOnly = false) = 0;

    /**
     * If the tree of commit `rev` would export byte-identically to a
     * Git archive of that commit -- i.e. it contains no submodule
     * (gitlink) entries and no `.gitattributes` file mentioning
     * `export-ignore` or `export-subst` -- return that tree's hash.
     * Otherwise return std::nullopt.
     */
    virtual std::optional<Hash> getArchiveCompatibleTree(const Hash & rev) = 0;

    /**
     * Verify that commit `rev` is signed by one of the keys in
     * `publicKeys`. Throw an error if it isn't.
     */
    virtual void verifyCommit(const Hash & rev, const std::vector<fetchers::PublicKey> & publicKeys) = 0;

    /**
     * Given a Git tree hash, compute the hash of its NAR
     * serialisation. This is memoised on-disk.
     */
    virtual Hash treeHashToNarHash(const fetchers::Settings & settings, const Hash & treeHash) = 0;

    /**
     * If the specified Git object is a directory with a single entry
     * that is a directory, return the ID of that object.
     * Otherwise, return the passed ID unchanged.
     */
    virtual Hash dereferenceSingletonDirectory(const Hash & oid) = 0;
};

// A helper to ensure that the `git_*_free` functions get called.
template<auto del>
struct Deleter
{
    template<typename T>
    void operator()(T * p) const
    {
        del(p);
    };
};

// A helper to ensure that we don't leak objects returned by libgit2.
template<typename T>
struct Setter
{
    T & t;
    typename T::pointer p = nullptr;

    Setter(T & t)
        : t(t)
    {
    }

    ~Setter()
    {
        if (p)
            t = T(p);
    }

    operator typename T::pointer *()
    {
        return &p;
    }
};

/**
 * Checks that the string can be a valid git reference, branch or tag name.
 * Accepts shorthand references (one-level refnames are allowed), pseudorefs
 * like `HEAD`.
 *
 * @note This is a coarse test to make sure that the refname is at least something
 * that Git can make sense of.
 */
bool isLegalRefName(const std::string & refName);

} // namespace nix
