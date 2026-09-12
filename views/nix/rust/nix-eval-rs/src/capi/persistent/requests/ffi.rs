use super::{Batch, MAX_FILE_BYTES, MAX_OUTPUT_BYTES, MAX_REPORT_BYTES, MAX_REQUESTS};
use crate::capi::{IXE_ERR_BADCALL, IXE_OK, IxeBytes, c_char, out_string};
use std::panic::{AssertUnwindSafe, catch_unwind};

pub struct IxePersistentRequests(Batch);

#[repr(C)]
pub struct IxePersistentRequestView {
    installable: IxeBytes,
    apply: IxeBytes,
    has_apply: bool,
}

#[repr(C)]
pub struct IxePersistentLimits {
    file_bytes: usize,
    requests: usize,
    report_bytes: usize,
    output_bytes: usize,
}

#[unsafe(no_mangle)]
pub extern "C" fn ixe_persistent_limits() -> IxePersistentLimits {
    IxePersistentLimits {
        file_bytes: MAX_FILE_BYTES,
        requests: MAX_REQUESTS,
        report_bytes: MAX_REPORT_BYTES,
        output_bytes: MAX_OUTPUT_BYTES,
    }
}

fn view(value: &str) -> IxeBytes {
    IxeBytes { text: value.as_ptr(), len: value.len() }
}

unsafe fn error_boundary(error: *mut *mut c_char, body: impl FnOnce() -> Result<(), String>) -> i32 {
    if error.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: callers provide a writable output slot for the duration of the call.
    unsafe { *error = std::ptr::null_mut() };
    let message = match catch_unwind(AssertUnwindSafe(body)) {
        Ok(Ok(())) => return IXE_OK,
        Ok(Err(message)) => message,
        Err(_) => "panic handling persistent request file".to_owned(),
    };
    // The validated slot receives a string owned by the caller.
    out_string(message, error);
    IXE_ERR_BADCALL
}

/// # Safety
/// `input` must be readable for its length; output slots must be writable.
/// The returned batch must be released exactly once with requests_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_persistent_requests_new(
    input: IxeBytes,
    output: *mut *mut IxePersistentRequests,
    error: *mut *mut c_char,
) -> i32 {
    if output.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: output is a writable slot under this function's contract.
    unsafe { *output = std::ptr::null_mut() };
    // SAFETY: error follows the output-slot contract.
    unsafe { error_boundary(error, || {
        if input.len > MAX_FILE_BYTES {
            return Err("request file exceeds 4 MiB".to_owned());
        }
        // SAFETY: the borrowed input remains live throughout this call.
        let text = super::super::text(input)?;
        let batch = Batch::parse(text.as_bytes())?;
        *output = Box::into_raw(Box::new(IxePersistentRequests(batch)));
        Ok(())
    }) }
}

/// # Safety
/// `batch` must be null or a live owned pointer returned by requests_new.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_persistent_requests_free(batch: *mut IxePersistentRequests) {
    if !batch.is_null() {
        // SAFETY: the caller transfers the unique batch allocation back once.
        drop(unsafe { Box::from_raw(batch) });
    }
}

/// # Safety
/// `batch` must be live and exclusively borrowed. Slots must be writable.
/// Returned text views remain valid until batch destruction; they are not owned.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_persistent_requests_next(
    batch: *mut IxePersistentRequests,
    output: *mut IxePersistentRequestView,
    done: *mut bool,
    error: *mut *mut c_char,
) -> i32 {
    if batch.is_null() || output.is_null() || done.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: all pointers satisfy the documented ownership/output contract.
    unsafe { error_boundary(error, || {
        *done = false;
        *output = IxePersistentRequestView { installable: view(""), apply: view(""), has_apply: false };
        match (*batch).0.begin()? {
            None => *done = true,
            Some(request) => {
                *output = IxePersistentRequestView {
                    installable: view(&request.installable),
                    apply: view(request.apply.as_deref().unwrap_or("")),
                    has_apply: request.apply.is_some(),
                };
            }
        }
        Ok(())
    }) }
}

