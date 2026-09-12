//! Owned document/report boundary for the C++ traversal and logger adapters.
use super::{IXE_ERR_BADCALL, IXE_OK, IxeBytes, out_string};
use crate::flake_show::{Document, Kind, Report};
use std::ffi::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

pub struct IxeFlakeShowDocument(Document);
pub struct IxeFlakeShowReport {
    report: Report,
    warnings: Vec<IxeFlakeShowWarning>,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeFlakeShowChild {
    pub name: IxeBytes,
    pub node: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeFlakeShowWarning {
    pub kind: u32,
    pub message: IxeBytes,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeFlakeShowReportView {
    pub output: IxeBytes,
    pub warnings: *const IxeFlakeShowWarning,
    pub warnings_len: usize,
}

type Result<T> = std::result::Result<T, String>;

fn run(error: *mut *mut c_char, action: impl FnOnce() -> Result<()>) -> i32 {
    if error.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: every caller requires a writable error slot.
    unsafe {
        *error = ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(action))
        .unwrap_or_else(|_| Err("flake-show operation panicked".into()));
    match result {
        Ok(()) => IXE_OK,
        Err(why) => {
            let _ = out_string(why, error);
            IXE_ERR_BADCALL
        }
    }
}

unsafe fn bytes<'a>(view: IxeBytes) -> Result<&'a [u8]> {
    if view.len == 0 {
        return Ok(&[]);
    }
    if view.text.is_null() || view.len > isize::MAX as usize {
        return Err("invalid flake-show byte span".into());
    }
    // SAFETY: non-null, addressable length checked; allocation validity belongs to the caller.
    Ok(unsafe { std::slice::from_raw_parts(view.text, view.len) })
}

unsafe fn text(view: IxeBytes) -> Result<String> {
    // SAFETY: forwarded byte-span contract.
    std::str::from_utf8(unsafe { bytes(view)? })
        .map(str::to_owned)
        .map_err(|error| error.to_string())
}

fn view(text: &str) -> IxeBytes {
    IxeBytes {
        text: text.as_ptr(),
        len: text.len(),
    }
}

/// # Safety
/// `out` and `error` must be distinct writable slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_show_new(
    out: *mut *mut IxeFlakeShowDocument,
    error: *mut *mut c_char,
) -> i32 {
    run(error, || {
        if out.is_null() {
            return Err("null flake-show document output".into());
        }
        // SAFETY: checked writable slot; ownership transfers to the caller.
        unsafe {
            *out = Box::into_raw(Box::new(IxeFlakeShowDocument(Document::default())));
        }
        Ok(())
    })
}

/// # Safety
/// `document` is null or an owned live document, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_show_free(document: *mut IxeFlakeShowDocument) {
    if !document.is_null() {
        // SAFETY: caller returns ownership of this exact Box allocation.
        drop(unsafe { Box::from_raw(document) });
    }
}

/// # Safety
/// Document must be live and exclusively borrowed; byte spans and child array
/// must be readable for their lengths. Output/error slots are writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_show_add(
    document: *mut IxeFlakeShowDocument,
    kind: u32,
    name: IxeBytes,
    has_description: i32,
    description: IxeBytes,
    children: *const IxeFlakeShowChild,
    children_len: usize,
    out: *mut u64,
    error: *mut *mut c_char,
) -> i32 {
    run(error, || {
        if out.is_null() {
            return Err("null flake-show node output".into());
        }
        // SAFETY: caller supplies a writable output and exclusively borrowed document.
        unsafe {
            *out = 0;
        }
        let document = unsafe { document.as_mut() }.ok_or("null flake-show document")?;
        let kind = Kind::from_code(kind)?;
        if has_description != 0 && has_description != 1 {
            return Err("invalid description presence flag".into());
        }
        if has_description == 0 && description.len != 0 {
            return Err("absent description carries bytes".into());
        }
        // SAFETY: byte spans are borrowed for this call.
        let name = unsafe { text(name)? };
        let description = if has_description == 1 {
            Some(unsafe { text(description)? })
        } else {
            None
        };
        let children = if children_len == 0 {
            &[]
        } else {
            if children.is_null()
                || children_len > isize::MAX as usize / size_of::<IxeFlakeShowChild>()
            {
                return Err("invalid flake-show child array".into());
            }
            // SAFETY: caller provides a readable array; addressable length was checked.
            unsafe { std::slice::from_raw_parts(children, children_len) }
        };
        let children = children
            .iter()
            .map(|child| {
                // SAFETY: each child name follows the borrowed byte-span contract.
                Ok((unsafe { text(child.name)? }, child.node))
            })
            .collect::<Result<Vec<_>>>()?;
        let id = document.0.add(kind, name, description, children)?;
        // SAFETY: output slot was validated above.
        unsafe {
            *out = id;
        }
        Ok(())
    })
}

