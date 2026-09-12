#pragma once
///@file

#include "nix/flake/flakeref.hh"

#include <nlohmann/json_fwd.hpp>

namespace nix {
class Store;
class StorePath;
} // namespace nix

namespace nix::flake {

typedef std::vector<FlakeId> InputAttrPath;

/**
 * A non-empty input attribute path.
 *
 * Input attribute paths identify inputs in a flake. An empty path would
 * refer to the flake itself rather than an input, which contradicts the
 * purpose of operations like override or update.
 */
class NonEmptyInputAttrPath
{
    InputAttrPath path;

    explicit NonEmptyInputAttrPath(InputAttrPath && p)
        : path(std::move(p))
    {
        assert(!path.empty());
    }

public:
    /**
     * Parse and validate a non-empty input attribute path.
     * Returns std::nullopt if the path is empty.
     */
    static std::optional<NonEmptyInputAttrPath> parse(std::string_view s);

    /**
     * Construct from an already-parsed path.
     * Returns std::nullopt if the path is empty.
     */
    static std::optional<NonEmptyInputAttrPath> make(InputAttrPath path);

    /**
     * Append an element to a path, creating a non-empty path.
     * This is always safe because adding an element guarantees non-emptiness.
     */
    static NonEmptyInputAttrPath append(const InputAttrPath & prefix, const FlakeId & element)
    {
        InputAttrPath path = prefix;
        path.push_back(element);
        return NonEmptyInputAttrPath{std::move(path)};
    }

    const InputAttrPath & get() const
    {
        return path;
    }

    operator const InputAttrPath &() const
    {
        return path;
    }

    /**
     * Get the final component of the path (the input name).
     * For a path like "a/b/c", returns "c".
     */
    const FlakeId & inputName() const
    {
        return path.back();
    }

    /**
     * Get the parent path (all components except the last).
     * For a path like "a/b/c", returns "a/b".
     */
    InputAttrPath parent() const
    {
        InputAttrPath result = path;
        result.pop_back();
        return result;
    }

    auto operator<=>(const NonEmptyInputAttrPath & other) const = default;
};

/** Stable identity within one Rust-owned graph. */
struct NodeId
{
    uint64_t value;
    auto operator<=>(const NodeId &) const = default;
};

using Edge = std::variant<NodeId, InputAttrPath>;

/** Host fetch objects for a graph node. Adjacency is owned only by Rust. */
struct LockedNode
{
    FlakeRef lockedRef, originalRef;
    bool isFlake = true;
    std::optional<InputAttrPath> parentInputAttrPath;

    LockedNode(
        const FlakeRef & lockedRef,
        const FlakeRef & originalRef,
        bool isFlake = true,
        std::optional<InputAttrPath> parentInputAttrPath = {})
        : lockedRef(lockedRef)
        , originalRef(originalRef)
        , isFlake(isFlake)
        , parentInputAttrPath(std::move(parentInputAttrPath))
    {
    }

    LockedNode(const fetchers::Settings & fetchSettings, const nlohmann::json & json);
    StorePath computeStorePath(Store & store) const;
};

struct LockPrefetchImpl;

/** A Rust-owned dependency schedule shared by concurrent fetch workers. */
class LockPrefetchSchedule
{
    std::shared_ptr<LockPrefetchImpl> impl;

    explicit LockPrefetchSchedule(std::shared_ptr<LockPrefetchImpl> impl)
        : impl(std::move(impl))
    {
    }
    friend class LockFile;

public:
    std::vector<NodeId> ready() const;
    std::vector<NodeId> complete(NodeId node, bool succeeded) const;
    void checkComplete() const;
};

struct LockFileImpl;

class LockFile
{
    std::shared_ptr<LockFileImpl> impl;

public:
    static constexpr NodeId root{0};
    LockFile();
    LockFile(const fetchers::Settings & fetchSettings, std::string_view contents, std::string_view path);

    using KeyMap = std::map<NodeId, std::string>;

    NodeId addNode(
        const FlakeRef & lockedRef,
        const FlakeRef & originalRef,
        bool isFlake = true,
        std::optional<InputAttrPath> parentInputAttrPath = {});
    void setInput(NodeId node, const FlakeId & name, const Edge & edge);
    std::map<FlakeId, Edge> inputs(NodeId node) const;
    const LockedNode * node(NodeId node) const;
    /** Each directly reachable node once, in Rust graph traversal order. */
    std::vector<NodeId> reachableNodes() const;
    LockPrefetchSchedule prefetchSchedule() const;

    std::pair<nlohmann::json, KeyMap> toJSON() const;
    std::pair<std::string, KeyMap> to_string() const;
    std::optional<FlakeRef> isUnlocked(const fetchers::Settings & fetchSettings) const;
    bool operator==(const LockFile & other) const;
    std::optional<NodeId> findInput(const InputAttrPath & path) const;
    std::map<InputAttrPath, Edge> getAllInputs() const;
    static std::string diff(const LockFile & oldLocks, const LockFile & newLocks);
    void check();
};

std::ostream & operator<<(std::ostream & stream, const LockFile & lockFile);

InputAttrPath parseInputAttrPath(std::string_view s);

std::string printInputAttrPath(const InputAttrPath & path);

} // namespace nix::flake
