#include "nix/fetchers/fetchers.hh"
#include "nix/fetchers/jj-tree.hh"
#include "nix/util/file-system.hh"
#include "nix/util/fmt.hh"
#include "nix/util/url.hh"
#include "nix/util/url-parts.hh"
#include "nix/store/store-api.hh"

#include <regex>

namespace nix::fetchers {

/* A jj input is identified by the blake3 id of its root tree, which jj
   maintains incrementally while snapshotting. The fetcher reads the tree
   through the in-process tree ABI (`jj-tree.hh`): no `jj` subprocess, no
   export to a temporary directory, no NAR walk. The accessor announces the
   id as its `knownTreeRoot`, from which the store path follows with zero
   file reads (paths.cc, fetch-to-store.cc).

   Attributes:
     url         jj+file URL of a workspace on jj's native (ix-local) store.
                 A git-backed jj repository is refused by the ABI; such a
                 repository is a `git+file` input (flakeref.cc already routes
                 a colocated checkout there).
     rev         a commit id, 64 hex chars (blake3). Selects that commit.
     ref         a local bookmark name. Selects the commit it points at.
     treeHash    SRI blake3 id of the root tree: the lock identity. Written
                 by the fetcher; when present on input it is verified.
                 An input is locked only with BOTH `rev` and `treeHash`
                 (`isLocked`): the id says what the tree is, the rev says
                 where to read it without snapshotting anything.
     lastModified, name
                 as for the other VCS fetchers.

   No `revCount`. jj's index stores a generation number (longest-path
   distance from the root commit), not an ancestor count, and the two part
   ways at the first merge; the attribute is rejected rather than filled
   with a number that means something else.

   Without `rev` or `ref` the input is the working copy: jj snapshots it and
   `@` is the commit. Every state of the working copy therefore has a
   revision, which is why writing into a jj source (a lock file, say) needs
   no commit step: `putFile` below writes the bytes and stops, where git's
   override has to `git add` the path and, under `--commit-lock-file`,
   commit it before the result is nameable.

   Two sources can serve a locked input: the repository the URL names, and
   the local store, when it holds a registered object under the locked tree
   id (a `nix copy` from elsewhere, or an earlier forced copy). The
   repository always wins when it exists. The order is load-bearing, not a
   preference: a store object is a flattened copy that cannot name its
   subtrees, while the repository can, so an input read from the store
   would have to refuse every relative `path:./sub` input declared inside
   it. Consulting the store first would therefore make a relative input's
   fate depend on whether something had forced its parent's copy since the
   last evaluation -- rc=0 one evaluation and a refusal the next, with no
   way out short of deleting the store object. The store road exists for a
   host that has the object and no repository, and it is taken only there. */
struct JjInputScheme : InputScheme
{
    std::optional<Input> inputFromURL(const Settings & settings, const ParsedURL & url, bool requireTree) const override
    {
        if (url.scheme != "jj+file")
            return {};

        auto url2(url);
        url2.scheme = std::string(url2.scheme, 3);
        url2.query.clear();

        Attrs attrs;
        attrs.emplace("type", "jj");

        for (auto & [name, value] : url.query) {
            if (name == "rev" || name == "ref")
                attrs.emplace(name, value);
            /* `dir` belongs to the flake layer, which selects a
               subdirectory; the fetcher serves the whole tree. Drop it so
               it does not leak into the stored URL (cf. git.cc). */
            else if (name == "dir")
                continue;
            else
                url2.query.emplace(name, value);
        }

        attrs.emplace("url", url2.to_string());

        return inputFromAttrs(settings, attrs);
    }

    std::string_view schemeName() const override
    {
        return "jj";
    }

    std::string schemeDescription() const override
    {
        return "a Jujutsu (jj) repository on jj's native object store, read in-process by tree id";
    }

    const std::map<std::string, AttributeInfo> & allowedAttrs() const override
    {
        /* No `narHash`: a jj input is locked by `treeHash`, and a lock that
           carries a NAR hash for one was written by an older fetcher whose
           store paths this one does not reproduce. Rejecting the attribute
           makes such a lock fail at parse time with the attribute named,
           instead of at a hash comparison. */
        static const std::map<std::string, AttributeInfo> attrs = {
            {"url", {}},
            {"ref", {}},
            {"rev", {}},
            {"treeHash", {}},
            {"lastModified", {}},
            {"name", {}},
        };
        return attrs;
    }

