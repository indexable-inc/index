#include "nix/cmd/rust-search.hh"
#include "ixe-search.h"
#include "nix/util/terminal.hh"

namespace nix {

namespace {
struct OwnedString
{
    char * text = nullptr;

    ~OwnedString()
    {
        ixe_string_free(text);
    }

    std::string str() const
    {
        return text ? text : "";
    }
};

std::vector<IxeBytes> views(const std::vector<std::string> & strings)
{
    std::vector<IxeBytes> result;
    result.reserve(strings.size());
    for (auto & text : strings)
        result.push_back({reinterpret_cast<const unsigned char *>(text.data()), text.size()});
    return result;
}
} // namespace

void RustSearchPlan::Deleter::operator()(IxeSearchPlan * plan) const
{
    ixe_search_plan_free(plan);
}

RustSearchPlan::RustSearchPlan(
    const std::vector<std::string> & include, const std::vector<std::string> & exclude, bool json)
{
    auto includeViews = views(include);
    auto excludeViews = views(exclude);
    OwnedString error;
    IxeSearchPlan * result = nullptr;
    if (ixe_search_plan_new(
            includeViews.data(),
            includeViews.size(),
            excludeViews.data(),
            excludeViews.size(),
            json,
            isTTY() && isTTY(getStandardOutput()),
            &result,
            &error.text))
        throw UsageError("%s", error.str());
    plan.reset(result);
}

void RustSearchCatalogue::Deleter::operator()(IxeSearchCatalogue * catalogue) const
{
    ixe_search_catalogue_free(catalogue);
}

RustSearchCatalogue::RustSearchCatalogue(std::string canonical)
    : canonical(std::move(canonical))
{
    OwnedString error;
    IxeSearchCatalogue * result = nullptr;
    if (ixe_search_catalogue_decode(
            reinterpret_cast<const unsigned char *>(this->canonical.data()),
            this->canonical.size(),
            &result,
            &error.text))
        throw Error("%s", error.str());
    catalogue.reset(result);
}

std::string RustSearchPlan::render(const RustSearchCatalogue & catalogue) const
{
    OwnedString output, error;
    if (ixe_search_plan_render(plan.get(), catalogue.catalogue.get(), &output.text, &error.text))
        throw Error("%s", error.str());
    return output.str();
}

} // namespace nix
