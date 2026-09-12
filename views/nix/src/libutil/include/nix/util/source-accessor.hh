#pragma once

#include <filesystem>

#include "nix/util/canon-path.hh"
#include "nix/util/fun.hh"
#include "nix/util/hash.hh"
#include "nix/util/ref.hh"

namespace nix {

struct Sink;

/**
 * Note there is a decent chance this type soon goes away because the problem is solved another way.
 * See the discussion in https://github.com/NixOS/nix/pull/9985.
 */
enum class SymlinkResolution {
    /**
     * Resolve symlinks in the ancestors only.
     *
     * Only the last component of the result is possibly a symlink.
     */
    Ancestors,

    /**
     * Resolve symlinks fully, realpath(3)-style.
     *
     * No component of the result will be a symlink.
     */
    Full,
};

MakeError(SourceAccessorError, Error);
MakeError(FileNotFound, SourceAccessorError);
MakeError(NotASymlink, SourceAccessorError);
MakeError(NotADirectory, SourceAccessorError);
MakeError(NotARegularFile, SourceAccessorError);

/**
 * A tree id a fetcher already knows for the tree its accessor's root
 * serves: the BLAKE3 Merkle root that jj's native (non-Git) object store
 * maintained incrementally while snapshotting.
 *
 * Exactly one addressing scheme is represented, on purpose. The only use
 * for a known tree id is to turn it into a store path, which requires
 * ingesting the tree the same way the id was computed, and the one method
 * that ingests trees this way is `ContentAddressMethod::Raw::JjTree`. Every
 * consumer pairs this id with that method and nothing else (`paths.cc`,
 * `fetch-to-store.cc`). Nix cannot recompute the id from the files, which
 * is why an accessor announcing it must serve the object store's bytes
 * verbatim: the id is trusted, never checked.
 *
 * Git tree ids are deliberately NOT announced any more. A git input is
 * locked by `narHash` (`Input::fetchToStore`, the road `nix flake
 * prefetch` and `nix flake archive` take), so a mount that addressed the
 * same input by its git tree id would give one input two store paths
 * depending on the road, and could never verify the lock it was handed.
 * Git is the boundary bridge; its identity here is the NAR hash, one per
 * input.
 *
 * A struct rather than a bare `std::optional<Hash>` so that the id cannot
 * be mistaken for a hash of some other serialization at a call site.
 */
struct KnownTreeRoot
{
    /** The BLAKE3 id of the root tree object. */
    Hash id;
};

/**
 * A read-only filesystem abstraction. This is used by the Nix
 * evaluator and elsewhere for accessing sources in various
 * filesystem-like entities (such as the real filesystem, tarballs or
 * Git repositories).
 */
struct SourceAccessor : std::enable_shared_from_this<SourceAccessor>
{
    const size_t number;

    std::string displayPrefix, displaySuffix;

    SourceAccessor();

    virtual ~SourceAccessor() {}

    /**
     * Return the contents of a file as a string.
     *
     * @note Unlike Unix, this method should *not* follow symlinks. Nix
     * by default wants to manipulate symlinks explicitly, and not
     * implicitly follow them, as they are frequently untrusted user data
     * and thus may point to arbitrary locations. Acting on the targets
     * targets of symlinks should only occasionally be done, and only
     * with care.
     */
    std::string readFile(const CanonPath & path);

    /**
     * Write the contents of a file as a sink. `sizeCallback` must be
     * called with the size of the file before any data is written to
     * the sink.
     *
     * @note Like the other `readFile`, this method should *not* follow
     * symlinks.
     *
     * @note subclasses of `SourceAccessor` need to implement at least
     * one of the `readFile()` variants.
     */
    virtual void readFile(const CanonPath & path, Sink & sink, fun<void(uint64_t)> sizeCallback = [](uint64_t size) {});

    virtual bool pathExists(const CanonPath & path);

