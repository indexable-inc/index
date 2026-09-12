#include "nix/store/filetransfer.hh"
#include "nix/fetchers/cache.hh"
#include "nix/store/globals.hh"
#include "nix/store/store-api.hh"
#include "nix/util/types.hh"
#include "nix/util/url-parts.hh"
#include "nix/util/git.hh"
#include "nix/util/hash.hh"
#include "nix/fetchers/fetchers.hh"
#include "nix/fetchers/fetch-settings.hh"
#include "nix/fetchers/tarball.hh"
#include "nix/util/tarfile.hh"
#include "nix/fetchers/git-utils.hh"

#include <optional>
#include <nlohmann/json.hpp>
#include <fstream>

namespace nix::fetchers {

struct DownloadUrl
{
    ParsedURL url;
    Headers headers;
};

// A github, gitlab, or sourcehut host
const static std::string hostRegexS = "[a-zA-Z0-9.-]*"; // FIXME: check
std::regex hostRegex(hostRegexS, std::regex::ECMAScript);

struct GitArchiveInputScheme : InputScheme
{
    virtual std::optional<std::pair<std::string, std::string>>
    accessHeaderFromToken(const std::string & token) const = 0;

    std::optional<Input>
    inputFromURL(const fetchers::Settings & settings, const ParsedURL & url, bool requireTree) const override
    {
        if (url.scheme != schemeName())
            return {};

        /* This ignores empty path segments for back-compat. Older versions used a tokenizeString here. */
        auto path = url.pathSegments(/*skipEmpty=*/true) | std::ranges::to<std::vector<std::string>>();

        std::optional<std::string> rev;
        std::optional<std::string> ref;
        std::optional<std::string> host_url;
        std::optional<bool> submodules;
        std::optional<bool> lfs;

        auto size = path.size();
        if (size == 3) {
            if (std::regex_match(path[2], revRegex))
                rev = path[2];
            else
                ref = path[2];
        } else if (size > 3) {
            std::string rs;
            for (auto i = std::next(path.begin(), 2); i != path.end(); i++) {
                rs += *i;
                if (std::next(i) != path.end()) {
                    rs += "/";
                }
            }
            ref = rs;
        } else if (size < 2)
            throw BadURL("URL '%s' is invalid", url);

        for (auto & [name, value] : url.query) {
            if (name == "rev") {
                if (rev)
                    throw BadURL("URL '%s' contains multiple commit hashes", url);
                rev = value;
            } else if (name == "ref") {
                if (ref)
                    throw BadURL("URL '%s' contains multiple branch/tag names", url);
                ref = value;
            } else if (name == "host")
                host_url = value;
            else if (name == "submodules")
                submodules = value == "1";
            else if (name == "lfs")
                lfs = value == "1";
            // FIXME: barf on unsupported attributes
        }

        Attrs attrs;
        attrs.insert_or_assign("type", std::string{schemeName()});
        attrs.insert_or_assign("owner", path[0]);
        attrs.insert_or_assign("repo", path[1]);
        if (rev)
            attrs.insert_or_assign("rev", *rev);
        if (ref)
            attrs.insert_or_assign("ref", *ref);
        if (host_url)
            attrs.insert_or_assign("host", *host_url);
        if (submodules)
            attrs.insert_or_assign("submodules", Explicit<bool>{*submodules});
        if (lfs)
            attrs.insert_or_assign("lfs", Explicit<bool>{*lfs});

        auto narHash = url.query.find("narHash");
        if (narHash != url.query.end())
            attrs.insert_or_assign("narHash", narHash->second);

        return inputFromAttrs(settings, attrs);
    }

