#include <nlohmann/json.hpp>
#include <iomanip>
#include <ctime>
#include <memory>
#include <algorithm>
#include <sstream>

#include "ixe-lock-graph.h"
#include "nix/fetchers/fetch-settings.hh"
#include "nix/flake/lockfile.hh"
#include "nix/util/strings.hh"
#include "nix/fetchers/attrs.hh"
#include "nix/fetchers/fetchers.hh"
#include "nix/store/path.hh"
#include "nix/util/ansicolor.hh"
#include "nix/util/error.hh"
#include "nix/util/fmt.hh"
#include "nix/util/logging.hh"

namespace nix::flake {

static void checkGraphError(char * raw)
{
    if (!raw)
        return;
    std::unique_ptr<char, decltype(&ixe_lock_graph_string_free)> error(raw, ixe_lock_graph_string_free);
    throw Error("%s", error.get());
}

static IxeLockBytes bytes(std::string_view text)
{
    return {reinterpret_cast<const uint8_t *>(text.data()), text.size()};
}

static std::vector<IxeLockBytes> pathViews(const InputAttrPath & path)
{
    std::vector<IxeLockBytes> result;
    result.reserve(path.size());
    for (const auto & part : path)
        result.push_back(bytes(part));
    return result;
}

static InputAttrPath copyPath(IxeLockPathView path)
{
    InputAttrPath result;
    result.reserve(path.len);
    for (size_t i = 0; i < path.len; ++i)
        result.emplace_back(reinterpret_cast<const char *>(path.data[i].data), path.data[i].len);
    return result;
}

struct LockFileImpl
{
    std::unique_ptr<IxeLockGraph, decltype(&ixe_lock_graph_free)> graph;
    std::map<NodeId, LockedNode> payloads;

    LockFileImpl()
        : graph(nullptr, ixe_lock_graph_free)
    {
        IxeLockGraph * raw = nullptr;
        checkGraphError(ixe_lock_graph_new_empty(&raw));
        graph.reset(raw);
    }

    explicit LockFileImpl(std::string_view source)
        : graph(nullptr, ixe_lock_graph_free)
    {
        IxeLockGraph * raw = nullptr;
        checkGraphError(ixe_lock_graph_parse(bytes(source), &raw));
        graph.reset(raw);
    }

    nlohmann::json json(char * (*read)(const IxeLockGraph *, char **) ) const
    {
        char * raw = nullptr;
        checkGraphError(read(graph.get(), &raw));
        std::unique_ptr<char, decltype(&ixe_lock_graph_string_free)> text(raw, ixe_lock_graph_string_free);
        return nlohmann::json::parse(text.get());
    }
};

struct LockPrefetchImpl
{
    std::unique_ptr<IxeLockSchedule, decltype(&ixe_lock_schedule_free)> schedule;

