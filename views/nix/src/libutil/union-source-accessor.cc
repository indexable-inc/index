#include "nix/util/source-accessor.hh"

namespace nix {

struct UnionSourceAccessor : SourceAccessor
{
    std::vector<ref<SourceAccessor>> accessors;

    UnionSourceAccessor(std::vector<ref<SourceAccessor>> _accessors)
        : accessors(std::move(_accessors))
    {
        displayPrefix.clear();
    }

    void readFile(const CanonPath & path, Sink & sink, fun<void(uint64_t)> sizeCallback) override
    {
        for (auto & accessor : accessors) {
            auto st = accessor->maybeLstat(path);
            if (st) {
                accessor->readFile(path, sink, sizeCallback);
                return;
            }
        }
        throw FileNotFound("path '%s' does not exist", showPath(path));
    }

    std::optional<Stat> maybeLstat(const CanonPath & path) override
    {
        for (auto & accessor : accessors) {
            auto st = accessor->maybeLstat(path);
            if (st)
                return st;
        }
        return std::nullopt;
    }

    DirEntries readDirectory(const CanonPath & path) override
    {
        DirEntries result;
        bool exists = false;
        for (auto & accessor : accessors) {
            auto st = accessor->maybeLstat(path);
            if (!st)
                continue;
            exists = true;
            for (auto & entry : accessor->readDirectory(path))
                // Don't override entries from previous accessors.
                result.insert(entry);
        }
        if (!exists)
            throw FileNotFound("path '%s' does not exist", showPath(path));
        return result;
    }

    /* Which accessor answers for a path decides which tree the read belongs
       to, so this resolves the same way every other method here does, by
       first hit. The union does not override `getFingerprint`, so a path it
       answers for arrives at the read-set trace with no fingerprint at all;
       without this the trace could not name the tree either, and an unnamed
       tree can only be paired across runs by position. */
    std::string_view identityClass(const CanonPath & path) override
    {
        for (auto & accessor : accessors)
            if (accessor->maybeLstat(path))
                return accessor->identityClass(path);
        /* Nothing has the path, and "absent" is still an answer the trace
           records; the read belongs to the union as a whole, which has no name
           of its own. The first member stands for it: membership order is fixed
           at construction, so the name is the same in every run. Returning ""
           here left the pure-eval `HOME=""` probes of `/.config/nixpkgs/*`
           anonymous and the compare refusing every trace of a flake. */
        return accessors.empty() ? "" : accessors.front()->identityClass(path);
    }

    std::string readLink(const CanonPath & path) override
    {
        for (auto & accessor : accessors) {
            auto st = accessor->maybeLstat(path);
            if (st)
                return accessor->readLink(path);
        }
        throw FileNotFound("path '%s' does not exist", showPath(path));
    }

    std::string showPath(const CanonPath & path) override
    {
        for (auto & accessor : accessors)
            return accessor->showPath(path);
        return SourceAccessor::showPath(path);
    }

    std::optional<std::filesystem::path> getPhysicalPath(const CanonPath & path) override
    {
        for (auto & accessor : accessors) {
            auto p = accessor->getPhysicalPath(path);
            if (p)
                return p;
        }
        return std::nullopt;
    }

    std::pair<CanonPath, std::optional<std::string>> getFingerprint(const CanonPath & path) override
    {
        if (fingerprint)
            return {path, fingerprint};
        for (auto & accessor : accessors) {
            auto [subpath, fingerprint] = accessor->getFingerprint(path);
            if (fingerprint)
                return {subpath, fingerprint};
        }
        return {path, std::nullopt};
    }

    /* A subtree accessor is one object with one id. The union's view of a
       directory that two layers have is not one: `readDirectory` merges
       their entries, so no layer's object is what reads of `path` see, and
       such a path names no subtree. When exactly one layer has the path its
       answer stands, `nullptr` included (a layer that has the path but
       cannot name subtrees hides nothing, there is nothing beneath it to
       hide). The rule is what keeps the answer a function of the trees and
       not of the store's state: the impure evaluator's root filesystem is
       this union of the real filesystem over the store mounts, and a lazily
       mounted store path that later got materialized is in both layers. A
       first-hit answer there would name the subtree object until the copy
       and a plain directory after it. */
    std::shared_ptr<SourceAccessor> getSubtree(const CanonPath & path) override
    {
        SourceAccessor * only = nullptr;
        for (auto & accessor : accessors) {
            if (!accessor->maybeLstat(path))
                continue;
            if (only)
                return nullptr;
            only = &*accessor;
        }
        if (!only)
            throw FileNotFound("path '%s' does not exist", showPath(path));
        return only->getSubtree(path);
    }

    void invalidateCache(const CanonPath & path) override
    {
        for (auto & accessor : accessors)
            accessor->invalidateCache(path);
    }
};

ref<SourceAccessor> makeUnionSourceAccessor(std::vector<ref<SourceAccessor>> && accessors)
{
    return make_ref<UnionSourceAccessor>(std::move(accessors));
}

} // namespace nix
