#pragma once
/**
 * @file
 *
 * Read-set instrumentation: phase 1 of incremental evaluation.
 *
 * Records, for a small number of designated evaluation boundaries, the set
 * of inputs that were read while that boundary was on top of the stack.
 * Nothing here caches anything or changes what evaluation produces; the
 * output is a trace file used to answer how much of an evaluation a change
 * to one file actually invalidates.
 *
 * The boundaries are the ones the design calls tracked entries: the result
 * of an `import`, a `builtins.derivationStrict` call, and (opt in, because
 * it perturbs evaluation order) an option value in the NixOS module
 * fixpoint. Everything between two boundaries is ordinary computation and
 * is attributed to the innermost one.
 */

#include "nix/expr/nixexpr.hh"
#include "nix/util/canon-path.hh"
#include "nix/util/file-descriptor.hh"
#include "nix/util/source-read-hook.hh"
#include "nix/util/source-path.hh"

#include <array>
#include <cstdint>
#include <filesystem>
#include <limits>
#include <map>
#include <optional>
#include <string>
#include <string_view>
#include <utility>
#include <vector>

#include <boost/unordered/unordered_flat_map.hpp>
#include <boost/unordered/unordered_flat_set.hpp>

namespace nix {

class EvalState;

/** What kind of boundary a tracked entry is. */
enum class TrackedEntryKind : uint8_t {
    /** The whole evaluation, so that time outside every other entry is still accounted for. */
    root,
    /** The result of forcing an imported file's top level expression. */
    import,
    /** A `builtins.derivationStrict` call, keyed on the derivation name. */
    derivation,
    /** An option value in the NixOS module fixpoint, keyed on the option path. */
    option,
};

/** How an edge between two tracked entries came to be observed. */
enum class EdgeKind : uint8_t {
    /** The producer was entered for the first time while the consumer was innermost. */
    demand,
    /** The producer's memoised result was served without the boundary being entered again. */
    reuse,
    /** The producer's derivation output is an input of the consumer's derivation. */
    derivation,
    /**
     * The producer's value, or a value computed from it, was read by the
     * consumer. Carried by the provenance side table rather than by any
     * boundary nesting: a string literal from an imported file crosses into
     * a derivation argument through the module fixpoint without entering a
     * boundary, which is the class the read-set-recall measurements showed
     * no input class can reach.
     */
    value,
};

std::string_view showTrackedEntryKind(TrackedEntryKind kind);
std::string_view showSourceReadKind(SourceReadKind kind);
std::string_view showEdgeKind(EdgeKind kind);

/**
 * Records inputs into whichever tracked entry is innermost, and writes one
 * JSON object per entry to a trace file as each entry is popped.
 *
 * One instance per `EvalState`. Not thread safe: multi threaded evaluation
 * would need one stack per thread, which phase 1 does not attempt. The
 * `readSetTraceFile` setting is refused when parallel evaluation is on.
 */
class ReadSetTracker
{
public:
    ReadSetTracker(EvalState & state, std::optional<std::filesystem::path> traceFile, bool hashContents);
    ~ReadSetTracker();

    ReadSetTracker(const ReadSetTracker &) = delete;
    ReadSetTracker & operator=(const ReadSetTracker &) = delete;

    void push(TrackedEntryKind kind, std::string key, size_t accessor = 0);
    void pop();
    void setCurrentKey(std::string key);

    /** Record a read of a file, listing, symlink or subtree. */
    void recordRead(SourceAccessor & accessor, const CanonPath & path, SourceReadKind kind);

    /**
     * Record what a read observed: the bytes of a file, the type of a path,
     * the entries of a directory, a symlink target, or the hash of a dumped
     * subtree. Without this an input records that an observation happened but
     * not its answer, and two traces cannot tell an unchanged answer from one
     * that was never compared.
     */
    void
    recordObserved(SourceAccessor & accessor, const CanonPath & path, SourceReadKind kind, std::string_view observed);

    /** Record an observation of a source position. */
    void recordPosition(const Pos & pos, bool withLineColumn);

    /**
     * Record a read of a tree's version: `rev`, `lastModified`, `narHash` and
     * the rest. Not a file read, so nothing in the filesystem stands in for it;
     * without this class an evaluation that embeds a revision in a derivation
     * validates against unchanged file inputs and a cache serves the previous
     * commit's answer.
     */
    void recordTreeAttr(std::string_view treeId, std::string_view attr, Value * value);

