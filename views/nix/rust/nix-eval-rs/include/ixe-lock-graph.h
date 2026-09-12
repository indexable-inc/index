#ifndef IXE_LOCK_GRAPH_H
#define IXE_LOCK_GRAPH_H
#include <stddef.h>
#include <stdint.h>
#ifdef __cplusplus
extern "C" {
#endif

typedef struct IxeLockGraph IxeLockGraph;
typedef struct IxeLockSchedule IxeLockSchedule;
typedef struct IxeLockInputs IxeLockInputs;
typedef struct IxeLockPath IxeLockPath;
typedef struct IxeLockNodes IxeLockNodes;

typedef struct
{
    const uint8_t * data;
    size_t len;
} IxeLockBytes;

typedef struct
{
    const IxeLockBytes * data;
    size_t len;
} IxeLockPathView;

typedef struct
{
    IxeLockBytes name;
    uint8_t kind; /* 0 = direct node, 1 = follows */
    uint64_t target;
    IxeLockPathView follows;
} IxeLockEdgeView;

/* Fallible calls return NULL on success, otherwise an owned error string.
 * Outputs are written only on success. All borrowed views live until their
 * snapshot is freed; snapshots remain valid across graph mutations. */
char * ixe_lock_graph_new_empty(IxeLockGraph ** out);
char * ixe_lock_graph_parse(IxeLockBytes source, IxeLockGraph ** out);
char * ixe_lock_graph_add(IxeLockGraph * graph, IxeLockBytes payload, uint64_t * out);
char * ixe_lock_graph_set_direct(IxeLockGraph * graph, uint64_t node, IxeLockBytes name, uint64_t target);
char * ixe_lock_graph_set_follows(IxeLockGraph * graph, uint64_t node, IxeLockBytes name, IxeLockPathView path);
char * ixe_lock_graph_inputs(const IxeLockGraph * graph, uint64_t node, IxeLockInputs ** out);
size_t ixe_lock_inputs_len(const IxeLockInputs * inputs);
char * ixe_lock_inputs_get(const IxeLockInputs * inputs, size_t index, IxeLockEdgeView * out);
void ixe_lock_inputs_free(IxeLockInputs * inputs);
char * ixe_lock_graph_find(const IxeLockGraph * graph, IxeLockPathView path, uint64_t * out, uint8_t * found);
char * ixe_lock_graph_check(const IxeLockGraph * graph);
/* out points to exactly 32 writable bytes. */
char * ixe_lock_graph_identity(const IxeLockGraph * graph, uint8_t * out);
char * ixe_lock_graph_reachable(const IxeLockGraph * graph, IxeLockNodes ** out);
size_t ixe_lock_nodes_len(const IxeLockNodes * nodes);
char * ixe_lock_nodes_get(const IxeLockNodes * nodes, size_t index, uint64_t * out);
void ixe_lock_nodes_free(IxeLockNodes * nodes);
char * ixe_lock_graph_payloads(const IxeLockGraph * graph, char ** out);
char * ixe_lock_graph_serialize(const IxeLockGraph * graph, char ** out);
char * ixe_lock_graph_all_inputs(const IxeLockGraph * graph, char ** out);
/* The schedule owns dependency state independently of the graph. All
 * completion operations are thread-safe; snapshots have independent ownership. */
char * ixe_lock_graph_prefetch_schedule(const IxeLockGraph * graph, IxeLockSchedule ** out);
char * ixe_lock_schedule_ready(const IxeLockSchedule * schedule, IxeLockNodes ** out);
char *
ixe_lock_schedule_complete(const IxeLockSchedule * schedule, uint64_t node, uint8_t succeeded, IxeLockNodes ** out);
char * ixe_lock_schedule_check_complete(const IxeLockSchedule * schedule);
void ixe_lock_schedule_free(IxeLockSchedule * schedule);
char * ixe_lock_graph_parse_path(IxeLockBytes source, IxeLockPath ** out);
IxeLockPathView ixe_lock_path_view(const IxeLockPath * path);
void ixe_lock_path_free(IxeLockPath * path);
void ixe_lock_graph_free(IxeLockGraph * graph);
void ixe_lock_graph_string_free(char * text);
#ifdef __cplusplus
}
#endif
#endif
