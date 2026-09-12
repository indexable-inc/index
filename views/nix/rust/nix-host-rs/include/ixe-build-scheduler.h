#pragma once

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* A worker owns one supervisor. All calls are exclusive, including queries.
 * Elapsed milliseconds come from one monotonic clock, never wall-clock time.
 * The caller retains processes, pipe channels and weak goal references.
 * Nonzero status owns an error released with ixe_build_scheduler_error_free.
 * Required output and error slots must be writable and disjoint. */
typedef struct IxeBuildScheduler IxeBuildScheduler;

#define IXE_BUILD_JOB_BUILD 0
#define IXE_BUILD_JOB_SUBSTITUTION 1
#define IXE_BUILD_JOB_ADMINISTRATION 2
#define IXE_BUILD_TIMEOUT_NONE 0
#define IXE_BUILD_TIMEOUT_SILENT 1
#define IXE_BUILD_TIMEOUT_NO_PROGRESS 2
#define IXE_BUILD_TIMEOUT_TOTAL 3

typedef struct IxeBuildSchedulerConfig
{
    uint64_t max_builds;
    uint64_t max_substitutions;
    int64_t silent_seconds;
    int64_t build_seconds;
    uint64_t poll_seconds;
    unsigned int monitor_progress;
} IxeBuildSchedulerConfig;

typedef struct IxeBuildWaitPlan
{
    unsigned int has_timeout;
    unsigned int timeout_ms;
} IxeBuildWaitPlan;

typedef struct IxeBuildChildDecision
{
    unsigned int timeout_kind;
    uint64_t timeout_seconds;
} IxeBuildChildDecision;

int ixe_build_scheduler_new(const IxeBuildSchedulerConfig * config, IxeBuildScheduler ** out, char ** error);
/* Frees only bookkeeping; host cancellation must kill and join its children. */
void ixe_build_scheduler_free(IxeBuildScheduler * scheduler);
void ixe_build_scheduler_error_free(char * error);
int ixe_build_scheduler_start(
    IxeBuildScheduler * scheduler,
    unsigned int job_category,
    unsigned int occupies_slot,
    unsigned int respect_timeouts,
    uint64_t now_ms,
    uint64_t * out,
    char ** error);
/* Duplicate unregister is harmless and returns wake_sleepers = 0. */
int ixe_build_scheduler_stop(
    IxeBuildScheduler * scheduler, uint64_t id, unsigned int wake_sleepers, unsigned int * out, char ** error);
int ixe_build_scheduler_slot_available(
    IxeBuildScheduler * scheduler, unsigned int job_category, unsigned int * out, char ** error);
int ixe_build_scheduler_running(
    IxeBuildScheduler * scheduler, unsigned int job_category, uint64_t * out, char ** error);
int ixe_build_scheduler_note_output(IxeBuildScheduler * scheduler, uint64_t id, uint64_t now_ms, char ** error);
int ixe_build_scheduler_monitors_progress(
    IxeBuildScheduler * scheduler, uint64_t id, unsigned int * out, char ** error);
int ixe_build_scheduler_wait_plan(
    IxeBuildScheduler * scheduler,
    uint64_t now_ms,
    unsigned int lock_waiters,
    unsigned int gc_poll,
    IxeBuildWaitPlan * out,
    char ** error);
/* no_progress_seconds = 0 means the host observed no progress timeout. */
int ixe_build_scheduler_inspect_child(
    IxeBuildScheduler * scheduler,
    uint64_t id,
    uint64_t now_ms,
    unsigned int busy,
    int64_t no_progress_seconds,
    IxeBuildChildDecision * out,
    char ** error);
int ixe_build_scheduler_finish_poll(
    IxeBuildScheduler * scheduler, uint64_t now_ms, unsigned int lock_waiters, unsigned int * out, char ** error);

#ifdef __cplusplus
}
#endif
