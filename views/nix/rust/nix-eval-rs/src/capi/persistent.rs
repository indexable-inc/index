//! JSON-lines reports for the production persistent evaluation command.
//!
//! Values use the same selected JSON question as `nix eval --json`. Counters
//! describe that question; wall and CPU durations include source resolution.

use super::{IXE_ERR_BADCALL, IxeBytes, c_char, out_string};
use crate::perf::Snapshot;
use std::panic::{AssertUnwindSafe, catch_unwind};

mod requests;

#[repr(C)]
pub struct IxePersistentReportInput {
    installable: IxeBytes,
    value_json: IxeBytes,
    wall_ns: u64,
    cpu_ns: u64,
    inputs_evicted: usize,
    witness_memory_hits: u64,
    witness_disk_loads: u64,
    witness_cache_bytes: u64,
    witness_cache_entries: u64,
}

struct RequestText<'a> {
    installable: &'a str,
    value_json: &'a str,
}

fn report(
    request: RequestText<'_>,
    input: &IxePersistentReportInput,
    counters: Snapshot,
) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(request.value_json)
        .map_err(|error| format!("invalid JSON evaluation result: {error}"))?;
    let report = serde_json::json!({
        "installable": request.installable,
        "value": value,
        "wallMs": input.wall_ns as f64 / 1_000_000.0,
        "cpuMs": input.cpu_ns as f64 / 1_000_000.0,
        "inputsEvicted": input.inputs_evicted,
        "stats": {
            "countersEnabled": cfg!(feature = "perf"),
            "compiles": counters.compiles,
            "compileHits": counters.compile_hits,
            "memoServed": counters.memo_served,
            "subtreeMemoHits": counters.import_cache_hits,
            "subtreeMemoMisses": counters.import_cache_misses,
            "subtreeEntryForces": counters.import_entry_forces,
            "hostQuestions": counters.questions,
            "importHits": counters.import_hits,
            "copyMemoServed": counters.copy_memo_served,
            "witnessMemoryHits": input.witness_memory_hits,
            "witnessDiskLoads": input.witness_disk_loads,
            "witnessCacheBytes": input.witness_cache_bytes,
            "witnessCacheEntries": input.witness_cache_entries,
        },
    });
    Ok(format!("{report}\n"))
}

unsafe fn text<'a>(view: IxeBytes) -> Result<&'a str, String> {
    if view.len == 0 {
        return Ok("");
    }
    if view.text.is_null() || view.len > isize::MAX as usize {
        return Err("invalid persistent request byte view".into());
    }
    // SAFETY: the caller guarantees this allocation remains readable.
    let bytes = unsafe { std::slice::from_raw_parts(view.text, view.len) };
    std::str::from_utf8(bytes).map_err(|error| error.to_string())
}

