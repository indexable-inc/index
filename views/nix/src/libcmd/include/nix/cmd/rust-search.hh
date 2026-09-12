#pragma once
#include "nix/cmd/rust-eval-session.hh"

struct IxeSearchPlan;
struct IxeSearchCatalogue;

namespace nix {

/** The decoded Rust catalogue stays alive from cache validation through rendering. */
class RustSearchCatalogue
{
    struct Deleter
    {
        void operator()(IxeSearchCatalogue *) const;
    };

    std::unique_ptr<IxeSearchCatalogue, Deleter> catalogue;
    std::string canonical;
    friend class RustSearchPlan;
public:
    explicit RustSearchCatalogue(std::string canonical);

    const std::string & encode() const
    {
        return canonical;
    }
};

/** Owns compiled Rust regexes and render policy; compile before evaluating. */
class RustSearchPlan
{
    struct Deleter
    {
        void operator()(IxeSearchPlan *) const;
    };

    std::unique_ptr<IxeSearchPlan, Deleter> plan;
public:
    RustSearchPlan(const std::vector<std::string> & include, const std::vector<std::string> & exclude, bool json);
    std::string render(const RustSearchCatalogue & catalogue) const;
};

RustSearchCatalogue rustEvalSearchCatalogue(EvalState & state, const RustEvaluand & evaluand, bool defaultFlakeRoots);
} // namespace nix
