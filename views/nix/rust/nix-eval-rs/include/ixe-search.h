#pragma once
#include "ixe.h"
#ifdef __cplusplus
extern "C" {
#endif

typedef struct IxeSearchPlan IxeSearchPlan;
typedef struct IxeSearchCatalogue IxeSearchCatalogue;
/* Search executes the exact retained SearchPackages question in this session.
 * The canonical unfiltered catalogue is owned; free it with ixe_string_free. */
int ixe_search_execute(IxeSession * session, uint64_t root, char ** out);
/* Borrowed inputs, disjoint writable output slots. Fallible plan/codec calls
 * return 0 or IXE_ERR_BADCALL and an owned diagnostic, freed by ixe_string_free. */
int ixe_search_catalogue_decode(const unsigned char * text, size_t len, IxeSearchCatalogue ** out, char ** error);
void ixe_search_catalogue_free(IxeSearchCatalogue * catalogue);
int ixe_search_plan_new(
    const IxeBytes * include,
    size_t include_len,
    const IxeBytes * exclude,
    size_t exclude_len,
    int json,
    int colored,
    IxeSearchPlan ** out,
    char ** error);
int ixe_search_plan_render(
    const IxeSearchPlan * plan, const IxeSearchCatalogue * catalogue, char ** out, char ** error);
void ixe_search_plan_free(IxeSearchPlan * plan);
#ifdef __cplusplus
}
#endif
