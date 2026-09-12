#pragma once
#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Independent, exclusively owned stream handles. No call is reentrant on a
 * handle. Free each handle once with its matching free function; null is OK.
 * All input ranges are borrowed only for the call, and may be null iff empty.
 * Output storage must be aligned, writable, and disjoint from input/handles.
 * Status: 0 success, 1 invalid input, 2 failed/finished stream, 3 Rust panic,
 * 4 sink failure. Any failing operation poisons its handle. Constructors
 * initialise non-null output slots to null before validating inputs.
 */
typedef struct
{
    const uint8_t * data;
    size_t len;
} IxsBytes;

typedef struct
{
    IxsBytes from;
    IxsBytes to;
} IxsRewrite;

typedef struct
{
    uint8_t bytes[64];
    size_t len;
    uint64_t input_bytes;
} IxsDigest;

/* The callback must catch C++ exceptions and return nonzero on failure. It
 * must neither retain the borrowed bytes nor call back into the same handle. */
typedef int32_t (*IxsEmit)(void * context, const uint8_t * bytes, size_t len);

/* hashes contains count consecutive 32-byte store hash parts. Results are
 * indices into that original array; duplicates and invalid hashes are rejected. */
int32_t ixs_refscan_new(const uint8_t * hashes, size_t count, void ** output);
int32_t ixs_refscan_feed(void * handle, const uint8_t * data, size_t len);
int32_t ixs_refscan_result(void * handle, size_t * indices, size_t capacity, size_t * count);
void ixs_refscan_free(void * handle);

/* Keys must be nonempty and unique; replacement lengths must equal key
 * lengths. Replacements never match generated output. First match wins in
 * input order; ties use lexicographic key order. finish closes the stream. */
int32_t ixs_rewriter_new(const IxsRewrite * rules, size_t count, void ** output);
int32_t ixs_rewriter_feed(void * handle, const uint8_t * data, size_t len, bool finish, IxsEmit emit, void * context);
void ixs_rewriter_free(void * handle);

/* Algorithm IDs: 0 MD5, 1 SHA1, 2 SHA256, 3 SHA512, 4 BLAKE3.
 * Hashes original bytes with non-overlapping modulus occurrences zeroed,
 * then appends |<decimal offset> for each occurrence in input order. The
 * returned byte count excludes that offset trailer. finish is idempotent. */
int32_t ixs_modulo_new(uint32_t algorithm, const uint8_t * modulus, size_t len, void ** output);
int32_t ixs_modulo_feed(void * handle, const uint8_t * data, size_t len);
int32_t ixs_modulo_finish(void * handle, IxsDigest * output);
void ixs_modulo_free(void * handle);

#ifdef __cplusplus
}
#endif
