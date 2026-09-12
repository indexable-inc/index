//! The host passes monotonic elapsed milliseconds and retains all OS handles.

use nix_build_scheduler::{Category, Config, Scheduler, TimeoutKind};
use std::ffi::{CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

pub struct IxeBuildScheduler {
    inner: Scheduler,
}

#[repr(C)]
pub struct IxeBuildSchedulerConfig {
    pub max_builds: u64,
    pub max_substitutions: u64,
    pub silent_seconds: i64,
    pub build_seconds: i64,
    pub poll_seconds: u64,
    pub monitor_progress: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct IxeBuildWaitPlan {
    pub has_timeout: u32,
    pub timeout_ms: u32,
}

#[repr(C)]
#[derive(Default)]
pub struct IxeBuildChildDecision {
    pub timeout_kind: u32,
    pub timeout_seconds: u64,
}

fn boolean(value: u32) -> Result<bool, &'static str> {
    match value {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err("invalid build scheduler boolean"),
    }
}

fn category(value: u32) -> Result<Category, &'static str> {
    match value {
        0 => Ok(Category::Build),
        1 => Ok(Category::Substitution),
        2 => Ok(Category::Administration),
        _ => Err("invalid build job category"),
    }
}

/// All error messages are static strings without interior NULs. A panic is
/// contained at this ABI; its payload is never exposed as a foreign exception.
unsafe fn report(error: *mut *mut c_char, work: impl FnOnce() -> Result<(), &'static str>) -> i32 {
    if error.is_null() {
        return 1;
    }
    // SAFETY: caller supplies a writable, disjoint error slot.
    unsafe { *error = ptr::null_mut() };
    let failure = match catch_unwind(AssertUnwindSafe(work)) {
        Ok(Ok(())) => return 0,
        Ok(Err(message)) => message,
        Err(_) => "build scheduler panicked",
    };
    if let Ok(message) = CString::new(failure) {
        // SAFETY: error remains live; ownership is transferred to error_free.
        unsafe { *error = message.into_raw() };
    }
    1
}

unsafe fn output<T: Default>(
    scheduler: *mut IxeBuildScheduler,
    out: *mut T,
    error: *mut *mut c_char,
    work: impl FnOnce(&mut Scheduler) -> Result<T, &'static str>,
) -> i32 {
    // SAFETY: forwarded caller contract; raw pointers are validated before use.
    unsafe {
        report(error, || {
            if out.is_null() {
                return Err("null build scheduler output");
            }
            *out = T::default();
            let scheduler = scheduler.as_mut().ok_or("null build scheduler")?;
            *out = work(&mut scheduler.inner)?;
            Ok(())
        })
    }
}

/// # Safety
/// Config and output/error slots must be live, aligned and disjoint. Out owns
/// the new scheduler on success. A failed call leaves out null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_new(
    config: *const IxeBuildSchedulerConfig,
    out: *mut *mut IxeBuildScheduler,
    error: *mut *mut c_char,
) -> i32 {
    // SAFETY: caller supplies writable slots and a readable config.
    unsafe {
        report(error, || {
            if out.is_null() {
                return Err("null build scheduler output");
            }
            *out = ptr::null_mut();
            let config = config.as_ref().ok_or("null build scheduler config")?;
            let inner = Scheduler::new(Config {
                max_builds: config.max_builds,
                max_substitutions: config.max_substitutions,
                silent_seconds: u64::try_from(config.silent_seconds)
                    .map_err(|_| "max-silent-time must not be negative")?,
                build_seconds: u64::try_from(config.build_seconds)
                    .map_err(|_| "build timeout must not be negative")?,
                poll_seconds: config.poll_seconds,
                monitor_progress: boolean(config.monitor_progress)?,
            });
            *out = Box::into_raw(Box::new(IxeBuildScheduler { inner }));
            Ok(())
        })
    }
}

/// # Safety
/// Scheduler must be null or a live owned handle, with no concurrent users.
/// This releases bookkeeping only; the host must kill/join its own children.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_free(scheduler: *mut IxeBuildScheduler) {
    if !scheduler.is_null() {
        // SAFETY: ownership is returned once by the caller.
        drop(unsafe { Box::from_raw(scheduler) });
    }
}

/// # Safety
/// Error must be null or an owned error from this module, released exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_error_free(error: *mut c_char) {
    if !error.is_null() {
        // SAFETY: this exact allocation came from CString::into_raw above.
        drop(unsafe { CString::from_raw(error) });
    }
}

