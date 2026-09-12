#include "nix/fetchers/filtering-source-accessor.hh"
#include "nix/util/sync.hh"

#include <boost/unordered/unordered_flat_set.hpp>

namespace nix {

std::optional<std::filesystem::path> FilteringSourceAccessor::getPhysicalPath(const CanonPath & path)
{
    checkAccess(path);
    return next->getPhysicalPath(prefix / path);
}

void FilteringSourceAccessor::readFile(const CanonPath & path, Sink & sink, fun<void(uint64_t)> sizeCallback)
{
    checkAccess(path);
    return next->readFile(prefix / path, sink, sizeCallback);
}

bool FilteringSourceAccessor::pathExists(const CanonPath & path)
{
    return isAllowed(path) && next->pathExists(prefix / path);
}

std::optional<SourceAccessor::Stat> FilteringSourceAccessor::maybeLstat(const CanonPath & path)
{
    return isAllowed(path) ? next->maybeLstat(prefix / path) : std::nullopt;
}

SourceAccessor::Stat FilteringSourceAccessor::lstat(const CanonPath & path)
{
    checkAccess(path);
    return next->lstat(prefix / path);
}

SourceAccessor::DirEntries FilteringSourceAccessor::readDirectory(const CanonPath & path)
{
    checkAccess(path);
    DirEntries entries;
    for (auto & entry : next->readDirectory(prefix / path)) {
        if (isAllowed(path / entry.first))
            entries.insert(std::move(entry));
    }
    return entries;
}

std::string FilteringSourceAccessor::readLink(const CanonPath & path)
{
    checkAccess(path);
    return next->readLink(prefix / path);
}

std::string FilteringSourceAccessor::showPath(const CanonPath & path)
{
    return displayPrefix + next->showPath(prefix / path) + displaySuffix;
}

std::pair<CanonPath, std::optional<std::string>> FilteringSourceAccessor::getFingerprint(const CanonPath & path)
{
    if (fingerprint)
        return {path, fingerprint};
    return next->getFingerprint(prefix / path);
}

std::string_view FilteringSourceAccessor::identityClass(const CanonPath & path)
{
    return next->identityClass(prefix / path);
}

std::shared_ptr<SourceAccessor> FilteringSourceAccessor::getSubtree(const CanonPath & path)
{
    /* A subtree accessor serves every path beneath its root straight from
       the tree below, so handing one out answers, once, for all of them. A
       filter is a predicate over single paths: passing it at `path` says
       nothing about the children (an allow-list admits `/a` when `/a/b` is
       listed and still refuses `/a/c`), so delegating here would serve the
       children unfiltered. Only a filter that knows its admission at `path`
       to be prefix-closed may delegate, and the generic one knows no such
       thing: it names no subtree, and the caller keeps reading through it
       path by path. The access check stays so that a refused path fails
       here, where the refusal is named, not on a later read. */
    checkAccess(path);
    return nullptr;
}

void FilteringSourceAccessor::invalidateCache(const CanonPath & path)
{
    next->invalidateCache(prefix / path);
}

void FilteringSourceAccessor::checkAccess(const CanonPath & path)
{
    if (!isAllowed(path))
        throw makeNotAllowedError(path);
}

struct AllowListSourceAccessorImpl : AllowListSourceAccessor
{
    SharedSync<std::set<CanonPath>> allowedPrefixes;
    SharedSync<boost::unordered_flat_set<CanonPath>> allowedPaths;

    AllowListSourceAccessorImpl(
        ref<SourceAccessor> next,
        std::set<CanonPath> && allowedPrefixes,
        boost::unordered_flat_set<CanonPath> && allowedPaths,
        MakeNotAllowedError && makeNotAllowedError)
        : AllowListSourceAccessor(SourcePath(next), std::move(makeNotAllowedError))
        , allowedPrefixes(std::move(allowedPrefixes))
        , allowedPaths(std::move(allowedPaths))
    {
    }

    bool isAllowed(const CanonPath & path) override
    {
        return allowedPaths.readLock()->contains(path) || path.isAllowed(*allowedPrefixes.readLock());
    }

    void allowPrefix(CanonPath prefix) override
    {
        allowedPrefixes.lock()->insert(std::move(prefix));
    }

    /* Whether `path` and everything beneath it is allowed: `path` or an
       ancestor of it is an allowed prefix. `isAllowed` admits more than this
       (a path that is the ancestor of an allowed one, so that directories on
       the way down can be listed; and the exact paths in `allowedPaths`),
       and neither of those admissions extends to children. */
    bool isAllowedWithChildren(const CanonPath & path)
    {
        auto prefixes = allowedPrefixes.readLock();
        auto p = path;
        while (true) {
            if (prefixes->contains(p))
                return true;
            if (p.isRoot())
                return false;
            p.pop();
        }
    }

    /* The allow-list is the one filter whose admission can be prefix-closed,
       so it is the one that may hand out a subtree: a subtree accessor for a
       path under an allowed prefix serves nothing the filter would refuse
       path by path. Anywhere else the answer is the base class's `nullptr`. */
    std::shared_ptr<SourceAccessor> getSubtree(const CanonPath & path) override
    {
        checkAccess(path);
        return isAllowedWithChildren(path) ? next->getSubtree(prefix / path) : nullptr;
    }
};

ref<AllowListSourceAccessor> AllowListSourceAccessor::create(
    ref<SourceAccessor> next,
    std::set<CanonPath> && allowedPrefixes,
    boost::unordered_flat_set<CanonPath> && allowedPaths,
    MakeNotAllowedError && makeNotAllowedError)
{
    return make_ref<AllowListSourceAccessorImpl>(
        next, std::move(allowedPrefixes), std::move(allowedPaths), std::move(makeNotAllowedError));
}

bool CachingFilteringSourceAccessor::isAllowed(const CanonPath & path)
{
    auto i = cache.find(path);
    if (i != cache.end())
        return i->second;
    auto res = isAllowedUncached(path);
    cache.emplace(path, res);
    return res;
}

} // namespace nix
