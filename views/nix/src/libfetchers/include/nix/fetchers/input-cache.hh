#pragma once

#include "nix/fetchers/fetchers.hh"

namespace nix::fetchers {

enum class UseRegistries : int;
struct Settings;

struct InputCache
{
    struct CachedResult
    {
        ref<SourceAccessor> accessor;
        Input resolvedInput;
        Input lockedInput;
        Attrs extraAttrs;
    };

    CachedResult
    getAccessor(const Settings & settings, Store & store, const Input & originalInput, UseRegistries useRegistries);

    struct CachedInput
    {
        Input lockedInput;
        ref<SourceAccessor> accessor;
    };

    virtual std::optional<CachedInput> lookup(const Input & originalInput) const = 0;

    virtual void upsert(Input key, CachedInput cachedInput) = 0;

    virtual void clear() = 0;

    /**
     * Drop every entry whose input is not locked, and report how many went.
     *
     * An evaluator that outlives one evaluation must not serve a second one
     * from this cache wholesale. A locked input names its own content, so the
     * accessor behind it can never go stale and keeping it is what lets a
     * later evaluation reuse everything already parsed and forced under it.
     * An unlocked input names a mutable location instead: the working tree
     * being edited resolves to the same key before and after the edit, so a
     * cache hit would answer the second evaluation with the first one's bytes
     * and the edit would be invisible. Evicting exactly the unlocked entries
     * keeps the reuse and drops the staleness.
     */
    virtual size_t evictUnlocked(const Settings & settings) = 0;

    static ref<InputCache> create();

    virtual ~InputCache() = default;
};

} // namespace nix::fetchers
