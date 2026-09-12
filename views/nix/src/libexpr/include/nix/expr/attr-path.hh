#pragma once
///@file

#include "nix/expr/eval.hh"

#include <string>

namespace nix {

MakeError(AttrPathNotFound, Error);

std::pair<Value *, PosIdx>
findAlongAttrPath(EvalState & state, const std::string & attrPath, Bindings & autoArgs, Value & vIn);

/**
 * Split an attribute path on unquoted dots, honouring quoted components
 * (`a."b.c"`). The one splitter: the Rust bridge selects with it too.
 */
Strings parseAttrPath(std::string_view s);

/**
 * Render components as `a.b."c d"`, quoting whatever is not an identifier.
 * The one renderer: `AttrPath::to_string` and the Rust bridge's flake-show
 * paths both go through it.
 */
std::string showAttrPath(const std::vector<std::string_view> & components);
std::string showAttrPath(const std::vector<std::string> & components);

struct AttrPath : std::vector<Symbol>
{
    using std::vector<Symbol>::vector;

    static AttrPath parse(EvalState & state, std::string_view s);

    std::string to_string(EvalState & state) const;

    std::vector<SymbolStr> resolve(EvalState & state) const;
};

} // namespace nix