    enum Type {
        tRegular,
        tSymlink,
        tDirectory,
        /**
          Any other node types that may be encountered on the file system, such as device nodes, sockets, named pipe,
          and possibly even more exotic things.

          Responsible for `"unknown"` from `builtins.readFileType "/dev/null"`.

          Unlike `DT_UNKNOWN`, this must not be used for deferring the lookup of types.
        */
        tChar,
        tBlock,
        tSocket,
        tFifo,
        tUnknown
    };

    struct Stat
    {
        Type type = tUnknown;

        /**
         * For regular files only: the size of the file. Not all
         * accessors return this since it may be too expensive to
         * compute.
         */
        std::optional<uint64_t> fileSize;

        /**
         * For regular files only: whether this is an executable.
         */
        bool isExecutable = false;

        /**
         * For regular files only: the position of the contents of this
         * file in the NAR. Only returned by NAR accessors.
         */
        std::optional<uint64_t> narOffset;

        bool isNotNARSerialisable();
        std::string typeString();
    };

    virtual Stat lstat(const CanonPath & path);

    virtual std::optional<Stat> maybeLstat(const CanonPath & path) = 0;

    typedef std::optional<Type> DirEntry;

    typedef std::map<std::string, DirEntry> DirEntries;

    /**
     * @note Like `readFile`, this method should *not* follow symlinks.
     */
    virtual DirEntries readDirectory(const CanonPath & path) = 0;

    virtual std::string readLink(const CanonPath & path) = 0;

    virtual void dumpPath(const CanonPath & path, Sink & sink, PathFilter & filter = defaultPathFilter);

    Hash
    hashPath(const CanonPath & path, PathFilter & filter = defaultPathFilter, HashAlgorithm ha = HashAlgorithm::SHA256);

    /**
     * Return a corresponding path in the root filesystem, if
     * possible. This is only possible for filesystems that are
     * materialized in the root filesystem.
     */
    virtual std::optional<std::filesystem::path> getPhysicalPath(const CanonPath & path)
    {
        return std::nullopt;
    }

    bool operator==(const SourceAccessor & x) const
    {
        return number == x.number;
    }

    auto operator<=>(const SourceAccessor & x) const
    {
        return number <=> x.number;
    }

    void setPathDisplay(std::string displayPrefix, std::string displaySuffix = "");

    /**
     * A name for the kind of thing this accessor is, stable across two runs of
     * the same evaluation.
     *
     * `displayPrefix` cannot serve. For the accessor answering for the local
     * filesystem it is deliberately empty, so that paths print as `/foo/bar`
     * rather than carrying a prefix, and that is the accessor behind every
     * ordinary `import`. Giving it a display to satisfy a trace would change
     * every error message that mentions a path.
     *
     * An empty return means this accessor cannot name itself, which the
     * read-set trace records as such rather than papering over: an unnamed
     * tree can only be matched across runs by the order it was first seen in,
     * and that is a guess indistinguishable from a measurement.
     *
     * Takes a path, and composite accessors delegate on it, for the same
     * reason `getFingerprint` does: which tree answers for a path is a
     * property of the path, not of the outermost accessor.
     */
    virtual std::string_view identityClass(const CanonPath & path)
    {
        return "";
    }

    virtual std::string showPath(const CanonPath & path);

    /**
     * Resolve any symlinks in `path` according to the given
     * resolution mode.
     *
     * @param mode might only be a temporary solution for this.
     * See the discussion in https://github.com/NixOS/nix/pull/9985.
     */
    CanonPath resolveSymlinks(const CanonPath & path, SymlinkResolution mode = SymlinkResolution::Full);

    /**
     * A string that uniquely represents the contents of this
     * accessor. This is used for caching lookups (see `fetchToStore()`).
     */
    std::optional<std::string> fingerprint;