    const std::map<std::string, AttributeInfo> & allowedAttrs() const override
    {
        static const std::map<std::string, AttributeInfo> attrs = {
            {
                "owner",
                {},
            },
            {
                "repo",
                {},
            },
            {
                "ref",
                {},
            },
            {
                "rev",
                {},
            },
            {
                "narHash",
                {},
            },
            {
                "lastModified",
                {},
            },
            {
                "host",
                {},
            },
            {
                "submodules",
                {
                    .type = "Bool",
                    .required = false,
                    .doc = R"(
                      Also fetch submodules. Forge archive tarballs never
                      contain submodule content, so enabling this fetches the
                      repository through the equivalent `git+https` input
                      instead (same revision, `submodules = true`); see the
                      `git` input scheme for the exact fetch semantics.

                      Note that a Git checkout is not always bit-identical to
                      the forge's archive of the same revision: the archive
                      honors the `export-ignore` and `export-subst` Git
                      attributes, a Git checkout does not. Inputs that request
                      neither `submodules` nor `lfs` are unaffected and keep
                      tarball semantics and hashes.

                      Default: `false`
                    )",
                },
            },
            {
                "lfs",
                {
                    .type = "Bool",
                    .required = false,
                    .doc = R"(
                      Also fetch Git LFS files. Forge archive tarballs contain
                      LFS pointer files rather than the large file content, so
                      enabling this fetches the repository through the
                      equivalent `git+https` input instead, like `submodules`.

                      Default: `false`
                    )",
                },
            },
        };
        return attrs;
    }

    std::optional<Input> inputFromAttrs(const fetchers::Settings & settings, const Attrs & attrs) const override
    {
        getStrAttr(attrs, "owner");
        getStrAttr(attrs, "repo");

        auto ref = maybeGetStrAttr(attrs, "ref");
        auto rev = maybeGetStrAttr(attrs, "rev");
        if (ref && rev)
            throw BadURL(
                "input %s contains both a commit hash ('%s') and a branch/tag name ('%s')",
                attrsToJSON(attrs),
                *rev,
                *ref);

        if (rev)
            Hash::parseAny(*rev, HashAlgorithm::SHA1);

        if (ref && !isLegalRefName(*ref))
            throw BadURL("input %s contains an invalid branch/tag name", attrsToJSON(attrs));

        if (auto host = maybeGetStrAttr(attrs, "host"); host && !std::regex_match(*host, hostRegex))
            throw BadURL("input %s contains an invalid instance host", attrsToJSON(attrs));

        /* Delegate to the git scheme: an archive tarball cannot provide
           the requested tree, and constructing the git input here means
           everything downstream, locking included, sees the input that
           is actually fetched. A lock file therefore records a plain
           `git` input that any Nix understands, instead of a forge
           input carrying an attribute other Nix versions reject. */
        if (auto gitAttrs = gitEquivalentAttrs(attrs))
            return Input::fromAttrs(settings, std::move(*gitAttrs));

        Input input{};
        input.attrs = attrs;
        /* An explicit `false` is the archive default; drop it so locked
           forge inputs never carry an attribute other schemes and older
           Nix versions reject. */
        input.attrs.erase("submodules");
        input.attrs.erase("lfs");
        return input;
    }

    ParsedURL toURL(const Input & input) const override
    {
        auto owner = getStrAttr(input.attrs, "owner");
        auto repo = getStrAttr(input.attrs, "repo");
        auto ref = input.getRef();
        auto rev = input.getRev();
        std::vector<std::string> path{owner, repo};
        assert(!(ref && rev));
        if (ref)
            path.push_back(*ref);
        if (rev)
            path.push_back(rev->to_string(HashFormat::Base16, false));
        auto url = ParsedURL{
            .scheme = std::string{schemeName()},
            .path = path,
        };
        if (auto narHash = input.getNarHash())
            url.query.insert_or_assign("narHash", narHash->to_string(HashFormat::SRI, true));
        auto host = maybeGetStrAttr(input.attrs, "host");
        if (host)
            url.query.insert_or_assign("host", *host);
        return url;
    }

    // Search for the longest possible match starting from the beginning and ending at either the end or a path segment.
    std::optional<std::string> getAccessToken(
        const fetchers::Settings & settings, const std::string & host, const std::string & url) const override
    {
        auto tokens = settings.accessTokens.get();
        std::string answer;
        size_t answer_match_len = 0;
        if (!url.empty()) {
            for (auto & token : tokens) {
                auto first = url.find(token.first);
                if (first != std::string::npos && token.first.length() > answer_match_len && first == 0
                    && url.substr(0, token.first.length()) == token.first
                    && (url.length() == token.first.length() || url[token.first.length()] == '/')) {
                    answer = token.second;
                    answer_match_len = token.first.length();
                }
            }
            if (!answer.empty())
                return answer;
        }
        if (auto token = get(tokens, host))
            return *token;
        return {};
    }

    Headers
    makeHeadersWithAuthTokens(const fetchers::Settings & settings, const std::string & host, const Input & input) const
    {
        auto owner = getStrAttr(input.attrs, "owner");
        auto repo = getStrAttr(input.attrs, "repo");
        auto hostAndPath = fmt("%s/%s/%s", host, owner, repo);
        return makeHeadersWithAuthTokens(settings, host, hostAndPath);
    }

    Headers makeHeadersWithAuthTokens(
        const fetchers::Settings & settings, const std::string & host, const std::string & hostAndPath) const
    {
        Headers headers;
        auto accessToken = getAccessToken(settings, host, hostAndPath);
        if (accessToken) {
            auto hdr = accessHeaderFromToken(*accessToken);
            if (hdr)
                headers.push_back(*hdr);
            else
                warn("Unrecognized access token for host '%s'", host);
        }
        return headers;
    }

    struct RefInfo
    {
        Hash rev;
        std::optional<Hash> treeHash;
    };

    virtual RefInfo getRevFromRef(const Settings & settings, nix::Store & store, const Input & input) const = 0;

    virtual DownloadUrl getDownloadUrl(const Settings & settings, const Input & input) const = 0;

    /**
     * The URL for fetching the repository through the Git protocol (the
     * same repository `clone()` clones).
     */
    virtual ParsedURL getGitUrl(const Input & input) const = 0;

    /**
     * The attributes of the equivalent `git` input, for inputs that
     * request tree features an archive tarball cannot provide. Forge
     * archives are `git archive` output, which never contains submodule
     * content or Git LFS files, so `submodules = true` either failed
     * ("input attribute 'submodules' not supported by scheme 'github'",
     * https://github.com/NixOS/nix/issues/13571) or was silently
     * ignored, yielding empty submodule directories
     * (https://github.com/NixOS/nix/issues/14982). This mirrors the
     * github-to-git+https mapping `clone()` already performs.
     *
     * Returns std::nullopt when neither feature is requested; plain
     * archive inputs keep tarball fetch semantics and hashes.
     */
    std::optional<Attrs> gitEquivalentAttrs(const Attrs & attrs) const
    {
        auto submodules = maybeGetBoolAttr(attrs, "submodules").value_or(false);
        auto lfs = maybeGetBoolAttr(attrs, "lfs").value_or(false);
        if (!submodules && !lfs)
            return std::nullopt;

        /* getGitUrl() only reads the identity attributes. */
        Input probe{};
        probe.attrs = attrs;

        Attrs res;
        res.insert_or_assign("type", std::string{"git"});
        res.insert_or_assign("url", getGitUrl(probe).to_string());
        /* An archive is the tree of one revision without history, so
           the equivalent Git fetch is a shallow one. */
        res.insert_or_assign("shallow", Explicit<bool>{true});
        if (submodules)
            res.insert_or_assign("submodules", Explicit<bool>{true});
        if (lfs)
            res.insert_or_assign("lfs", Explicit<bool>{true});
        if (auto ref = maybeGetStrAttr(attrs, "ref"))
            res.insert_or_assign("ref", *ref);
        if (auto rev = maybeGetStrAttr(attrs, "rev"))
            res.insert_or_assign("rev", *rev);
        /* The verification attributes stay meaningful across the scheme
           change: `narHash` still names the expected result tree, and
           `lastModified` the commit time (forge archives set file
           mtimes to the commit time, which is also what the git scheme
           reports). The rest (owner/repo/host are folded into the URL,
           `__final` is owned by the lock layer) must not be forwarded. */
        if (auto narHash = maybeGetStrAttr(attrs, "narHash"))
            res.insert_or_assign("narHash", *narHash);
        if (auto lastModified = maybeGetIntAttr(attrs, "lastModified"))
            res.insert_or_assign("lastModified", *lastModified);
        return res;
    }

    struct TarballInfo
    {
        Hash treeHash;
        time_t lastModified;
    };

    /**
     * Try to obtain the tree of commit `rev` by fetching it through the
     * Git smart protocol into the tarball cache, which doubles as a
     * global Git object store: the protocol only transfers objects that
     * are missing locally, so updating an input downloads roughly the
     * delta against the previously fetched revision rather than a full
     * archive of the new one.
     *
     * Returns std::nullopt when the tree would not be bit-identical to
     * the unpacked archive (submodules, export attributes) or when the
     * server cannot serve the revision; the caller then falls back to
     * the archive download.
     */
    std::optional<TarballInfo>
    fetchArchiveViaGit(const Settings & settings, const Input & input, const Hash & rev) const
    {
        auto cache = settings.getTarballCache();

        auto url = getGitUrl(input);

        try {
            if (!cache->hasObject(rev)) {
                /* Advertising the previously fetched commit of this
                   repository during negotiation is what lets the server
                   send only the missing objects, so keep a per-repository
                   ref pointing at the last fetched commit. The ref name is
                   a digest of the URL because owner/repo segments are not
                   always legal refname components (e.g. a repository named
                   `.github`). */
                auto negotiationRef =
                    fmt("refs/forge/%s",
                        hashString(HashAlgorithm::SHA256, url.to_string()).to_string(HashFormat::Nix32, false));
                cache->fetch(
                    url.to_string(),
                    fmt("%s:%s", rev.gitRev(), negotiationRef),
                    /*shallow=*/true,
                    /*packfilesOnly=*/true);
            }

            if (!cache->hasObject(rev))
                return std::nullopt;

            auto treeHash = cache->getArchiveCompatibleTree(rev);

            if (!treeHash) {
                debug(
                    "revision '%s' of '%s' would not export identically to its archive "
                    "(submodules or export attributes); falling back to an archive fetch",
                    rev.gitRev(),
                    input.to_string());
                return std::nullopt;
            }

            return TarballInfo{.treeHash = *treeHash, .lastModified = (time_t) cache->getLastModified(rev)};
        } catch (Error & e) {
            warn(
                "failed to fetch revision '%s' of '%s' via the Git protocol; "
                "falling back to an archive fetch: %s",
                rev.gitRev(),
                input.to_string(),
                e.msg());
            return std::nullopt;
        }
    }

    std::pair<Input, TarballInfo> downloadArchive(const Settings & settings, Store & store, Input input) const
    {
        if (!maybeGetStrAttr(input.attrs, "ref"))
            input.attrs.insert_or_assign("ref", "HEAD");

        std::optional<Hash> upstreamTreeHash;

        auto rev = input.getRev();
        if (!rev) {
            auto refInfo = getRevFromRef(settings, store, input);
            rev = refInfo.rev;
            upstreamTreeHash = refInfo.treeHash;
            debug("HEAD revision for '%s' is %s", input.to_string(), refInfo.rev.gitRev());
        }

        input.attrs.erase("ref");
        input.attrs.insert_or_assign("rev", rev->gitRev());

        auto cache = settings.getCache();

        Cache::Key treeHashKey{"gitRevToTreeHash", {{"rev", rev->gitRev()}}};
        Cache::Key lastModifiedKey{"gitRevToLastModified", {{"rev", rev->gitRev()}}};

        if (auto treeHashAttrs = cache->lookup(treeHashKey)) {
            if (auto lastModifiedAttrs = cache->lookup(lastModifiedKey)) {
                auto treeHash = getRevAttr(*treeHashAttrs, "treeHash");
                auto lastModified = getIntAttr(*lastModifiedAttrs, "lastModified");
                if (settings.getTarballCache()->hasObject(treeHash))
                    return {std::move(input), TarballInfo{.treeHash = treeHash, .lastModified = (time_t) lastModified}};
                else
                    debug("Git tree with hash '%s' has disappeared from the cache, refetching...", treeHash.gitRev());
            }
        }

        if (settings.forgeFetchViaGit) {
            if (auto tarballInfo = fetchArchiveViaGit(settings, input, *rev)) {
                cache->upsert(treeHashKey, Attrs{{"treeHash", tarballInfo->treeHash.gitRev()}});
                cache->upsert(lastModifiedKey, Attrs{{"lastModified", (uint64_t) tarballInfo->lastModified}});
                return {std::move(input), *tarballInfo};
            }
        }

        /* Stream the tarball into the tarball cache. */
        auto url = getDownloadUrl(settings, input);

        auto source = sinkToSource([&](Sink & sink) {
            FileTransferRequest req(url.url);
            req.headers = url.headers;
            getFileTransfer()->download(std::move(req), sink);
        });

        auto act = std::make_unique<Activity>(
            *logger, lvlInfo, actUnknown, fmt("unpacking '%s' into the Git cache", input.to_string()));

        TarArchive archive{*source};
        auto tarballCache = settings.getTarballCache();
        auto parseSink = tarballCache->getFileSystemObjectSink();
        auto lastModified = unpackTarfileToSink(archive, *parseSink);
        auto tree = parseSink->flush();

        act.reset();

        TarballInfo tarballInfo{
            .treeHash = tarballCache->dereferenceSingletonDirectory(tree), .lastModified = lastModified};

        cache->upsert(treeHashKey, Attrs{{"treeHash", tarballInfo.treeHash.gitRev()}});
        cache->upsert(lastModifiedKey, Attrs{{"lastModified", (uint64_t) tarballInfo.lastModified}});

#if 0
        if (upstreamTreeHash != tarballInfo.treeHash)
            warn(
                "Git tree hash mismatch for revision '%s' of '%s': "
                "expected '%s', got '%s'. "
                "This can happen if the Git repository uses submodules.",
                rev->gitRev(), input.to_string(), upstreamTreeHash->gitRev(), tarballInfo.treeHash.gitRev());
#endif

        return {std::move(input), tarballInfo};
    }

    std::pair<ref<SourceAccessor>, Input>
    getAccessor(const Settings & settings, Store & store, const Input & _input) const override
    {
        /* Flake `self` attributes are applied to an already constructed
           input by direct attribute insertion (see applySelfAttrs() in
           libflake), so a submodules/LFS request can reach the fetch
           stage without ever passing through inputFromAttrs(); redirect
           it here too. The returned input is the locked git input, so
           lock files record the input that was actually fetched. */
        if (auto gitAttrs = gitEquivalentAttrs(_input.attrs))
            return Input::fromAttrs(settings, std::move(*gitAttrs)).getAccessor(settings, store);

        auto [input, tarballInfo] = downloadArchive(settings, store, _input);

        input.attrs.insert_or_assign("lastModified", uint64_t(tarballInfo.lastModified));

        auto accessor =
            settings.getTarballCache()->getAccessor(tarballInfo.treeHash, {}, "«" + input.to_string() + "»");

        return {accessor, input};
    }

    bool isLocked(const Settings & settings, const Input & input) const override
    {
        /* Since we can't verify the integrity of the tarball from the
           Git revision alone, we also require a NAR hash for
           locking. FIXME: in the future, we may want to require a Git
           tree hash instead of a NAR hash. */
        return input.getRev().has_value() && (settings.trustTarballsFromGitForges || input.getNarHash().has_value());
    }

    std::optional<ExperimentalFeature> experimentalFeature() const override
    {
        return Xp::Flakes;
    }

    std::optional<std::string> getFingerprint(Store & store, const Input & input) const override
    {
        if (auto rev = input.getRev())
            return rev->gitRev();
        else
            return std::nullopt;
    }
};