/// # Safety
/// `batch` must be live and exclusively borrowed; payload readable for its length.
/// On success output is owned and uses ixe_string_free. Framing failure poisons
/// the batch, and a reported evaluation failure prohibits later requests.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_persistent_requests_complete(
    batch: *mut IxePersistentRequests,
    success: bool,
    payload: IxeBytes,
    output: *mut *mut c_char,
    error: *mut *mut c_char,
) -> i32 {
    if batch.is_null() || output.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: pointers and views satisfy the documented contracts.
    unsafe {
        *output = std::ptr::null_mut();
        let status = error_boundary(error, || {
            if payload.len > MAX_REPORT_BYTES {
                (*batch).0.phase = super::Phase::Failed;
                return Err("request report exceeds 8 MiB".to_owned());
            }
            let text = match super::super::text(payload) {
                Ok(text) => text,
                Err(error) => {
                    (*batch).0.phase = super::Phase::Failed;
                    return Err(error);
                }
            };
            let encoded = (*batch).0.complete(success, text)?;
            if out_string(encoded, output) != IXE_OK {
                (*batch).0.phase = super::Phase::Failed;
                return Err("cannot allocate persistent request report".to_owned());
            }
            Ok(())
        });
        if status != IXE_OK {
            (*batch).0.phase = super::Phase::Failed;
        }
        status
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::capi::ixe_string_free;
    use std::ffi::CStr;

    #[test]
    fn abi_owns_input_and_preserves_borrowed_views_through_completion() {
        let input = String::from(r#"{"version":1,"requests":[{"id":"opaque\u0000id","installable":"value","apply":"x: x + 1"}]}"#);
        let mut batch = std::ptr::null_mut();
        let mut error = std::ptr::null_mut();
        // SAFETY: every output slot is live, and the input is readable for the
        // call. The owned batch and returned strings are each freed once below.
        unsafe {
            assert_eq!(ixe_persistent_requests_new(view(&input), &mut batch, &mut error), IXE_OK);
            assert!(error.is_null());
            drop(input);
            let mut request = IxePersistentRequestView {
                installable: view(""), apply: view(""), has_apply: false,
            };
            let mut done = false;
            assert_eq!(ixe_persistent_requests_next(batch, &mut request, &mut done, &mut error), IXE_OK);
            assert!(!done);
            assert!(request.has_apply);
            assert_eq!(super::super::super::text(request.apply).as_deref(), Ok("x: x + 1"));
            let saved_installable = request.installable;
            let mut output = std::ptr::null_mut();
            assert_eq!(ixe_persistent_requests_complete(batch, true,
                view(r#"{"installable":"value","value":42}"#), &mut output, &mut error), IXE_OK);
            let encoded = CStr::from_ptr(output).to_str();
            assert!(encoded.is_ok_and(|text| text.contains(r#""id":"opaque\u0000id""#)));
            ixe_string_free(output);
            assert_eq!(super::super::super::text(saved_installable).as_deref(), Ok("value"));
            assert_eq!(ixe_persistent_requests_next(batch, &mut request, &mut done, &mut error), IXE_OK);
            assert!(done);
            ixe_persistent_requests_free(batch);
        }
    }

    #[test]
    fn abi_rejects_null_input_and_invalid_utf8_without_publishing_a_batch() {
        for input in [
            IxeBytes { text: std::ptr::null(), len: 1 },
            IxeBytes { text: b"\xff".as_ptr(), len: 1 },
        ] {
            let mut batch = std::ptr::null_mut();
            let mut error = std::ptr::null_mut();
            // SAFETY: null input is explicitly rejected by the API; the other
            // view points to one readable static byte. Slots are writable.
            unsafe {
                assert_eq!(ixe_persistent_requests_new(input, &mut batch, &mut error), IXE_ERR_BADCALL);
                assert!(batch.is_null());
                assert!(!error.is_null());
                ixe_string_free(error);
                ixe_persistent_requests_free(batch);
            }
        }
    }
}