    /**
     * If set, the jj tree id that this accessor's root provably serves. Set
     * only by fetchers that read straight out of jj's content-addressed
     * object store and know the served tree is byte-identical to the named
     * object: no omitted entries, no content stored outside the tree.
     *
     * A consumer turns it into a store path through
     * `ContentAddressMethod::Raw::JjTree` and nothing else, with zero file
     * reads, where the NAR method's flat hash re-reads the whole tree on
     * every content change. Nix cannot name `ContentAddressMethod` here
     * (that is a libstore type), which is why the pairing is stated at each
     * consumer.
     *
     * An accessor that announces an id must also answer `getSubtree`: see
     * there. The two are one contract, because an announced id says the
     * tree is a Merkle object, and a Merkle object's directories have ids
     * of their own.
     */
    std::optional<KnownTreeRoot> knownTreeRoot;

    /**
     * Return an accessor whose root is the directory `path` of this tree,
     * or `nullptr` when this tree cannot name its subtrees.
     *
     * Only an accessor reading a Merkle object store can answer, and it
     * answers with the subtree's own object: the result carries a
     * `knownTreeRoot` naming that subtree, a `fingerprint` derived from it,
     * and serves the subtree's bytes verbatim, so it is a complete tree in
     * its own right. A subtree id depends on the subtree's content alone,
     * never on where it sits, so the same directory committed at the root
     * of another repository has the same id and, through
     * `ContentAddressMethod::Raw::JjTree`, the same store path. That is what
     * lets a relative `path:./sub` flake input inside a jj-backed flake
     * evaluate as its own store object with no identity of its own in the
     * lock file: the parent's tree id already fixes it (`flake.cc`,
     * `resolveRelativePath`).
     *
     * Throws `FileNotFound` if `path` does not exist and `NotADirectory`
     * if it is not a directory.
     *
     * `nullptr` is a licence, not an error: it tells the caller to address
     * the directory as a subpath of this tree's store object
     * (`<parent>/sub`). That is sound only for a tree whose store path is a
     * hash of its own bytes (a plain filesystem, a NAR): then `<parent>/sub`
     * is as good an identity as the directory has anywhere. It is NOT sound
     * for a tree that announces a `knownTreeRoot`: the directory then has an
     * id of its own in the object store, and `<parent>/sub` would be a
     * second identity for the same bytes, chosen by which accessor happened
     * to serve the parent. So an accessor that announces an id must answer
     * with the subtree object or throw, never `nullptr`, and a caller that
     * receives `nullptr` from a mount that announces an id refuses rather
     * than composes (`flake.cc` does).
     *
     * Composite accessors (mounts, unions, filters) delegate on the path
     * like `getFingerprint`: which tree answers is a property of the path.
     */
    virtual std::shared_ptr<SourceAccessor> getSubtree(const CanonPath & path)
    {
        return nullptr;
    }

    /**
     * Return an accessor serving the directory `path` of this tree with
     * `filter` applied, as a complete tree object of its own, or `nullptr`
     * when this tree cannot name such an object.
     *
     * The result is what `builtins.path { filter = ...; }` denotes: the
     * entries under `path` for which `filter` answers true, `filter` being
     * asked in THIS accessor's coordinates (the absolute path `dumpPath`
     * would pass it), a refused directory pruning everything beneath it.
     * Only an accessor reading a Merkle object store can answer, and it
     * answers as `getSubtree` does: the result carries a `knownTreeRoot`
     * naming the filtered tree, so its store path follows from that id with
     * zero file reads, and a subtree the filter left intact keeps its own
     * id and so its own object. The id is a function of the kept bytes
     * alone, which is what makes a filtered copy stable under edits
     * elsewhere in its source.
     *
     * A directory the filter empties is dropped from the result (the object
     * store has no empty directories), where a NAR copy under the same
     * filter keeps an empty directory. That is the one visible difference
     * between the two roads.
     *
     * Throws as `getSubtree` does. Composites do not delegate: the caller
     * that wants the object asks the mount (`EvalState::addPathToStore`),
     * because the impure root filesystem is a union in which a materialized
     * mount is present twice and cannot be addressed as one tree
     * (`UnionSourceAccessor::getSubtree`).
     */
    virtual std::shared_ptr<SourceAccessor> getFilteredTree(const CanonPath & path, PathFilter & filter)
    {
        return nullptr;
    }

