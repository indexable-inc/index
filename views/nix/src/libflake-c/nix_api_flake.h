#ifndef NIX_API_FLAKE_H
#define NIX_API_FLAKE_H
/** @defgroup libflake libflake
 * @brief Bindings to the Nix Flakes library
 *
 * @{
 */
/** @file
 * @brief Main entry for the libflake C bindings
 */

#include "nix_api_fetchers.h"
#include "nix_api_util.h"

#ifdef __cplusplus
extern "C" {
#endif
// cffi start

/**
 * @brief Context and parameters for parsing a flake reference
 * @see nix_flake_reference_parse_flags_free
 * @see nix_flake_reference_and_fragment_from_string
 */
typedef struct nix_flake_reference_parse_flags nix_flake_reference_parse_flags;

/**
 * @brief A reference to a flake
 *
 * A flake reference specifies how to fetch a flake.
 *
 * @see nix_flake_reference_and_fragment_from_string
 * @see nix_flake_reference_free
 */
typedef struct nix_flake_reference nix_flake_reference;

// Function prototypes
/**
 * @brief A new `nix_flake_reference_parse_flags` with defaults
 */
nix_flake_reference_parse_flags * nix_flake_reference_parse_flags_new(nix_c_context * context);

/**
 * @brief Deallocate and release the resources associated with a `nix_flake_reference_parse_flags`.
 * Does not fail.
 * @param[in] flags the `nix_flake_reference_parse_flags *` to free
 */
void nix_flake_reference_parse_flags_free(nix_flake_reference_parse_flags * flags);

/**
 * @brief Provide a base directory for parsing relative flake references
 * @param[out] context Optional, stores error information
 * @param[in] flags The flags to modify
 * @param[in] baseDirectory The base directory to add
 * @param[in] baseDirectoryLen The length of baseDirectory
 * @return NIX_OK on success, NIX_ERR on failure
 */
nix_err nix_flake_reference_parse_flags_set_base_directory(
    nix_c_context * context,
    nix_flake_reference_parse_flags * flags,
    const char * baseDirectory,
    size_t baseDirectoryLen);

/**
 * @brief Parse a URL-like string into a `nix_flake_reference`.
 *
 * @param[out] context **context** – Optional, stores error information
 * @param[in] fetchSettings **context** – The fetch settings to use
 * @param[in] parseFlags **context** – Specific context and parameters such as base directory
 *
 * @param[in] str **input** – The URI-like string to parse
 * @param[in] strLen **input** – The length of `str`
 *
 * @param[out] flakeReferenceOut **result** – The resulting flake reference
 * @param[in] fragmentCallback **result** – A callback to call with the fragment part of the URL
 * @param[in] fragmentCallbackUserData **result** – User data to pass to the fragment callback
 *
 * @return NIX_OK on success, NIX_ERR on failure
 */
nix_err nix_flake_reference_and_fragment_from_string(
    nix_c_context * context,
    nix_fetchers_settings * fetchSettings,
    nix_flake_reference_parse_flags * parseFlags,
    const char * str,
    size_t strLen,
    nix_flake_reference ** flakeReferenceOut,
    nix_get_string_callback fragmentCallback,
    void * fragmentCallbackUserData);

/**
 * @brief Deallocate and release the resources associated with a `nix_flake_reference`.
 *
 * Does not fail.
 *
 * @param[in] store the `nix_flake_reference *` to free
 */
void nix_flake_reference_free(nix_flake_reference * store);

#ifdef __cplusplus
} // extern "C"
#endif

#endif
