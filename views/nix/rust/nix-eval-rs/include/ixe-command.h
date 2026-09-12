#pragma once

#include "ixe.h"

#ifdef __cplusplus
extern "C" {
#endif

#define IXE_EVAL_RAW 1u
#define IXE_EVAL_JSON 2u
#define IXE_EVAL_PRETTY 4u
#define IXE_EVAL_FILE 8u
#define IXE_EVAL_EXPR 16u
#define IXE_EVAL_WRITE_TO 32u

/* Validate before reading a source or resolving an installable. On success,
 * render receives the canonical IXE_RENDER_* mode for the cache question.
 * On failure, error receives an ixe_string_free-owned diagnostic. */
int ixe_eval_command_validate(
    unsigned int flags, const unsigned char * installable, size_t installable_len, int * render, char ** error);

/* Executes the typed Select question retained by this session. No selection
 * or rendering parameters can differ from the question used for its cache key. */
int ixe_eval_command_execute(IxeSession * session, IxeHandle root, char ** out);

/* Selects the first resolvable candidate from the retained typed question and
 * returns an owned handle plus its actual path. The root remains owned by the
 * caller. This single selection does not traverse a whole DerivationSet. */
int ixe_question_select(IxeSession * session, IxeHandle root, IxeHandle * selected, char ** selected_path);

/* Visits exactly index in a retained DerivationSet selection. Missing paths,
 * out-of-range indices, and all other question kinds fail. The returned handle
 * is owned by the caller; the original root remains valid. */
int ixe_question_select_at(IxeSession * session, IxeHandle root, size_t index, IxeHandle * selected);

/* Selects from a retained Application question and returns its expected type
 * together with the chosen path. Both strings and the handle are caller-owned.
 * Flake apps/defaultApp selections expect "app"; all other selections expect
 * "derivation". The question's keyed selection policy determines source kind. */
int ixe_question_select_app(
    IxeSession * session, IxeHandle root, IxeHandle * selected, char ** selected_path, char ** expected_type);

/* Formats either a fresh or cached canonical answer. A null transformed result
 * means retain the original answer without copying. Otherwise use the returned
 * ixe_string_free-owned bytes. Append one newline only when requested by the
 * output flag. Presentation never changes the evaluation cache key. */
int ixe_eval_command_format(
    unsigned int flags,
    const unsigned char * answer,
    size_t answer_len,
    char ** transformed,
    int * append_newline,
    char ** error);

#ifdef __cplusplus
}
#endif