    /**
     * Return the fingerprint for `path`. This is usually the
     * fingerprint of the current accessor, but for composite
     * accessors (like `MountedSourceAccessor`), we want to return the
     * fingerprint of the "inner" accessor if the current one lacks a
     * fingerprint.
     *
     * So this method is intended to return the most-outer accessor
     * that has a fingerprint for `path`. It also returns the path that `path`
     * corresponds to in that accessor.
     *
     * For example: in a `MountedSourceAccessor` that has
     * `/nix/store/foo` mounted,
     * `getFingerprint("/nix/store/foo/bar")` will return the path
     * `/bar` and the fingerprint of the `/nix/store/foo` accessor.
     */
    virtual std::pair<CanonPath, std::optional<std::string>> getFingerprint(const CanonPath & path)
    {
        return {path, fingerprint};
    }

    /**
     * The last-modified time of this tree, if known. Fetchers that
     * know it set it (the Git fetcher uses the commit time) so that
     * composite accessors can surface it per subtree.
     */
    std::optional<time_t> lastModified;

    /**
     * Return the maximum last-modified time of the files in this
     * tree, if available.
     */
    virtual std::optional<time_t> getLastModified()
    {
        return lastModified;
    }

    /**
     * Return the last-modified time of the tree containing `path`.
     * Composite accessors (mounts) answer from the innermost mount,
     * so a Git submodule reports its own commit time rather than its
     * parent's; plain accessors answer path-independently.
     */
    virtual std::optional<time_t> getLastModified(const CanonPath & path)
    {
        return getLastModified();
    }

    /**
     * The revision this tree was fetched from, if known (the Git
     * fetcher sets it to the commit hash).
     */
    std::optional<Hash> rev;

    /**
     * Return the revision pinning the tree rooted at `path`. A rev
     * describes a whole tree, so plain accessors only answer at the
     * root; composite accessors answer at mount roots, so a Git
     * submodule reports its own gitlink commit while paths inside a
     * tree report nothing.
     */
    virtual std::optional<Hash> getRev(const CanonPath & path)
    {
        return path.isRoot() ? rev : std::nullopt;
    }

    /**
     * Invalidate any cached value the accessor may have for the specified path.
     */
    virtual void invalidateCache(const CanonPath & path) {}
};

/**
 * Return a source accessor that contains only an empty root directory.
 */
ref<SourceAccessor> makeEmptySourceAccessor();

/**
 * Exception thrown when accessing a filtered path (see
 * `FilteringSourceAccessor`).
 */
MakeError(RestrictedPathError, Error);

struct SymlinkNotAllowed final : public CloneableError<SymlinkNotAllowed, Error>
{
    CanonPath path;

    SymlinkNotAllowed(CanonPath path)
        : CloneableError("relative path '%s' points to a symlink, which is not allowed", path.rel())
        , path(std::move(path))
    {
    }

    template<typename... Args>
    SymlinkNotAllowed(CanonPath path, const std::string & fs, Args &&... args)
        : CloneableError(fs, std::forward<Args>(args)...)
        , path(std::move(path))
    {
    }
};

/**
 * Return an accessor for the root filesystem.
 */
ref<SourceAccessor> getFSSourceAccessor();

/**
 * Construct an accessor for the filesystem rooted at `root`. Note
 * that it is not possible to escape `root` by appending `..` path
 * elements, and that absolute symlinks are resolved relative to
 * `root`.
 */
ref<SourceAccessor> makeFSSourceAccessor(std::filesystem::path root, bool trackLastModified = false);

/**
 * Construct an accessor that presents a "union" view of a vector of
 * underlying accessors. Earlier accessors take precedence over later.
 */
ref<SourceAccessor> makeUnionSourceAccessor(std::vector<ref<SourceAccessor>> && accessors);

} // namespace nix