    /** Record a query against the store: `builtins.storePath`, IFD, a path realisation. */
    void recordStoreQuery(std::string_view storePath);

    /**
     * Name the value the innermost entry produces, so that a later demand of
     * that same value records an edge rather than vanishing.
     *
     * A boundary is entered once and its result memoised, so every demand
     * after the first is served from the memo table and never reaches the
     * boundary. Phase 1 recorded only the first, as the entry's parent, which
     * makes the entry graph a spanning tree of first demands rather than the
     * graph of which values flowed into which.
     *
     * The pointer is an identity and is never dereferenced. The caller has to
     * be a memo table that keeps the value alive for the rest of the
     * evaluation, which is what makes a later sighting of the same address the
     * same value rather than a recycled one.
     */
    void noteProduces(const void * value);

    /**
     * Record that the innermost entry demanded a value that an earlier entry
     * produced. Does nothing for a value no boundary produced, which is nearly
     * every value.
     */
    void noteDemand(const void * value);

    /**
     * Name the derivation the innermost entry produced. Derivations are the
     * boundary whose reuse no pointer identity catches: a consumer selects
     * `.drvPath` off an attribute set forced long ago, so nothing is called a
     * second time and there is no demand to observe.
     */
    void noteDerivationProduced(std::string_view drvPath);

    /**
     * Record that the derivation the innermost entry is building consumes
     * another derivation's output. This is the edge that carries the case
     * phase 1 measured and could not decide: of the derivations one commit
     * moves, one reads the edited bytes and the rest move because their input
     * derivations moved.
     */
    void noteDerivationDemand(std::string_view drvPath);

    /**
     * The payload identity of a value: the pointer that survives a 16-byte
     * `Value` struct copy. Nix evaluation copies `Value` structs freely, so
     * an address-of-Value identity breaks at the first assignment; the
     * string data and the bindings are shared by pointer and survive every
     * copy, select, list rebuild and attrset merge. Null for types whose
     * payload is inline (integers, booleans, floats), which is the class
     * this mechanism cannot carry and the summary reports.
     */
    static const void * provKey(const Value & v);

    /**
     * Register a value as carrying the given provenance source: a tracked
     * entry id when non negative, `-(inputId + 1)` for a recorded input.
     * Every value found to share the payload later is treated as the same
     * value having flowed there.
     */
    void provRegister(const Value & v, int64_t source);

    /** Register the value as produced by the innermost open entry. */
    void provRegisterInnermost(const Value & v);

    /**
     * The attribute-selection hook. If the result carries provenance, the
     * innermost entry consumed it (an edge or an input); otherwise, if the
     * base container carries provenance, the result inherits it and the
     * read itself is a consumption. This is what lets a registration on an
     * import's top-level attrset reach the string literal three selects
     * down, and it is the propagation ENG-12310 names as the missing edge.
     */
    void provSelect(const Value & base, const Value & result);

    /** Record a read of a value: an edge or input on the innermost entry if the value is registered. */
    void provConsume(const Value & v);

    /**
     * Register the preallocated values of a file's string literals as
     * carrying that file's contents input. A literal that later lands in a
     * derivation argument then names the file it came from in that
     * derivation's own input set, which is the flow the read-set-recall
     * measurements showed no boundary nesting can see: the literal crosses
     * the module fixpoint as a value, entering no boundary at all.
     */
    void registerLiterals(const SourcePath & path, const std::vector<Value *> & literals);

    /**
     * Register a value whose bytes were read from a file, `builtins.readFile`
     * being the one producer. The consuming entry already records the read;
     * the registration is what lets the value's later flow into some other
     * entry name the file there too.
     */
    void provRegisterFileValue(const SourcePath & path, const Value & v);

    /**
     * Bracket a computation that builds a new value out of read ones, so
     * the result inherits the provenance of everything consumed inside:
     * string concatenation, substring, hashing, JSON rendering. Used via
     * `ProvScope`.
     */
    size_t provScopeBegin();
    void provScopeEnd(size_t start, const Value & result);
    void provScopeAbort();

    /** How many entries of each kind have been emitted. Used by the tests that guard against a silent no op. */
    uint64_t entriesOfKind(TrackedEntryKind kind) const
    {
        return kindCounts[static_cast<size_t>(kind)];
    }

