#pragma once
///@file

#include "nix/util/error.hh"
#include "nix/util/fmt.hh"
#include "nix/expr/refusal-census.hh"

#include <string_view>
#include <vector>

namespace nix {

/**
 * Stable tokens for the refusals the command layer raises.
 *
 * These are shapes of *invocation* the Rust backend is not wired for --
 * `--apply`, `--xml`, an expression on stdin -- rather than constructs it
 * cannot evaluate. They never reach the evaluator, so the evaluator never
 * raises them, and a census that counted only the evaluator's tokens would
 * miss them entirely while looking clean.
 *
 * The names come from one vocabulary shared with the evaluator, declared in
 * `rust/nix-eval-rs/src/refusal.rs` and enumerable over the C ABI. They are
 * spelled here as constants rather than literals at each throw site so that
 * `theCommandTokensAreInTheAbiVocabulary` can check every one of them against
 * that enumeration: a typo would otherwise mint a category that exists in
 * nothing but this file, and a histogram row nobody can explain is worse than
 * a missing one.
 *
 * This header lives in libexpr rather than beside the commands that raise
 * these refusals, because one of them does not sit there:
 * `EvalState::requireBackendCanServe()` is the catch-all every unwired command
 * falls through, it throws from inside the evaluator, and libexpr cannot
 * include from `src/nix`. "Command layer" describes who *causes* a refusal,
 * not which library the throw compiles into, and the two are worth keeping in
 * one vocabulary whichever side of that line they land on.
 */
namespace refusalTokens {
constexpr std::string_view writeTo = "command-write-to";
constexpr std::string_view xmlOutput = "command-xml-output";
constexpr std::string_view stdinSource = "command-stdin";
constexpr std::string_view installable = "command-installable";
constexpr std::string_view outputSelection = "command-output-selection";
constexpr std::string_view file = "command-file";
constexpr std::string_view unsupported = "command-unsupported";
constexpr std::string_view notADerivation = "command-not-a-derivation";
constexpr std::string_view outputsToInstall = "command-outputs-to-install";
constexpr std::string_view notAnApp = "command-not-an-app";
} // namespace refusalTokens

/**
 * Every command-layer token, for the guard that holds them to the ABI's list.
 *
 * Hand-listed rather than derived, because there is nothing to derive from:
 * these are `constexpr` names, not an enum, and C++ cannot enumerate them.
 * That is exactly why the guard exists -- a token added above and forgotten
 * here would be invisible to it, so `theCommandTokensAreInTheAbiVocabulary`
 * also checks the count against the ABI's own command-layer total, which
 * catches the omission from the other side.
 */
inline const std::vector<std::string_view> & allCommandRefusalTokens()
{
    static const std::vector<std::string_view> tokens{
        refusalTokens::writeTo,
        refusalTokens::xmlOutput,
        refusalTokens::stdinSource,
        refusalTokens::installable,
        refusalTokens::outputSelection,
        refusalTokens::file,
        refusalTokens::unsupported,
        refusalTokens::notADerivation,
        refusalTokens::outputsToInstall,
        refusalTokens::notAnApp,
    };
    return tokens;
}

/**
 * The name a refusal carries when it crossed a boundary without one.
 *
 * A category of its own rather than a guess: a census can then see how much
 * of its population it cannot classify, instead of silently attributing it to
 * whatever token seemed closest. `ixe_eval_expr` has no session and therefore
 * no token to report, so every refusal on that path lands here -- which is a
 * gap worth seeing in the histogram rather than hiding.
 */
constexpr std::string_view unrecordedRefusal = "unrecorded";

/**
 * What `refuse` throws: an `Error` a catch site can tell apart from every
 * other failure, because a refusal is the evaluator declining, and no caller
 * that recovers from ordinary failures (a shell falling back to plain bash)
 * may recover from that -- it would be the C++ evaluator's behaviour served
 * under a setting that promised the Rust one, silently.
 */
MakeError(RustEvalRefusal, Error);

/**
 * Refuse, naming the kind as well as the reason.
 *
 * Counts the refusal and writes the journal line before throwing, because the
 * throw is what ends the process: anything recorded only on the way out is
 * recorded nowhere. The message keeps the `rust-eval unimplemented:` marker
 * the gate and `nixpkgs-frontier.sh` already grep for, so this changes what a
 * census can group by without changing what anything reading the text sees.
 */
template<typename... Args>
[[noreturn]] void refuse(std::string_view token, const std::string & format, const Args &... args)
{
    // `nix::fmt`, the same printf-style formatter `Error` itself uses, so a
    // call site keeps the format string it already had. Not `fmt::format`:
    // that is a different library with `{}` placeholders, and the parameter
    // was also called `fmt`, which shadowed the namespace and made the
    // mistake compile as something even stranger.
    //
    // The detail is formatted once and used twice, so the census and the
    // message can never disagree about what was refused.
    auto detail = nix::fmt(format, args...);
    RefusalCensus::record(token, detail);
    throw RustEvalRefusal("rust-eval unimplemented: %s", detail);
}

/**
 * The same refusal, plus a sentence for the human that the census never sees.
 *
 * `refuse()` formats one string and uses it twice so the count and the message
 * cannot disagree, which is right for as long as the detail is also the whole
 * explanation. It stops being right at the catch-all refusal, where the two
 * readers want opposite things: the census wants the command name and nothing
 * else, because that is the histogram row that says which command to wire up
 * next, while the user wants to be told which backend to re-run with. Glue the
 * sentence onto the front of the detail and every command becomes its own row
 * of prose that nothing can group.
 *
 * So `detail` is still what is counted and still opens the message, and
 * `advice` is appended to the message alone. Push text out of `detail` rather
 * than into it: a census row that is a paragraph is useless, whereas a message
 * that is only an identifier is merely terse.
 */
[[noreturn]] inline void
refuseWithAdvice(std::string_view token, const std::string & detail, const std::string & advice)
{
    RefusalCensus::record(token, detail);
    throw Error("rust-eval unimplemented: %s\n%s", detail, advice);
}

} // namespace nix