struct GitHubInputScheme : GitArchiveInputScheme
{
    std::string_view schemeName() const override
    {
        return "github";
    }

    std::string schemeDescription() const override
    {
        // TODO
        return "";
    }

    std::optional<std::pair<std::string, std::string>> accessHeaderFromToken(const std::string & token) const override
    {
        // Github supports PAT/OAuth2 tokens and HTTP Basic
        // Authentication.  The former simply specifies the token, the
        // latter can use the token as the password.  Only the first
        // is used here. See
        // https://developer.github.com/v3/#authentication and
        // https://docs.github.com/en/developers/apps/authorizing-oath-apps
        return std::pair<std::string, std::string>("Authorization", fmt("token %s", token));
    }

    std::string getHost(const Input & input) const
    {
        return maybeGetStrAttr(input.attrs, "host").value_or("github.com");
    }

    std::string getOwner(const Input & input) const
    {
        return getStrAttr(input.attrs, "owner");
    }

    std::string getRepo(const Input & input) const
    {
        return getStrAttr(input.attrs, "repo");
    }

    RefInfo getRevFromRef(const Settings & settings, nix::Store & store, const Input & input) const override
    {
        auto host = getHost(input);
        auto url = fmt(
            host == "github.com" ? "https://api.%s/repos/%s/%s/commits/%s" : "https://%s/api/v3/repos/%s/%s/commits/%s",
            host,
            getOwner(input),
            getRepo(input),
            *input.getRef());

        Headers headers = makeHeadersWithAuthTokens(settings, host, input);

        auto downloadResult = downloadFile(store, settings, url, "source", headers);
        auto json = nlohmann::json::parse(
            store.requireStoreObjectAccessor(downloadResult.storePath)->readFile(CanonPath::root));

        return RefInfo{
            .rev = Hash::parseAny(std::string{json["sha"]}, HashAlgorithm::SHA1),
            .treeHash = Hash::parseAny(std::string{json["commit"]["tree"]["sha"]}, HashAlgorithm::SHA1)};
    }

