#pragma once
#include "ixe.h"
#ifdef __cplusplus
extern "C" {
#endif

#define IXE_FLAKE_SHOW_BRANCH 0u
#define IXE_FLAKE_SHOW_DERIVATION 1u
#define IXE_FLAKE_SHOW_APP 2u
#define IXE_FLAKE_SHOW_TEMPLATE 3u
#define IXE_FLAKE_SHOW_NIXPKGS_OVERLAY 4u
#define IXE_FLAKE_SHOW_NIXOS_CONFIGURATION 5u
#define IXE_FLAKE_SHOW_NIXOS_MODULE 6u
#define IXE_FLAKE_SHOW_UNKNOWN 7u
#define IXE_FLAKE_SHOW_EMPTY 8u
#define IXE_FLAKE_SHOW_NON_DERIVATION 9u
#define IXE_FLAKE_SHOW_OMITTED_SYSTEM 10u
#define IXE_FLAKE_SHOW_OMITTED_LEGACY 11u
#define IXE_FLAKE_SHOW_OMITTED_IFD 12u

typedef struct IxeFlakeShowDocument IxeFlakeShowDocument;
typedef struct IxeFlakeShowReport IxeFlakeShowReport;

typedef struct
{
    IxeBytes name;
    uint64_t node;
} IxeFlakeShowChild;

typedef struct
{
    unsigned int kind;
    IxeBytes message;
} IxeFlakeShowWarning;

typedef struct
{
    IxeBytes output;
    const IxeFlakeShowWarning * warnings;
    size_t warnings_len;
} IxeFlakeShowReportView;

/* All fallible operations return 0 or IXE_ERR_BADCALL and an owned error
 * string freed with ixe_string_free. Inputs are borrowed for the call only.
 * Output pointer slots must be non-null, writable and disjoint. Documents
 * and reports are owned, single-threaded objects; free each exactly once. */
int ixe_flake_show_new(IxeFlakeShowDocument ** out, char ** error);
void ixe_flake_show_free(IxeFlakeShowDocument * document);
/* Build children before their parent. Node IDs belong to one document.
 * Only derivations carry a name; only derivations/apps/templates carry a
 * description. Description presence is distinct from an empty description. */
int ixe_flake_show_add(
    IxeFlakeShowDocument * document,
    unsigned int kind,
    IxeBytes name,
    int has_description,
    IxeBytes description,
    const IxeFlakeShowChild * children,
    size_t children_len,
    uint64_t * out,
    char ** error);
/* Finish a branch root. Unattached construction fragments are discarded. */
int ixe_flake_show_finish(IxeFlakeShowDocument * document, uint64_t root, char ** error);
/* Encode a finished document, or strictly validate and decode cached bytes. */
int ixe_flake_show_encode(const IxeFlakeShowDocument * document, char ** out, char ** error);
int ixe_flake_show_decode(
    const unsigned char * encoded, size_t encoded_len, IxeFlakeShowDocument ** out, char ** error);
/* Render atomically into an owned report: no callbacks or partial output. */
int ixe_flake_show_render(
    const IxeFlakeShowDocument * document,
    IxeBytes root_label,
    int json,
    int colored,
    IxeFlakeShowReport ** out,
    char ** error);
/* View pointers remain valid until report_free. No bytes are copied. */
int ixe_flake_show_report_view(const IxeFlakeShowReport * report, IxeFlakeShowReportView * out);
void ixe_flake_show_report_free(IxeFlakeShowReport * report);

#ifdef __cplusplus
}
#endif