/// # Safety
/// `document` must be live/exclusive and `error` a writable slot.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_show_finish(
    document: *mut IxeFlakeShowDocument,
    root: u64,
    error: *mut *mut c_char,
) -> i32 {
    run(error, || {
        // SAFETY: caller's exclusive document contract.
        unsafe { document.as_mut() }
            .ok_or("null flake-show document")?
            .0
            .finish(root)
    })
}

/// # Safety
/// Document is live; output/error are distinct writable slots. The encoded
/// string is owned and freed with `ixe_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_show_encode(
    document: *const IxeFlakeShowDocument,
    out: *mut *mut c_char,
    error: *mut *mut c_char,
) -> i32 {
    if out == error {
        return IXE_ERR_BADCALL;
    }
    run(error, || {
        if out.is_null() {
            return Err("null flake-show encoding output".into());
        }
        // SAFETY: caller provides a writable output slot and live document.
        unsafe {
            *out = ptr::null_mut();
        }
        let document = unsafe { document.as_ref() }.ok_or("null flake-show document")?;
        if out_string(document.0.encode()?, out) != IXE_OK {
            return Err("cannot allocate flake-show encoding".into());
        }
        Ok(())
    })
}

/// # Safety
/// Encoded span is readable; output/error are distinct writable slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_show_decode(
    encoded: *const u8,
    encoded_len: usize,
    out: *mut *mut IxeFlakeShowDocument,
    error: *mut *mut c_char,
) -> i32 {
    run(error, || {
        if out.is_null() {
            return Err("null flake-show decoding output".into());
        }
        // SAFETY: caller provides a writable output slot.
        unsafe {
            *out = ptr::null_mut();
        }
        // SAFETY: caller's readable encoded byte span.
        let document = Document::decode(unsafe {
            bytes(IxeBytes {
                text: encoded,
                len: encoded_len,
            })?
        })?;
        // SAFETY: validated output receives ownership of the new allocation.
        unsafe {
            *out = Box::into_raw(Box::new(IxeFlakeShowDocument(document)));
        }
        Ok(())
    })
}

/// # Safety
/// Document and label are readable; output/error are writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_show_render(
    document: *const IxeFlakeShowDocument,
    root_label: IxeBytes,
    json: i32,
    colored: i32,
    out: *mut *mut IxeFlakeShowReport,
    error: *mut *mut c_char,
) -> i32 {
    run(error, || {
        if out.is_null() {
            return Err("null flake-show report output".into());
        }
        // SAFETY: writable output and readable document per caller contract.
        unsafe {
            *out = ptr::null_mut();
        }
        let document = unsafe { document.as_ref() }.ok_or("null flake-show document")?;
        if ![0, 1].contains(&json) || ![0, 1].contains(&colored) {
            return Err("invalid flake-show render mode".into());
        }
        // SAFETY: label is readable for its declared length.
        let report = document
            .0
            .render(&unsafe { text(root_label)? }, json != 0, colored != 0)?;
        let warnings = report
            .warnings
            .iter()
            .map(|warning| IxeFlakeShowWarning {
                kind: warning.kind as u32,
                message: view(&warning.message),
            })
            .collect();
        // String allocations remain stable when their owning report moves into this Box.
        unsafe {
            *out = Box::into_raw(Box::new(IxeFlakeShowReport { report, warnings }));
        }
        Ok(())
    })
}

/// # Safety
/// Report is live; output is writable. Borrowed view pointers expire on report_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_show_report_view(
    report: *const IxeFlakeShowReport,
    out: *mut IxeFlakeShowReportView,
) -> i32 {
    if out.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller supplies a readable report or null.
    let Some(report) = (unsafe { report.as_ref() }) else {
        return IXE_ERR_BADCALL;
    };
    // SAFETY: caller provides one writable view slot.
    unsafe {
        *out = IxeFlakeShowReportView {
            output: view(&report.report.output),
            warnings: report.warnings.as_ptr(),
            warnings_len: report.warnings.len(),
        };
    }
    IXE_OK
}

