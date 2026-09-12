#pragma once

#include "nix/util/signals.hh"

namespace nix {

/** Wake threads blocked on host values when evaluation is interrupted. */
std::unique_ptr<InterruptCallback> registerValueInterruptCallback();

} // namespace nix
