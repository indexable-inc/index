#ifndef IXE_CLOSURE_DIFF_H
#define IXE_CLOSURE_DIFF_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* One entry per unique store path; different outputs may share a name. */
typedef struct
{
    const uint8_t * name;
    size_t name_len;
    uint64_t nar_size;
} IxeClosureEntry;

typedef struct
{
    const uint8_t * text;
    size_t len;
} IxeClosureVersion;

/* Owned bytes, not NUL terminated. success is 1 for a report, 0 for an error.
 * Free exactly once, including empty and error results. */
typedef struct
{
    uint8_t * data;
    size_t len;
    int32_t success;
} IxeClosureReport;

/* Input arrays and UTF-8 strings are borrowed for the duration of the call.
 * A NULL input pointer is valid only when its length is zero. */
IxeClosureReport ixe_closure_diff(
    const IxeClosureEntry * before,
    size_t before_len,
    const IxeClosureEntry * after,
    size_t after_len,
    const uint8_t * indent,
    size_t indent_len);

void ixe_closure_report_free(IxeClosureReport report);

/* Shared by closure reports and profile package-version presentation. */
IxeClosureReport ixe_closure_versions(const IxeClosureVersion * versions, size_t len);

#ifdef __cplusplus
}
#endif
#endif
