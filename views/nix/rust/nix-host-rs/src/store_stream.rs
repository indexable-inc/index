//! C ABI adapter for the independent safe store-stream core.

use nix_store_stream::{
    HashAlgorithm, ModuloHasher, REFERENCE_LENGTH, RefScanner, Result, RewriteRule, Rewriter,
    StreamError,
};
use std::ffi::c_void;
use std::panic::{AssertUnwindSafe, catch_unwind};

fn hash_algorithm(value: u32) -> Result<HashAlgorithm> {
    match value {
        0 => Ok(HashAlgorithm::Md5),
        1 => Ok(HashAlgorithm::Sha1),
        2 => Ok(HashAlgorithm::Sha256),
        3 => Ok(HashAlgorithm::Sha512),
        4 => Ok(HashAlgorithm::Blake3),
        _ => Err(StreamError::Invalid),
    }
}

// The C boundary uses independent handles: no VM, store callback, or global
// mutable state is reachable from this module.
struct Handle<T> {
    value: T,
    failed: bool,
}

#[repr(C)]
pub struct IxsBytes {
    pub data: *const u8,
    pub len: usize,
}

#[repr(C)]
pub struct IxsRewrite {
    pub from: IxsBytes,
    pub to: IxsBytes,
}

#[repr(C)]
pub struct IxsDigest {
    pub bytes: [u8; 64],
    pub len: usize,
    pub input_bytes: u64,
}

type Emit = unsafe extern "C" fn(*mut c_void, *const u8, usize) -> i32;

fn status(operation: impl FnOnce() -> Result<()>) -> i32 {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => 0,
        Ok(Err(StreamError::Invalid)) => 1,
        Ok(Err(StreamError::Failed)) => 2,
        Err(_) => 3,
        Ok(Err(StreamError::Sink)) => 4,
    }
}

unsafe fn bytes<'a>(pointer: *const u8, length: usize) -> Result<&'a [u8]> {
    if length == 0 {
        return Ok(&[]);
    }
    if pointer.is_null() || length > isize::MAX as usize {
        return Err(StreamError::Invalid);
    }
    // SAFETY: the ABI requires this non-null range to be readable for the call.
    Ok(unsafe { std::slice::from_raw_parts(pointer, length) })
}

unsafe fn operate<T>(pointer: *mut c_void, operation: impl FnOnce(&mut T) -> Result<()>) -> i32 {
    status(|| {
        // SAFETY: non-null handles must come from the matching constructor and
        // are exclusively borrowed until this call returns.
        let handle = unsafe { pointer.cast::<Handle<T>>().as_mut() }.ok_or(StreamError::Invalid)?;
        if handle.failed {
            return Err(StreamError::Failed);
        }
        handle.failed = true;
        operation(&mut handle.value)?;
        handle.failed = false;
        Ok(())
    })
}

unsafe fn create<T>(output: *mut *mut c_void, constructor: impl FnOnce() -> Result<T>) -> i32 {
    status(|| {
        // SAFETY: a non-null output points to writable, properly aligned storage.
        let output = unsafe { output.as_mut() }.ok_or(StreamError::Invalid)?;
        *output = std::ptr::null_mut();
        *output = Box::into_raw(Box::new(Handle {
            value: constructor()?,
            failed: false,
        }))
        .cast();
        Ok(())
    })
}

unsafe fn destroy<T>(pointer: *mut c_void) {
    if !pointer.is_null() {
        // SAFETY: ownership is returned once to the matching Rust constructor.
        drop(unsafe { Box::from_raw(pointer.cast::<Handle<T>>()) });
    }
}

/// # Safety
/// See ix-store-stream.h: readable packed hashes and a writable output handle.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixs_refscan_new(
    hashes: *const u8,
    count: usize,
    output: *mut *mut c_void,
) -> i32 {
    unsafe {
        create(output, || {
            RefScanner::new(bytes(
                hashes,
                count
                    .checked_mul(REFERENCE_LENGTH)
                    .ok_or(StreamError::Invalid)?,
            )?)
        })
    }
}

/// # Safety
/// The handle is live and exclusive; data is readable for len bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixs_refscan_feed(handle: *mut c_void, data: *const u8, len: usize) -> i32 {
    unsafe {
        operate(handle, |scanner: &mut RefScanner| {
            scanner.feed(bytes(data, len)?);
            Ok(())
        })
    }
}

