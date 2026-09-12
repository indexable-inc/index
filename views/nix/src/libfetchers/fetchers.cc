#include "nix/fetchers/fetchers.hh"
#include "nix/store/store-api.hh"
#include "nix/util/source-path.hh"
#include "nix/fetchers/fetch-to-store.hh"
#include "nix/util/json-utils.hh"
#include "nix/fetchers/fetch-settings.hh"
#include "nix/fetchers/fetch-to-store.hh"
#include "nix/util/url.hh"
#include "nix/util/archive.hh"

#include <nlohmann/json.hpp>

namespace nix::fetchers {

using InputSchemeMap = std::map<std::string_view, std::shared_ptr<InputScheme>>;

static InputSchemeMap & inputSchemes()
{
    static InputSchemeMap inputSchemeMap;
    return inputSchemeMap;
}

void registerInputScheme(std::shared_ptr<InputScheme> && inputScheme)
{
    auto schemeName = inputScheme->schemeName();
    if (!inputSchemes().emplace(schemeName, std::move(inputScheme)).second)
        throw Error("Input scheme with name %s already registered", schemeName);
}

const InputSchemeMap & getAllInputSchemes()
{
    return inputSchemes();
}

Input Input::fromURL(const Settings & settings, const std::string & url, bool requireTree)
{
    return fromURL(settings, parseURL(url), requireTree);
}

static void fixupInput(Input & input)
{
    // Check common attributes.
    input.getType();
    input.getRef();
    input.getRevCount();
    input.getLastModified();
}

Input Input::fromURL(const Settings & settings, const ParsedURL & url, bool requireTree)
{
    for (auto & [_, inputScheme] : inputSchemes()) {
        auto res = inputScheme->inputFromURL(settings, url, requireTree);
        if (res) {
            experimentalFeatureSettings.require(inputScheme->experimentalFeature());
            /* A scheme may delegate construction to another scheme (the
               forge archive schemes construct a `git` input when
               submodules are requested); keep the delegate's scheme. */
            if (!res->scheme)
                res->scheme = inputScheme;
            fixupInput(*res);
            return std::move(*res);
        }
    }

    // Provide a helpful hint when user tries file+git instead of git+file
    auto parsedScheme = parseUrlScheme(url.scheme);
    if (parsedScheme.application == "file" && parsedScheme.transport == "git") {
        throw Error("input '%s' is unsupported; did you mean 'git+file' instead of 'file+git'?", url);
    }

    throw Error("input '%s' is unsupported", url);
}

Input Input::fromAttrs(const Settings & settings, Attrs && attrs)
{
    auto schemeName = ({
        auto schemeNameOpt = maybeGetStrAttr(attrs, "type");
        if (!schemeNameOpt)
            throw Error("'type' attribute to specify input scheme is required but not provided");
        *std::move(schemeNameOpt);
    });

    auto raw = [&]() {
        // Return an input without a scheme; most operations will fail,
        // but not all of them. Doing this is to support those other
        // operations which are supposed to be robust on
        // unknown/uninterpretable inputs.
        Input input;
        input.attrs = attrs;
        fixupInput(input);
        return input;
    };

    std::shared_ptr<InputScheme> inputScheme = ({
        auto i = get(inputSchemes(), schemeName);
        i ? *i : nullptr;
    });

    if (!inputScheme)
        return raw();

    experimentalFeatureSettings.require(inputScheme->experimentalFeature());

    auto allowedAttrs = inputScheme->allowedAttrs();

    for (auto & [name, _] : attrs)
        if (name != "type" && name != "__final" && allowedAttrs.count(name) == 0)
            throw Error("input attribute '%s' not supported by scheme '%s'", name, schemeName);

    auto res = inputScheme->inputFromAttrs(settings, attrs);
    if (!res)
        return raw();
    /* A scheme may delegate construction to another scheme (the forge
       archive schemes construct a `git` input when submodules are
       requested); keep the delegate's scheme. */
    if (!res->scheme)
        res->scheme = inputScheme;
    fixupInput(*res);
    return std::move(*res);
}

std::optional<std::string> Input::getFingerprint(Store & store) const
{
    if (!scheme)
        return std::nullopt;

    if (cachedFingerprint)
        return *cachedFingerprint;

    auto fingerprint = scheme->getFingerprint(store, *this);

    cachedFingerprint = fingerprint;

    return fingerprint;
}

ParsedURL Input::toURL() const
{
    if (!scheme)
        throw Error("cannot show unsupported input '%s'", attrsToJSON(attrs));
    return scheme->toURL(*this);
}

std::string Input::toURLString(const StringMap & extraQuery) const
{
    auto url = toURL();
    for (auto & attr : extraQuery)
        url.query.insert(attr);
    return url.to_string();
}

std::string Input::to_string() const
{
    return toURL().to_string();
}

bool Input::isDirect() const
{
    return !scheme || scheme->isDirect(*this);
}

bool Input::isLocked(const Settings & settings) const
{
    return scheme && scheme->isLocked(settings, *this);
}

bool Input::isFinal() const
{
    return maybeGetBoolAttr(attrs, "__final").value_or(false);
}

std::optional<std::filesystem::path> Input::isRelative() const
{
    assert(scheme);
    return scheme->isRelative(*this);
}

Attrs Input::toAttrs() const
{
    return attrs;
}

bool Input::operator==(const Input & other) const noexcept
{
    return attrs == other.attrs;
}

// FIXME: remove
std::pair<StorePath, Input> Input::fetchToStore(const Settings & settings, Store & store) const
{
    if (!scheme)
        throw Error("cannot fetch unsupported input '%s'", attrsToJSON(toAttrs()));

    auto [storePath, input] = [&]() -> std::pair<StorePath, Input> {
        try {
            auto [accessor, result] = getAccessorUnchecked(settings, store);

            /* A tree read out of jj's object store is addressed by the id
               it announces and locked by `treeHash`; every other tree is
               NAR-copied and locked by `narHash`. One identity per input:
               a jj input never carries `narHash` (its scheme rejects the
               attribute). */
            bool byTreeId = accessor->knownTreeRoot.has_value();

            auto [storePath, hash] = nix::fetchToStore2(
                settings,
                store,
                SourcePath(accessor),
                FetchMode::Copy,
                result.getName(),
                byTreeId ? ContentAddressMethod::Raw::JjTree : ContentAddressMethod::Raw::NixArchive);

            result.attrs.insert_or_assign(byTreeId ? "treeHash" : "narHash", hash.to_string(HashFormat::SRI, true));

            result.attrs.insert_or_assign("__final", Explicit<bool>(true));

            assert(result.isFinal());

            checkLocks(*this, result);

            return {storePath, result};
        } catch (Error & e) {
            e.addTrace({}, "while fetching the input '%s'", to_string());
            throw;
        }
    }();

    return {std::move(storePath), input};
}

void Input::checkLocks(Input specified, Input & result)
{
    /* If the original input is final, then we just return the
       original attributes, dropping any new fields returned by the
       fetcher. However, any fields that are in both the specified and
       result input must be identical. */
    if (specified.isFinal()) {

        /* Backwards compatibility hack: we had some lock files in the
           past that 'narHash' fields with incorrect base-64
           formatting (lacking the trailing '=', e.g. 'sha256-ri...Mw'
           instead of ''sha256-ri...Mw='). So fix that. */
        if (auto prevNarHash = specified.getNarHash())
            specified.attrs.insert_or_assign("narHash", prevNarHash->to_string(HashFormat::SRI, true));

        if (auto narHash = result.getNarHash())
            result.attrs.insert_or_assign("narHash", narHash->to_string(HashFormat::SRI, true));

        for (auto & field : specified.attrs) {
            auto field2 = result.attrs.find(field.first);
            if (field2 != result.attrs.end() && field.second != field2->second)
                throw Error(
                    "mismatch in field '%s' of input '%s', got '%s'",
                    field.first,
                    attrsToJSON(specified.attrs),
                    attrsToJSON(result.attrs));
        }

        result.attrs = specified.attrs;

        return;
    }

    if (auto prevNarHash = specified.getNarHash()) {
        if (result.getNarHash() != prevNarHash) {
            if (result.getNarHash())
                throw Error(
                    (unsigned int) 102,
                    "NAR hash mismatch in input '%s', expected '%s' but got '%s'",
                    specified.to_string(),
                    prevNarHash->to_string(HashFormat::SRI, true),
                    result.getNarHash()->to_string(HashFormat::SRI, true));
            else
                throw Error(
                    (unsigned int) 102,
                    "NAR hash mismatch in input '%s', expected '%s' but got none",
                    specified.to_string(),
                    prevNarHash->to_string(HashFormat::SRI, true));
        }
    }

    /* No `treeHash` comparison here: a tree id is checked where the tree is
       resolved, by the scheme. A `jj` input's scheme compares the `treeHash`
       it was given against the tree the resolved revision names before it
       hands out an accessor (jj.cc, "is locked to tree ... but revision ...
       has tree"), which is the only place both the claim and the fact are at
       hand; and a final input (every lock-file entry) is held to all of its
       attributes by the field comparison above. A third comparison here was
       reachable by neither road. */

    if (auto prevLastModified = specified.getLastModified()) {
        if (result.getLastModified() != prevLastModified)
            throw Error(
                "'lastModified' attribute mismatch in input '%s', expected %d, got %d",
                result.to_string(),
                *prevLastModified,
                result.getLastModified().value_or(-1));
    }

    if (auto prevRev = specified.getRev()) {
        if (result.getRev() != prevRev)
            throw Error("'rev' attribute mismatch in input '%s', expected %s", result.to_string(), prevRev->gitRev());
    }

    if (auto prevRevCount = specified.getRevCount()) {
        if (result.getRevCount() != prevRevCount)
            throw Error("'revCount' attribute mismatch in input '%s', expected %d", result.to_string(), *prevRevCount);
    }
}

std::pair<ref<SourceAccessor>, Input> Input::getAccessor(const Settings & settings, Store & store) const
{
    try {
        auto [accessor, result] = getAccessorUnchecked(settings, store);

        result.attrs.insert_or_assign("__final", Explicit<bool>(true));

        checkLocks(*this, result);

        return {accessor, std::move(result)};
    } catch (Error & e) {
        e.addTrace({}, "while fetching the input '%s'", to_string());
        throw;
    }
}

std::pair<ref<SourceAccessor>, Input> Input::getAccessorUnchecked(const Settings & settings, Store & store) const
{
    // FIXME: cache the accessor

    if (!scheme)
        throw Error("cannot fetch unsupported input '%s'", attrsToJSON(toAttrs()));

    /* The tree may already be in the Nix store, or it could be
       substituted (which is often faster than fetching from the
       original source). So check that. We only do this for final
       inputs, otherwise there is a risk that we don't return the
       same attributes (like `lastModified`) that the "real" fetcher
       would return.

       FIXME: add a setting to disable this.
       FIXME: substituting may be slower than fetching normally,
       e.g. for fetchers like Git that are incremental!
    */
    if (isFinal() && getNarHash()) {
        try {
            auto storePath = computeStorePath(store);

            store.ensurePath(storePath);

            debug("using substituted/cached input '%s' in '%s'", to_string(), store.printStorePath(storePath));

            auto accessor = store.requireStoreObjectAccessor(storePath);

            accessor->fingerprint = getFingerprint(store);

            // Store a cache entry for the substituted tree so later fetches
            // can reuse the existing nar instead of copying the unpacked
            // input back into the store on every evaluation.
            if (accessor->fingerprint) {
                settings.getCache()->upsert(
                    makeSourcePathToHashCacheKey(
                        *accessor->fingerprint, ContentAddressMethod::Raw::NixArchive, CanonPath::root),
                    {{"hash", store.queryPathInfo(storePath)->narHash.to_string(HashFormat::SRI, true)}});
            }

            accessor->setPathDisplay("«" + to_string() + "»");

            return {accessor, *this};
        } catch (Error & e) {
            debug("substitution of input '%s' failed: %s", to_string(), e.what());
        }
    }

    /* No store shortcut for a `treeHash` lock here. The jj scheme serves a
       registered store object itself, and only when the repository the lock
       names is absent (jj.cc, `getAccessor`): a store object cannot name its
       subtrees, so serving it while the repository is at hand would make a
       relative input's identity depend on whether something had forced the
       parent's copy. */
    auto [accessor, result] = scheme->getAccessor(settings, store, *this);

    if (!accessor->fingerprint)
        accessor->fingerprint = result.getFingerprint(store);
    else
        result.cachedFingerprint = accessor->fingerprint;

    return {accessor, std::move(result)};
}

void Input::clone(const Settings & settings, Store & store, const std::filesystem::path & destDir) const
{
    assert(scheme);
    scheme->clone(settings, store, *this, destDir);
}

std::optional<std::filesystem::path> Input::getSourcePath() const
{
    assert(scheme);
    return scheme->getSourcePath(*this);
}

void Input::putFile(const CanonPath & path, std::string_view contents, std::optional<std::string> commitMsg) const
{
    assert(scheme);
    return scheme->putFile(*this, path, contents, commitMsg);
}

bool Input::putFileRequiresCommit() const
{
    assert(scheme);
    return scheme->putFileRequiresCommit();
}

std::string Input::getName() const
{
    return maybeGetStrAttr(attrs, "name").value_or("source");
}

StorePath Input::computeStorePath(Store & store) const
{
    if (auto treeHash = getTreeHash())
        return store.makeFixedOutputPath(
            getName(),
            FixedOutputInfo{
                .method = FileIngestionMethod::JjTree,
                .hash = *treeHash,
                .references = {},
            });
    auto narHash = getNarHash();
    if (!narHash)
        throw Error("cannot compute store path for unlocked input '%s'", to_string());
    return store.makeFixedOutputPath(
        getName(),
        FixedOutputInfo{
            .method = FileIngestionMethod::NixArchive,
            .hash = *narHash,
            .references = {},
        });
}

std::string Input::getType() const
{
    return getStrAttr(attrs, "type");
}

std::optional<Hash> Input::getNarHash() const
{
    if (auto s = maybeGetStrAttr(attrs, "narHash")) {
        auto hash = s->empty() ? Hash(HashAlgorithm::SHA256) : Hash::parseSRI(*s);
        if (hash.algo != HashAlgorithm::SHA256)
            throw UsageError("narHash must use SHA-256");
        return hash;
    }
    return {};
}

std::optional<Hash> Input::getTreeHash() const
{
    /* An input no scheme claims cannot say what its attributes mean. */
    if (!scheme)
        return std::nullopt;
    return scheme->getTreeHash(*this);
}

std::optional<Hash> InputScheme::getTreeHash(const Input & input) const
{
    auto s = maybeGetStrAttr(input.attrs, "treeHash");
    if (!s)
        return std::nullopt;
    /* The id is jj's, not a user's algorithm choice: see `nativeIdXpSettings`. */
    auto hash = Hash::parseSRI(*s, nativeIdXpSettings());
    if (hash.algo != HashAlgorithm::BLAKE3)
        throw UsageError(
            "treeHash of input '%s' must be a BLAKE3 Jujutsu tree id, but '%s' uses %s",
            /* From the attrs, not `input.to_string()`: this runs from
               `inputFromAttrs`, where the Input has no scheme attached yet,
               and `to_string()` on such an Input throws ("cannot show
               unsupported input") WHILE THIS MESSAGE IS BEING FORMATTED,
               replacing the refusal the caller was meant to read. */
            maybeGetStrAttr(input.attrs, "url").value_or(attrsToJSON(input.attrs).dump()),
            *s,
            printHashAlgo(hash.algo));
    return hash;
}

std::optional<std::string> Input::getRef() const
{
    if (auto s = maybeGetStrAttr(attrs, "ref"))
        return *s;
    return {};
}

Hash parseRev(std::string_view s)
{
    constexpr size_t blake3RevLength = 2 * regularHashSize(HashAlgorithm::BLAKE3);

    /* A commit id is minted by the repository's backend, not chosen by the
       user: see `nativeIdXpSettings`. */
    if (s.size() == blake3RevLength)
        return Hash::parseExplicitFormatUnprefixed(s, HashAlgorithm::BLAKE3, HashFormat::Base16, nativeIdXpSettings());

    try {
        return Hash::parseAny(s, HashAlgorithm::SHA1);
    } catch (BadHash &) {
        throw BadHash(
            "'%s' is not a revision: expected 40 hexadecimal characters (a Git/SHA-1 commit id) or %d "
            "(a BLAKE3 one, as minted by Jujutsu's native backend), but got %d",
            s,
            blake3RevLength,
            s.size());
    }
}

std::optional<Hash> Input::getRev() const
{
    std::optional<Hash> hash = {};

    if (auto s = maybeGetStrAttr(attrs, "rev")) {
        try {
            hash = Hash::parseAnyPrefixed(*s);
        } catch (BadHash & e) {
            /* Unprefixed, so the algorithm comes from the length. SHA-1 stays
               the reading for everything it used to cover, for backwards
               compatibility with existing usages (e.g. `builtins.fetchTree`
               calls or flake inputs). */
            hash = parseRev(*s);
        }
    }

    return hash;
}

std::optional<uint64_t> Input::getRevCount() const
{
    if (auto n = maybeGetIntAttr(attrs, "revCount"))
        return *n;
    return {};
}

std::optional<time_t> Input::getLastModified() const
{
    if (auto n = maybeGetIntAttr(attrs, "lastModified"))
        return *n;
    return {};
}

std::optional<std::string> Input::getHistoryJson(const Settings & settings, Store & store) const
{
    if (!scheme)
        return std::nullopt;
    return scheme->getHistoryJson(settings, store, *this);
}

ParsedURL InputScheme::toURL(const Input & input) const
{
    throw Error("don't know how to convert input '%s' to a URL", attrsToJSON(input.attrs));
}

std::optional<std::filesystem::path> InputScheme::getSourcePath(const Input & input) const
{
    return {};
}

void InputScheme::putFile(
    const Input & input, const CanonPath & path, std::string_view contents, std::optional<std::string> commitMsg) const
{
    throw Error("input '%s' does not support modifying file '%s'", input.to_string(), path);
}

void InputScheme::clone(
    const Settings & settings, Store & store, const Input & input, const std::filesystem::path & destDir) const
{
    if (std::filesystem::exists(destDir))
        throw Error("cannot clone into existing path %s", PathFmt(destDir));

    auto [accessor, input2] = getAccessor(settings, store, input);

    Activity act(*logger, lvlTalkative, actUnknown, fmt("copying '%s' to %s...", input2.to_string(), PathFmt(destDir)));

    RestoreSink sink(/*startFsync=*/false);
    sink.dstPath = destDir;
    copyRecursive(*accessor, CanonPath::root, sink, CanonPath::root);
}

std::optional<ExperimentalFeature> InputScheme::experimentalFeature() const
{
    return {};
}

std::string publicKeys_to_string(const std::vector<PublicKey> & publicKeys)
{
    return ((nlohmann::json) publicKeys).dump();
}

} // namespace nix::fetchers

namespace nlohmann {

using namespace nix;

#ifndef DOXYGEN_SKIP

fetchers::PublicKey adl_serializer<fetchers::PublicKey>::from_json(const json & json)
{
    fetchers::PublicKey res = {};
    auto & obj = getObject(json);
    if (auto * type = optionalValueAt(obj, "type"))
        res.type = getString(*type);

    res.key = getString(valueAt(obj, "key"));

    return res;
}

void adl_serializer<fetchers::PublicKey>::to_json(json & json, const fetchers::PublicKey & p)
{
    json["type"] = p.type;
    json["key"] = p.key;
}

#endif

} // namespace nlohmann