    DownloadUrl getDownloadUrl(const Settings & settings, const Input & input) const override
    {
        auto host = getHost(input);

        Headers headers = makeHeadersWithAuthTokens(settings, host, input);

        // If we have no auth headers then we default to the public archive
        // urls so we do not run into rate limits.
        const auto urlFmt = host != "github.com" ? "https://%s/api/v3/repos/%s/%s/tarball/%s"
                            : headers.empty()    ? "https://%s/%s/%s/archive/%s.tar.gz"
                                                 : "https://api.%s/repos/%s/%s/tarball/%s";

        const auto url =
            fmt(urlFmt, host, getOwner(input), getRepo(input), input.getRev()->to_string(HashFormat::Base16, false));

        return DownloadUrl{parseURL(url), headers};
    }

    ParsedURL getGitUrl(const Input & input) const override
    {
        return parseURL(fmt("https://%s/%s/%s.git", getHost(input), getOwner(input), getRepo(input)));
    }

    void clone(const Settings & settings, Store & store, const Input & input, const std::filesystem::path & destDir)
        const override
    {
        auto host = getHost(input);
        Input::fromURL(settings, fmt("git+https://%s/%s/%s.git", host, getOwner(input), getRepo(input)))
            .applyOverrides(input.getRef(), input.getRev())
            .clone(settings, store, destDir);
    }
};