/// # Safety
/// The handle is live, indices has capacity writable elements, and count is writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixs_refscan_result(
    handle: *mut c_void,
    indices: *mut usize,
    capacity: usize,
    count: *mut usize,
) -> i32 {
    unsafe {
        operate(handle, |scanner: &mut RefScanner| {
            let count = count.as_mut().ok_or(StreamError::Invalid)?;
            if capacity < scanner.found().len()
                || (!scanner.found().is_empty() && indices.is_null())
            {
                return Err(StreamError::Invalid);
            }
            *count = scanner.found().len();
            if !scanner.found().is_empty() {
                std::ptr::copy_nonoverlapping(
                    scanner.found().as_ptr(),
                    indices,
                    scanner.found().len(),
                );
            }
            Ok(())
        })
    }
}

/// # Safety
/// Null or an exclusively owned scanner handle, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixs_refscan_free(handle: *mut c_void) {
    unsafe {
        destroy::<RefScanner>(handle);
    }
}

/// # Safety
/// All rule byte ranges are readable; output is writable. Rules are copied.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixs_rewriter_new(
    rules: *const IxsRewrite,
    count: usize,
    output: *mut *mut c_void,
) -> i32 {
    unsafe {
        create(output, || {
            if count > isize::MAX as usize / std::mem::size_of::<IxsRewrite>()
                || (count != 0 && rules.is_null())
            {
                return Err(StreamError::Invalid);
            }
            let rules = if count == 0 {
                &[]
            } else {
                std::slice::from_raw_parts(rules, count)
            };
            Rewriter::new(
                rules
                    .iter()
                    .map(|rule| {
                        Ok(RewriteRule {
                            from: bytes(rule.from.data, rule.from.len)?.to_vec(),
                            to: bytes(rule.to.data, rule.to.len)?.to_vec(),
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            )
        })
    }
}

/// # Safety
/// Handle/data satisfy the stream contract. emit must not unwind or retain bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixs_rewriter_feed(
    handle: *mut c_void,
    data: *const u8,
    len: usize,
    finish: bool,
    emit: Option<Emit>,
    context: *mut c_void,
) -> i32 {
    unsafe {
        operate(handle, |rewriter: &mut Rewriter| {
            let emit = emit.ok_or(StreamError::Invalid)?;
            rewriter.feed(bytes(data, len)?, finish, |output| {
                if emit(context, output.as_ptr(), output.len()) == 0 {
                    Ok(())
                } else {
                    Err(StreamError::Sink)
                }
            })
        })
    }
}

/// # Safety
/// Null or an exclusively owned rewriter handle, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixs_rewriter_free(handle: *mut c_void) {
    unsafe {
        destroy::<Rewriter>(handle);
    }
}

/// # Safety
/// modulus is readable, output is writable. Algorithm IDs are defined in the header.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixs_modulo_new(
    algorithm: u32,
    modulus: *const u8,
    len: usize,
    output: *mut *mut c_void,
) -> i32 {
    unsafe {
        create(output, || {
            ModuloHasher::new(hash_algorithm(algorithm)?, bytes(modulus, len)?)
        })
    }
}

/// # Safety
/// The handle is live and exclusive; data is readable for len bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixs_modulo_feed(handle: *mut c_void, data: *const u8, len: usize) -> i32 {
    unsafe {
        operate(handle, |hasher: &mut ModuloHasher| {
            hasher.feed(bytes(data, len)?)
        })
    }
}

/// # Safety
/// The handle is live and exclusive; output is writable and does not alias it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixs_modulo_finish(handle: *mut c_void, output: *mut IxsDigest) -> i32 {
    unsafe {
        operate(handle, |hasher: &mut ModuloHasher| {
            let output = output.as_mut().ok_or(StreamError::Invalid)?;
            let digest = hasher.finish()?;
            let destination = output
                .bytes
                .get_mut(..digest.len())
                .ok_or(StreamError::Invalid)?;
            destination.copy_from_slice(digest);
            output.len = digest.len();
            output.input_bytes = hasher.input_bytes();
            Ok(())
        })
    }
}

/// # Safety
/// Null or an exclusively owned modulo-hash handle, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixs_modulo_free(handle: *mut c_void) {
    unsafe {
        destroy::<ModuloHasher>(handle);
    }
}

#[cfg(test)]
mod tests;
