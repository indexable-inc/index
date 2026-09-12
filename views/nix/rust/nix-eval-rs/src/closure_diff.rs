//! Closure comparison and presentation. The host supplies unique store paths
//! with their names and NAR sizes; Rust owns grouping and the complete report.

use std::collections::{BTreeMap, BTreeSet};
use std::panic::{AssertUnwindSafe, catch_unwind};

struct Entry<'a> {
    name: &'a str,
    nar_size: u64,
}

#[derive(Default)]
struct Package<'a> {
    versions: BTreeSet<&'a str>,
    size: i128,
}

fn group<'a>(entries: &[Entry<'a>]) -> Result<BTreeMap<&'a str, Package<'a>>, String> {
    let mut packages = BTreeMap::<&str, Package<'_>>::new();
    for entry in entries {
        // Store path names do not identify outputs separately. Keep the
        // existing convention of stripping lowercase suffixes and lib32/lib64.
        let name = match entry.name.rsplit_once('-') {
            Some((name, suffix))
                if matches!(suffix, "lib32" | "lib64")
                    || (!suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_lowercase())) =>
            {
                name
            }
            _ => entry.name,
        };
        let parsed = crate::drv_name::split(name.as_bytes());
        // The shared byte parser splits only at ASCII dash boundaries.
        let name = std::str::from_utf8(parsed.name).map_err(|error| error.to_string())?;
        let version = std::str::from_utf8(parsed.version).map_err(|error| error.to_string())?;
        let package = packages.entry(name).or_default();
        package.versions.insert(version);
        package.size = package
            .size
            .checked_add(i128::from(entry.nar_size))
            .ok_or("closure size exceeds signed 128-bit range")?;
    }
    Ok(packages)
}

fn versions(versions: &BTreeSet<&str>) -> String {
    if versions.is_empty() {
        return "∅".to_owned();
    }
    versions
        .iter()
        .map(|version| if version.is_empty() { "ε" } else { version })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ")
}

fn size(delta: i128) -> String {
    let mut magnitude = delta.unsigned_abs() / 1024;
    let mut denominator = 1024.0;
    let mut suffix = "KiB";
    for next in ["MiB", "GiB", "TiB", "PiB", "EiB", "ZiB", "YiB"] {
        if magnitude <= 1024 {
            break;
        }
        magnitude /= 1024;
        denominator *= 1024.0;
        suffix = next;
    }
    format!("{:.1} {suffix}", delta as f64 / denominator)
}

fn compare(before: &[Entry<'_>], after: &[Entry<'_>], indent: &str) -> Result<String, String> {
    let before = group(before)?;
    let after = group(after)?;
    let names: BTreeSet<_> = before.keys().chain(after.keys()).copied().collect();
    let empty = Package::default();
    let mut lines = Vec::new();
    for name in names {
        let before = before.get(name).unwrap_or(&empty);
        let after = after.get(name).unwrap_or(&empty);
        let delta = after.size - before.size;
        let removed: BTreeSet<_> = before
            .versions
            .difference(&after.versions)
            .copied()
            .collect();
        let added: BTreeSet<_> = after
            .versions
            .difference(&before.versions)
            .copied()
            .collect();
        let mut items = Vec::new();
        if !removed.is_empty() || !added.is_empty() {
            items.push(format!("{} → {}", versions(&removed), versions(&added)));
        }
        if delta.unsigned_abs() >= 8 * 1024 {
            let color = if delta > 0 {
                "\x1b[31;1m"
            } else {
                "\x1b[32;1m"
            };
            items.push(format!("{color}{}\x1b[0m", size(delta)));
        }
        if !items.is_empty() {
            lines.push(format!("{indent}{name}: {}", items.join(", ")));
        }
    }
    Ok(lines.join("\n"))
}

#[repr(C)]
pub struct IxeClosureEntry {
    name: *const u8,
    name_len: usize,
    nar_size: u64,
}

#[repr(C)]
pub struct IxeClosureVersion {
    text: *const u8,
    len: usize,
}

#[repr(C)]
pub struct IxeClosureReport {
    data: *mut u8,
    len: usize,
    success: i32,
}

unsafe fn text<'a>(data: *const u8, len: usize) -> Result<&'a str, String> {
    if len == 0 {
        return Ok("");
    }
    if data.is_null() || len > isize::MAX as usize {
        return Err("invalid closure text view".into());
    }
    // SAFETY: the caller provides a live readable byte allocation.
    let bytes = unsafe { std::slice::from_raw_parts(data, len) };
    if bytes.contains(&0) {
        return Err("closure text contains NUL".into());
    }
    std::str::from_utf8(bytes).map_err(|error| error.to_string())
}