struct GitLabInputScheme : GitArchiveInputScheme
{
    std::string_view schemeName() const override
    {
        return "gitlab";
    }

    std::string schemeDescription() const override
    {
        // TODO
        return "";
    }

    std::optional<std::pair<std::string, std::string>> accessHeaderFromToken(const std::string & token) const override
    {
        // Gitlab supports 4 kinds of authorization, two of which are
        // relevant here: OAuth2 and PAT (Private Access Token).  The
        // user can indicate which token is used by specifying the
        // token as <TYPE>:<VALUE>, where type is "OAuth2" or "PAT".
        // If the <TYPE> is unrecognized, this will fall back to
        // treating this simply has <HDRNAME>:<HDRVAL>.  See
        // https://docs.gitlab.com/12.10/ee/api/README.html#authentication
        auto fldsplit = token.find_first_of(':');
        // n.b. C++20 would allow: if (token.starts_with("OAuth2:")) ...
        if ("OAuth2" == token.substr(0, fldsplit))
            return std::make_pair("Authorization", fmt("Bearer %s", token.substr(fldsplit + 1)));
        if ("PAT" == token.substr(0, fldsplit))
            return std::make_pair("Private-token", token.substr(fldsplit + 1));
        warn("Unrecognized GitLab token type %s", token.substr(0, fldsplit));
        return std::make_pair(token.substr(0, fldsplit), token.substr(fldsplit + 1));
    }

