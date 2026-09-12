#include "nix/fetchers/input-cache.hh"
#include "nix/fetchers/registry.hh"
#include "nix/util/sync.hh"
#include "nix/util/source-path.hh"

namespace nix::fetchers {

InputCache::CachedResult InputCache::getAccessor(
    const Settings & settings, Store & store, const Input & originalInput, UseRegistries useRegistries)
{
    Input resolvedInput = originalInput;
    /* Attributes of the registry RESOLUTION (`dir`, for an entry that points
       into a subdirectory of a tree), not of the fetched tree. They belong
       to this call's original -> resolved mapping, so they come from this
       call's registry lookup and are never taken from, or stored in, the
       cache. The same resolved input is routinely cached first by a road
       with no registry mapping at all -- `--inputs-from` locks the
       referenced flake, which caches each of its inputs under that input's
       own ref -- and returning such an entry's (empty) attributes in place
       of the lookup's dropped the `dir`: the flake at the workspace ROOT
       evaluated in place of the subdirectory flake the registry entry
       named, with rc=0 and a missing-attribute error naming the wrong
       flake. Flag-registry entries can also change between calls in one
       process, which is one more reason the mapping is not memoisable. */
    Attrs extraAttrs;

    if (!originalInput.isDirect()) {
        if (useRegistries == UseRegistries::No)
            throw Error(
                "'%s' is an indirect flake reference, but registry lookups are not allowed",
                originalInput.to_string());
        auto [res, extraAttrs2] = lookupInRegistries(settings, store, originalInput, useRegistries);
        resolvedInput = std::move(res);
        extraAttrs = std::move(extraAttrs2);
    }

    auto fetched = lookup(resolvedInput);

    if (!fetched) {
        auto [accessor, lockedInput] = resolvedInput.getAccessor(settings, store);
        fetched.emplace(CachedInput{.lockedInput = lockedInput, .accessor = accessor});
        upsert(resolvedInput, *fetched);
        /* Also cache under the locked input, so a later lookup by the
           locked ref (e.g. relative-input metadata stamping during lock
           computation) reuses this accessor instead of refetching or
           taking the substitution shortcut, which returns a store
           accessor stripped of the fetcher's tree metadata. An indirect
           original is deliberately NOT a key: it does not name a tree
           (`evictUnlocked` would drop it as unlocked anyway), and a hit on
           it would skip the registry lookup the attributes above must come
           from. */
        upsert(fetched->lockedInput, *fetched);
    }

    debug("got tree '%s' from '%s'", fetched->accessor, fetched->lockedInput.to_string());

    return {fetched->accessor, resolvedInput, fetched->lockedInput, extraAttrs};
}

struct InputCacheImpl : InputCache
{
    Sync<std::map<Input, CachedInput>> cache_;

    std::optional<CachedInput> lookup(const Input & originalInput) const override
    {
        auto cache(cache_.readLock());
        auto i = cache->find(originalInput);
        if (i == cache->end())
            return std::nullopt;
        debug(
            "mapping '%s' to previously seen input '%s' -> '%s",
            originalInput.to_string(),
            i->first.to_string(),
            i->second.lockedInput.to_string());
        return i->second;
    }

    void upsert(Input key, CachedInput cachedInput) override
    {
        cache_.lock()->insert_or_assign(std::move(key), std::move(cachedInput));
    }

    void clear() override
    {
        cache_.lock()->clear();
    }

    size_t evictUnlocked(const Settings & settings) override
    {
        auto cache(cache_.lock());
        size_t evicted = 0;
        for (auto i = cache->begin(); i != cache->end();) {
            /* Both halves have to be locked. The map is keyed by the original
               input as well as by the locked one, so testing only the key
               keeps an entry a dirty tree reached under a locked-looking
               alias, and testing only the value keeps one whose key is the
               mutable path. */
            if (i->first.isLocked(settings) && i->second.lockedInput.isLocked(settings))
                ++i;
            else {
                i = cache->erase(i);
                evicted++;
            }
        }
        return evicted;
    }
};

ref<InputCache> InputCache::create()
{
    return make_ref<InputCacheImpl>();
}

} // namespace nix::fetchers
