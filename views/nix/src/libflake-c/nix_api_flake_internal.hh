#pragma once
#include <optional>

#include "nix/util/ref.hh"
#include "nix/flake/flakeref.hh"

struct nix_flake_reference_parse_flags
{
    std::optional<std::filesystem::path> baseDirectory;
};

struct nix_flake_reference
{
    nix::ref<nix::FlakeRef> flakeRef;
};
