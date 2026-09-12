#pragma once
///@file

#include <cstdint>
#include <map>
#include <string>
#include <string_view>
#include <vector>

namespace nix {

/**
 * Counts of refusals by stable token, for the process.
 *
 * # Why the journal line and not this is the production census
 *
 * In production a refusal is *fatal*: the command throws and the process
 * exits. So any census that depends on orderly shutdown structurally
 * under-counts the very thing it measures -- and it under-counts silently,
 * reading as "no refusals" exactly when a run died on one. `nix-instantiate`
 * is the case that matters, since `nix-instantiate --eval` is the only command
 * serving the Rust backend today, and its `maybePrintStats()` sits on the
 * success path after the throw.
 *
 * The journal line `recordRefusal` emits does not depend on shutdown. It is
 * written at the moment of refusal and rides journald into ClickHouse, a
 * channel that survives the process. So the journal is the production census,
 * and these counters are the local-debugging view plus -- once shadow mode
 * lands -- the shadow view, where a refusal is caught rather than fatal and
 * the process lives to report it.
 *
 * # Why process-wide rather than per-`EvalState`
 *
 * `nrRustEvals` is `EvalState` members and this deliberately
 * is not. Four of the command layer's refusal sites throw before
 * `getEvalState()` is ever reached, so a per-`EvalState` counter cannot see
 * them at all. The invariant worth keeping from that mechanism is *one
 * accounting path feeding one derivation in the stats block*, not the storage
 * location; when the two conflict, the mechanism yields to the invariant.
 */
struct RefusalCensus
{
    /**
     * Record one refusal: count it, and emit the journal line.
     *
     * `token` must be one of the stable names; the command layer's are checked
     * against the evaluator's ABI vocabulary where that is linked, so a typo
     * cannot mint a category that exists in nothing but this map.
     */
    static void record(std::string_view token, std::string_view detail);

    /**
     * A snapshot of the counts so far, for the stats block. Tokens that have
     * not been seen are absent; the caller supplies the denominator, because
     * only it knows the full vocabulary.
     */
    static std::map<std::string, uint64_t> snapshot();

    /**
     * Total refusals recorded, so a reader can tell "no refusals" from "this
     * build counts none of them".
     */
    static uint64_t total();

    /**
     * The token of the most recent refusal, or empty if there has been none.
     *
     * For shadow mode, which has to say *whether* the Rust arm refused and
     * *which* refusal it was, and can do neither from the exception: a
     * refusal arrives as a plain `Error` whose only marker is the words
     * "rust-eval unimplemented" in its message. Reading the count before and
     * after the call and then asking this is exact, and it stays exact if
     * somebody rewords the message -- which is the entire reason tokens exist.
     */
    static std::string lastToken();

    /**
     * The full token vocabulary, so the histogram has a denominator.
     *
     * Registered by whoever links the evaluator's C ABI rather than listed
     * here, because the vocabulary is defined in
     * `rust/nix-eval-rs/src/refusal.rs` and enumerable over that ABI, and a
     * second hand-written copy in libexpr would drift the moment either side
     * gained a token. libexpr cannot include `ixe.h`; libcmd can (it links
     * the archive), and does it once at load.
     */
    static void setVocabulary(std::vector<std::string> tokens);

    /**
     * The registered vocabulary, or empty when nothing registered one --
     * which is what a build without the Rust evaluator looks like.
     *
     * Empty is reported as such rather than being filled in from the tokens
     * that happened to occur. "These are all the tokens and four of them are
     * zero" and "these are the four tokens I saw" are different claims, and
     * the flip criterion is the first one.
     */
    static const std::vector<std::string> & vocabulary();
};

/**
 * The name of the command this process is running, for a refusal detail the
 * census can group on.
 *
 * A refusal raised inside the evaluator cannot see which command asked for the
 * evaluation -- `nix build` and `nix flake metadata` construct `EvalState` the
 * same way -- and the catch-all refusal is the one where the command is the
 * whole finding, because it names the next thing to wire up. Without it,
 * `command-unsupported` is one histogram row covering the entire unserved
 * surface: enough to know there is work, not enough to order it.
 *
 * `get()` falls back to `argv[0]`'s basename, which `handleExceptions()` has
 * already recorded by the time anything evaluates, so `nix-build` and
 * `nix-instantiate` name themselves with nothing plumbed at all. Only the
 * `nix` multi-command has to call `set()`, because bare "nix" is exactly the
 * row this exists to split.
 */
struct RefusingCommand
{
    /**
     * Name the command, e.g. `nix flake metadata`. Called once, before
     * anything can evaluate; nothing here expects a second caller.
     */
    static void set(std::string_view name);

    /**
     * The name, or `argv[0]`'s basename if nobody set one.
     */
    static std::string get();
};

} // namespace nix