    std::optional<Input> inputFromAttrs(const Settings & settings, const Attrs & attrs) const override
    {
        parseURL(getStrAttr(attrs, "url"));

        /* `Error`, not `BadURL`, for every refusal in this function. A
           `BadURL` from a scheme means "this is not a URL of mine" to
           `parseURLFlakeRef` (flakeref.cc), which swallows it and retries
           the string as a filesystem path: the refusal below would then
           surface as "getting status of 'jj+file:/...'" with the real reason
           and the `git+file` route gone. The URL was recognised; what is
           wrong is an attribute of it. */
        if (auto ref = maybeGetStrAttr(attrs, "ref"))
            if (!std::regex_match(*ref, refRegex))
                throw Error("invalid Jujutsu bookmark name '%s'", *ref);

        Input input{};
        input.attrs = attrs;

        /* Both ids are checked here, at the boundary, so that every later
           reader can assume blake3. `Input::getRev` accepts 40 hex chars as
           SHA-1 for the Git-based schemes; on this scheme that spelling
           names a commit in a repository the ABI does not read. */
        if (auto rev = input.getRev(); rev && rev->algo != HashAlgorithm::BLAKE3)
            throw Error(
                "'%s' is not a Jujutsu native commit id (64 hex characters); a git-backed jj repository is a 'git+file' input",
                rev->gitRev());
        /* `InputScheme::getTreeHash`, not `input.getTreeHash()`: the scheme
           is attached to the Input only after this function returns
           (`Input::fromAttrs`), so the Input-side spelling would read as
           "no scheme, no opinion" and validate nothing. Every call below
           uses the same inherited one, so there is one parse of `treeHash`
           for this scheme and it cannot be silently skipped. */
        InputScheme::getTreeHash(input);

        return input;
    }

    ParsedURL toURL(const Input & input) const override
    {
        auto url = parseURL(getStrAttr(input.attrs, "url"));
        url.scheme = "jj+" + url.scheme;
        if (auto rev = input.getRev())
            url.query.insert_or_assign("rev", rev->gitRev());
        if (auto ref = input.getRef())
            url.query.insert_or_assign("ref", *ref);
        return url;
    }

    std::optional<std::filesystem::path> getSourcePath(const Input & input) const override
    {
        auto url = parseURL(getStrAttr(input.attrs, "url"));
        if (url.scheme == "file" && !input.getRef() && !input.getRev())
            return urlPathToPath(url.path);
        return {};
    }

    void putFile(
        const Input & input,
        const CanonPath & path,
        std::string_view contents,
        std::optional<std::string> commitMsg) const override
    {
        auto repoPath = getSourcePath(input);
        if (!repoPath)
            throw Error(
                "cannot commit '%s' to Jujutsu repository '%s' because it's not a working tree",
                path,
                input.to_string());

        writeFile(*repoPath / path.rel(), contents);

        /* jj tracks a new file at the next snapshot, and the next fetch of
           this input snapshots. Nothing to "add". */
    }

    std::filesystem::path getActualPath(const Input & input) const
    {
        auto url = parseURL(getStrAttr(input.attrs, "url"));
        if (url.scheme != "file")
            throw Error(
                "Jujutsu input '%s' is not a local working copy; only file:// URLs are supported", input.to_string());
        return absPath(urlPathToPath(url.path));
    }

    /* The store object registered under the tree a lock names, served as
       that input. Only for a final input (a lock entry): its attributes are
       complete, so nothing needs the repository to fill them in, and only
       from a store that holds the object; nothing is substituted, because
       the substituters would put a network round trip in front of every
       evaluation of every jj input, for a tree only a repository can vouch
       for.

       A jj-tree object is not self-certifying (`ValidPathInfo::
       isSelfCertifying`), so it is in the store only because a trusted
       registrant or a trusted key's signature put it there, and the `ca` it
       was registered under has to name this very id: an object at the
       derived path with another `ca` is not that voucher and is refused, not
       worked around.

       The accessor announces the locked id as its tree root so that the
       mount derives the same store path with no reads, the way the
       repository road does. It has no subtree objects; `flake.cc` refuses a
       relative input under it, naming the repository, rather than composing
       a subpath (see `SourceAccessor::getSubtree`). */
    ref<SourceAccessor> storeObjectFor(
        Store & store, const Input & input, const Hash & treeHash, const std::filesystem::path & repoPath) const
    {
        auto storePath = input.computeStorePath(store);
        store.addTempRoot(storePath);

        if (!store.isValidPath(storePath))
            throw Error(
                "%s is not a Jujutsu repository (it has no '.jj' directory), and the tree that input '%s' is locked "
                "to is not in the store either (%s is not a valid path). Bring the repository back, or copy the "
                "object here ('nix copy' from a store that has it).",
                PathFmt(repoPath),
                input.to_string(),
                store.printStorePath(storePath));

        auto expected = ContentAddress{.method = ContentAddressMethod::Raw::JjTree, .hash = treeHash};
        auto info = store.queryPathInfo(storePath);
        if (info->ca != expected)
            throw Error(
                "store path '%s' is registered with content address '%s', not the Jujutsu tree id '%s' that input "
                "'%s' is locked to",
                store.printStorePath(storePath),
                renderContentAddress(info->ca),
                expected.render(),
                input.to_string());

        debug(
            "no Jujutsu repository at %s; serving tree-locked input '%s' from store object '%s'",
            PathFmt(repoPath),
            input.to_string(),
            store.printStorePath(storePath));

        auto accessor = store.requireStoreObjectAccessor(storePath);
        accessor->knownTreeRoot = KnownTreeRoot{.id = treeHash};
        accessor->setPathDisplay("«" + input.to_string() + "»");
        return accessor;
    }

