#include "nix/util/source-path.hh"
#include "nix/util/util.hh"
#include "nix/store/store-dir-config.hh"
#include "nix/store/derivations.hh"
#include "nix/store/globals.hh"

namespace nix {

StorePath StoreDirConfig::parseStorePath(std::string_view path) const
{
    // On Windows, `/nix/store` is not a canonical path. More broadly it
    // is unclear whether this function should be using the native
    // notion of a canonical path at all. For example, it makes to
    // support remote stores whose store dir is a non-native path (e.g.
    // Windows <-> Unix ssh-ing).
    auto p =
#ifdef _WIN32
        std::filesystem::path(path)
#else
        canonPath(std::string(path))
#endif
        ;
    if (p.parent_path() != storeDir)
        throw BadStorePath("path %s is not in the Nix store", PathFmt(p));
    return StorePath(p.filename().string());
}

std::optional<StorePath> StoreDirConfig::maybeParseStorePath(std::string_view path) const
{
    try {
        return parseStorePath(path);
    } catch (Error &) {
        return {};
    }
}

bool StoreDirConfig::isStorePath(std::string_view path) const
{
    return (bool) maybeParseStorePath(path);
}

StorePathSet StoreDirConfig::parseStorePathSet(const StringSet & paths) const
{
    StorePathSet res;
    for (auto & i : paths)
        res.insert(parseStorePath(i));
    return res;
}

std::string StoreDirConfig::printStorePath(const StorePath & path) const
{
    return (storeDir + "/").append(path.to_string());
}

StringSet StoreDirConfig::printStorePathSet(const StorePathSet & paths) const
{
    StringSet res;
    for (auto & i : paths)
        res.insert(printStorePath(i));
    return res;
}

/*
The exact specification of store paths is in `protocols/store-path.md`
in the Nix manual. These few functions implement that specification.

If changes to these functions go beyond mere implementation changes i.e.
also update the user-visible behavior, please update the specification
to match.
*/

StorePath StoreDirConfig::makeStorePath(std::string_view type, std::string_view hash, std::string_view name) const
{
    /* e.g., "source:sha256:1abc...:/nix/store:foo.tar.gz" */
    auto s = std::string(type) + ":" + std::string(hash) + ":" + storeDir + ":" + std::string(name);
    auto h = compressHash(hashString(HashAlgorithm::SHA256, s), 20);
    return StorePath(h, name);
}

StorePath StoreDirConfig::makeStorePath(std::string_view type, const Hash & hash, std::string_view name) const
{
    return makeStorePath(type, hash.to_string(HashFormat::Base16, true), name);
}

StorePath StoreDirConfig::makeOutputPath(std::string_view id, const Hash & hash, std::string_view name) const
{
    return makeStorePath("output:" + std::string{id}, hash, outputPathName(name, id));
}

/* Stuff the references (if any) into the type.  This is a bit
   hacky, but we can't put them in, say, <s2> (per the grammar above)
   since that would be ambiguous. */
static std::string makeType(const StoreDirConfig & store, std::string && type, const StoreReferences & references)
{
    for (auto & i : references.others) {
        type += ":";
        type += store.printStorePath(i);
    }
    if (references.self)
        type += ":self";
    return std::move(type);
}

StorePath StoreDirConfig::makeFixedOutputPath(std::string_view name, const FixedOutputInfo & info) const
{
    if (info.method == FileIngestionMethod::Git
        && !(info.hash.algo == HashAlgorithm::SHA1 || info.hash.algo == HashAlgorithm::SHA256)) {
        throw Error(
            "Git file ingestion must use SHA-1 or SHA-256 hash, but instead using: %s", printHashAlgo(info.hash.algo));
    }

    /* A jj tree id is BLAKE3; any other algorithm is not such an id, and a
       path made from it would name an object no jj store can serve. */
    if (info.method == FileIngestionMethod::JjTree && info.hash.algo != HashAlgorithm::BLAKE3) {
        throw Error(
            "Jujutsu tree ingestion must use a BLAKE3 tree id, but instead using: %s", printHashAlgo(info.hash.algo));
    }

    /* The `source` scheme is the only one that can name references (including
       a self-reference), because `makeType` folds them into the digest input.
       It is available to any hash algorithm whose NAR digest is unambiguous in
       that input: `makeStorePath` renders the hash with its algorithm prefix,
       so `source:sha256:<hex>:...` and `source:blake3:<hex>:...` are distinct
       strings and the two algorithms cannot collide on a store path. */
    if ((info.hash.algo == HashAlgorithm::SHA256 || info.hash.algo == HashAlgorithm::BLAKE3)
        && info.method == FileIngestionMethod::NixArchive) {
        return makeStorePath(makeType(*this, "source", info.references), info.hash, name);
    } else {
        if (!info.references.empty()) {
            throw Error(
                "fixed output derivation '%s' is not allowed to refer to other store paths.\nYou may need to use the 'unsafeDiscardReferences' derivation attribute, see the manual for more details.",
                name);
        }
        // make a unique digest based on the parameters for creating this store object
        auto payload =
            "fixed:out:" + makeFileIngestionPrefix(info.method) + info.hash.to_string(HashFormat::Base16, true) + ":";
        auto digest = hashString(HashAlgorithm::SHA256, payload);
        return makeStorePath("output:out", digest, name);
    }
}

StorePath
StoreDirConfig::makeFixedOutputPathFromCA(std::string_view name, const ContentAddressWithReferences & ca) const
{
    // New template
    return std::visit(
        overloaded{
            [&](const TextInfo & ti) {
                assert(ti.hash.algo == HashAlgorithm::SHA256);
                return makeStorePath(
                    makeType(
                        *this,
                        "text",
                        StoreReferences{
                            .others = ti.references,
                            .self = false,
                        }),
                    ti.hash,
                    name);
            },
            [&](const FixedOutputInfo & foi) { return makeFixedOutputPath(name, foi); }},
        ca.raw);
}

std::pair<StorePath, Hash> StoreDirConfig::computeStorePath(
    std::string_view name,
    const SourcePath & path,
    ContentAddressMethod method,
    HashAlgorithm hashAlgo,
    const StorePathSet & references,
    PathFilter & filter) const
{
    auto [h, size] = hashPath(path, method.getFileIngestionMethod(), hashAlgo, filter);
    if (settings.warnLargePathThreshold && size && *size >= settings.warnLargePathThreshold)
        warn("hashed large path '%s' (%s)", path, renderSize(*size));
    return {
        makeFixedOutputPathFromCA(
            name,
            ContentAddressWithReferences::fromParts(
                method,
                h,
                {
                    .others = references,
                    .self = false,
                })),
        h,
    };
}

} // namespace nix
