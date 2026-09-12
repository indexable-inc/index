#pragma once
///@file The nix-instantiate seam into the Rust evaluator (rust/nix-eval-rs).
/// M1 scope: whole-expression evaluation of source text.

#include "nix/expr/eval.hh"
#include "nix/cmd/rust-eval-session.hh"

namespace nix {

/// Evaluate source text with the Rust backend and print the result the way
/// processExpr would: each attribute path walked under `autoArgs`
/// (cppnix's findAlongAttrPath auto-calls before every component), the value
/// reached auto-called once more when there are arguments, then rendered.
/// Throws EvalError on evaluation failure and RustEvalRefusal with the marker
/// "rust-eval unimplemented" on the shapes the backend (or this bridge) does
/// not cover: lazy printing of a value with children, and `--xml` with
/// source locations (`xmlLocation`, nix-instantiate's
/// `xmlOutputSourceLocation` -- the Rust document has no position
/// attributes, so only the `--no-location` spelling is served).
/// `file` is the absolute path `source` was read from, or empty when it came
/// from `--expr`. It is what `__curPos` reports (ENG-12713).
void rustEvalPrint(
    EvalState & state,
    const std::string & source,
    const std::string & baseDir,
    const std::string & file,
    const Strings & attrPaths,
    int outputKind,
    bool xmlLocation,
    bool strict,
    const std::vector<RustAutoArg> & autoArgs);

} // namespace nix
