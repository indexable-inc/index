//! External callback code needs an immutable identity before sharing persisted answers.
use nix_eval_rs::{capi::*, eval};
use std::ffi::{CStr, c_char, c_void};

unsafe extern "C" fn trace(_ctx: *mut c_void, _message: *const u8, _len: usize) {}

fn take_text(pointer: *mut c_char) -> String {
    if pointer.is_null() {
        return String::new();
    }
    // SAFETY: callers pass owned C strings returned by this ABI, once each.
    let text = unsafe { CStr::from_ptr(pointer) }
        .to_string_lossy()
        .into_owned();
    // SAFETY: the owned allocation above has not been freed previously.
    unsafe {
        ixe_string_free(pointer);
    }
    text
}

struct Fixture {
    session: *mut IxeSession,
    dir: std::path::PathBuf,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // SAFETY: this fixture owns its live session; null resets the cache setting.
        unsafe {
            ixe_session_free(self.session);
            ixe_set_eval_cache_dir(std::ptr::null(), 0);
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn external_host_requires_identity_before_any_persistent_eval() -> Result<(), String> {
    assert_eq!(eval::host_build_identity(), None);
    let host = IxeHostVtable {
        trace: Some(trace),
        ..IxeHostVtable::empty()
    };
    // SAFETY: the vtable is live and copied by session creation.
    let session = unsafe { ixe_session_new(&host) };
    assert!(!session.is_null(), "uncached embedding must remain usable");
    let fixture = Fixture {
        session,
        dir: std::env::temp_dir().join(format!(
            "ixe-host-build-identity-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|error| error.to_string())?
                .as_nanos()
        )),
    };
    let source = b"1 + 2";
    let mut handle = 0;
    // SAFETY: live session, source and output; omitted optional strings use null/zero.
    assert_eq!(
        unsafe {
            ixe_session_eval(
                session,
                source.as_ptr(),
                source.len(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                &mut handle,
            )
        },
        0
    );
    let path = fixture.dir.to_string_lossy();
    // SAFETY: the setter copies this live path.
    unsafe {
        ixe_set_eval_cache_dir(path.as_ptr(), path.len());
    }
    // SAFETY: the vtable is live and copied.
    assert!(unsafe { ixe_session_new(&host) }.is_null());
    assert!(take_text(ixe_take_setting_conflict()).contains("requires a host build identity"));
    // SAFETY: same valid live session as above, now with persistence configured.
    assert_eq!(
        unsafe {
            ixe_session_eval(
                session,
                source.as_ptr(),
                source.len(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                &mut handle,
            )
        },
        4
    );
    let mut mode = 0;
    let mut answer = std::ptr::null_mut();
    // SAFETY: live session/source/output slots; all optional arrays are empty.
    assert_eq!(
        unsafe {
            ixe_session_eval_question(
                session,
                source.as_ptr(),
                source.len(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                0,
                std::ptr::null(),
                0,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                0,
                0,
                &mut mode,
                &mut handle,
                &mut answer,
            )
        },
        4
    );
    assert!(answer.is_null());
    assert_eq!(handle, 0);
    // SAFETY: all source/host/output pointers are live; optional slots are null.
    assert_eq!(
        unsafe {
            ixe_eval_expr(
                &host,
                source.as_ptr(),
                source.len(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                &mut answer,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        },
        4
    );
    assert!(take_text(answer).contains("requires a host build identity"));
    // Pure Rust host code belongs to the Rust compiler fingerprint already.
    let empty = IxeHostVtable::empty();
    // SAFETY: local vtable is copied; returned session is freed once.
    let pure = unsafe { ixe_session_new(&empty) };
    assert!(!pure.is_null());
    unsafe {
        ixe_session_free(pure);
    }
    // Invalid identities must not consume the set-once slot.
    for invalid in [b"".as_slice(), b"\xff".as_slice()] {
        // SAFETY: slice is readable for its declared length.
        assert_eq!(
            unsafe { ixe_set_host_build_identity(invalid.as_ptr(), invalid.len()) },
            4
        );
    }
    let identity = b"/immutable/test-host-a";
    // SAFETY: immutable byte slice lives through both calls.
    assert_eq!(
        unsafe { ixe_set_host_build_identity(identity.as_ptr(), identity.len()) },
        0
    );
    assert_eq!(
        unsafe { ixe_set_host_build_identity(identity.as_ptr(), identity.len()) },
        0
    );
    let other = b"/immutable/test-host-b";
    // SAFETY: immutable byte slice lives through the call.
    assert_eq!(
        unsafe { ixe_set_host_build_identity(other.as_ptr(), other.len()) },
        4
    );
    assert!(take_text(ixe_take_setting_conflict()).contains("host build identity"));
    assert_eq!(eval::host_build_identity(), Some("/immutable/test-host-a"));
    // SAFETY: the old session reloads the newly supplied identity before evaluating.
    assert_eq!(
        unsafe {
            ixe_session_eval(
                session,
                source.as_ptr(),
                source.len(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                &mut handle,
            )
        },
        0
    );
    // SAFETY: callbacks remain live, and the new persistent session is freed once.
    let identified = unsafe { ixe_session_new(&host) };
    assert!(!identified.is_null());
    unsafe {
        ixe_session_free(identified);
    }
    Ok(())
}