/// # Safety
/// Byte views must be readable for their lengths and both output slots writable.
/// Strings returned through either slot must be freed once with ixe_string_free.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_persistent_report(
    input: IxePersistentReportInput,
    output: *mut *mut c_char,
    error: *mut *mut c_char,
) -> i32 {
    if output.is_null() || error.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: the caller supplies writable output slots.
    unsafe {
        *output = std::ptr::null_mut();
        *error = std::ptr::null_mut();
    }
    let result = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: input views follow the ABI caller contract.
        let installable = unsafe { text(input.installable) }?;
        // SAFETY: input views follow the ABI caller contract.
        let value = unsafe { text(input.value_json) }?;
        report(
            RequestText {
                installable,
                value_json: value,
            },
            &input,
            crate::perf::snapshot(),
        )
    }))
    .unwrap_or_else(|_| Err("panic formatting persistent evaluation report".into()));
    match result {
        Ok(text) => out_string(text, output),
        Err(message) => {
            out_string(message, error);
            IXE_ERR_BADCALL
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::IXE_OK;
    use super::*;

    fn input(installable: &str, value: &str) -> IxePersistentReportInput {
        IxePersistentReportInput {
            installable: IxeBytes {
                text: installable.as_ptr(),
                len: installable.len(),
            },
            value_json: IxeBytes {
                text: value.as_ptr(),
                len: value.len(),
            },
            wall_ns: 1_500_000,
            cpu_ns: 250_000,
            inputs_evicted: 3,
            witness_memory_hits: 2,
            witness_disk_loads: 1,
            witness_cache_bytes: 4096,
            witness_cache_entries: 1,
        }
    }

    #[test]
    fn reports_typed_values_and_question_work_without_cpp_counters() -> Result<(), String> {
        let counters = Snapshot {
            compiles: 2,
            compile_hits: 3,
            memo_served: 1,
            questions: 7,
            import_hits: 4,
            copy_memo_served: 5,
            ..Snapshot::default()
        };
        let text = report(
            RequestText {
                installable: ".#answer",
                value_json: "{\"a\":[1,true,null]}",
            },
            &input("", ""),
            counters,
        )?;
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
        assert_eq!(
            value.get("value"),
            Some(&serde_json::json!({"a": [1, true, null]}))
        );
        assert_eq!(value.get("wallMs"), Some(&serde_json::json!(1.5)));
        assert_eq!(value.get("cpuMs"), Some(&serde_json::json!(0.25)));
        assert_eq!(value.get("inputsEvicted"), Some(&serde_json::json!(3)));
        assert_eq!(
            value.get("stats"),
            Some(&serde_json::json!({
                "countersEnabled": cfg!(feature = "perf"), "compiles": 2,
                "compileHits": 3, "memoServed": 1, "hostQuestions": 7,
                "subtreeMemoHits": 0, "subtreeMemoMisses": 0, "subtreeEntryForces": 0,
                "importHits": 4, "copyMemoServed": 5,
                "witnessMemoryHits": 2, "witnessDiskLoads": 1,
                "witnessCacheBytes": 4096, "witnessCacheEntries": 1,
            }))
        );
        assert!(value.get("thunks").is_none());
        assert!(value.get("evalFileCalls").is_none());
        assert!(value.get("evalFilePathHits").is_none());
        assert_eq!(text.lines().count(), 1);
        Ok(())
    }

    #[test]
    fn malformed_json_is_an_error_and_strings_cannot_inject_report_lines() -> Result<(), String> {
        assert!(
            report(
                RequestText {
                    installable: "",
                    value_json: "broken"
                },
                &input("", ""),
                Snapshot::default()
            )
            .is_err()
        );
        let text = report(
            RequestText {
                installable: "a\nb",
                value_json: "\"a\\nb\"",
            },
            &input("", ""),
            Snapshot::default(),
        )?;
        assert_eq!(text.lines().count(), 1);
        Ok(())
    }

    #[test]
    fn abi_releases_success_and_error_strings() -> Result<(), String> {
        let mut output = std::ptr::null_mut();
        let mut error = std::ptr::null_mut();
        // SAFETY: borrowed inputs stay live and output slots are writable.
        unsafe {
            assert_eq!(
                ixe_persistent_report(input(".#value", "42"), &mut output, &mut error),
                IXE_OK
            );
            assert!(error.is_null());
            assert!(!output.is_null());
            let parsed = std::ffi::CStr::from_ptr(output)
                .to_str()
                .map_err(|e| e.to_string())?;
            assert!(parsed.contains("\"value\":42"));
            super::super::ixe_string_free(output);
            assert_eq!(
                ixe_persistent_report(input(".#value", "broken"), &mut output, &mut error),
                IXE_ERR_BADCALL
            );
            assert!(output.is_null());
            assert!(!error.is_null());
            super::super::ixe_string_free(error);
            let mut invalid = input("", "42");
            invalid.installable.text = std::ptr::null();
            invalid.installable.len = 1;
            assert_eq!(
                ixe_persistent_report(invalid, &mut output, &mut error),
                IXE_ERR_BADCALL
            );
            assert!(output.is_null());
            super::super::ixe_string_free(error);
        }
        Ok(())
    }
}
