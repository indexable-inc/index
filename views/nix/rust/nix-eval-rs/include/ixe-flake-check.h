#pragma once
#include "ixe.h"
#ifdef __cplusplus
extern "C" {
#endif
/* Question flags: bit 0 Hydra, bit 1 all systems, bit 2 keep going, bit 3 evaluate only.
 * Each scope uses a new session with its IFD policy set beforehand. */
int ixe_flake_check_execute(IxeSession * session, IxeHandle root, char ** out);
typedef struct IxeFlakeCheckReport IxeFlakeCheckReport;

typedef struct
{
    size_t derivations;
    size_t errors;
    size_t omitted_systems;
} IxeFlakeCheckCounts;

typedef struct
{
    IxeBytes attribute_path;
    IxeBytes drv_path;
    int build;
} IxeFlakeCheckDerivation;

/* Decode once against the retained question's scope and system. Output and error
 * are disjoint writable slots. Diagnostics use ixe_string_free. */
int ixe_flake_check_report_decode(
    IxeBytes text, IxeBytes store_dir, IxeBytes local_system, int flags, IxeFlakeCheckReport ** out, char ** error);
void ixe_flake_check_report_free(IxeFlakeCheckReport * report);
int ixe_flake_check_report_counts(const IxeFlakeCheckReport * report, IxeFlakeCheckCounts * out);
/* Views borrow report until report_free. Bounds/null errors return IXE_ERR_BADCALL. */
int ixe_flake_check_report_derivation(const IxeFlakeCheckReport * report, size_t index, IxeFlakeCheckDerivation * out);
/* kind 0: validation error; kind 1: omitted system. */
int ixe_flake_check_report_message(const IxeFlakeCheckReport * report, int kind, size_t index, IxeBytes * out);
#ifdef __cplusplus
}
#endif
