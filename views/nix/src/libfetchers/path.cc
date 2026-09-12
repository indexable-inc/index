#include "nix/fetchers/fetchers.hh"
#include "nix/fetchers/filtering-source-accessor.hh"
#include "nix/store/store-api.hh"
#include "nix/fetchers/fetch-settings.hh"
#include "nix/util/file-system.hh"
#include "nix/util/source-accessor.hh"
#include "nix/fetchers/fetch-to-store.hh"

namespace nix::fetchers {

/* A store object's accessor is rooted at the object; a `path:` input may
   name a directory (or file) inside one, and the input is that directory,
   so it is served rooted there. `FilteringSourceAccessor::prefix` is the
   re-rooting libfetchers already has; the policy is the whole tree, which
   is what an always-true `isAllowed` says, and the not-allowed error is
   therefore unreachable. Everything below the subpath is as immutable as
   the object around it, so the mount's store-object test (`mountInput`)
   still holds: the physical path stays inside the real store. The subpath
   is NAR-hashed on mount, a read of that directory, which is the price any
   NAR-addressed input pays. */
struct StoreSubpathAccessor : FilteringSourceAccessor
{
    StoreSubpathAccessor(SourcePath src)
        : FilteringSourceAccessor(
              src, [](const CanonPath & path) { return RestrictedPathError("access to path '%s' is forbidden", path); })
    {
    }

    bool isAllowed(const CanonPath & path) override
    {
        return true;
    }
};

struct PathInputScheme : InputScheme
{
    std::optional<Input> inputFromURL(const Settings & settings, const ParsedURL & url, bool requireTree) const override
    {
        if (url.scheme != "path")
            return {};

        if (url.authority && url.authority->host.size())
            throw Error("path URL '%s' should not have an authority ('%s')", url, *url.authority);

        Input input{};
        input.attrs.insert_or_assign("type", "path");
        input.attrs.insert_or_assign("path", urlPathToPath(url.path).string());

        for (auto & [name, value] : url.query)
            if (name == "rev" || name == "narHash")
                input.attrs.insert_or_assign(name, value);
            else if (name == "revCount" || name == "lastModified") {
                if (auto n = string2Int<uint64_t>(value))
                    input.attrs.insert_or_assign(name, *n);
                else
                    throw Error("path URL '%s' has invalid parameter '%s'", url, name);
            } else
                throw Error("path URL '%s' has unsupported parameter '%s'", url, name);

        return input;
    }

    std::string_view schemeName() const override
    {
        return "path";
    }

    std::string schemeDescription() const override
    {
        // TODO
        return "";
    }

    const std::map<std::string, AttributeInfo> & allowedAttrs() const override
    {
        static const std::map<std::string, AttributeInfo> attrs = {
            {
                "path",
                {},
            },
            /* Allow the user to pass in "fake" tree info
               attributes. This is useful for making a pinned tree work
               the same as the repository from which is exported (e.g.
               path:/nix/store/...-source?lastModified=1585388205&rev=b0c285...).
             */
            {
                "rev",
                {},
            },
            {
                "revCount",
                {},
            },
            {
                "lastModified",
                {},
            },
            {
                "narHash",
                {},
            },
        };
        return attrs;
    }

    std::optional<Input> inputFromAttrs(const Settings & settings, const Attrs & attrs) const override
    {
        getStrAttr(attrs, "path");

        Input input{};
        input.attrs = attrs;
        return input;
    }

    ParsedURL toURL(const Input & input) const override
    {
        auto query = attrsToQuery(input.attrs);
        query.erase("path");
        query.erase("type");
        query.erase("__final");
        return ParsedURL{
            .scheme = "path",
            .path = splitString<std::vector<std::string>>(getStrAttr(input.attrs, "path"), "/"),
            .query = query,
        };
    }

    std::optional<std::filesystem::path> getSourcePath(const Input & input) const override
    {
        return getAbsPath(input);
    }

