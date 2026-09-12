//! Execute a strict check under the question's captured policy and recording.
use super::*;
use crate::flake_check::Report;
use crate::session::Question;

/// # Safety
/// Session is exclusively borrowed and root is its live handle. Output is a
/// writable owned-string slot, released with ixe_string_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_check_execute(
    session: *mut IxeSession,
    root: u64,
    out: *mut *mut c_char,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null flake-check output");
    }
    // SAFETY: caller provides a writable output slot, checked non-null.
    unsafe { *out = std::ptr::null_mut() };
    let Some(Question::FlakeCheck { selection, options }) =
        session.question.as_ref().map(|q| q.question.clone())
    else {
        return session.bad("flake check requires its own question in flight");
    };
    if selection.apply.is_some() || selection.auto_call {
        return session.bad("flake check does not accept transformations");
    }
    let Some(root) = session.get(root).cloned() else {
        return session.bad("unknown flake-check root");
    };
    let selected =
        match command::select(session, root, &selection.attr_paths, selection.index_lists) {
            Ok(selected) => selected.slot,
            Err(status) => return status,
        };
    let interrupt = session.host.interrupt();
    let mut fresh = crate::eval::JobMemo::default();
    let (vm, host, memo) = machine_and_host(session, &mut fresh);
    match crate::flake_check::execute(vm, host, memo, selected, options, interrupt.as_deref()) {
        Ok(report) => out_string(report.encode(), out),
        Err(error) => session.fail(&error),
    }
}

/// An owned decoded report. All record/message views borrow this allocation.
pub struct IxeFlakeCheckReport {
    report: Report,
    paths: Vec<String>,
    omitted: Vec<String>,
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct IxeFlakeCheckCounts {
    pub derivations: usize,
    pub errors: usize,
    pub omitted_systems: usize,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeFlakeCheckDerivation {
    pub attribute_path: IxeBytes,
    pub drv_path: IxeBytes,
    pub build: i32,
}

fn view(text: &str) -> IxeBytes {
    IxeBytes {
        text: text.as_ptr(),
        len: text.len(),
    }
}

/// # Safety
/// Input byte spans must be readable for their lengths. Output/error must be
/// disjoint writable slots. The returned report is freed with report_free;
/// diagnostics are freed with ixe_string_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_check_report_decode(
    text: IxeBytes,
    store_dir: IxeBytes,
    local_system: IxeBytes,
    flags: i32,
    out: *mut *mut IxeFlakeCheckReport,
    error: *mut *mut c_char,
) -> i32 {
    if out.is_null() || error.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller supplies disjoint writable output slots, checked non-null.
    unsafe {
        *out = std::ptr::null_mut();
        *error = std::ptr::null_mut();
    }
    let result = (|| {
        let read = |bytes: IxeBytes| -> Result<&[u8], String> {
            if bytes.len == 0 {
                return Ok(&[]);
            }
            if bytes.text.is_null() || bytes.len > isize::MAX as usize {
                return Err("invalid flake-check byte span".into());
            }
            // SAFETY: caller guarantees readable span; null and length checked.
            Ok(unsafe { std::slice::from_raw_parts(bytes.text, bytes.len) })
        };
        let store = std::str::from_utf8(read(store_dir)?).map_err(|e| e.to_string())?;
        let system = std::str::from_utf8(read(local_system)?).map_err(|e| e.to_string())?;
        if system.is_empty() {
            return Err("empty flake-check current system".into());
        }
        let options = crate::flake_check::Options::from_flags(flags)?;
        let report = Report::decode(read(text)?, store, options, system)?;
        let paths = report
            .derivations
            .iter()
            .map(|drv| crate::flake_check::path_text(&drv.path))
            .collect();
        let omitted = report.omitted_systems.iter().cloned().collect();
        Ok(IxeFlakeCheckReport {
            report,
            paths,
            omitted,
        })
    })();
    match result {
        Ok(report) => {
            // SAFETY: output slot was validated above; ownership passes to caller.
            unsafe {
                *out = Box::into_raw(Box::new(report));
            }
            IXE_OK
        }
        Err(why) => {
            out_string(why, error);
            IXE_ERR_BADCALL
        }
    }
}

/// # Safety
/// Report is null or a live allocation returned by report_decode, freed once.
/// No borrowed views may be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_check_report_free(report: *mut IxeFlakeCheckReport) {
    if !report.is_null() {
        // SAFETY: caller transfers the live allocation returned by Box::into_raw.
        drop(unsafe { Box::from_raw(report) });
    }
}

/// # Safety
/// Report is a live decoded allocation; out is writable and disjoint from it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_check_report_counts(
    report: *const IxeFlakeCheckReport,
    out: *mut IxeFlakeCheckCounts,
) -> i32 {
    if out.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller guarantees writable output, checked non-null.
    unsafe {
        *out = IxeFlakeCheckCounts::default();
    }
    if report.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller guarantees a live immutable allocation, checked non-null.
    let report = unsafe { &*report };
    // SAFETY: output is writable and disjoint from report by contract.
    unsafe {
        *out = IxeFlakeCheckCounts {
            derivations: report.report.derivations.len(),
            errors: report.report.errors.len(),
            omitted_systems: report.omitted.len(),
        };
    }
    IXE_OK
}

/// # Safety
/// Report is live and out is a disjoint writable slot. Returned views remain
/// readable until report_free; no mutation or ownership is transferred.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_check_report_derivation(
    report: *const IxeFlakeCheckReport,
    index: usize,
    out: *mut IxeFlakeCheckDerivation,
) -> i32 {
    if out.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: output is writable and non-null.
    unsafe {
        *out = IxeFlakeCheckDerivation {
            attribute_path: view(""),
            drv_path: view(""),
            build: 0,
        };
    }
    if report.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller supplies a live immutable report allocation.
    let report = unsafe { &*report };
    let Some(drv) = report.report.derivations.get(index) else {
        return IXE_ERR_BADCALL;
    };
    let Some(path) = report.paths.get(index) else {
        return IXE_ERR_BADCALL;
    };
    // SAFETY: writable output does not alias report; strings borrow the live owner.
    unsafe {
        *out = IxeFlakeCheckDerivation {
            attribute_path: view(path),
            drv_path: view(&drv.drv_path),
            build: i32::from(drv.build),
        };
    }
    IXE_OK
}

/// # Safety
/// Report is live and out is a disjoint writable slot. Kind 0 selects errors,
/// kind 1 omitted systems. Returned bytes borrow report until report_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_check_report_message(
    report: *const IxeFlakeCheckReport,
    kind: i32,
    index: usize,
    out: *mut IxeBytes,
) -> i32 {
    if out.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: output is writable and non-null.
    unsafe {
        *out = view("");
    }
    if report.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller guarantees a live immutable report allocation.
    let report = unsafe { &*report };
    let messages = match kind {
        0 => &report.report.errors,
        1 => &report.omitted,
        _ => return IXE_ERR_BADCALL,
    };
    let Some(message) = messages.get(index) else {
        return IXE_ERR_BADCALL;
    };
    // SAFETY: output is disjoint and writable; bytes borrow live owner.
    unsafe {
        *out = view(message);
    }
    IXE_OK
}