/// # Safety
/// Scheduler is exclusively borrowed and out/error are writable disjoint slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_start(
    scheduler: *mut IxeBuildScheduler,
    job_category: u32,
    occupies_slot: u32,
    respect_timeouts: u32,
    now_ms: u64,
    out: *mut u64,
    error: *mut *mut c_char,
) -> i32 {
    // SAFETY: forwarded caller contract.
    unsafe {
        output(scheduler, out, error, |scheduler| {
            scheduler.start(
                category(job_category)?,
                boolean(occupies_slot)?,
                boolean(respect_timeouts)?,
                now_ms,
            )
        })
    }
}

/// # Safety
/// Scheduler is exclusively borrowed and out/error are writable disjoint slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_stop(
    scheduler: *mut IxeBuildScheduler,
    id: u64,
    wake_sleepers: u32,
    out: *mut u32,
    error: *mut *mut c_char,
) -> i32 {
    // SAFETY: forwarded caller contract.
    unsafe {
        output(scheduler, out, error, |scheduler| {
            Ok(u32::from(scheduler.stop(id, boolean(wake_sleepers)?)))
        })
    }
}

/// # Safety
/// Scheduler is exclusively borrowed and out/error are writable disjoint slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_slot_available(
    scheduler: *mut IxeBuildScheduler,
    job_category: u32,
    out: *mut u32,
    error: *mut *mut c_char,
) -> i32 {
    // SAFETY: forwarded caller contract.
    unsafe {
        output(scheduler, out, error, |scheduler| {
            Ok(u32::from(scheduler.slot_available(category(job_category)?)))
        })
    }
}

/// # Safety
/// Scheduler is exclusively borrowed and out/error are writable disjoint slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_running(
    scheduler: *mut IxeBuildScheduler,
    job_category: u32,
    out: *mut u64,
    error: *mut *mut c_char,
) -> i32 {
    // SAFETY: forwarded caller contract.
    unsafe {
        output(scheduler, out, error, |scheduler| {
            Ok(scheduler.running(category(job_category)?))
        })
    }
}

/// # Safety
/// Scheduler is exclusively borrowed; error is a writable disjoint slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_note_output(
    scheduler: *mut IxeBuildScheduler,
    id: u64,
    now_ms: u64,
    error: *mut *mut c_char,
) -> i32 {
    // SAFETY: caller owns scheduler; pointer is checked before dereference.
    unsafe {
        report(error, || {
            scheduler
                .as_mut()
                .ok_or("null build scheduler")?
                .inner
                .note_output(id, now_ms)
        })
    }
}

/// # Safety
/// Scheduler is exclusively borrowed and out/error are writable disjoint slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_monitors_progress(
    scheduler: *mut IxeBuildScheduler,
    id: u64,
    out: *mut u32,
    error: *mut *mut c_char,
) -> i32 {
    // SAFETY: forwarded caller contract.
    unsafe {
        output(scheduler, out, error, |scheduler| {
            scheduler.monitors_progress(id).map(u32::from)
        })
    }
}

/// # Safety
/// Scheduler is exclusively borrowed and out/error are writable disjoint slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_wait_plan(
    scheduler: *mut IxeBuildScheduler,
    now_ms: u64,
    lock_waiters: u32,
    gc_poll: u32,
    out: *mut IxeBuildWaitPlan,
    error: *mut *mut c_char,
) -> i32 {
    // SAFETY: forwarded caller contract.
    unsafe {
        output(scheduler, out, error, |scheduler| {
            let timeout = scheduler.wait_plan(now_ms, boolean(lock_waiters)?, boolean(gc_poll)?)?;
            Ok(IxeBuildWaitPlan {
                has_timeout: u32::from(timeout.is_some()),
                timeout_ms: timeout.unwrap_or(0),
            })
        })
    }
}

/// # Safety
/// Scheduler is exclusively borrowed and out/error are writable disjoint slots.
/// no_progress_seconds is zero when the host observed no progress timeout.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_inspect_child(
    scheduler: *mut IxeBuildScheduler,
    id: u64,
    now_ms: u64,
    busy: u32,
    no_progress_seconds: i64,
    out: *mut IxeBuildChildDecision,
    error: *mut *mut c_char,
) -> i32 {
    // SAFETY: forwarded caller contract.
    unsafe {
        output(scheduler, out, error, |scheduler| {
            let no_progress_seconds = u64::try_from(no_progress_seconds)
                .map_err(|_| "no-progress timeout must not be negative")?;
            let timeout = scheduler.inspect_child(
                id,
                now_ms,
                boolean(busy)?,
                (no_progress_seconds != 0).then_some(no_progress_seconds),
            )?;
            Ok(match timeout {
                None => IxeBuildChildDecision::default(),
                Some(timeout) => IxeBuildChildDecision {
                    timeout_kind: match timeout.kind {
                        TimeoutKind::Silent => 1,
                        TimeoutKind::NoProgress => 2,
                        TimeoutKind::Build => 3,
                    },
                    timeout_seconds: timeout.seconds,
                },
            })
        })
    }
}

