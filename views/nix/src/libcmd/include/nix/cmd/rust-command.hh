#pragma once

#include "nix/cmd/rust-eval-session.hh"

namespace nix {

struct RustEvalCommandOptions
{
    bool raw = false;
    bool json = false;
    bool pretty = false;
    bool file = false;
    bool expr = false;
    bool writeTo = false;
    std::string installable;
};

/// Rust validates the request before the host reads or fetches its source.
void rustValidateEvalCommand(const RustEvalCommandOptions & options);

/// Returns final stdout bytes, including any command-owned formatting.
std::string rustEvalCommand(
    EvalState & state,
    const RustEvaluand & evaluand,
    const RustEvalCommandOptions & options,
    const RustEvalCache * cache = nullptr);

} // namespace nix