/// # Safety
/// Report is null or an owned live allocation, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_show_report_free(report: *mut IxeFlakeShowReport) {
    if !report.is_null() {
        // SAFETY: caller returns the exact allocation produced by render.
        drop(unsafe { Box::from_raw(report) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    fn take_error(error: *mut c_char) -> String {
        if error.is_null() {
            return String::new();
        }
        // SAFETY: each caller transfers one owned ABI diagnostic to this helper.
        let text = unsafe { CStr::from_ptr(error) }
            .to_string_lossy()
            .into_owned();
        // SAFETY: this is the owned string read above, freed exactly once.
        unsafe {
            crate::capi::ixe_string_free(error);
        }
        text
    }

    struct OwnedDocument(*mut IxeFlakeShowDocument);
    impl Drop for OwnedDocument {
        fn drop(&mut self) {
            // SAFETY: test helper owns this live document, which may be null.
            unsafe {
                ixe_flake_show_free(self.0);
            }
        }
    }

    impl OwnedDocument {
        fn new() -> Self {
            let mut pointer = ptr::null_mut();
            let mut error = ptr::null_mut();
            // SAFETY: distinct writable output slots.
            let status = unsafe { ixe_flake_show_new(&mut pointer, &mut error) };
            assert_eq!(status, 0, "{}", take_error(error));
            assert!(!pointer.is_null());
            Self(pointer)
        }

        fn add(&self, kind: Kind, children: &[IxeFlakeShowChild]) -> u64 {
            let mut node = 0;
            let mut error = ptr::null_mut();
            // SAFETY: live document, child slice and distinct output slots.
            let status = unsafe {
                ixe_flake_show_add(
                    self.0,
                    kind as u32,
                    view(""),
                    0,
                    view(""),
                    children.as_ptr(),
                    children.len(),
                    &mut node,
                    &mut error,
                )
            };
            assert_eq!(status, 0, "{}", take_error(error));
            assert_ne!(node, 0);
            node
        }
    }

    #[test]
    fn owned_report_survives_document_and_codec_allocations() {
        let document = OwnedDocument::new();
        let child = document.add(Kind::NonDerivation, &[]);
        let root = document.add(
            Kind::Branch,
            &[IxeFlakeShowChild {
                name: view("bad.name"),
                node: child,
            }],
        );
        let mut error = ptr::null_mut();
        // SAFETY: live document and writable error slot.
        let status = unsafe { ixe_flake_show_finish(document.0, root, &mut error) };
        assert_eq!(status, 0, "{}", take_error(error));
        let mut encoded = ptr::null_mut();
        // SAFETY: readable document and distinct writable output slots.
        let status = unsafe { ixe_flake_show_encode(document.0, &mut encoded, &mut error) };
        assert_eq!(status, 0, "{}", take_error(error));
        assert!(!encoded.is_null());
        // SAFETY: encode returned an owned NUL-terminated string.
        let bytes = unsafe { CStr::from_ptr(encoded) }.to_bytes();
        let mut decoded = ptr::null_mut();
        // SAFETY: the encoded allocation is still live and output slots are distinct.
        let status =
            unsafe { ixe_flake_show_decode(bytes.as_ptr(), bytes.len(), &mut decoded, &mut error) };
        assert_eq!(status, 0, "{}", take_error(error));
        let decoded = OwnedDocument(decoded);
        // SAFETY: encoded allocation is owned and no longer borrowed.
        unsafe {
            crate::capi::ixe_string_free(encoded);
        }
        let mut report = ptr::null_mut();
        // SAFETY: readable finished document, label and distinct output slots.
        let status = unsafe {
            ixe_flake_show_render(decoded.0, view("root"), 1, 0, &mut report, &mut error)
        };
        assert_eq!(status, 0, "{}", take_error(error));
        assert!(!report.is_null());
        drop(decoded);
        drop(document);
        let mut output = IxeFlakeShowReportView {
            output: view(""),
            warnings: ptr::null(),
            warnings_len: 0,
        };
        // SAFETY: report still owns every byte even after both documents were destroyed.
        assert_eq!(
            unsafe { ixe_flake_show_report_view(report, &mut output) },
            0
        );
        assert_eq!(
            unsafe { text(output.output) },
            Ok("{\"bad.name\":{}}".into())
        );
        assert_eq!(output.warnings_len, 1);
        assert!(!output.warnings.is_null());
        // SAFETY: the returned view contains exactly warnings_len readable entries.
        let warnings = unsafe { std::slice::from_raw_parts(output.warnings, output.warnings_len) };
        let Some(warning) = warnings.first() else {
            unreachable!("missing warning")
        };
        assert_eq!(warning.kind, Kind::NonDerivation as u32);
        assert_eq!(
            unsafe { text(warning.message) },
            Ok("\"bad.name\".name is not a derivation".into())
        );
        // SAFETY: the report is owned by this test and is freed once, after all views.
        unsafe {
            ixe_flake_show_report_free(report);
        }
    }

    #[test]
    fn invalid_abi_calls_return_owned_errors_without_partial_results() {
        let document = OwnedDocument::new();
        let mut error = ptr::null_mut();
        // SAFETY: null output is a checked invalid argument; error is writable.
        assert_eq!(
            unsafe { ixe_flake_show_new(ptr::null_mut(), &mut error) },
            4
        );
        assert!(take_error(error).contains("output"));
        let mut node = 99;
        // SAFETY: malformed null span is rejected before dereferencing it.
        let status = unsafe {
            ixe_flake_show_add(
                document.0,
                1,
                IxeBytes {
                    text: ptr::null(),
                    len: 1,
                },
                0,
                view(""),
                ptr::null(),
                0,
                &mut node,
                &mut error,
            )
        };
        assert_eq!(status, 4);
        assert_eq!(node, 0);
        assert!(take_error(error).contains("byte span"));
        let mut report = ptr::null_mut();
        // SAFETY: live unfinished document; output/error writable and distinct.
        let status = unsafe {
            ixe_flake_show_render(document.0, view("root"), 0, 0, &mut report, &mut error)
        };
        assert_eq!(status, 4);
        assert!(report.is_null());
        assert!(take_error(error).contains("unfinished"));
        let mut decoded = ptr::null_mut();
        let invalid = b"{\"version\":1,\"root\":0,\"nodes\":[[3]]}";
        // SAFETY: invalid cache bytes are still a valid readable input span.
        let status = unsafe {
            ixe_flake_show_decode(invalid.as_ptr(), invalid.len(), &mut decoded, &mut error)
        };
        assert_eq!(status, 4);
        assert!(decoded.is_null());
        assert!(take_error(error).contains("description"));
        // SAFETY: null free operations are explicitly supported.
        unsafe {
            ixe_flake_show_free(ptr::null_mut());
            ixe_flake_show_report_free(ptr::null_mut());
        }
    }

    #[test]
    fn header_kind_codes_match_every_typed_variant() -> Result<()> {
        let header = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("include/ixe-flake-show.h"),
        )
        .map_err(|error| error.to_string())?;
        let expected = [
            ("BRANCH", Kind::Branch),
            ("DERIVATION", Kind::Derivation),
            ("APP", Kind::App),
            ("TEMPLATE", Kind::Template),
            ("NIXPKGS_OVERLAY", Kind::NixpkgsOverlay),
            ("NIXOS_CONFIGURATION", Kind::NixosConfiguration),
            ("NIXOS_MODULE", Kind::NixosModule),
            ("UNKNOWN", Kind::Unknown),
            ("EMPTY", Kind::Empty),
            ("NON_DERIVATION", Kind::NonDerivation),
            ("OMITTED_SYSTEM", Kind::OmittedSystem),
            ("OMITTED_LEGACY", Kind::OmittedLegacy),
            ("OMITTED_IFD", Kind::OmittedIfd),
        ];
        let mut actual = std::collections::BTreeMap::new();
        for line in header.lines() {
            let mut words = line.split_whitespace();
            if words.next() != Some("#define") {
                continue;
            }
            let Some(name) = words
                .next()
                .and_then(|name| name.strip_prefix("IXE_FLAKE_SHOW_"))
            else {
                continue;
            };
            let number = words
                .next()
                .ok_or("missing node kind value")?
                .trim_end_matches('u')
                .parse::<u32>()
                .map_err(|error| error.to_string())?;
            assert!(actual.insert(name, number).is_none());
        }
        assert_eq!(
            actual,
            expected
                .into_iter()
                .map(|(name, kind)| (name, kind as u32))
                .collect()
        );
        assert_eq!(actual.len(), Kind::ALL.len());
        Ok(())
    }
}
