#ifndef IXE_SOURCE_POSITION_H
#define IXE_SOURCE_POSITION_H

#include "ixe.h"

#ifdef __cplusplus
extern "C" {
#endif

typedef struct
{
    char * file;
    uint32_t line;
} IxeSourcePosition;

/* Execute the source-position question retained by the session. The canonical
 * answer is freed with ixe_string_free and may be filed in the question memo. */
int ixe_source_position_execute(IxeSession * session, IxeHandle root, char ** out);

/* Parse either a fresh or memoized answer. On success out->file is owned and
 * freed with ixe_string_free. On failure *error is owned and freed likewise.
 * All output slots are disjoint and writable; text is readable for len bytes. */
int ixe_source_position_decode(const uint8_t * text, size_t len, IxeSourcePosition * out, char ** error);

#ifdef __cplusplus
}
#endif
#endif
