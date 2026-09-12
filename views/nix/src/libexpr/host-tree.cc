#include "nix/expr/eval.hh"
#include "nix/expr/eval-inline.hh"
#include "nix/expr/eval-readset.hh"
#include "nix/expr/json-to-value.hh"
#include "nix/fetchers/attrs.hh"
#include "nix/fetchers/fetchers.hh"
#include "nix/store/store-api.hh"

#include <ctime>
#include <iomanip>
#include <nlohmann/json.hpp>

namespace nix {

/**
 * The version-bearing attributes of a fetched tree: what a flake means by
 * `self.rev` and friends. These describe which revision of a tree was used
 * rather than anything inside it, so no file read can stand in for them, which
 * makes them the one input class a read set over the filesystem cannot see. An
 * evaluation that embeds `self.rev` in a derivation and a cache that validates
 * on file inputs alone therefore disagree, and the cache wins, serving a
 * derivation built from the previous commit.
 *
 * Recording them at the point they are created would attribute every one to
 * whichever boundary happened to be resolving flakes, so instead each is a
 * thunk that records the read when something forces it. That keeps the
 * attribution where it belongs: an entry that never looks at the revision does
 * not acquire it as an input, which is what makes "how deep does the revision
 * reach" a question the trace can answer.
 */
static void allocRecordedTreeAttr(
    EvalState & state, BindingsBuilder & attrs, std::string_view name, std::string_view treeId, Value * value)
{
    if (!state.readSetTracker) [[likely]] {
        attrs.insert(state.symbols.create(name), value);
        return;
    }

    /* One primop per attribute, holding the name so that the recorded input
       says which of them was read. The `fun` in `PrimOp::impl` wraps a
       `std::function`, so it can carry that name; the primop outlives the
       thunk because it is allocated with the same lifetime as the evaluator. */
    auto * primOp = new PrimOp{
        .name = "recordedTreeAttr",
        .arity = 1,
        .addTrace = false,
        .impl =
            [attrName = std::string(name),
             tree = std::string(treeId)](EvalState & state, const PosIdx pos, Value ** args, Value & v) {
                if (state.readSetTracker)
                    state.readSetTracker->recordTreeAttr(tree, attrName, args[0]);
                v = *args[0];
            },
    };
    auto * primOpValue = state.allocValue();
    primOpValue->mkPrimOp(primOp);
    auto * thunk = state.allocValue();
    thunk->mkApp(primOpValue, value);
    attrs.insert(state.symbols.create(name), thunk);
}

void emitTreeAttrs(
    EvalState & state,
    const StorePath & storePath,
    const fetchers::Input & input,
    Value & v,
    bool emptyRevCountFallback,
    bool forceDirty)
{
    auto attrs = state.buildBindings(100);

    state.mkStorePathString(storePath, attrs.alloc(state.s.outPath));

    // FIXME: support arbitrary input attributes.

    /* Which tree these version attributes describe, with the version stripped
       out of the name. `input.to_string()` carries `rev` and `narHash`, so
       using it would make the same attribute of the same tree a differently
       named input at every commit, and a moved revision would be detected as a
       renamed input rather than as a changed value. That is the same defect
       this instrumentation exists to remove from file inputs, so it does not
       get to reappear here. */
    auto treeId = [&] {
        auto versionless = input.attrs;
        for (auto & key : {"rev", "narHash", "treeHash", "lastModified", "lastModifiedDate", "revCount"})
            versionless.erase(key);
        return fetchers::attrsToJSON(versionless).dump();
    }();

    auto recorded = [&](std::string_view name, auto && build) {
        auto * value = state.allocValue();
        build(*value);
        allocRecordedTreeAttr(state, attrs, name, treeId, value);
    };
    auto recordedString = [&](std::string_view name, std::string_view s) {
        recorded(name, [&](Value & v) { v.mkString(s, state.mem); });
    };
    auto recordedInt = [&](std::string_view name, int64_t i) { recorded(name, [&](Value & v) { v.mkInt(i); }); };

    if (auto narHash = input.getNarHash())
        recordedString("narHash", narHash->to_string(HashFormat::SRI, true));

    if (auto treeHash = input.getTreeHash())
        recordedString("treeHash", treeHash->to_string(HashFormat::SRI, true));

    if (input.getType() == "git")
        attrs.alloc("submodules").mkBool(fetchers::maybeGetBoolAttr(input.attrs, "submodules").value_or(false));

    if (!forceDirty) {

        /* Every tree that reaches here has a revision. A bare Git checkout
           is pinned to the commit it has checked out and a checkout with
           uncommitted changes is refused outright (`git.cc`,
           `pinCheckedOutCommit`), so the empty sha1 that
           `builtins.fetchGit` used to report for a dirty repository names
           a state no fetch can produce any more. */
        if (auto rev = input.getRev()) {
            recordedString("rev", rev->gitRev());
            recordedString("shortRev", rev->gitShortRev());
        }

        /* `revCount` is still optional, because a shallow fetch does not
           compute one (`git.cc` skips it under `shallow = true`), and
           `builtins.fetchGit` has always answered with a set that has the
           attribute. It keeps reporting 0 there rather than dropping it. */
        if (auto revCount = input.getRevCount())
            recordedInt("revCount", *revCount);
        else if (emptyRevCountFallback)
            recordedInt("revCount", 0);
    }

    if (auto lastModified = input.getLastModified()) {
        recordedInt("lastModified", *lastModified);
        recordedString("lastModifiedDate", fmt("%s", std::put_time(std::gmtime(&*lastModified), "%Y%m%d%H%M%S")));
    }

    /* The history is memoised in the fetcher cache keyed by the locked
       revision, so this is a lookup, not a fetch, on the happy path. On
       a miss (the source tree was substituted by narHash, or the
       fetcher cache was deleted) the scheme re-fetches the repository
       to recompute it. It is exposed only here (never via the input
       attributes), so it cannot end up in lock files. */
    if (auto history = input.getHistoryJson(state.fetchSettings, *state.store))
        parseJSON(state, *history, attrs.alloc("history"));

    v.mkAttrs(attrs);
}

} // namespace nix
