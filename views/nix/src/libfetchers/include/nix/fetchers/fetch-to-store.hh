#pragma once

#include "nix/util/source-path.hh"
#include "nix/store/store-api.hh"
#include "nix/util/file-system.hh"
#include "nix/util/repair-flag.hh"
#include "nix/util/file-content-address.hh"
#include "nix/fetchers/cache.hh"

namespace nix {

enum struct FetchMode { DryRun, Copy };

/**
 * Copy the `path` to the Nix store.
 */
StorePath fetchToStore(
    const fetchers::Settings & settings,
    Store & store,
    const SourcePath & path,
    FetchMode mode,
    std::string_view name = "source",
    ContentAddressMethod method = ContentAddressMethod::Raw::NixArchive,
    PathFilter * filter = nullptr,
    RepairFlag repair = NoRepair);

std::pair<StorePath, Hash> fetchToStore2(
    const fetchers::Settings & settings,
    Store & store,
    const SourcePath & path,
    FetchMode mode,
    std::string_view name = "source",
    ContentAddressMethod method = ContentAddressMethod::Raw::NixArchive,
    PathFilter * filter = nullptr,
    RepairFlag repair = NoRepair);

fetchers::Cache::Key
makeSourcePathToHashCacheKey(std::string_view fingerprint, ContentAddressMethod method, const CanonPath & path);

/**
 * The tree object at `path`, with `filter` applied when one is given, as
 * an accessor announcing its id (`SourceAccessor::knownTreeRoot`): the
 * thing `ContentAddressMethod::Raw::JjTree` can address. `nullptr` when
 * `path` is not a directory or its accessor cannot name the object
 * (`getSubtree` / `getFilteredTree` say when). The root of an accessor
 * unfiltered is the accessor itself, with no call made.
 */
std::shared_ptr<SourceAccessor> treeObjectAt(const SourcePath & path, PathFilter * filter);

} // namespace nix