    uint64_t totalInputs() const
    {
        return inputTable.size();
    }

    /** How many edges have been emitted, after deduplication. */
    uint64_t totalEdges() const
    {
        return nrEdges;
    }

    /** How many edges of one kind were recorded, before deduplication. */
    uint64_t edgesOfKind(EdgeKind kind) const
    {
        return edgeKindCounts[static_cast<size_t>(kind)];
    }

private:
    friend struct ReadSetFrame;

    struct Entry
    {
        uint64_t id;
        uint64_t parent;
        TrackedEntryKind kind;
        std::string key;
        /** Which accessor the key is relative to, for a key that is a path. */
        size_t accessor;
        uint32_t depth;
        /** Input ids read while this entry was innermost. */
        std::vector<uint32_t> inputs;
        /**
         * Ids of the entries whose values flowed into this one. Invalidation
         * travels along these from producer to consumer: an entry whose own
         * read set is unchanged is still stale when something it demanded is,
         * and 80,285 of the 91,758 entries phase 1 traced read no files at
         * all, so for most of the graph this is the only evidence there is.
         */
        std::vector<uint64_t> edges;
        uint64_t wallStartNs, cpuStartNs;
        /** Time spent in entries pushed while this one was innermost. */
        uint64_t wallChildNs = 0, cpuChildNs = 0;
        uint64_t valuesStart;
        uint64_t valuesChild = 0;
        /** Id of the last input recorded, so a repeated read of one path is cheap. */
        uint32_t lastInput = std::numeric_limits<uint32_t>::max();
        /** Id of the last edge recorded, so a repeated demand of one producer is cheap. */
        uint64_t lastEdge = std::numeric_limits<uint64_t>::max();
        /**
         * What this entry produced, where that is a value two runs can be
         * compared on. Only a derivation has one: its store path is a hash of
         * everything that went into it, so an entry whose `produced` did not
         * move produced the same answer whatever its inputs did.
         */
        std::string produced;
    };

    uint32_t
    internInput(SourceAccessor * accessor, size_t accessorNumber, std::string_view path, std::string_view kind);
    void addInput(uint32_t id);
    /** Append an edge from the innermost entry to the entry that produced what it just demanded. */
    void addEdge(uint64_t producer, EdgeKind kind);

    /**
     * A small id for the tree an accessor answers for, assigned on first
     * sight. Nix offers no edit-stable name for a tree: `displayPrefix`,
     * `getFingerprint` and the mount root all carry the version, so naming an
     * input by any of them makes every input under an edited tree a different
     * input. The id separates which tree from which version of it, and the
     * tree's identifying fields are written to the trace once so that an
     * analysis can map ids between two runs.
     */
    uint32_t treeIdFor(SourceAccessor * accessor, std::string_view path, std::string_view rel, std::string_view fp);
    void emit(const Entry & entry, uint64_t wallNs, uint64_t cpuNs, uint64_t values);
    void write(std::string_view line);

    EvalState & state;
    bool hashContents;
    AutoCloseFD fd;
    std::string buffer;

    std::vector<Entry> stack;
    uint64_t nextEntryId = 0;
    std::array<uint64_t, 4> kindCounts{};

    /** Interned inputs. The key is accessor number, kind and path; the value is the input id. */
    boost::unordered_flat_map<std::string, uint32_t> inputTable;

    /** Inputs whose observed value has already been written to the trace. */
    boost::unordered_flat_map<uint32_t, std::string> observedInputs;

    /** Tree identity to id, keyed on the accessor number and the store or mount root. */
    boost::unordered_flat_map<std::string, uint32_t> treeIds;

    /**
     * Values a tracked entry produced, mapped to the entry that produced them.
     * Raw pointers into collected memory, held only for values that a memo
     * table already keeps alive for the whole evaluation, so this retains
     * nothing of its own and cannot be handed a recycled address.
     */
    boost::unordered_flat_map<const void *, uint64_t> entryByValue;

    /** Derivation store path to the entry that produced that derivation. */
    boost::unordered_flat_map<std::string, uint64_t> entryByDrvPath;