unsafe fn entries<'a>(data: *const IxeClosureEntry, len: usize) -> Result<Vec<Entry<'a>>, String> {
    if len == 0 {
        return Ok(Vec::new());
    }
    if data.is_null()
        || !data.is_aligned()
        || len > isize::MAX as usize / size_of::<IxeClosureEntry>()
    {
        return Err("invalid closure entry view".into());
    }
    // SAFETY: the caller supplies this aligned array and keeps every name live.
    unsafe { std::slice::from_raw_parts(data, len) }
        .iter()
        .map(|entry| {
            Ok(Entry {
                // SAFETY: each entry's name obeys the same borrowed-view contract.
                name: unsafe { text(entry.name, entry.name_len) }?,
                nar_size: entry.nar_size,
            })
        })
        .collect()
}

/// # Safety
/// Arrays and strings must remain readable for the call. Each array contains
/// one entry per unique store path. Free the returned report exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_closure_diff(
    before: *const IxeClosureEntry,
    before_len: usize,
    after: *const IxeClosureEntry,
    after_len: usize,
    indent: *const u8,
    indent_len: usize,
) -> IxeClosureReport {
    report(|| {
        // SAFETY: the ABI caller keeps the views live for this operation.
        let (before, after, indent) = unsafe {
            (
                entries(before, before_len)?,
                entries(after, after_len)?,
                text(indent, indent_len)?,
            )
        };
        compare(&before, &after, indent)
    })
}

/// # Safety
/// The array and its strings must remain readable for this call. Free the
/// returned report exactly once, including empty and error results.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_closure_versions(
    data: *const IxeClosureVersion,
    len: usize,
) -> IxeClosureReport {
    report(|| {
        if len == 0 {
            return Ok(versions(&BTreeSet::new()));
        }
        if data.is_null()
            || !data.is_aligned()
            || len > isize::MAX as usize / size_of::<IxeClosureVersion>()
        {
            return Err("invalid closure version view".into());
        }
        // SAFETY: the caller supplies this array and keeps each version live.
        let values = unsafe { std::slice::from_raw_parts(data, len) };
        let versions = values
            .iter()
            // SAFETY: each string follows the borrowed-view contract.
            .map(|value| unsafe { text(value.text, value.len) })
            .collect::<Result<BTreeSet<_>, _>>()?;
        Ok(self::versions(&versions))
    })
}

fn report(action: impl FnOnce() -> Result<String, String>) -> IxeClosureReport {
    let result = catch_unwind(AssertUnwindSafe(action))
        .unwrap_or_else(|_| Err("panic in closure comparison".into()));
    let (text, success) = match result {
        Ok(text) => (text, 1),
        Err(error) => (error, 0),
    };
    let bytes = text.into_bytes().into_boxed_slice();
    let len = bytes.len();
    IxeClosureReport {
        data: Box::into_raw(bytes).cast::<u8>(),
        len,
        success,
    }
}

