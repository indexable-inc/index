#pragma once
///@file

#include "nix/util/types.hh"
#include "nix/util/source-path.hh"

namespace nix {

class EvalState;

/**
 * Helper function to generate args that invoke $EDITOR on
 * filename:lineno.
 *
 * An editor needs a file on disk. A file inside a lazily mounted input
 * has none until the input is forced into the store, so `file` is forced
 * through `state` first when it names a store path; the editor then opens
 * the store copy, as it did before inputs were mounted lazily.
 */
Strings editorFor(EvalState & state, const SourcePath & file, uint32_t line);

} // namespace nix
