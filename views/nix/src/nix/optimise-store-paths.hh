#pragma once

#include "nix/store/local-store.hh"
#include "nix/util/config-global.hh"

namespace nix {

/** Optimise only explicit, registered top-level paths; never traverse their closure. */
inline void optimiseStorePaths(ref<Store> store, const std::vector<std::string> & paths)
{
    auto local = store.dynamic_pointer_cast<LocalStore>();
    if (!local)
        throw Error("optimising explicit paths requires a local store (use --store local)");
    StorePathSet selected;
    for (const auto & path : paths) {
        auto parsed = store->parseStorePath(path);
        local->addTempRoot(parsed);
        if (!local->isValidPath(parsed))
            throw InvalidPath("cannot optimise unregistered path '%s'", path);
        selected.insert(std::move(parsed));
    }
    // optimisePath is also used by automatic import optimisation and obeys this
    // setting. An explicit command must enable it in this process, even when
    // the host disables automatic optimisation. No persistent setting changes.
    globalConfig.set("auto-optimise-store", "true");
    for (const auto & path : selected) {
        notice("optimising explicit path '%s'", store->printStorePath(path));
        local->optimisePath(local->toRealPath(path), NoRepair);
    }
}

}