    void putFile(
        const Input & input,
        const CanonPath & path,
        std::string_view contents,
        std::optional<std::string> commitMsg) const override
    {
        writeFile(getAbsPath(input) / path.rel(), contents);
    }

    std::optional<std::filesystem::path> isRelative(const Input & input) const override
    {
        std::filesystem::path path = getStrAttr(input.attrs, "path");
        if (path.is_absolute())
            return std::nullopt;
        else
            return path;
    }

    bool isLocked(const Settings & settings, const Input & input) const override
    {
        return (bool) input.getNarHash();
    }

    /* The store object's NAR hash plus the subpath into it: a sound key for
       the source-path cache, since both are immutable. Without one, every
       evaluation of a `path:/nix/store/...` input re-walks the object's NAR
       ("uncacheable"), and the functional suite's barf mode turns that into
       a refusal. Only store paths are served by this scheme, but a
       fingerprint may be asked of an input that has not been fetched (and
       refused) yet, so anything else answers "none" rather than throwing
       from a question. */
    std::optional<std::string> getFingerprint(Store & store, const Input & input) const override
    {
        if (isRelative(input))
            return std::nullopt;
        auto absPath = getAbsPath(input);
        /* Anything outside the store is refused at fetch time and has no
           identity to key a cache with, so it answers "none"; a fingerprint
           may be asked of an input that has not been fetched (and refused)
           yet. Inside the store the answer must not degrade: a failure to
           read the object's info propagates rather than silently turning
           every evaluation into a NAR re-walk of the object. */
        if (!store.isInStore(absPath.string()))
            return std::nullopt;
        auto [storePath, subPath] = store.toStorePath(absPath.string());
        /* An object this store does not hold yet (a host object seen through a
           chroot store; `getAccessor` copies it in) has no info here to key a
           cache with. Once it has been copied, the lookup below answers. */
        if (!store.isValidPath(storePath))
            return std::nullopt;
        auto info = store.queryPathInfo(storePath);
        return fmt("path:%s:%s", info->narHash.to_string(HashFormat::Base16, false), subPath.abs());
    }

    std::filesystem::path getAbsPath(const Input & input) const
    {
        std::filesystem::path path = getStrAttr(input.attrs, "path");

        if (path.is_absolute())
            return canonPath(path);

        /* A relative path names a directory inside another tree and has no
           meaning on its own: as a flake input it resolves through the
           parent flake's accessor (`flake.cc`, `resolveRelativePath`) and
           never reaches this scheme. Anything else that lands here (a
           direct `fetchTree { type = "path"; path = "./x"; }`) has no
           parent to resolve against. */
        throw Error(
            "cannot fetch input '%s' because it uses a relative path; a relative path resolves only as a flake "
            "input, inside the flake that declares it",
            input.to_string());
    }

