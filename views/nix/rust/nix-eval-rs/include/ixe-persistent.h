#ifndef IXE_PERSISTENT_H
#define IXE_PERSISTENT_H

#include "ixe.h"
#include <stdbool.h>

#ifdef __cplusplus
extern "C" {
#endif

typedef struct
{
    IxeBytes installable;
    IxeBytes value_json;
    uint64_t wall_ns;
    uint64_t cpu_ns;
    size_t inputs_evicted;
    uint64_t witness_memory_hits;
    uint64_t witness_disk_loads;
    uint64_t witness_cache_bytes;
    uint64_t witness_cache_entries;
} IxePersistentReportInput;

/* Format one completed request using the current Rust question counters.
 * Views must remain readable for this call; output slots must be writable.
 * Returns 0 on success. Both returned strings use ixe_string_free. */
int ixe_persistent_report(IxePersistentReportInput input, char ** output, char ** error);

typedef struct IxePersistentRequests IxePersistentRequests;
typedef struct {
    IxeBytes installable;
    IxeBytes apply;
    bool has_apply;
} IxePersistentRequestView;
typedef struct {
    size_t file_bytes, requests, report_bytes, output_bytes;
} IxePersistentLimits;

IxePersistentLimits ixe_persistent_limits(void);
/* Validate the entire file before returning any request. Output/error strings
 * use ixe_string_free. Batch ownership is unique and single-threaded. */
int ixe_persistent_requests_new(IxeBytes input, IxePersistentRequests ** output, char ** error);
void ixe_persistent_requests_free(IxePersistentRequests * batch);
/* Views borrow the batch and survive until its destruction. Calls must alternate
 * next and complete; done is true only after all requests complete successfully. */
int ixe_persistent_requests_next(
    IxePersistentRequests * batch, IxePersistentRequestView * output, bool * done, char ** error);
/* A successful payload is the unchanged ixe_persistent_report document; a failed
 * payload is diagnostic text. Adds version/id/status and accounts bounded JSONL
 * output. A failed request or framing error terminates the batch. */
int ixe_persistent_requests_complete(
    IxePersistentRequests * batch, bool success, IxeBytes payload, char ** output, char ** error);

#ifdef __cplusplus
}
#endif
#endif
