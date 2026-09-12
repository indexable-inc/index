#include "nix/util/mounted-source-accessor.hh"

#include <boost/unordered/concurrent_flat_map.hpp>

namespace nix {

struct MountedSourceAccessorImpl : MountedSourceAccessor
{
    boost::concurrent_flat_map<CanonPath, ref<SourceAccessor>> mounts;

    MountedSourceAccessorImpl(std::map<CanonPath, ref<SourceAccessor>> _mounts)
    {
        displayPrefix.clear();

        // Currently we require a root filesystem. This could be relaxed.
        assert(_mounts.contains(CanonPath::root));

        for (auto & [path, accessor] : _mounts)
            mount(path, accessor);

        // FIXME: return dummy parent directories automatically?
    }

    void readFile(const CanonPath & path, Sink & sink, fun<void(uint64_t)> sizeCallback) override
    {
        auto [accessor, subpath] = lookup(path);
        return accessor->readFile(subpath, sink, sizeCallback);
    }

    Stat lstat(const CanonPath & path) override
    {
        auto [accessor, subpath] = lookup(path);
        return accessor->lstat(subpath);
    }

    std::optional<Stat> maybeLstat(const CanonPath & path) override
    {
        auto [accessor, subpath] = lookup(path);
        return accessor->maybeLstat(subpath);
    }

    DirEntries readDirectory(const CanonPath & path) override
    {
        auto [accessor, subpath] = lookup(path);
        return accessor->readDirectory(subpath);
    }

    std::string readLink(const CanonPath & path) override
    {
        auto [accessor, subpath] = lookup(path);
        return accessor->readLink(subpath);
    }

    std::string showPath(const CanonPath & path) override
    {
        auto [accessor, subpath] = lookup(path);
        return displayPrefix + accessor->showPath(subpath) + displaySuffix;
    }

    std::pair<ref<SourceAccessor>, CanonPath> lookup(CanonPath path)
    {
        // Find the nearest parent of `path` that is a mount point.
        std::vector<std::string> subpath;
        while (true) {
            if (auto mount = getMount(path)) {
                std::reverse(subpath.begin(), subpath.end());
                return {ref(mount), CanonPath(subpath)};
            }

            assert(!path.isRoot());
            subpath.push_back(std::string(*path.baseName()));
            path.pop();
        }
    }

    std::optional<std::filesystem::path> getPhysicalPath(const CanonPath & path) override
    {
        auto [accessor, subpath] = lookup(path);
        return accessor->getPhysicalPath(subpath);
    }

    void mount(CanonPath mountPoint, ref<SourceAccessor> accessor) override
    {
        mounts.emplace(std::move(mountPoint), std::move(accessor));
    }

    std::shared_ptr<SourceAccessor> getMount(CanonPath mountPoint) override
    {
        if (auto res = getConcurrent(mounts, mountPoint))
            return *res;
        else
            return nullptr;
    }

    std::optional<CanonPath> findMount(const SourceAccessor & accessor) override
    {
        std::optional<CanonPath> found;
        bool ambiguous = false;
        mounts.cvisit_all([&](const auto & entry) {
            if (&*entry.second != &accessor)
                return;
            if (found && *found != entry.first)
                ambiguous = true;
            else
                found = entry.first;
        });
        return ambiguous ? std::nullopt : found;
    }

    std::pair<CanonPath, std::optional<std::string>> getFingerprint(const CanonPath & path) override
    {
        if (fingerprint)
            return {path, fingerprint};
        auto [accessor, subpath] = lookup(path);
        return accessor->getFingerprint(subpath);
    }

    std::string_view identityClass(const CanonPath & path) override
    {
        auto [accessor, subpath] = lookup(path);
        return accessor->identityClass(subpath);
    }

    std::shared_ptr<SourceAccessor> getSubtree(const CanonPath & path) override
    {
        auto [accessor, subpath] = lookup(path);
        return accessor->getSubtree(subpath);
    }

    /* The path-taking overrides below would otherwise hide the
       zero-argument base overloads. */
    using SourceAccessor::getLastModified;
    using SourceAccessor::getRev;

    std::optional<Hash> getRev(const CanonPath & path) override
    {
        auto [accessor, subpath] = lookup(path);
        return accessor->getRev(subpath);
    }

    std::optional<time_t> getLastModified(const CanonPath & path) override
    {
        auto [accessor, subpath] = lookup(path);
        /* A mount that does not know its own time (e.g. a plain
           filesystem accessor for a dirty workdir) falls back to the
           whole tree's value. */
        if (auto t = accessor->getLastModified(subpath))
            return t;
        return getLastModified();
    }

    void invalidateCache(const CanonPath & path) override
    {
        auto [accessor, subpath] = lookup(path);
        accessor->invalidateCache(subpath);
    }
};

ref<MountedSourceAccessor> makeMountedSourceAccessor(std::map<CanonPath, ref<SourceAccessor>> mounts)
{
    return make_ref<MountedSourceAccessorImpl>(std::move(mounts));
}

} // namespace nix