    std::pair<ref<SourceAccessor>, Input>
    getAccessor(const Settings & settings, Store & store, const Input & _input) const override
    {
        Input input(_input);

        auto repoPath = getActualPath(input);

        if (!pathExists(repoPath / ".jj")) {
            /* The absence test is this one `.jj` probe and nothing broader:
               a repository that exists but fails to open (a git-backed store,
               a missing object, a stale working copy) reports its own fault
               below, and the store object never papers over it. */
            if (input.isFinal())
                if (auto treeHash = input.getTreeHash())
                    return {storeObjectFor(store, input, *treeHash, repoPath), std::move(input)};
            throw Error("%s is not a Jujutsu repository (it has no '.jj' directory)", PathFmt(repoPath));
        }

        auto repo = JjTreeRepo::open(repoPath);

        /* Exactly one way to name the commit. `rev` wins over `ref` when
           both are given (applyOverrides can produce that from a lock):
           the rev is the pinned identity, the ref a label for it. */
        auto commit = input.getRev()   ? repo->resolveRev(*input.getRev())
                      : input.getRef() ? repo->resolveBookmark(*input.getRef())
                                       : repo->snapshotWorkingCopy();

        auto meta = repo->commitMeta(commit);

        /* Refused before any tree read. A conflicted tree has no single
           content; whatever a fetcher rendered at the conflicted paths
           would be a choice nobody made, and it would build. */
        if (meta.hasConflict)
            throw Error(
                "Jujutsu revision %s in %s has unresolved conflicts; resolve them before using it as a source",
                commit.gitRev(),
                PathFmt(repoPath));

        auto tree = repo->commitTree(commit);

        /* A lock names the tree; a different tree under the same rev cannot
           happen (a commit id covers its tree), so this only fires when the
           lock and the rev disagree with each other, i.e. the lock was
           edited. Compare ids; never ingest to check. */
        if (auto locked = input.getTreeHash(); locked && *locked != tree)
            throw Error(
                "Jujutsu input '%s' is locked to tree %s but revision %s has tree %s",
                input.to_string(),
                locked->to_string(HashFormat::SRI, true),
                commit.gitRev(),
                tree.to_string(HashFormat::SRI, true));

        input.attrs.insert_or_assign("rev", commit.gitRev());
        input.attrs.insert_or_assign("treeHash", tree.to_string(HashFormat::SRI, true));
        input.attrs.insert_or_assign("lastModified", uint64_t(meta.lastModified));

        auto accessor = repo->getAccessor(tree, "«" + input.to_string() + "»");
        accessor->lastModified = meta.lastModified;
        accessor->rev = commit;

        return {accessor, std::move(input)};
    }

    bool isLocked(const Settings & settings, const Input & input) const override
    {
        /* Both, not the tree id alone. "Locked" means the fetch reads
           exactly this tree without consulting anything mutable: `rev`
           names the commit to read, and `treeHash` is the identity that
           commit is held to. With the id alone the only way to find a
           commit is to snapshot the working copy -- an operation written to
           the repository, answering with whatever is on disk now -- and
           then compare, which is the verification of an UNLOCKED input
           (what a `narHash`-only `path` input gets), not the reading of a
           locked one. Every lock entry carries both, because the fetch that
           wrote it produced both, so nothing that was locked stops being
           locked; a hand-written `treeHash` without `rev` is now warned
           about as unlocked in pure mode instead of counting as pinned. */
        return input.getRev().has_value() && input.getTreeHash().has_value();
    }

    std::optional<std::string> getFingerprint(Store & store, const Input & input) const override
    {
        /* The tree id, not the commit: `jj describe`, `jj new` and every
           other metadata-only rewrite mints a new commit over the same
           tree, and everything this fingerprint keys (the source-path
           cache, the flake eval cache) is a function of the tree. The
           eval cache adds `lastModified` itself (flake.cc), the one
           attribute here that the tree does not determine. The prefix keeps
           these keys out of the plain-rev namespace other fetchers use. */
        if (auto treeHash = input.getTreeHash())
            return "jj-tree:" + treeHash->gitRev();
        return std::nullopt;
    }
};

static auto rJjInputScheme = OnStartup([] { registerInputScheme(std::make_unique<JjInputScheme>()); });

} // namespace nix::fetchers
