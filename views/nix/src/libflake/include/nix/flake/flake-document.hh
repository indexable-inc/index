#pragma once
///@file

#include <functional>
#include <nlohmann/json.hpp>
#include "nix/expr/eval.hh"
#include "nix/util/source-path.hh"

namespace nix::flake {

/**
 * Rust evaluates and validates computed flake metadata. The normalized
 * document contains description, inputs, self_attrs and config. Input
 * references and attribute/configuration values carry explicit kind tags.
 * Output formals become implicit inputs; outputs itself is never invoked.
 * Paths retain their mounted root and are relative to the flake directory.
 *
 * libcmd installs the reader because it links the Rust evaluator. libflake
 * consumes the document without evaluating expressions.
 */
using FlakeDocumentReader = std::function<nlohmann::json(EvalState &, const SourcePath &)>;

FlakeDocumentReader & flakeDocumentReader();

} // namespace nix::flake