    RefInfo getRevFromRef(const Settings & settings, nix::Store & store, const Input & input) const override
    {
        auto host = maybeGetStrAttr(input.attrs, "host").value_or("gitlab.com");
        // See rate limiting note below
        auto url =
            fmt("https://%s/api/v4/projects/%s%%2F%s/repository/commits?ref_name=%s",
                host,
                getStrAttr(input.attrs, "owner"),
                getStrAttr(input.attrs, "repo"),
                *input.getRef());

        Headers headers = makeHeadersWithAuthTokens(settings, host, input);

        auto downloadResult = downloadFile(store, settings, url, "source", headers);
        auto json = nlohmann::json::parse(
            store.requireStoreObjectAccessor(downloadResult.storePath)->readFile(CanonPath::root));

        if (json.is_array() && json.size() >= 1 && json[0]["id"] != nullptr) {
            return RefInfo{.rev = Hash::parseAny(std::string(json[0]["id"]), HashAlgorithm::SHA1)};
        }
        if (json.is_array() && json.size() == 0) {
            throw Error("No commits returned by GitLab API -- does the git ref really exist?");
        } else {
            throw Error("Unexpected response received from GitLab: %s", json);
        }
    }

    DownloadUrl getDownloadUrl(const Settings & settings, const Input & input) const override
    {
        // This endpoint has a rate limit threshold that may be
        // server-specific and vary based whether the user is
        // authenticated via an accessToken or not, but the usual rate
        // is 10 reqs/sec/ip-addr.  See
        // https://docs.gitlab.com/ee/user/gitlab_com/index.html#gitlabcom-specific-rate-limits
        auto host = maybeGetStrAttr(input.attrs, "host").value_or("gitlab.com");
        auto url =
            fmt("https://%s/api/v4/projects/%s%%2F%s/repository/archive.tar.gz?sha=%s",
                host,
                getStrAttr(input.attrs, "owner"),
                getStrAttr(input.attrs, "repo"),
                input.getRev()->to_string(HashFormat::Base16, false));

        Headers headers = makeHeadersWithAuthTokens(settings, host, input);
        return DownloadUrl{parseURL(url), headers};
    }