    /**
     * Payload pointer to the id of the provenance set it carries. Holds
     * raw pointers into collected memory deliberately: a recycled address
     * can only add a spurious edge, which costs recomputation and never a
     * stale answer, and keeping the keys alive would retain most of the
     * heap.
     */
    boost::unordered_flat_map<const void *, uint32_t> provByPayload;
    /** Interned provenance sets: sorted vectors of sources. */
    std::vector<std::vector<int64_t>> provSets;
    std::map<std::vector<int64_t>, uint32_t> provSetIds;
    /** Sources consumed since the outermost open ProvScope began, with the scope depth they were consumed at. */
    std::vector<std::pair<int, int64_t>> provHitLog;
    /** Scratch buffer for building a scope's union without an allocation per scope. */
    std::vector<int64_t> provScratch;
    int provScopeDepth = 0;
    uint64_t nrProvRegistered = 0;
    uint64_t nrProvHits = 0;
    uint64_t nrProvSelects = 0;

    uint32_t provInternSet(const std::vector<int64_t> & set);
    void provConsumeSet(uint32_t setId);

    uint64_t nrEdges = 0;
    uint64_t nrEntriesWithEdges = 0;
    std::array<uint64_t, 4> edgeKindCounts{};

    /** Clocks at construction, so trace timestamps are relative to the start of evaluation. */
    uint64_t startWallNs;
    uint64_t startCpuNs;

    /**
     * Nanoseconds of wall clock elapsed before the first read of a file
     * under a path that is not in the store. This is the fork snapshot
     * prefix: the share of evaluation that happens before any of the tree
     * under edit is touched.
     */
    uint64_t firstNonStoreReadNs = 0;
    std::string firstNonStoreReadPath;
    uint64_t nrReads = 0;
};

/**
 * Pushes a tracked entry for as long as it is in scope. Constructed
 * unconditionally at every boundary, with a null tracker when
 * instrumentation is off, so the boundary needs no branch of its own. The
 * key is a callable rather than a string so that building the key costs
 * nothing in the off case, which is every ordinary evaluation.
 */
struct ReadSetFrame
{
    ReadSetTracker * tracker;

    template<typename MakeKey>
    ReadSetFrame(ReadSetTracker * tracker, TrackedEntryKind kind, MakeKey && makeKey, size_t accessor = 0)
        : tracker(tracker)
    {
        if (tracker) [[unlikely]]
            tracker->push(kind, makeKey(), accessor);
    }

    ReadSetFrame(const ReadSetFrame &) = delete;
    ReadSetFrame(ReadSetFrame &&) = delete;

    /**
     * Name the value this entry produces, so that a consumer served from a
     * memo table records an edge. See `ReadSetTracker::noteProduces`.
     */
    void produces(const void * value)
    {
        if (tracker) [[unlikely]]
            tracker->noteProduces(value);
    }

    ~ReadSetFrame()
    {
        if (tracker) [[unlikely]]
            tracker->pop();
    }

    /**
     * Register the payload of the value this entry produced, once it has
     * been forced. `produces` keys on the value's address for the memo
     * table; this keys on the payload, which is what survives the struct
     * copies every consumer receives.
     */
    void producesValue(const Value & v)
    {
        if (tracker) [[unlikely]]
            tracker->provRegisterInnermost(v);
    }

    /**
     * Replace the key of the entry this frame pushed, for a boundary whose
     * name is only known after some evaluation has already happened, as
     * with a derivation's name.
     */
    template<typename MakeKey>
    void setKey(MakeKey && makeKey)
    {
        if (tracker) [[unlikely]]
            tracker->setCurrentKey(makeKey());
    }
};

/**
 * Brackets a computation that derives a new value from values it reads, so
 * that the result inherits the provenance of everything consumed while the
 * scope was open. Conservative in the safe direction: a consumption that did
 * not actually flow into the result adds a spurious edge, never a missing
 * one.
 */
struct ProvScope
{
    ReadSetTracker * tracker;
    size_t start = 0;
    bool finished = false;

    ProvScope(ReadSetTracker * tracker)
        : tracker(tracker)
    {
        if (tracker) [[unlikely]]
            start = tracker->provScopeBegin();
    }

    ProvScope(const ProvScope &) = delete;
    ProvScope(ProvScope &&) = delete;

    void finish(const Value & v)
    {
        if (tracker) [[unlikely]]
            tracker->provScopeEnd(start, v);
        finished = true;
    }

    ~ProvScope()
    {
        if (tracker && !finished) [[unlikely]]
            tracker->provScopeAbort();
    }
};

} // namespace nix