/// # Safety
/// Scheduler is exclusively borrowed and out/error are writable disjoint slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_build_scheduler_finish_poll(
    scheduler: *mut IxeBuildScheduler,
    now_ms: u64,
    lock_waiters: u32,
    out: *mut u32,
    error: *mut *mut c_char,
) -> i32 {
    // SAFETY: forwarded caller contract.
    unsafe {
        output(scheduler, out, error, |scheduler| {
            scheduler
                .finish_poll(now_ms, boolean(lock_waiters)?)
                .map(u32::from)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_lifecycle_errors_and_outputs_are_owned() {
        // SAFETY: all slots are distinct stack locals; the only handles and
        // strings used below are returned by this ABI and freed exactly once.
        unsafe {
            let config = IxeBuildSchedulerConfig {
                max_builds: 1,
                max_substitutions: 0,
                silent_seconds: 1,
                build_seconds: 2,
                poll_seconds: 60,
                monitor_progress: 1,
            };
            let mut scheduler = ptr::null_mut();
            let mut error = ptr::null_mut();
            assert_eq!(
                ixe_build_scheduler_new(&config, &mut scheduler, &mut error),
                0
            );
            assert!(error.is_null());
            let mut id = 0;
            assert_eq!(
                ixe_build_scheduler_start(scheduler, 0, 1, 1, 0, &mut id, &mut error),
                0
            );
            assert_ne!(id, 0);
            let mut count = 0;
            assert_eq!(
                ixe_build_scheduler_running(scheduler, 0, &mut count, &mut error),
                0
            );
            assert_eq!(count, 1);
            let mut yes = 9;
            assert_eq!(
                ixe_build_scheduler_slot_available(scheduler, 0, &mut yes, &mut error),
                0
            );
            assert_eq!(yes, 0);
            assert_eq!(
                ixe_build_scheduler_monitors_progress(scheduler, id, &mut yes, &mut error),
                0
            );
            assert_eq!(yes, 1);
            assert_eq!(
                ixe_build_scheduler_note_output(scheduler, id, 500, &mut error),
                0
            );
            let mut plan = IxeBuildWaitPlan::default();
            assert_eq!(
                ixe_build_scheduler_wait_plan(scheduler, 500, 1, 1, &mut plan, &mut error),
                0
            );
            assert_eq!(plan.has_timeout, 1);
            assert_eq!(plan.timeout_ms, 1_000);
            let mut decision = IxeBuildChildDecision::default();
            assert_eq!(
                ixe_build_scheduler_inspect_child(
                    scheduler,
                    id,
                    1_500,
                    1,
                    0,
                    &mut decision,
                    &mut error
                ),
                0
            );
            assert_eq!(decision.timeout_kind, 1);
            assert_eq!(decision.timeout_seconds, 1);
            assert_eq!(
                ixe_build_scheduler_finish_poll(scheduler, 1_500, 1, &mut yes, &mut error),
                0
            );
            assert_eq!(yes, 0);
            assert_eq!(
                ixe_build_scheduler_stop(scheduler, id, 1, &mut yes, &mut error),
                0
            );
            assert_eq!(yes, 1);
            assert_eq!(
                ixe_build_scheduler_stop(scheduler, id, 1, &mut yes, &mut error),
                0
            );
            assert_eq!(yes, 0);
            assert_eq!(
                ixe_build_scheduler_monitors_progress(scheduler, id, &mut yes, &mut error),
                1
            );
            assert!(!error.is_null());
            assert_eq!(yes, 0);
            ixe_build_scheduler_error_free(error);
            assert_eq!(
                ixe_build_scheduler_slot_available(scheduler, 99, &mut yes, &mut error),
                1
            );
            ixe_build_scheduler_error_free(error);
            assert_eq!(
                ixe_build_scheduler_start(scheduler, 0, 2, 1, 0, &mut id, &mut error),
                1
            );
            assert_eq!(id, 0);
            ixe_build_scheduler_error_free(error);
            assert_eq!(
                ixe_build_scheduler_running(ptr::null_mut(), 0, &mut count, &mut error),
                1
            );
            assert_eq!(count, 0);
            ixe_build_scheduler_error_free(error);
            assert_eq!(
                ixe_build_scheduler_running(scheduler, 0, ptr::null_mut(), &mut error),
                1
            );
            ixe_build_scheduler_error_free(error);
            ixe_build_scheduler_free(scheduler);
            ixe_build_scheduler_free(ptr::null_mut());
            ixe_build_scheduler_error_free(ptr::null_mut());
            assert_eq!(
                ixe_build_scheduler_new(ptr::null(), &mut scheduler, &mut error),
                1
            );
            assert!(scheduler.is_null());
            ixe_build_scheduler_error_free(error);
        }
    }
}
