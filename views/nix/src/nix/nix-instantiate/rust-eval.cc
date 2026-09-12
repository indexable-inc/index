#include "rust-eval.hh"
#include "nix/cmd/rust-eval-session.hh"
#include "nix/expr/rust-eval-refusal.hh"

#include "nix/util/error.hh"
#include "nix/expr/eval-error.hh"
#include "nix/expr/eval.hh"

#include <iostream>

namespace nix {

/// nix-instantiate.cc's `OutputKind`, which is local to that file. Repeated
/// rather than shared because moving it into the header would put a
/// nix-instantiate detail in front of every other caller of this bridge; the
/// cost is that the two have to be changed together, and the values are
/// checked against it below.
enum OutputKindMirror { okPlain = 0, okRaw = 1, okXML = 2, okJSON = 3 };

void rustEvalPrint(
    EvalState & state,
    const std::string & source,
    const std::string & baseDir,
    const std::string & file,
    const Strings & attrPaths,
    int outputKind,
    bool xmlLocation,
    bool strict,
    const std::vector<RustAutoArg> & autoArgs)
{
    // Two shapes of `--xml` refused. It is served through builtins.toXML's
    // walker -- cppnix's printValueAsXML is one function for both -- but that
    // document has no source positions, so the location-bearing spelling
    // (the default; `--no-location` is what turns it off) is refused rather
    // than answered without the attributes cppnix would print; and without
    // `--strict` cppnix writes `<unevaluated />` for thunks, which is the
    // same evaluator-internal fact the lazy plain printer refuses (below, on
    // the Rust side, where the value's shape is known). `--json` and `--raw`
    // force as they print, strict or not, so they are one answer either way.
    if (outputKind == okXML && xmlLocation)
        refuse(refusalTokens::xmlOutput, "--xml with source locations (run with --no-location)");
    if (outputKind == okXML && !strict)
        refuse(refusalTokens::xmlOutput, "--xml without --strict (cppnix prints <unevaluated /> for thunks)");
    if (outputKind != okPlain && outputKind != okRaw && outputKind != okJSON && outputKind != okXML)
        throw Error("rust-eval: unknown output kind %d", outputKind);

    auto render = outputKind == okJSON  ? RustRender::Json
                  : outputKind == okRaw ? RustRender::Raw
                  : outputKind == okXML ? RustRender::Xml
                  : strict              ? RustRender::Plain
                                        : RustRender::PlainLazy;

    for (auto & attrPath : attrPaths) {
        /* cppnix's processExpr: findAlongAttrPath under the arguments, then
           -- only when there are arguments -- autoCallFunction on what it
           reached, then print. The evaluator does both applications, keyed
           on the arguments; `autoCall` names the second for the key. One
           question shape for every attribute path and render, memoised as
           one: the one-call `ixe_eval_expr` shortcut this used to take for a
           plain whole expression was a second row for the same answer and
           had no way to carry --arg. */
        auto text = rustEvalRender(
            state,
            RustEvaluand{
                .src = RustSource{.source = source, .baseDir = baseDir, .file = file},
                .args = {},
                .attrPaths = {attrPath},
                .autoArgs = autoArgs,
                .autoCall = !autoArgs.empty(),
            },
            render);
        if (render == RustRender::Raw || render == RustRender::Xml)
            // Deliberately no newline. For raw, matching cppnix: the default
            // Bash PS1 on NixOS opens with one. For XML because the document
            // already ends with one -- XMLWriter closes the root element with
            // a newline of its own, and cppnix's okXML branch appends nothing.
            std::cout << text;
        else
            std::cout << text << std::endl;
    }
}

} // namespace nix