    std::pair<ref<SourceAccessor>, Input>
    getAccessor(const Settings & settings, Store & store, const Input & _input) const override
    {
        Input input(_input);

        auto absPath = getAbsPath(input);

        /* A directory on the filesystem has no identity. Nothing fixes its
           contents while an evaluation reads them, so two reads of one
           flake ref can see two trees, and a writer landing mid-evaluation
           puts both of them into one result. Nix used to paper over that
           by copying the whole directory into the store on every
           evaluation: it cost a full read of the tree per eval and it did
           not even close the race, because the copy can be raced too.

           The copy is gone, so the only trees this scheme serves are the
           ones that already have an identity: store objects, immutable
           once registered. Anything else is refused here, in the fetcher,
           where the fix can be named -- putting the directory under a
           version control system is what gives it an identity, and the
           resulting revision is what the input then locks to. */
        if (!store.isInStore(absPath.string())) {
            if (!pathExists(absPath))
                throw Error("input '%s' does not exist", input.to_string());

            /* `.git` is tested first because that is the precedence a bare
               path already gets (`flakeref.cc`), and because a colocated
               repository -- one with both directories -- is Git-backed:
               the jj fetcher reads jj's native store and refuses such a
               repository, naming `git+file` itself. */
            auto advice = [&]() -> std::string {
                if (pathExists(absPath / ".git"))
                    return fmt(
                        "It is a Git working tree: refer to it as 'git+file://%s', which locks the input to the "
                        "commit it has checked out (that tree must have no uncommitted changes), or as "
                        "'git+file://%s?rev=<commit>' to name a commit directly.",
                        absPath.string(),
                        absPath.string());
                if (pathExists(absPath / ".jj"))
                    return fmt(
                        "It is a Jujutsu workspace: refer to it as 'jj+file://%s', which locks the input to the tree "
                        "id of the commit it has checked out.",
                        absPath.string());
                return fmt(
                    "Give it an identity first: 'jj init' in it, then refer to it as 'jj+file://%s'.",
                    absPath.string());
            }();

            throw Error(
                "input '%s' is a mutable %s (%s), which has no identity a lock file can name and is therefore "
                "not fetchable. %s",
                input.to_string(),
                std::filesystem::is_directory(absPath) ? "directory" : "path",
                absPath.string(),
                advice);
        }

        /* `toStorePath`, not `maybeParseStorePath`: the input may name a path
           INSIDE a store object, which is as immutable as the object and is
           served rooted at that path. */
        auto [storePath, subpath] = store.toStorePath(absPath.string());

        if (!store.isValidPath(storePath)) {
            /* Under the store directory, yet not an object of THIS store: a
               chroot store (`nix` inside a build sandbox, `--store /tmp/x`)
               names its objects under the same store directory as the host
               store whose objects it sees on disk, so a `path:` input of a
               host object lands here. Those bytes are a store object all the
               same, immutable by the host store's contract, so they are
               brought into this store the way `nix copy` would bring them:
               once, content-addressed, and from then on served like any
               object here. This is the one copy left in this scheme, and it
               is a store-to-store transfer of an immutable object, not the
               retired snapshot of a live directory. A store path that is on
               no disk either is an error naming both places it was looked
               for. */
            auto onDisk = store.printStorePath(storePath);
            if (!pathExists(onDisk))
                throw Error(
                    "input '%s' names a store path that store '%s' does not hold and that does not exist on disk",
                    input.to_string(),
                    store.config.getReference().render(/*withParams=*/true));
            storePath = fetchToStore(settings, store, SourcePath{makeFSSourceAccessor(onDisk)}, FetchMode::Copy, storePath.name());
        }
        store.addTempRoot(storePath);

        /* The store object is served in place, under its own name: it is
           already content-addressed, so re-ingesting it would only mint a
           second store path for bytes the store already holds. A subpath of
           it is a different tree and gets its own. */
        ref<SourceAccessor> accessor = store.requireStoreObjectAccessor(storePath);

        if (!subpath.isRoot()) {
            /* Named now, at the input, rather than on the first read of a
               path that does not exist. */
            accessor->lstat(subpath);
            accessor = make_ref<StoreSubpathAccessor>(SourcePath{accessor, subpath});
        }

        /* Trust the lastModified value supplied by the user, if
           any. It's not a "secure" attribute so we don't care.
           Store objects carry no meaningful mtime of their own (every
           file in the store is stamped at the epoch), which is why the
           default is 0 -- as it already was for a store path that did not
           have to be copied. */
        if (!input.getLastModified())
            input.attrs.insert_or_assign("lastModified", uint64_t(0));

        return {accessor, std::move(input)};
    }

    std::optional<ExperimentalFeature> experimentalFeature() const override
    {
        return Xp::Flakes;
    }
};

static auto rPathInputScheme = OnStartup([] { registerInputScheme(std::make_unique<PathInputScheme>()); });

} // namespace nix::fetchers