/// # Safety
/// `report` must be the unmodified result of ixe_closure_diff and not freed yet.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_closure_report_free(report: IxeClosureReport) {
    let slice = std::ptr::slice_from_raw_parts_mut(report.data, report.len);
    // SAFETY: reconstruct the allocation returned by the matching operation.
    drop(unsafe { Box::from_raw(slice) });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, nar_size: u64) -> Entry<'_> {
        Entry { name, nar_size }
    }

    #[test]
    fn compares_versions_across_outputs_and_sorts_packages() -> Result<(), String> {
        let before = [
            entry("z-1.0-bin", 1),
            entry("z-1.0-lib64", 1),
            entry("a-2", 1),
        ];
        let after = [entry("z-2.0", 2), entry("a-3", 1)];
        assert_eq!(
            compare(&before, &after, "  ")?,
            "  a: 2 → 3\n  z: 1.0 → 2.0"
        );
        Ok(())
    }

    #[test]
    fn distinguishes_absent_unversioned_and_unchanged_versions() -> Result<(), String> {
        let before = [
            entry("manifest.nix", 1),
            entry("pkg-1", 1),
            entry("pkg-2", 1),
        ];
        let after = [entry("pkg-2", 1), entry("pkg-3", 1), entry("new", 1)];
        assert_eq!(
            compare(&before, &after, "")?,
            "manifest.nix: ε → ∅\nnew: ∅ → ε\npkg: 1 → 3"
        );
        Ok(())
    }

    #[test]
    fn strips_only_recognized_output_suffixes() -> Result<(), String> {
        let entries = [
            entry("apache-httpd-2.0-bin", 1),
            entry("pkg-1-X", 1),
            entry("pkg-1-", 1),
        ];
        assert_eq!(
            compare(&[], &entries, "")?,
            "apache-httpd: ∅ → 2.0\npkg: ∅ → 1-, 1-X"
        );
        Ok(())
    }

    #[test]
    fn threshold_is_inclusive_and_same_version_size_changes_are_reported() -> Result<(), String> {
        assert_eq!(
            compare(&[entry("pkg-1", 0)], &[entry("pkg-1", 8191)], "")?,
            ""
        );
        assert_eq!(
            compare(&[entry("pkg-1", 0)], &[entry("pkg-1", 8192)], "")?,
            "pkg: \x1b[31;1m8.0 KiB\x1b[0m"
        );
        assert_eq!(
            compare(&[entry("pkg-1", 8192)], &[entry("pkg-1", 0)], "")?,
            "pkg: \x1b[32;1m-8.0 KiB\x1b[0m"
        );
        Ok(())
    }

    #[test]
    fn aggregate_sizes_do_not_wrap_at_u64_or_i64_limits() -> Result<(), String> {
        let huge = [entry("pkg-1-bin", u64::MAX), entry("pkg-1-lib", u64::MAX)];
        assert_eq!(
            compare(&[], &huge, "")?,
            "pkg: ∅ → 1, \x1b[31;1m32.0 EiB\x1b[0m"
        );
        assert_eq!(
            compare(&huge, &[], "")?,
            "pkg: 1 → ∅, \x1b[32;1m-32.0 EiB\x1b[0m"
        );
        Ok(())
    }

    #[test]
    fn size_units_preserve_boundary_precision() {
        assert_eq!(size(1024 * 1024), "1024.0 KiB");
        assert_eq!(size(2 * 1024 * 1024), "2.0 MiB");
    }

    #[test]
    fn abi_validates_views_and_releases_empty_and_error_reports() {
        // SAFETY: zero-length views need no allocation; the nonempty null view
        // is deliberately rejected before a slice is constructed.
        unsafe {
            let empty = ixe_closure_diff(
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(empty.success, 1);
            assert_eq!(empty.len, 0);
            ixe_closure_report_free(empty);
            let invalid = ixe_closure_diff(
                std::ptr::null(),
                1,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
            );
            assert_eq!(invalid.success, 0);
            assert!(invalid.len > 0);
            ixe_closure_report_free(invalid);
        }
    }

    #[test]
    fn abi_reads_borrowed_names_and_owns_report_bytes() {
        let report = {
            let name = String::from("pkg-1-bin");
            let after = IxeClosureEntry {
                name: name.as_ptr(),
                name_len: name.len(),
                nar_size: 8192,
            };
            // SAFETY: both the array element and its name remain live for the call.
            unsafe { ixe_closure_diff(std::ptr::null(), 0, &after, 1, b"  ".as_ptr(), 2) }
        };
        // SAFETY: returned bytes remain owned by report after the input is gone.
        unsafe {
            assert_eq!(report.success, 1);
            assert_eq!(
                std::slice::from_raw_parts(report.data, report.len),
                b"  pkg: \xe2\x88\x85 \xe2\x86\x92 1, \x1b[31;1m8.0 KiB\x1b[0m"
            );
            ixe_closure_report_free(report);
        }
    }

    #[test]
    fn abi_rejects_nul_and_invalid_utf8_without_reading_past_names() {
        for name in [b"pkg\0suffix".as_slice(), b"pkg-\xff".as_slice()] {
            let entry = IxeClosureEntry {
                name: name.as_ptr(),
                name_len: name.len(),
                nar_size: 0,
            };
            // SAFETY: both byte strings and the entry remain readable. Invalid
            // text must fail before any grouping or display operation.
            unsafe {
                let report = ixe_closure_diff(std::ptr::null(), 0, &entry, 1, std::ptr::null(), 0);
                assert_eq!(report.success, 0);
                ixe_closure_report_free(report);
            }
        }
    }

    #[test]
    fn shared_version_formatter_orders_empty_versions_after_numbers() {
        let values = ["", "2", "1", "2"].map(|value| IxeClosureVersion {
            text: value.as_ptr(),
            len: value.len(),
        });
        // SAFETY: the array and its static strings live through the call.
        unsafe {
            let report = ixe_closure_versions(values.as_ptr(), values.len());
            assert_eq!(report.success, 1);
            assert_eq!(
                std::slice::from_raw_parts(report.data, report.len),
                "1, 2, ε".as_bytes()
            );
            ixe_closure_report_free(report);
            let report = ixe_closure_versions(std::ptr::null(), 0);
            assert_eq!(
                std::slice::from_raw_parts(report.data, report.len),
                "∅".as_bytes()
            );
            ixe_closure_report_free(report);
        }
    }
}