    explicit LockPrefetchImpl(IxeLockSchedule * handle)
        : schedule(handle, ixe_lock_schedule_free)
    {
    }
};

static std::vector<NodeId> takeNodes(IxeLockNodes * raw)
{
    std::unique_ptr<IxeLockNodes, decltype(&ixe_lock_nodes_free)> snapshot(raw, ixe_lock_nodes_free);
    std::vector<NodeId> nodes;
    nodes.reserve(ixe_lock_nodes_len(snapshot.get()));
    for (size_t i = 0; i < ixe_lock_nodes_len(snapshot.get()); ++i) {
        NodeId id{};
        checkGraphError(ixe_lock_nodes_get(snapshot.get(), i, &id.value));
        nodes.push_back(id);
    }
    return nodes;
}

LockPrefetchSchedule LockFile::prefetchSchedule() const
{
    IxeLockSchedule * raw = nullptr;
    checkGraphError(ixe_lock_graph_prefetch_schedule(impl->graph.get(), &raw));
    return LockPrefetchSchedule(std::make_shared<LockPrefetchImpl>(raw));
}

std::vector<NodeId> LockPrefetchSchedule::ready() const
{
    IxeLockNodes * raw = nullptr;
    checkGraphError(ixe_lock_schedule_ready(impl->schedule.get(), &raw));
    return takeNodes(raw);
}

std::vector<NodeId> LockPrefetchSchedule::complete(NodeId node, bool succeeded) const
{
    IxeLockNodes * raw = nullptr;
    checkGraphError(ixe_lock_schedule_complete(impl->schedule.get(), node.value, succeeded ? 1 : 0, &raw));
    return takeNodes(raw);
}

void LockPrefetchSchedule::checkComplete() const
{
    checkGraphError(ixe_lock_schedule_check_complete(impl->schedule.get()));
}

LockedNode::LockedNode(const fetchers::Settings & fetchSettings, const nlohmann::json & json)
    : lockedRef(FlakeRef::fromAttrs(fetchSettings, fetchers::jsonToAttrs(json.at("locked"))))
    , originalRef(FlakeRef::fromAttrs(fetchSettings, fetchers::jsonToAttrs(json.at("original"))))
    , isFlake(json.value("flake", true))
    , parentInputAttrPath(
          json.contains("parent") ? std::optional(json.at("parent").get<InputAttrPath>()) : std::nullopt)
{
    if (!lockedRef.input.isLocked(fetchSettings) && !lockedRef.input.isRelative()) {
        if (lockedRef.input.getNarHash() || lockedRef.input.getTreeHash())
            warn(
                "Lock file entry '%s' is unlocked but checked by content hash. "
                "This is not reproducible after garbage collection or sharing.",
                lockedRef.to_string());
        else
            throw Error(
                "Lock file contains unlocked input '%s' with no content hash; re-lock the flake.",
                fetchers::attrsToJSON(lockedRef.input.toAttrs()));
    }
    // A lock node is a final fetch request. __final is a host fetch flag,
    // not a field in the lock document schema.
    lockedRef.input.attrs.insert_or_assign("__final", Explicit<bool>(true));
}

StorePath LockedNode::computeStorePath(Store & store) const
{
    return lockedRef.input.computeStorePath(store);
}

LockFile::LockFile()
    : impl(std::make_shared<LockFileImpl>())
{
}

LockFile::LockFile(const fetchers::Settings & fetchSettings, std::string_view contents, std::string_view path)
{
    try {
        impl = std::make_shared<LockFileImpl>(contents);
        for (auto & item : impl->json(ixe_lock_graph_payloads))
            impl->payloads.emplace(
                NodeId{item.at("id").get<uint64_t>()}, LockedNode(fetchSettings, item.at("payload")));
    } catch (Error & error) {
        error.addTrace({}, "while reading lock file '%s'", path);
        throw;
    }
}

NodeId LockFile::addNode(
    const FlakeRef & lockedRef,
    const FlakeRef & originalRef,
    bool isFlake,
    std::optional<InputAttrPath> parentInputAttrPath)
{
    auto locked = fetchers::attrsToJSON(lockedRef.toAttrs());
    locked.erase("__final");
    nlohmann::json payload = {{"locked", locked}, {"original", fetchers::attrsToJSON(originalRef.toAttrs())}};
    if (!isFlake)
        payload["flake"] = false;
    if (parentInputAttrPath)
        payload["parent"] = *parentInputAttrPath;
    NodeId id{};
    checkGraphError(ixe_lock_graph_add(impl->graph.get(), bytes(payload.dump()), &id.value));
    impl->payloads.emplace(id, LockedNode(lockedRef, originalRef, isFlake, std::move(parentInputAttrPath)));
    return id;
}

static Edge edgeFromJSON(const nlohmann::json & edge)
{
    if (edge.is_array())
        return edge.get<InputAttrPath>();
    return NodeId{edge.get<uint64_t>()};
}

void LockFile::setInput(NodeId node, const FlakeId & name, const Edge & edge)
{
    if (auto target = std::get_if<NodeId>(&edge)) {
        checkGraphError(ixe_lock_graph_set_direct(impl->graph.get(), node.value, bytes(name), target->value));
    } else {
        auto path = pathViews(std::get<InputAttrPath>(edge));
        checkGraphError(
            ixe_lock_graph_set_follows(impl->graph.get(), node.value, bytes(name), {path.data(), path.size()}));
    }
}

std::map<FlakeId, Edge> LockFile::inputs(NodeId node) const
{
    IxeLockInputs * raw = nullptr;
    checkGraphError(ixe_lock_graph_inputs(impl->graph.get(), node.value, &raw));
    std::unique_ptr<IxeLockInputs, decltype(&ixe_lock_inputs_free)> snapshot(raw, ixe_lock_inputs_free);
    std::map<FlakeId, Edge> result;
    for (size_t i = 0; i < ixe_lock_inputs_len(snapshot.get()); ++i) {
        IxeLockEdgeView view{};
        checkGraphError(ixe_lock_inputs_get(snapshot.get(), i, &view));
        std::string name(reinterpret_cast<const char *>(view.name.data), view.name.len);
        if (view.kind == 0)
            result.emplace(std::move(name), NodeId{view.target});
        else if (view.kind == 1)
            result.emplace(std::move(name), copyPath(view.follows));
        else
            throw Error("invalid Rust lock edge kind %d", view.kind);
    }
    return result;
}

const LockedNode * LockFile::node(NodeId id) const
{
    if (id == root)
        return nullptr;
    auto found = impl->payloads.find(id);
    if (found == impl->payloads.end())
        throw Error("unknown lock node %d", id.value);
    return &found->second;
}

std::optional<NodeId> LockFile::findInput(const InputAttrPath & path) const
{
    auto views = pathViews(path);
    NodeId result{};
    uint8_t found = 0;
    checkGraphError(ixe_lock_graph_find(impl->graph.get(), {views.data(), views.size()}, &result.value, &found));
    if (!found)
        return std::nullopt;
    return result;
}

std::pair<nlohmann::json, LockFile::KeyMap> LockFile::toJSON() const
{
    auto serialized = impl->json(ixe_lock_graph_serialize);
    KeyMap keys;
    for (auto & item : serialized.at("keys"))
        keys.emplace(NodeId{item.at("id").get<uint64_t>()}, item.at("key").get<std::string>());
    return {serialized.at("document"), std::move(keys)};
}

std::pair<std::string, LockFile::KeyMap> LockFile::to_string() const
{
    auto [document, keys] = toJSON();
    return {document.dump(2), std::move(keys)};
}

std::ostream & operator<<(std::ostream & stream, const LockFile & lockFile)
{
    return stream << lockFile.to_string().first;
}

std::vector<NodeId> LockFile::reachableNodes() const
{
    IxeLockNodes * raw = nullptr;
    checkGraphError(ixe_lock_graph_reachable(impl->graph.get(), &raw));
    return takeNodes(raw);
}

std::optional<FlakeRef> LockFile::isUnlocked(const fetchers::Settings & fetchSettings) const
{
    for (auto id : reachableNodes()) {
        auto payload = node(id);
        if (!payload)
            continue;
        auto & input = payload->lockedRef.input;
        bool locked = input.isLocked(fetchSettings)
                      || (fetchSettings.allowDirtyLocks && (input.getNarHash() || input.getTreeHash()));
        if ((!locked || !input.isFinal()) && !input.isRelative())
            return payload->lockedRef;
    }
    return {};
}

bool LockFile::operator==(const LockFile & other) const
{
    uint8_t left[32], right[32];
    checkGraphError(ixe_lock_graph_identity(impl->graph.get(), left));
    checkGraphError(ixe_lock_graph_identity(other.impl->graph.get(), right));
    return std::equal(std::begin(left), std::end(left), std::begin(right));
}

std::map<InputAttrPath, Edge> LockFile::getAllInputs() const
{
    std::map<InputAttrPath, Edge> result;
    for (auto & item : impl->json(ixe_lock_graph_all_inputs))
        result.emplace(item.at("path").get<InputAttrPath>(), edgeFromJSON(item.at("edge")));
    return result;
}

static std::string describe(const LockFile & graph, const Edge & edge)
{
    if (auto follows = std::get_if<InputAttrPath>(&edge))
        return fmt("follows '%s'", printInputAttrPath(*follows));
    auto & ref = graph.node(std::get<NodeId>(edge))->lockedRef;
    auto text = fmt("'%s'", ref.to_string());
    if (auto lastModified = ref.input.getLastModified())
        text += fmt(" (%s)", std::put_time(std::gmtime(&*lastModified), "%Y-%m-%d"));
    return text;
}

std::string LockFile::diff(const LockFile & oldLocks, const LockFile & newLocks)
{
    auto oldFlat = oldLocks.getAllInputs();
    auto newFlat = newLocks.getAllInputs();
    auto i = oldFlat.begin();
    auto j = newFlat.begin();
    std::string result;
    while (i != oldFlat.end() || j != newFlat.end()) {
        if (j != newFlat.end() && (i == oldFlat.end() || i->first > j->first)) {
            result +=
                fmt("• " ANSI_GREEN "Added input '%s':" ANSI_NORMAL "\n    %s\n",
                    printInputAttrPath(j->first),
                    describe(newLocks, j->second));
            ++j;
        } else if (i != oldFlat.end() && (j == newFlat.end() || i->first < j->first)) {
            result += fmt("• " ANSI_RED "Removed input '%s'" ANSI_NORMAL "\n", printInputAttrPath(i->first));
            ++i;
        } else {
            auto oldNode = std::get_if<NodeId>(&i->second);
            auto newNode = std::get_if<NodeId>(&j->second);
            bool equal =
                oldNode && newNode
                    ? oldLocks.node(*oldNode)->lockedRef == newLocks.node(*newNode)->lockedRef
                    : !oldNode && !newNode && std::get<InputAttrPath>(i->second) == std::get<InputAttrPath>(j->second);
            if (!equal)
                result +=
                    fmt("• " ANSI_BOLD "Updated input '%s':" ANSI_NORMAL "\n    %s\n  → %s\n",
                        printInputAttrPath(i->first),
                        describe(oldLocks, i->second),
                        describe(newLocks, j->second));
            ++i;
            ++j;
        }
    }
    return result;
}

void LockFile::check()
{
    checkGraphError(ixe_lock_graph_check(impl->graph.get()));
}

InputAttrPath parseInputAttrPath(std::string_view source)
{
    IxeLockPath * raw = nullptr;
    checkGraphError(ixe_lock_graph_parse_path(bytes(source), &raw));
    std::unique_ptr<IxeLockPath, decltype(&ixe_lock_path_free)> path(raw, ixe_lock_path_free);
    return copyPath(ixe_lock_path_view(path.get()));
}

std::optional<NonEmptyInputAttrPath> NonEmptyInputAttrPath::parse(std::string_view source)
{
    return make(parseInputAttrPath(source));
}

std::optional<NonEmptyInputAttrPath> NonEmptyInputAttrPath::make(InputAttrPath path)
{
    if (path.empty())
        return std::nullopt;
    return NonEmptyInputAttrPath{std::move(path)};
}

std::string printInputAttrPath(const InputAttrPath & path)
{
    return concatStringsSep("/", path);
}

} // namespace nix::flake
