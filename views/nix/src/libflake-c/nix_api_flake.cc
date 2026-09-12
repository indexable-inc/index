#include <string>

#include "nix_api_flake.h"
#include "nix_api_flake_internal.hh"
#include "nix_api_util.h"
#include "nix_api_util_internal.h"
#include "nix_api_fetchers_internal.hh"
#include "nix_api_fetchers.h"

extern "C" {

nix_flake_reference_parse_flags * nix_flake_reference_parse_flags_new(nix_c_context * context)
{
    nix_clear_err(context);
    try {
        return new nix_flake_reference_parse_flags{
            .baseDirectory = std::nullopt,
        };
    }
    NIXC_CATCH_ERRS_NULL
}

void nix_flake_reference_parse_flags_free(nix_flake_reference_parse_flags * flags)
{
    delete flags;
}

nix_err nix_flake_reference_parse_flags_set_base_directory(
    nix_c_context * context,
    nix_flake_reference_parse_flags * flags,
    const char * baseDirectory,
    size_t baseDirectoryLen)
{
    nix_clear_err(context);
    try {
        flags->baseDirectory.emplace(std::string(baseDirectory, baseDirectoryLen));
        return NIX_OK;
    }
    NIXC_CATCH_ERRS
}

nix_err nix_flake_reference_and_fragment_from_string(
    nix_c_context * context,
    nix_fetchers_settings * fetchSettings,
    nix_flake_reference_parse_flags * parseFlags,
    const char * strData,
    size_t strSize,
    nix_flake_reference ** flakeReferenceOut,
    nix_get_string_callback fragmentCallback,
    void * fragmentCallbackUserData)
{
    nix_clear_err(context);
    *flakeReferenceOut = nullptr;
    try {
        std::string str(strData, strSize);

        auto [flakeRef, fragment] =
            nix::parseFlakeRefWithFragment(*fetchSettings->settings, str, parseFlags->baseDirectory, true);
        *flakeReferenceOut = new nix_flake_reference{nix::make_ref<nix::FlakeRef>(flakeRef)};
        return call_nix_get_string_callback(fragment, fragmentCallback, fragmentCallbackUserData);
    }
    NIXC_CATCH_ERRS
}

void nix_flake_reference_free(nix_flake_reference * flakeReference)
{
    delete flakeReference;
}

} // extern "C"
