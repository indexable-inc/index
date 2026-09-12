#include "nix/expr/attr-path.hh"
#include "nix/expr/eval-inline.hh"
#include "nix/expr/print.hh"
#include "nix/util/util.hh"

#include <sstream>

namespace nix {

Strings parseAttrPath(std::string_view s)
{
    Strings res;
    std::string cur;
    auto i = s.begin();
    while (i != s.end()) {
        if (*i == '.') {
            res.push_back(cur);
            cur.clear();
        } else if (*i == '"') {
            ++i;
            while (1) {
                if (i == s.end())
                    throw ParseError("missing closing quote in selection path '%1%'", s);
                if (*i == '"')
                    break;
                cur.push_back(*i++);
            }
        } else
            cur.push_back(*i);
        ++i;
    }
    if (!cur.empty())
        res.push_back(cur);
    return res;
}

AttrPath AttrPath::parse(EvalState & state, std::string_view s)
{
    AttrPath res;
    for (auto & a : parseAttrPath(s))
        res.push_back(state.symbols.create(a));
    return res;
}

std::string showAttrPath(const std::vector<std::string_view> & components)
{
    std::ostringstream out;
    for (const auto & [index, component] : enumerate(components)) {
        if (index != 0)
            out << '.';
        printIdentifier(out, component);
    }
    return out.str();
}

std::string showAttrPath(const std::vector<std::string> & components)
{
    return showAttrPath(std::vector<std::string_view>(components.begin(), components.end()));
}

std::string AttrPath::to_string(EvalState & state) const
{
    auto resolved = state.symbols.resolve({*this});
    return showAttrPath(std::vector<std::string_view>(resolved.begin(), resolved.end()));
}

std::vector<SymbolStr> AttrPath::resolve(EvalState & state) const
{
    return state.symbols.resolve({*this});
}

std::pair<Value *, PosIdx>
findAlongAttrPath(EvalState & state, const std::string & attrPath, Bindings & autoArgs, Value & vIn)
{
    Strings tokens = parseAttrPath(attrPath);

    Value * v = &vIn;
    PosIdx pos = noPos;

    for (auto & attr : tokens) {

        /* Is i an index (integer) or a normal attribute name? */
        auto attrIndex = string2Int<unsigned int>(attr);

        /* Evaluate the expression. */
        Value * vNew = state.allocValue();
        state.autoCallFunction(autoArgs, *v, *vNew);
        v = vNew;
        state.forceValue(*v, noPos);

        /* It should evaluate to either a set or an expression,
           according to what is specified in the attrPath. */

        if (!attrIndex) {

            if (v->type() != nAttrs)
                state
                    .error<TypeError>(
                        "the expression selected by the selection path '%1%' should be a set but is %2%",
                        attrPath,
                        showType(*v))
                    .debugThrow();
            if (attr.empty())
                throw Error("empty attribute name in selection path '%1%'", attrPath);

            auto a = v->attrs()->get(state.symbols.create(attr));
            if (!a) {
                StringSet attrNames;
                for (auto & attr : *v->attrs())
                    attrNames.insert(std::string(state.symbols[attr.name]));

                auto suggestions = Suggestions::bestMatches(attrNames, attr);
                throw AttrPathNotFound(
                    suggestions, "attribute '%1%' in selection path '%2%' not found", attr, attrPath);
            }
            v = &*a->value;
            pos = a->pos;
        }

        else {

            if (!v->isList())
                state
                    .error<TypeError>(
                        "the expression selected by the selection path '%1%' should be a list but is %2%",
                        attrPath,
                        showType(*v))
                    .debugThrow();
            if (*attrIndex >= v->listSize())
                throw AttrPathNotFound("list index %1% in selection path '%2%' is out of range", *attrIndex, attrPath);

            v = v->listView()[*attrIndex];
            pos = noPos;
        }
    }

    return {v, pos};
}

} // namespace nix
