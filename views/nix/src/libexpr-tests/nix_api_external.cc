#include "nix_api_store.h"
#include "nix_api_util.h"
#include "nix_api_expr.h"
#include "nix_api_expr_internal.h"
#include "nix_api_value.h"
#include "nix_api_external.h"

#include "nix/expr/tests/nix_api_expr.hh"
#include "nix/expr/value-to-json.hh"
#include <nlohmann/json.hpp>
#include "nix/util/tests/string_callback.hh"

#include <gtest/gtest.h>

namespace nixC {

class MyExternalValueDesc : public NixCExternalValueDesc
{
public:
    MyExternalValueDesc(int x)
        : _x(x)
    {
        print = print_function;
        showType = show_type_function;
        typeOf = type_of_function;
    }

private:
    int _x;

    static void print_function(void * self, nix_printer * printer) {}

    static void show_type_function(void * self, nix_string_return * res) {}

    static void type_of_function(void * self, nix_string_return * res)
    {
        MyExternalValueDesc * obj = static_cast<MyExternalValueDesc *>(self);

        std::string type_string = "nix-external<MyExternalValueDesc( ";
        type_string += std::to_string(obj->_x);
        type_string += " )>";
        nix_set_string_return(res, &*type_string.begin());
    }
};

TEST_F(nix_api_expr_test, nix_expr_eval_external)
{
    MyExternalValueDesc * external = new MyExternalValueDesc(42);
    ExternalValue * val = nix_create_external_value(ctx, external, external);
    nix_init_external(ctx, value, val);

    ASSERT_EQ(NIX_TYPE_EXTERNAL, nix_get_type(nullptr, value));
    ASSERT_EQ("nix-external<MyExternalValueDesc( 42 )>", value->value->external()->typeOf());
}

static void print_value_as_json_using_state(
    void * self, EvalState * state, bool strict, nix_string_context * c, bool copyToStore, nix_string_return * res)
{
    // Regression test: same cast bug as in nix_c_primop_wrapper (see primop_alloc_value).
    nix_value * v = nix_alloc_value(nullptr, state);
    assert(v != nullptr);
    nix_gc_decref(nullptr, v);

    nix_set_string_return(res, "42");
}

TEST_F(nix_api_expr_test, nix_external_printValueAsJSON_can_use_state)
{
    NixCExternalValueDesc desc{};
    desc.print = [](void *, nix_printer *) {};
    desc.showType = [](void *, nix_string_return *) {};
    desc.typeOf = [](void *, nix_string_return *) {};
    desc.printValueAsJSON = print_value_as_json_using_state;

    ExternalValue * val = nix_create_external_value(ctx, &desc, nullptr);
    assert_ctx_ok();
    nix_init_external(ctx, value, val);
    assert_ctx_ok();

    nix::NixStringContext context;
    auto json = nix::printValueAsJSON(state->state, true, *value->value, nix::noPos, context, false);
    assert_ctx_ok();
    ASSERT_EQ(42, json);
}

} // namespace nixC
