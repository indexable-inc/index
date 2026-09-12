#include "nix/cmd/command.hh"
#include "nix/store/store-api.hh"
#include "nix/util/finally.hh"
#include "ixe-closure-diff.h"

namespace nix {

namespace {

struct ClosureSnapshot
{
    // Set nodes keep the borrowed name bytes stable when this object moves.
    StorePathSet paths;
    std::vector<IxeClosureEntry> entries;
};

ClosureSnapshot closureSnapshot(ref<Store> store, const StorePath & root)
{
    ClosureSnapshot snapshot;
    store->computeFSClosure({root}, snapshot.paths);
    snapshot.entries.reserve(snapshot.paths.size());
    for (const auto & path : snapshot.paths) {
        auto name = path.name();
        snapshot.entries.push_back({
            .name = reinterpret_cast<const uint8_t *>(name.data()),
            .name_len = name.size(),
            .nar_size = store->queryPathInfo(path)->narSize,
        });
    }
    return snapshot;
}

std::string takeClosureReport(IxeClosureReport report)
{
    Finally release([&]() { ixe_closure_report_free(report); });
    std::string text(reinterpret_cast<const char *>(report.data), report.len);
    if (!report.success)
        throw Error("cannot format closure report: %s", text);
    return text;
}

} // namespace

std::string showVersions(const StringSet & versions)
{
    std::vector<IxeClosureVersion> views;
    views.reserve(versions.size());
    for (const auto & version : versions)
        views.push_back({reinterpret_cast<const uint8_t *>(version.data()), version.size()});
    return takeClosureReport(ixe_closure_versions(views.data(), views.size()));
}

void printClosureDiff(
    ref<Store> store, const StorePath & beforePath, const StorePath & afterPath, std::string_view indent)
{
    auto before = closureSnapshot(store, beforePath);
    auto after = closureSnapshot(store, afterPath);
    auto text = takeClosureReport(ixe_closure_diff(
        before.entries.data(),
        before.entries.size(),
        after.entries.data(),
        after.entries.size(),
        reinterpret_cast<const uint8_t *>(indent.data()),
        indent.size()));
    if (!text.empty())
        logger->cout("%s", text);
}

} // namespace nix
