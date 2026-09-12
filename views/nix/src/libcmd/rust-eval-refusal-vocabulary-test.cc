///@file
/// Every command-layer refusal token must resolve in the evaluator's ABI
/// vocabulary.
///
/// The two lists are maintained in different languages and cannot be derived
/// from one another: `rust/nix-eval-rs/src/refusal.rs` has an enum, and
/// `src/libexpr/include/nix/expr/rust-eval-refusal.hh` has `constexpr`
/// `string_view`s that C++ cannot
/// enumerate. Nothing but this check makes them agree.
///
/// What it prevents is a typo minting a category that exists in nothing but
/// the header: the refusal would be counted, the journal line would carry it,
/// and the histogram would grow a row whose denominator nobody can compute,
/// because the vocabulary the census reads its denominator from has never
/// heard of it. A row nobody can explain is worse than a missing one.
///
/// It runs as its own tiny executable rather than inside a suite because it
/// needs both sides linked; it lives beside libnixcmd, the one component that
/// links the Rust archive.

#include "nix/expr/rust-eval-refusal.hh"

#include <cstdio>
#include <set>
#include <string>
#include <string_view>

#include "ixe.h"

int main()
{
    std::set<std::string> abi;
    std::set<std::string> abiCommandLayer;
    const size_t count = ixe_refusal_token_count();
    for (size_t i = 0; i < count; ++i) {
        const char * name = ixe_refusal_token_at(i);
        if (!name) {
            std::fprintf(stderr, "ixe_refusal_token_at(%zu) returned null\n", i);
            return 1;
        }
        abi.insert(name);
        // 1 is the command layer, 2 is either. See ixe.h.
        const int raisedBy = ixe_refusal_token_raised_by(i);
        if (raisedBy == 1 || raisedBy == 2)
            abiCommandLayer.insert(name);
    }

    // An empty vocabulary would make every check below vacuously true, which
    // is the failure mode this whole ladder exists to avoid.
    if (abi.size() < 20) {
        std::fprintf(stderr, "the ABI vocabulary has only %zu tokens; it is not populated\n", abi.size());
        return 1;
    }

    int bad = 0;
    for (const auto & token : nix::allCommandRefusalTokens()) {
        if (!abi.contains(std::string(token))) {
            std::fprintf(
                stderr,
                "PHANTOM TOKEN: '%.*s' is used by the command layer and is in no ABI vocabulary. "
                "It would be counted and journalled under a name the census cannot give a "
                "denominator for.\n",
                static_cast<int>(token.size()),
                token.data());
            ++bad;
        }
    }

    // And from the other side: a token the ABI marks as command-layer that
    // this header never lists is one the guard above cannot see, because the
    // header's list is hand-written. Catching the omission here is what stops
    // "add a constant, forget the list" from silently shrinking the check.
    for (const auto & token : abiCommandLayer) {
        bool listed = false;
        for (const auto & known : nix::allCommandRefusalTokens())
            if (known == token)
                listed = true;
        if (!listed) {
            std::fprintf(
                stderr,
                "UNLISTED TOKEN: the ABI says '%s' is raised by the command layer, but "
                "allCommandRefusalTokens() does not list it, so nothing checks it.\n",
                token.c_str());
            ++bad;
        }
    }

    if (bad != 0)
        return 1;
    std::printf(
        "refusal vocabulary: %zu command-layer tokens all resolve in an ABI vocabulary of %zu\n",
        nix::allCommandRefusalTokens().size(),
        abi.size());
    return 0;
}
