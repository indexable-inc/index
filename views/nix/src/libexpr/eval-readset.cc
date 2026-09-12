#include "nix/expr/eval-readset.hh"
#include "nix/expr/eval.hh"
#include "nix/util/file-system.hh"
#include "nix/util/hash.hh"

#include <nlohmann/json.hpp>

#include <chrono>
#include <ctime>

namespace nix {

/**
 * The tracker of the evaluation currently running on this thread. Phase 1
 * assumes a single evaluation thread: the stack of active entries is a
 * property of one call stack, so a second thread pushing onto it would
 * attribute its reads to whatever the first thread happened to be doing.
 */
static thread_local ReadSetTracker * currentTracker = nullptr;

std::string_view showSourceReadKind(SourceReadKind kind)
{
    switch (kind) {
    case SourceReadKind::contents:
        return "contents";
    case SourceReadKind::listing:
        return "listing";
    case SourceReadKind::metadata:
        return "metadata";
    case SourceReadKind::link:
        return "link";
    case SourceReadKind::subtree:
        return "subtree";
    case SourceReadKind::position:
        return "position";
    }
    unreachable();
}

std::string_view showTrackedEntryKind(TrackedEntryKind kind)
{
    switch (kind) {
    case TrackedEntryKind::root:
        return "root";
    case TrackedEntryKind::import:
        return "import";
    case TrackedEntryKind::derivation:
        return "derivation";
    case TrackedEntryKind::option:
        return "option";
    }
    unreachable();
}

std::string_view showEdgeKind(EdgeKind kind)
{
    switch (kind) {
    case EdgeKind::demand:
        return "demand";
    case EdgeKind::reuse:
        return "reuse";
    case EdgeKind::derivation:
        return "derivation";
    case EdgeKind::value:
        return "value";
    }
    unreachable();
}

/**
 * The part of a fingerprint that says which tree this is, with the part that
 * says which version of it removed.
 *
 * A git fingerprint is the revision, then the flags selecting what the tree
 * contains (`;s` submodules, `;e` export-ignore, `;l` lfs), then for a dirty
 * working tree a trailing `;d=<digest>` of everything differing from the
 * commit. Only that last component is the version: two runs over the same
 * edited tree agree on the revision and on the flags and differ in the digest.
 * Dropping it is what lets those two runs recognise the same tree; keeping the
 * flags is what stops a submodules-on tree pairing with a submodules-off one,
 * which taking the prefix before the first `;` would have done.
 *
 * This is deliberately not what `getFingerprint` returns and must not become
 * it. That value is Nix's own cache key, where carrying the version is the
 * entire point, and `fetch-to-store` and the flake fingerprint both depend on
 * it saying which version.
 */
static std::string_view identityOfFingerprint(std::string_view fp)
{
    if (auto i = fp.rfind(";d="); i != std::string_view::npos)
        return fp.substr(0, i);
    return fp;
}

/**
 * Which view of a tree a record describes. One tree is seen up to three ways
 * in a single evaluation, bare, mounted at the filesystem root, and
 * materialised at a store path, and all three carry the same fingerprint. So
 * the identity alone does not distinguish them, and pairing two runs on it
 * puts one run's store-path view against the other's filesystem-root view,
 * which reads as every input under it having moved.
 *
 * The store path itself cannot be part of the name: it hashes the tree's
 * contents, so it moves with the edit. What survives is that it is one.
 */
static std::string viewOfRoot(std::string_view root)
{
    if (root.empty())
        return "bare";
    if (root == "/")
        return "fsroot";
    if (root == "/nix/store")
        return "store";
    if (root.starts_with("/nix/store/"))
        return "storepath";
    return std::string(root);
}

static uint64_t wallNs()
{
    return std::chrono::duration_cast<std::chrono::nanoseconds>(std::chrono::steady_clock::now().time_since_epoch())
        .count();
}

/**
 * Thread CPU time, which differs from wall clock by the time evaluation
 * spends waiting on the filesystem and on the daemon. Both are recorded
 * because the gap between them is itself a result: it is the part of an
 * evaluation that a faster evaluator would not fix.
 */
static uint64_t cpuNs()
{
#ifdef CLOCK_THREAD_CPUTIME_ID
    struct timespec ts;
    if (clock_gettime(CLOCK_THREAD_CPUTIME_ID, &ts) == 0)
        return uint64_t(ts.tv_sec) * 1000000000ull + uint64_t(ts.tv_nsec);
#endif
    return 0;
}

static void readHook(SourceAccessor & accessor, const CanonPath & path, SourceReadKind kind)
{
    if (currentTracker)
        currentTracker->recordRead(accessor, path, kind);
}

static void
observedHook(SourceAccessor & accessor, const CanonPath & path, SourceReadKind kind, std::string_view observed)
{
    if (currentTracker)
        currentTracker->recordObserved(accessor, path, kind, observed);
}

ReadSetTracker::ReadSetTracker(EvalState & state, std::optional<std::filesystem::path> traceFile, bool hashContents)
    : state(state)
    , hashContents(hashContents)
    , fd([&]() {
        if (!traceFile)
            return AutoCloseFD{};
        auto f = openNewFileForWrite(*traceFile, 0660, {.truncateExisting = true, .followSymlinksOnTruncate = true});
        if (!f)
            throw SysError("opening read-set trace file %s", PathFmt(*traceFile));
        return f;
    }())
    , startWallNs(wallNs())
    , startCpuNs(cpuNs())
{
    if (currentTracker)
        throw Error("a read-set tracker is already active on this thread");
    currentTracker = this;
    sourceReadHook.store(readHook, std::memory_order_relaxed);
    if (hashContents)
        sourceObservedHook.store(observedHook, std::memory_order_relaxed);

    stack.push_back(
        Entry{
            .id = nextEntryId++,
            .parent = 0,
            .kind = TrackedEntryKind::root,
            .key = "<root>",
            .accessor = 0,
            .depth = 0,
            .wallStartNs = startWallNs,
            .cpuStartNs = cpuNs(),
            .valuesStart = state.mem.getStats().nrValues.load(),
        });

    /* Files parsed under an earlier tracker are served from the parse cache
       and never pass through `EvalState::parse` again, so their literals
       would be invisible to this request without a replay. */
    for (auto & [path, literals] : state.parsedStringLiterals)
        registerLiterals(path, literals);
}

ReadSetTracker::~ReadSetTracker()
{
    try {
        /* Pop whatever is left, innermost first, so an evaluation that threw
           still emits its entries rather than only the ones that returned. */
        while (!stack.empty())
            pop();

        auto summary = nlohmann::json{
            {"t", "summary"},
            {"entries", nextEntryId},
            {"inputs", inputTable.size()},
            {"reads", nrReads},
            {"wall_ns", wallNs() - startWallNs},
            {"cpu_ns", cpuNs() - startCpuNs},
            {"first_non_store_read_ns", firstNonStoreReadNs},
            {"first_non_store_read_path", firstNonStoreReadPath},
            {"hash_contents", hashContents},
            {"edges", nrEdges},
            {"entries_with_edges", nrEntriesWithEdges},
            {"prov_registered", nrProvRegistered},
            {"prov_hits", nrProvHits},
            {"prov_selects", nrProvSelects},
            {"prov_sets", provSets.size()},
        };
        for (size_t i = 0; i < kindCounts.size(); ++i)
            summary["kind_" + std::string(showTrackedEntryKind(TrackedEntryKind(i)))] = kindCounts[i];
        /* Recorded rather than emitted, so these sum to at least `edges`: a
           consumer that demands one producer twice is one edge in the trace. */
        for (size_t i = 0; i < edgeKindCounts.size(); ++i)
            summary["edge_" + std::string(showEdgeKind(EdgeKind(i)))] = edgeKindCounts[i];
        write(summary.dump());

        if (!buffer.empty() && fd)
            writeFull(fd.get(), buffer);
    } catch (...) {
        ignoreExceptionInDestructor();
    }

    sourceReadHook.store(nullptr, std::memory_order_relaxed);
    sourceObservedHook.store(nullptr, std::memory_order_relaxed);
    currentTracker = nullptr;
}

void ReadSetTracker::write(std::string_view line)
{
    if (!fd)
        return;
    buffer += line;
    buffer += '\n';
    if (buffer.size() >= 1024 * 1024) {
        writeFull(fd.get(), buffer);
        buffer.clear();
    }
}

void ReadSetTracker::push(TrackedEntryKind kind, std::string key, size_t accessor)
{
    auto parent = stack.empty() ? 0 : stack.back().id;
    auto depth = stack.empty() ? 0 : stack.back().depth + 1;
    /* Whatever was innermost when a boundary is entered is what demanded it,
       so the edge belongs to the outer entry and has to be recorded before the
       new one becomes innermost. */
    if (!stack.empty())
        addEdge(nextEntryId, EdgeKind::demand);
    stack.push_back(
        Entry{
            .id = nextEntryId++,
            .parent = parent,
            .kind = kind,
            .key = std::move(key),
            .accessor = accessor,
            .depth = depth,
            .wallStartNs = wallNs(),
            .cpuStartNs = cpuNs(),
            .valuesStart = state.mem.getStats().nrValues.load(),
        });
}

void ReadSetTracker::setCurrentKey(std::string key)
{
    if (!stack.empty())
        stack.back().key = std::move(key);
}

void ReadSetTracker::pop()
{
    assert(!stack.empty());
    auto entry = std::move(stack.back());
    stack.pop_back();

    auto wall = wallNs() - entry.wallStartNs;
    auto cpu = cpuNs() - entry.cpuStartNs;
    auto values = state.mem.getStats().nrValues.load() - entry.valuesStart;

    if (!stack.empty()) {
        stack.back().wallChildNs += wall;
        stack.back().cpuChildNs += cpu;
        stack.back().valuesChild += values;
    }

    kindCounts[static_cast<size_t>(entry.kind)]++;
    emit(entry, wall, cpu, values);
}

void ReadSetTracker::emit(const Entry & entry, uint64_t wall, uint64_t cpu, uint64_t values)
{
    /* Deduplicate. Recording appends, because a set insertion per read is
       more expensive than a sort per entry when entries are small, which
       they are: the median entry reads a handful of files. */
    auto inputs = entry.inputs;
    std::sort(inputs.begin(), inputs.end());
    inputs.erase(std::unique(inputs.begin(), inputs.end()), inputs.end());

    auto edges = entry.edges;
    std::sort(edges.begin(), edges.end());
    edges.erase(std::unique(edges.begin(), edges.end()), edges.end());
    nrEdges += edges.size();
    if (!edges.empty())
        nrEntriesWithEdges++;

    auto j = nlohmann::json{
        {"t", "entry"},
        {"id", entry.id},
        {"parent", entry.parent},
        {"kind", showTrackedEntryKind(entry.kind)},
        {"key", entry.key},
        {"acc", entry.accessor},
        {"depth", entry.depth},
        {"start_ns", entry.wallStartNs - startWallNs},
        {"wall_ns", wall},
        {"wall_excl_ns", wall > entry.wallChildNs ? wall - entry.wallChildNs : 0},
        {"cpu_ns", cpu},
        {"cpu_excl_ns", cpu > entry.cpuChildNs ? cpu - entry.cpuChildNs : 0},
        {"values", values},
        {"values_excl", values > entry.valuesChild ? values - entry.valuesChild : 0},
        {"inputs", inputs},
        {"edges", edges},
    };
    if (!entry.produced.empty())
        j["produced"] = entry.produced;
    write(j.dump());
}

uint32_t
ReadSetTracker::treeIdFor(SourceAccessor * accessor, std::string_view path, std::string_view rel, std::string_view fp)
{
    /* The root the tree occupies: the part of the path that `rel` is relative
       to. That root carries the tree's version, which is exactly why it is used
       to look the id up and not as the id itself. */
    std::string root(path);
    if (!rel.empty() && rel != "/" && root.ends_with(rel))
        root.resize(root.size() - rel.size());

    std::string key;
    key += std::to_string(accessor ? accessor->number : 0);
    key += '\0';
    key += root;

    if (auto i = treeIds.find(key); i != treeIds.end())
        return i->second;

    auto id = uint32_t(treeIds.size());
    treeIds.emplace(key, id);

    /* What names this tree across two runs. The fingerprint without its
       version, if there is one; otherwise the accessor's own display, which is
       a constant for the filesystem root, «flakes-internal», <nix> and every
       «input» accessor, and which phase 1 already recorded and then did not
       use. An accessor offering neither has told us nothing that survives a
       run, and the trace says so rather than letting an analysis pair it by
       position and call the result a measurement. */
    std::string display = accessor ? accessor->displayPrefix + accessor->displaySuffix : "";
    std::string identity;
    if (!fp.empty())
        identity = std::string(identityOfFingerprint(fp));
    else if (!display.empty())
        identity = display;
    else if (accessor)
        identity = std::string(accessor->identityClass(CanonPath(path)));

    auto j = nlohmann::json{
        {"t", "tree"},
        {"id", id},
        {"acc", accessor ? accessor->number : 0},
        {"root", root},
        {"identity", identity},
        {"view", viewOfRoot(root)},
    };
    if (identity.empty())
        j["anonymous"] = true;
    if (accessor)
        j["display"] = display;
    if (!fp.empty())
        j["fp"] = fp;
    write(j.dump());
    return id;
}

uint32_t ReadSetTracker::internInput(
    SourceAccessor * accessor, size_t accessorNumber, std::string_view path, std::string_view kind)
{
    /* What the accessor says this path is: which tree answers for it, and
       where in that tree it sits. The tree-relative part is the half that
       survives an edit; the fingerprint is the half that does not, so the
       fingerprint identifies the tree's version and never names the input. */
    std::string rel(path);
    std::string fp;
    if (accessor) {
        try {
            auto [rel_, fp_] = accessor->getFingerprint(CanonPath(path));
            rel = rel_.abs();
            if (fp_)
                fp = *fp_;
        } catch (...) {
            /* A path the accessor cannot describe is still an input, named by
               the only thing available. */
        }
    }
    /* A tree attribute or a store query has no accessor and so is in no tree.
       Phase 1 handed those to `treeIdFor` anyway, which dutifully manufactured
       a tree record with no root, no fingerprint and no display, holding 27
       inputs whose `rel` is a serialised flake input rather than a path. It
       was never a tree and it is one of the records that forced an analysis to
       pair by position. */
    auto tree = accessor ? std::optional<uint32_t>(treeIdFor(accessor, path, rel, fp)) : std::nullopt;

    /* One string as the intern key rather than a tuple, so that the map has
       no allocation per lookup beyond the key itself. Keyed on the tree and
       the path within it, so that an edit anywhere in a tree does not rename
       every input under it: naming an input by its absolute path made 11,535
       of one entry's 22,933 inputs different for a one character edit, none of
       whose observed answers had changed. */
    std::string key;
    key.reserve(rel.size() + kind.size() + 24);
    key += tree ? std::to_string(*tree) : "-";
    key += '\0';
    key += kind;
    key += '\0';
    key += rel;

    if (auto i = inputTable.find(key); i != inputTable.end())
        return i->second;

    auto id = uint32_t(inputTable.size());
    inputTable.emplace(key, id);

    auto in = nlohmann::json{
        {"t", "in"},
        {"id", id},
        {"kind", kind},
        {"rel", rel},
        {"path", path},
        {"first_ns", wallNs() - startWallNs},
    };
    if (tree)
        in["tree"] = *tree;
    write(in.dump());
    return id;
}

void ReadSetTracker::addInput(uint32_t id)
{
    if (stack.empty())
        return;
    auto & entry = stack.back();
    /* The same path read twice in a row is the common case (a stat then a
       read, or a loop over one file), and this makes it free. */
    if (entry.lastInput == id)
        return;
    entry.lastInput = id;
    entry.inputs.push_back(id);
}

void ReadSetTracker::addEdge(uint64_t producer, EdgeKind kind)
{
    if (stack.empty())
        return;
    auto & entry = stack.back();
    /* An entry that demands itself is a boundary re entered during its own
       evaluation. That says nothing about which values flowed where, and a
       self edge would make every propagation over this graph circular. */
    if (producer == entry.id)
        return;
    /* The same producer demanded twice in a row is the common case, as when
       one file imports another in a loop, and this makes it free. */
    if (entry.lastEdge == producer)
        return;
    entry.lastEdge = producer;
    entry.edges.push_back(producer);
    edgeKindCounts[static_cast<size_t>(kind)]++;
}

void ReadSetTracker::noteProduces(const void * value)
{
    if (stack.empty() || !value)
        return;
    /* Assigned rather than inserted: after `resetFileCache` the same address
       can be produced again by a new entry, and the later producer is the one
       a demand after that point actually got its value from. */
    entryByValue.insert_or_assign(value, stack.back().id);
}

void ReadSetTracker::noteDemand(const void * value)
{
    if (stack.empty() || !value)
        return;
    if (auto i = entryByValue.find(value); i != entryByValue.end())
        addEdge(i->second, EdgeKind::reuse);
}

const void * ReadSetTracker::provKey(const Value & v)
{
    /* Only types whose payload is a shared allocation have an identity that
       survives a struct copy. An integer, boolean, float or small list is
       carried inline in the 16 bytes and every copy is indistinguishable
       from every other value with the same bits, so those cannot carry
       provenance; that is the residual escape class the commit message and
       the summary both name. */
    auto t = v.type();
    if (t == nString)
        return &v.string_data();
    if (t == nAttrs)
        return v.attrs();
    return nullptr;
}

uint32_t ReadSetTracker::provInternSet(const std::vector<int64_t> & set)
{
    auto [it, inserted] = provSetIds.try_emplace(set, uint32_t(provSets.size()));
    if (inserted)
        provSets.push_back(it->first);
    return it->second;
}

void ReadSetTracker::provRegister(const Value & v, int64_t source)
{
    auto key = provKey(v);
    if (!key)
        return;
    nrProvRegistered++;
    provByPayload.insert_or_assign(key, provInternSet({source}));
}

void ReadSetTracker::provRegisterInnermost(const Value & v)
{
    if (!stack.empty())
        provRegister(v, int64_t(stack.back().id));
}

void ReadSetTracker::provConsumeSet(uint32_t setId)
{
    nrProvHits++;
    for (auto source : provSets[setId]) {
        if (source >= 0)
            addEdge(uint64_t(source), EdgeKind::value);
        else
            addInput(uint32_t(-(source + 1)));
        if (provScopeDepth > 0)
            provHitLog.emplace_back(provScopeDepth, source);
    }
}

void ReadSetTracker::registerLiterals(const SourcePath & path, const std::vector<Value *> & literals)
{
    if (literals.empty())
        return;
    /* The same intern key a contents read of this file produces, so the
       provenance names an input the dirtiness walk already knows how to
       verify: the file's content hash. */
    auto id = internInput(&*path.accessor, path.accessor->number, path.path.abs(), "contents");
    auto source = -int64_t(id) - 1;
    for (auto * v : literals)
        provRegister(*v, source);
}

void ReadSetTracker::provRegisterFileValue(const SourcePath & path, const Value & v)
{
    auto id = internInput(&*path.accessor, path.accessor->number, path.path.abs(), "contents");
    provRegister(v, -int64_t(id) - 1);
}

void ReadSetTracker::provConsume(const Value & v)
{
    auto key = provKey(v);
    if (!key)
        return;
    if (auto i = provByPayload.find(key); i != provByPayload.end())
        provConsumeSet(i->second);
}

void ReadSetTracker::provSelect(const Value & base, const Value & result)
{
    nrProvSelects++;
    /* A result that already carries provenance answers the whole question:
       reading it is a consumption, and its sources are at least as precise
       as the container's. */
    auto rk = provKey(result);
    if (rk) {
        if (auto i = provByPayload.find(rk); i != provByPayload.end()) {
            provConsumeSet(i->second);
            return;
        }
    }
    auto bk = provKey(base);
    if (!bk)
        return;
    auto i = provByPayload.find(bk);
    if (i == provByPayload.end())
        return;
    /* Reading any attribute of a registered container depends on the
       container's producer, whatever the attribute's type. For a result
       that can carry provenance, the result inherits the container's
       sources, which is how a registration on an import's top-level value
       reaches a leaf three selects down. */
    provConsumeSet(i->second);
    if (rk) {
        nrProvRegistered++;
        provByPayload.emplace(rk, i->second);
    }
}

size_t ReadSetTracker::provScopeBegin()
{
    provScopeDepth++;
    return provHitLog.size();
}

void ReadSetTracker::provScopeAbort()
{
    provScopeDepth--;
    if (provScopeDepth == 0)
        provHitLog.clear();
}

void ReadSetTracker::provScopeEnd(size_t start, const Value & result)
{
    if (provHitLog.size() > start) {
        /* Only the hits made at this scope's own depth belong to this
           result. A hit inside a nested scope was already registered onto
           that scope's result, and reading that result here logs it again
           at this depth; unioning the nested hits too made every outer
           concatenation carry the sources of its entire evaluation subtree,
           which is where a 50% evaluation slowdown lived. */
        provScratch.clear();
        for (auto i = provHitLog.begin() + start; i != provHitLog.end(); ++i)
            if (i->first == provScopeDepth)
                provScratch.push_back(i->second);
        if (!provScratch.empty()) {
            std::sort(provScratch.begin(), provScratch.end());
            provScratch.erase(std::unique(provScratch.begin(), provScratch.end()), provScratch.end());
            auto setId = [&]() {
                auto it = provSetIds.find(provScratch);
                if (it != provSetIds.end())
                    return it->second;
                return provInternSet(provScratch);
            }();
            auto registerOne = [&](const Value & out) {
                auto key = provKey(out);
                if (!key)
                    return;
                nrProvRegistered++;
                provByPayload.insert_or_assign(key, setId);
            };
            /* `match` and `split` return lists of fresh strings; the list
               itself has no stable payload, so each element carries the set
               instead. */
            if (result.type() == nList)
                for (auto * elem : result.listView()) {
                    if (elem && !elem->isThunk())
                        registerOne(*elem);
                }
            else
                registerOne(result);
        }
    }
    provScopeAbort();
}

void ReadSetTracker::noteDerivationProduced(std::string_view drvPath)
{
    if (stack.empty())
        return;
    stack.back().produced = drvPath;
    entryByDrvPath.insert_or_assign(std::string(drvPath), stack.back().id);
}

void ReadSetTracker::noteDerivationDemand(std::string_view drvPath)
{
    if (stack.empty())
        return;
    /* A derivation whose inputs include one this evaluation did not build is
       an input from the store rather than from an entry, and `recordStoreQuery`
       is where that belongs. There is no edge to draw. */
    if (auto i = entryByDrvPath.find(std::string(drvPath)); i != entryByDrvPath.end())
        addEdge(i->second, EdgeKind::derivation);
}

/**
 * Is this a read of the store directory itself rather than of anything in a
 * tree? A `stat` of `/nix` answers differently the moment anything is added to
 * the store, so recording it gives 217 entries an input that can never
 * validate against a store that was written to, which is every store. The
 * store's contents are content-addressed and are already tracked per path.
 */
static bool isStoreRootRead(std::string_view abs, SourceReadKind kind)
{
    return (kind == SourceReadKind::metadata || kind == SourceReadKind::listing)
           && (abs == "/nix" || abs == "/nix/store");
}

void ReadSetTracker::recordRead(SourceAccessor & accessor, const CanonPath & path, SourceReadKind kind)
{
    nrReads++;

    auto abs = path.abs();

    if (isStoreRootRead(abs, kind))
        return;

    if (firstNonStoreReadNs == 0 && !abs.starts_with("/nix/store/")) {
        firstNonStoreReadNs = wallNs() - startWallNs;
        firstNonStoreReadPath = abs;
    }

    addInput(internInput(&accessor, accessor.number, abs, showSourceReadKind(kind)));
}

void ReadSetTracker::recordObserved(
    SourceAccessor & accessor, const CanonPath & path, SourceReadKind kind, std::string_view observed)
{
    if (!hashContents)
        return;

    if (isStoreRootRead(path.abs(), kind))
        return;

    auto id = internInput(&accessor, accessor.number, path.abs(), showSourceReadKind(kind));
    addInput(id);

    /* A short answer goes in as itself, because "directory" or "absent" is
       worth reading in a trace; anything longer is hashed. File contents are
       always hashed, however short, so that a trace never carries the bytes of
       a source file. */
    auto value = observed.size() <= 32 && kind != SourceReadKind::contents
                     ? std::string(observed)
                     : hashString(HashAlgorithm::SHA256, observed).to_string(HashFormat::Base16, false).substr(0, 32);

    /* One value per input. A second, different answer for the same input
       within one evaluation is written down rather than collapsed, because a
       path whose answer changed mid-evaluation is a fact worth seeing. */
    auto [i, inserted] = observedInputs.try_emplace(id, value);
    if (!inserted) {
        if (i->second == value)
            return;
        i->second = value;
    }

    write(
        nlohmann::json{
            {"t", "obs"},
            {"id", id},
            {"v", value},
            {"size", observed.size()},
            {"changed_during_eval", !inserted},
        }
            .dump());
}

void ReadSetTracker::recordPosition(const Pos & pos, bool withLineColumn)
{
    if (auto path = std::get_if<SourcePath>(&pos.origin)) {
        auto abs = path->path.abs();
        if (withLineColumn) {
            abs += ":";
            abs += std::to_string(pos.line);
            abs += ":";
            abs += std::to_string(pos.column);
        }
        addInput(internInput(&*path->accessor, path->accessor->number, abs, "position"));
        nrReads++;
    }
}

void ReadSetTracker::recordTreeAttr(std::string_view treeId, std::string_view attr, Value * value)
{
    /* Named by the tree and the attribute, so that two runs compare the same
       thing, and valued by what was read, so that a moved revision shows as a
       changed input rather than as a renamed one. Reusing `internInput` with a
       null accessor puts it in the same table as every other input, which is
       what lets one query over one trace answer invalidation with the revision
       counted and with it excluded. */
    std::string name(treeId);
    name += '#';
    name += attr;
    auto id = internInput(nullptr, 0, name, "tree-attr");
    addInput(id);
    nrReads++;

    /* The value is already forced: this runs from the primop that returns it. */
    std::string observed;
    if (value->type() == nString)
        observed = value->string_view();
    else if (value->type() == nInt)
        observed = std::to_string(value->integer().value);
    else
        observed = "«unprintable»";

    /* The attribute's value now flows onward as an ordinary string, read by
       entries whose own read sets never mention the tree: a `rev` that
       lands in `nixos-version` crosses into that derivation as a value. The
       payload registration is what lets the consumption hooks attribute
       that flow back to this input. */
    provRegister(*value, -int64_t(id) - 1);
    /* An integer attribute (`lastModified`, `revCount`) has no payload to
       register, but its read commonly happens inside the very string
       operation that renders it (`toString self.lastModified` forces the
       thunk mid-coercion), so logging the input into the open scope hands
       the rendered string the provenance the integer cannot carry. */
    if (provScopeDepth > 0)
        provHitLog.emplace_back(provScopeDepth, -int64_t(id) - 1);

    auto [i, inserted] = observedInputs.try_emplace(id, observed);
    if (!inserted) {
        if (i->second == observed)
            return;
        i->second = observed;
    }

    write(
        nlohmann::json{
            {"t", "obs"},
            {"id", id},
            {"v", observed},
            {"size", observed.size()},
            {"changed_during_eval", !inserted},
        }
            .dump());
}

void ReadSetTracker::recordStoreQuery(std::string_view storePath)
{
    addInput(internInput(nullptr, 0, storePath, "store"));
    nrReads++;
}

} // namespace nix