    ParsedURL getGitUrl(const Input & input) const override
    {
        auto host = maybeGetStrAttr(input.attrs, "host").value_or("gitlab.com");
        return parseURL(
            fmt("https://%s/%s/%s.git", host, getStrAttr(input.attrs, "owner"), getStrAttr(input.attrs, "repo")));
    }

    void clone(const Settings & settings, Store & store, const Input & input, const std::filesystem::path & destDir)
        const override
    {
        auto host = maybeGetStrAttr(input.attrs, "host").value_or("gitlab.com");
        // FIXME: get username somewhere
        Input::fromURL(
            settings,
            fmt("git+https://%s/%s/%s.git", host, getStrAttr(input.attrs, "owner"), getStrAttr(input.attrs, "repo")))
            .applyOverrides(input.getRef(), input.getRev())
            .clone(settings, store, destDir);
    }
};

struct SourceHutInputScheme : GitArchiveInputScheme
{
    std::string_view schemeName() const override
    {
        return "sourcehut";
    }

    std::string schemeDescription() const override
    {
        // TODO
        return "";
    }

    std::optional<std::pair<std::string, std::string>> accessHeaderFromToken(const std::string & token) const override
    {
        // SourceHut supports both PAT and OAuth2. See
        // https://man.sr.ht/meta.sr.ht/oauth.md
        return std::pair<std::string, std::string>("Authorization", fmt("Bearer %s", token));
        // Note: This currently serves no purpose, as this kind of authorization
        // does not allow for downloading tarballs on sourcehut private repos.
        // Once it is implemented, however, should work as expected.
    }

