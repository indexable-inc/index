use super::*;
use sha2::Digest as _;

const FIRST: &[u8; 32] = b"dc04vv14dak1c1r48qa0m23vr9jy8sm0";

unsafe extern "C" fn collect(context: *mut c_void, data: *const u8, len: usize) -> i32 {
    // SAFETY: tests supply a live Vec and the stream borrows initialized bytes.
    unsafe {
        (&mut *context.cast::<Vec<u8>>()).extend_from_slice(std::slice::from_raw_parts(data, len));
    }
    0
}

unsafe extern "C" fn refuse(_: *mut c_void, _: *const u8, _: usize) -> i32 {
    1
}

#[test]
fn ffi_rewriter_copies_rules_and_borrows_each_emitted_chunk() {
    let mut handle = std::ptr::null_mut();
    let mut output = Vec::<u8>::new();
    // SAFETY: constructor copies these stack-owned rules; callback context
    // outlives all feed calls and owns storage separate from the stream.
    unsafe {
        {
            let from = b"abc".to_vec();
            let to = b"xyz".to_vec();
            let rule = IxsRewrite {
                from: IxsBytes {
                    data: from.as_ptr(),
                    len: from.len(),
                },
                to: IxsBytes {
                    data: to.as_ptr(),
                    len: to.len(),
                },
            };
            assert_eq!(ixs_rewriter_new(&rule, 1, &mut handle), 0);
        }
        let context = (&raw mut output).cast();
        assert_eq!(
            ixs_rewriter_feed(handle, b"ab".as_ptr(), 2, false, Some(collect), context),
            0
        );
        assert!(output.is_empty());
        assert_eq!(
            ixs_rewriter_feed(handle, b"c!".as_ptr(), 2, true, Some(collect), context),
            0
        );
        assert_eq!(output, b"xyz!");
        assert_eq!(
            ixs_rewriter_feed(handle, std::ptr::null(), 0, true, Some(collect), context),
            0
        );
        assert_eq!(output, b"xyz!");
        ixs_rewriter_free(handle);
    }
}

#[test]
fn ffi_rewriter_catches_callback_failure_and_poisons_the_handle() {
    let mut handle = std::ptr::null_mut();
    // SAFETY: all pointers below refer to live test-owned storage, and each
    // successfully created handle is released once with its matching function.
    unsafe {
        assert_eq!(ixs_rewriter_new(std::ptr::null(), 0, &mut handle), 0);
        assert_eq!(
            ixs_rewriter_feed(
                handle,
                b"x".as_ptr(),
                1,
                false,
                Some(refuse),
                std::ptr::null_mut()
            ),
            4
        );
        let mut output = Vec::<u8>::new();
        assert_eq!(
            ixs_rewriter_feed(
                handle,
                b"y".as_ptr(),
                1,
                true,
                Some(collect),
                (&raw mut output).cast()
            ),
            2
        );
        assert!(output.is_empty());
        ixs_rewriter_free(handle);
    }
}

#[test]
fn ffi_rejects_null_ranges_and_oversized_counts_without_dereferencing_them() {
    let mut handle = std::ptr::dangling_mut::<c_void>();
    // SAFETY: invalid pointers are used only with validated empty ranges or
    // lengths that the ABI rejects before dereferencing.
    unsafe {
        assert_eq!(ixs_refscan_new(std::ptr::null(), 1, &mut handle), 1);
        assert!(handle.is_null());
        assert_eq!(
            ixs_refscan_new(std::ptr::null(), usize::MAX, &mut handle),
            1
        );
        assert_eq!(
            ixs_rewriter_new(std::ptr::null(), usize::MAX, &mut handle),
            1
        );
        assert_eq!(ixs_modulo_new(2, b"x".as_ptr(), 1, std::ptr::null_mut()), 1);
        assert_eq!(ixs_modulo_new(99, b"x".as_ptr(), 1, &mut handle), 1);
        assert!(handle.is_null());
        assert_eq!(ixs_modulo_new(2, b"x".as_ptr(), 1, &mut handle), 0);
        assert_eq!(ixs_modulo_feed(handle, std::ptr::null(), 1), 1);
        let mut digest = IxsDigest {
            bytes: [0; 64],
            len: 0,
            input_bytes: 0,
        };
        assert_eq!(ixs_modulo_finish(handle, &raw mut digest), 2);
        ixs_modulo_free(handle);
        ixs_refscan_free(std::ptr::null_mut());
        ixs_rewriter_free(std::ptr::null_mut());
        ixs_modulo_free(std::ptr::null_mut());
    }
}

#[test]
fn ffi_scanner_returns_candidate_indices_and_hash_finish_preserves_count() {
    let mut handle = std::ptr::null_mut();
    // SAFETY: pointers, capacities, and handle ownership follow the ABI.
    unsafe {
        assert_eq!(ixs_refscan_new(FIRST.as_ptr(), 1, &mut handle), 0);
        assert_eq!(ixs_refscan_feed(handle, FIRST.as_ptr(), FIRST.len()), 0);
        let mut index = usize::MAX;
        let mut count = 0;
        assert_eq!(ixs_refscan_result(handle, &mut index, 1, &mut count), 0);
        assert_eq!((index, count), (0, 1));
        ixs_refscan_free(handle);
        assert_eq!(
            ixs_modulo_new(2, FIRST.as_ptr(), FIRST.len(), &mut handle),
            0
        );
        assert_eq!(ixs_modulo_feed(handle, FIRST.as_ptr(), FIRST.len()), 0);
        let mut digest = IxsDigest {
            bytes: [0; 64],
            len: 0,
            input_bytes: 0,
        };
        assert_eq!(ixs_modulo_finish(handle, &raw mut digest), 0);
        assert_eq!(digest.len, 32);
        assert_eq!(digest.input_bytes, 32);
        assert_eq!(
            &digest.bytes[..32],
            sha2::Sha256::digest([vec![0; 32], b"|0".to_vec()].concat()).as_slice()
        );
        assert_eq!(ixs_modulo_feed(handle, b"x".as_ptr(), 1), 2);
        ixs_modulo_free(handle);
    }
}

#[test]
fn caught_panics_poison_state_instead_of_reusing_a_partial_operation() {
    let mut handle = std::ptr::null_mut();
    // SAFETY: test-owned valid handle, exclusively borrowed and freed once.
    unsafe {
        assert_eq!(ixs_refscan_new(FIRST.as_ptr(), 1, &mut handle), 0);
        assert_eq!(
            operate(handle, |_: &mut RefScanner| {
                panic!("injected failure");
            }),
            3
        );
        assert_eq!(ixs_refscan_feed(handle, FIRST.as_ptr(), FIRST.len()), 2);
        ixs_refscan_free(handle);
    }
}