    RefInfo getRevFromRef(const Settings & settings, nix::Store & store, const Input & input) const override
    {
        // TODO: In the future, when the sourcehut graphql API is implemented for mercurial
        // and with anonymous access, this method should use it instead.

        auto ref = *input.getRef();

        auto host = maybeGetStrAttr(input.attrs, "host").value_or("git.sr.ht");
        auto base_url =
            fmt("https://%s/%s/%s", host, getStrAttr(input.attrs, "owner"), getStrAttr(input.attrs, "repo"));

        Headers headers = makeHeadersWithAuthTokens(settings, host, input);

        std::string refUri;
        if (ref == "HEAD") {
            auto downloadFileResult = downloadFile(store, settings, fmt("%s/HEAD", base_url), "source", headers);
            auto contents = store.requireStoreObjectAccessor(downloadFileResult.storePath)->readFile(CanonPath::root);

            auto remoteLine = git::parseLsRemoteLine(getLine(contents).first);
            if (!remoteLine) {
                throw BadURL("in '%d', couldn't resolve HEAD ref '%d'", input.to_string(), ref);
            }
            refUri = remoteLine->target;
        } else {
            refUri = fmt("refs/(heads|tags)/%s", ref);
        }
        std::regex refRegex(refUri);

        auto downloadFileResult = downloadFile(store, settings, fmt("%s/info/refs", base_url), "source", headers);
        auto contents = store.requireStoreObjectAccessor(downloadFileResult.storePath)->readFile(CanonPath::root);
        std::istringstream is(contents);

        std::string line;
        std::optional<std::string> id;
        while (!id && getline(is, line)) {
            auto parsedLine = git::parseLsRemoteLine(line);
            if (parsedLine && parsedLine->reference && std::regex_match(*parsedLine->reference, refRegex))
                id = parsedLine->target;
        }

        if (!id)
            throw BadURL("in '%d', couldn't find ref '%d'", input.to_string(), ref);

        return RefInfo{.rev = Hash::parseAny(*id, HashAlgorithm::SHA1)};
    }

    DownloadUrl getDownloadUrl(const Settings & settings, const Input & input) const override
    {
        auto host = maybeGetStrAttr(input.attrs, "host").value_or("git.sr.ht");
        auto url =
            fmt("https://%s/%s/%s/archive/%s.tar.gz",
                host,
                getStrAttr(input.attrs, "owner"),
                getStrAttr(input.attrs, "repo"),
                input.getRev()->to_string(HashFormat::Base16, false));

        Headers headers = makeHeadersWithAuthTokens(settings, host, input);
        return DownloadUrl{parseURL(url), headers};
    }

    ParsedURL getGitUrl(const Input & input) const override
    {
        auto host = maybeGetStrAttr(input.attrs, "host").value_or("git.sr.ht");
        return parseURL(
            fmt("https://%s/%s/%s", host, getStrAttr(input.attrs, "owner"), getStrAttr(input.attrs, "repo")));
    }

    void clone(const Settings & settings, Store & store, const Input & input, const std::filesystem::path & destDir)
        const override
    {
        auto host = maybeGetStrAttr(input.attrs, "host").value_or("git.sr.ht");
        Input::fromURL(
            settings,
            fmt("git+https://%s/%s/%s", host, getStrAttr(input.attrs, "owner"), getStrAttr(input.attrs, "repo")))
            .applyOverrides(input.getRef(), input.getRev())
            .clone(settings, store, destDir);
    }
};

static auto rGitHubInputScheme = OnStartup([] { registerInputScheme(std::make_unique<GitHubInputScheme>()); });
static auto rGitLabInputScheme = OnStartup([] { registerInputScheme(std::make_unique<GitLabInputScheme>()); });
static auto rSourceHutInputScheme = OnStartup([] { registerInputScheme(std::make_unique<SourceHutInputScheme>()); });

} // namespace nix::fetchers
