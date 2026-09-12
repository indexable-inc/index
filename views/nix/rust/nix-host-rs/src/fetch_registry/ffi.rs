//! Typed registry boundary. Mutable owners use a mutex and immutable snapshots;
//! no call creates an exclusive Rust reference from a shared registry handle.

use nix_fetch_registry::{
    Attr, Attrs, Entry, LayerKind, Registry, Result, UseRegistries, apply_overrides, resolve,
};
use std::ffi::{CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Arc, Mutex, MutexGuard};

pub struct IxeRegistry(Mutex<Arc<Registry>>);
pub struct IxeRegistryEntries(Vec<EntrySnapshot>);
pub struct IxeRegistryAttrs(AttrsSnapshot);
pub struct IxeRegistryResolution {
    input: AttrsSnapshot,
    extra: AttrsSnapshot,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeRegistryBytes {
    pub data: *const u8,
    pub len: usize,
}

impl IxeRegistryBytes {
    fn view(value: &str) -> Self {
        Self {
            data: value.as_ptr(),
            len: value.len(),
        }
    }

    unsafe fn text<'a>(self) -> Result<&'a str> {
        // SAFETY: caller provides the readable bytes for this view.
        std::str::from_utf8(unsafe { array(self.data, self.len) }?)
            .map_err(|error| error.to_string())
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeRegistryAttr {
    pub name: IxeRegistryBytes,
    pub kind: u8,
    pub string: IxeRegistryBytes,
    pub number: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeRegistryAttrsView {
    pub data: *const IxeRegistryAttr,
    pub len: usize,
}

impl IxeRegistryAttrsView {
    unsafe fn owned(self) -> Result<Attrs> {
        let mut attrs = Attrs::new();
        // SAFETY: caller supplies a readable array and the nested byte views.
        for attr in unsafe { array(self.data, self.len) }? {
            // SAFETY: each name obeys the same borrowed view contract.
            let name = unsafe { attr.name.text() }?.to_owned();
            let value = match attr.kind {
                // SAFETY: the string payload is readable for its declared length.
                0 => Attr::String(unsafe { attr.string.text() }?.to_owned()),
                1 => Attr::Uint(attr.number),
                2 if attr.number <= 1 => Attr::Bool(attr.number == 1),
                _ => return Err("invalid registry attribute tag or boolean".into()),
            };
            if attrs.insert(name, value).is_some() {
                return Err("duplicate registry attribute".into());
            }
        }
        Ok(attrs)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeRegistryEntryView {
    pub from: IxeRegistryAttrsView,
    pub to: IxeRegistryAttrsView,
    pub extra: IxeRegistryAttrsView,
    pub exact: u8,
}

impl IxeRegistryEntryView {
    unsafe fn owned(self) -> Result<Entry> {
        if self.exact > 1 {
            return Err("registry exact flag must be zero or one".into());
        }
        // SAFETY: caller provides all three readable attribute views.
        let entry = unsafe {
            Entry {
                from: self.from.owned()?,
                to: self.to.owned()?,
                extra: self.extra.owned()?,
                exact: self.exact == 1,
            }
        };
        Ok(entry)
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeRegistryLayer {
    pub registry: *const IxeRegistry,
    pub kind: u8,
}

struct AttrsSnapshot {
    _attrs: Attrs,
    views: Vec<IxeRegistryAttr>,
}

impl AttrsSnapshot {
    fn new(attrs: Attrs) -> Self {
        let views = attrs
            .iter()
            .map(|(name, value)| {
                let mut view = IxeRegistryAttr {
                    name: IxeRegistryBytes::view(name),
                    kind: 0,
                    string: IxeRegistryBytes::view(""),
                    number: 0,
                };
                match value {
                    Attr::String(value) => view.string = IxeRegistryBytes::view(value),
                    Attr::Uint(value) => {
                        view.kind = 1;
                        view.number = *value;
                    }
                    Attr::Bool(value) => {
                        view.kind = 2;
                        view.number = u64::from(*value);
                    }
                }
                view
            })
            .collect();
        Self {
            _attrs: attrs,
            views,
        }
    }

    fn view(&self) -> IxeRegistryAttrsView {
        IxeRegistryAttrsView {
            data: self.views.as_ptr(),
            len: self.views.len(),
        }
    }
}

struct EntrySnapshot {
    from: AttrsSnapshot,
    to: AttrsSnapshot,
    extra: AttrsSnapshot,
    exact: bool,
}

impl EntrySnapshot {
    fn new(entry: Entry) -> Self {
        Self {
            from: AttrsSnapshot::new(entry.from),
            to: AttrsSnapshot::new(entry.to),
            extra: AttrsSnapshot::new(entry.extra),
            exact: entry.exact,
        }
    }

    fn view(&self) -> IxeRegistryEntryView {
        IxeRegistryEntryView {
            from: self.from.view(),
            to: self.to.view(),
            extra: self.extra.view(),
            exact: u8::from(self.exact),
        }
    }
}

unsafe fn array<'a, T>(data: *const T, len: usize) -> Result<&'a [T]> {
    if len == 0 {
        return Ok(&[]);
    }
    if data.is_null() || !data.is_aligned() || len > isize::MAX as usize / size_of::<T>() {
        return Err("invalid registry array view".into());
    }
    // SAFETY: alignment/length checked; caller owns readable initialized elements.
    Ok(unsafe { std::slice::from_raw_parts(data, len) })
}

unsafe fn object<'a, T>(value: *const T) -> Result<&'a T> {
    if value.is_null() || !value.is_aligned() {
        return Err("invalid registry handle".into());
    }
    // SAFETY: caller owns a live handle of this exact type.
    Ok(unsafe { &*value })
}

fn output<T>(out: *mut T) -> Result<()> {
    if out.is_null() || !out.is_aligned() {
        Err("invalid registry output pointer".into())
    } else {
        Ok(())
    }
}

fn lock(registry: &IxeRegistry) -> Result<MutexGuard<'_, Arc<Registry>>> {
    registry
        .0
        .lock()
        .map_err(|_| "registry mutex was poisoned".into())
}

fn run(action: impl FnOnce() -> Result<()>) -> *mut c_char {
    let error = match catch_unwind(AssertUnwindSafe(action)) {
        Ok(Ok(())) => return ptr::null_mut(),
        Ok(Err(error)) => error,
        Err(_) => "panic at registry boundary".into(),
    };
    // Registry names are untrusted, and the host logs errors directly.
    let error: String = error
        .chars()
        .flat_map(|ch| {
            if ch.is_control() {
                ch.escape_default().collect::<Vec<_>>()
            } else {
                vec![ch]
            }
        })
        .collect();
    // SAFETY: every control character, including NUL, was escaped above.
    unsafe { CString::from_vec_unchecked(error.into_bytes()) }.into_raw()
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_new(out: *mut *mut IxeRegistry) -> *mut c_char {
    run(|| {
        output(out)?;
        // SAFETY: caller supplies writable output storage.
        unsafe {
            out.write(Box::into_raw(Box::new(IxeRegistry(Mutex::new(Arc::new(
                Registry::default(),
            ))))))
        };
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_parse(
    source: IxeRegistryBytes,
    out: *mut *mut IxeRegistry,
) -> *mut c_char {
    run(|| {
        output(out)?;
        // SAFETY: source is readable and output writable for this call.
        let registry = Registry::parse(unsafe { source.text() }?)?;
        unsafe {
            out.write(Box::into_raw(Box::new(IxeRegistry(Mutex::new(Arc::new(
                registry,
            ))))))
        };
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_serialize(
    registry: *const IxeRegistry,
    out: *mut *mut c_char,
) -> *mut c_char {
    run(|| {
        output(out)?;
        // SAFETY: registry is a live shared handle.
        let source = lock(unsafe { object(registry) }?)?.serialize()?;
        let source = CString::new(source).map_err(|error| error.to_string())?;
        // SAFETY: caller supplies writable output storage.
        unsafe { out.write(source.into_raw()) };
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_entries(
    registry: *const IxeRegistry,
    out: *mut *mut IxeRegistryEntries,
) -> *mut c_char {
    run(|| {
        output(out)?;
        // SAFETY: registry is a live shared handle.
        let registry = Arc::clone(&*lock(unsafe { object(registry) }?)?);
        let entries = registry
            .entries()
            .iter()
            .cloned()
            .map(EntrySnapshot::new)
            .collect();
        // SAFETY: caller supplies writable output storage.
        unsafe { out.write(Box::into_raw(Box::new(IxeRegistryEntries(entries)))) };
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_entries_len(entries: *const IxeRegistryEntries) -> usize {
    // SAFETY: callers must retain the snapshot until enumeration ends.
    unsafe { object(entries) }.map_or(0, |entries| entries.0.len())
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_entries_get(
    entries: *const IxeRegistryEntries,
    index: usize,
    out: *mut IxeRegistryEntryView,
) -> *mut c_char {
    run(|| {
        output(out)?;
        // SAFETY: callers must retain the snapshot and provide writable output.
        let entry = unsafe { object(entries) }?
            .0
            .get(index)
            .ok_or("registry entry index out of range")?;
        unsafe { out.write(entry.view()) };
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_replace(
    registry: *const IxeRegistry,
    entries: *const IxeRegistryEntryView,
    len: usize,
) -> *mut c_char {
    run(|| {
        // SAFETY: caller provides initialized entry views and a live shared owner.
        let entries = unsafe { array(entries, len) }?
            .iter()
            .map(|entry| unsafe { entry.owned() })
            .collect::<Result<Vec<_>>>()?;
        let replacement = Registry::from_entries(entries)?;
        *lock(unsafe { object(registry) }?)? = Arc::new(replacement);
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_add(
    registry: *const IxeRegistry,
    entry: IxeRegistryEntryView,
) -> *mut c_char {
    run(|| {
        // SAFETY: caller supplies readable entry views and a live owner.
        let entry = unsafe { entry.owned() }?;
        Arc::make_mut(&mut *lock(unsafe { object(registry) }?)?).add(entry)
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_remove(
    registry: *const IxeRegistry,
    input: IxeRegistryAttrsView,
) -> *mut c_char {
    run(|| {
        // SAFETY: caller supplies readable input attributes and a live owner.
        let input = unsafe { input.owned() }?;
        Arc::make_mut(&mut *lock(unsafe { object(registry) }?)?).remove(&input)?;
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_resolve(
    layers: *const IxeRegistryLayer,
    len: usize,
    input: IxeRegistryAttrsView,
    mode: u8,
    out: *mut *mut IxeRegistryResolution,
) -> *mut c_char {
    run(|| {
        output(out)?;
        let mode = match mode {
            0 => UseRegistries::No,
            1 => UseRegistries::All,
            2 => UseRegistries::Limited,
            _ => return Err("invalid registry resolution mode".into()),
        };
        let mut snapshots = Vec::new();
        // SAFETY: caller provides live registry handles for the entire call.
        for layer in unsafe { array(layers, len) }? {
            let kind = match layer.kind {
                0 => LayerKind::Flag,
                1 => LayerKind::User,
                2 => LayerKind::System,
                3 => LayerKind::Global,
                4 => LayerKind::Custom,
                _ => return Err("invalid registry layer kind".into()),
            };
            snapshots.push((
                kind,
                Arc::clone(&*lock(unsafe { object(layer.registry) }?)?),
            ));
        }
        // SAFETY: input is a readable attribute view.
        let result = resolve(
            snapshots
                .iter()
                .map(|(kind, registry)| (*kind, registry.as_ref())),
            unsafe { input.owned() }?,
            mode,
        )?;
        let result = IxeRegistryResolution {
            input: AttrsSnapshot::new(result.input),
            extra: AttrsSnapshot::new(result.extra),
        };
        // SAFETY: caller supplies writable output storage.
        unsafe { out.write(Box::into_raw(Box::new(result))) };
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_resolution_input(
    result: *const IxeRegistryResolution,
    out: *mut IxeRegistryAttrsView,
) -> *mut c_char {
    run(|| {
        output(out)?;
        // SAFETY: caller retains result and supplies writable output storage.
        unsafe { out.write(object(result)?.input.view()) };
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_resolution_extra(
    result: *const IxeRegistryResolution,
    out: *mut IxeRegistryAttrsView,
) -> *mut c_char {
    run(|| {
        output(out)?;
        // SAFETY: caller retains result and supplies writable output storage.
        unsafe { out.write(object(result)?.extra.view()) };
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_apply_overrides(
    input: IxeRegistryAttrsView,
    has_ref: u8,
    reference: IxeRegistryBytes,
    has_rev: u8,
    revision: IxeRegistryBytes,
    out: *mut *mut IxeRegistryAttrs,
) -> *mut c_char {
    run(|| {
        output(out)?;
        if has_ref > 1 || has_rev > 1 {
            return Err("invalid registry override presence flag".into());
        }
        // SAFETY: the caller supplies readable attributes and present strings.
        let result = unsafe {
            apply_overrides(
                &input.owned()?,
                if has_ref == 1 {
                    Some(reference.text()?)
                } else {
                    None
                },
                if has_rev == 1 {
                    Some(revision.text()?)
                } else {
                    None
                },
            )
        }?;
        // SAFETY: caller supplies writable output storage.
        unsafe {
            out.write(Box::into_raw(Box::new(IxeRegistryAttrs(
                AttrsSnapshot::new(result),
            ))))
        };
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_attrs_view(
    attrs: *const IxeRegistryAttrs,
    out: *mut IxeRegistryAttrsView,
) -> *mut c_char {
    run(|| {
        output(out)?;
        // SAFETY: caller retains attrs and supplies writable output storage.
        unsafe { out.write(object(attrs)?.0.view()) };
        Ok(())
    })
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_free(registry: *mut IxeRegistry) {
    if !registry.is_null() {
        // SAFETY: exclusive ownership of exactly one returned handle is transferred.
        unsafe { drop(Box::from_raw(registry)) };
    }
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_entries_free(entries: *mut IxeRegistryEntries) {
    if !entries.is_null() {
        // SAFETY: exclusive ownership of exactly one returned snapshot is transferred.
        unsafe { drop(Box::from_raw(entries)) };
    }
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_resolution_free(result: *mut IxeRegistryResolution) {
    if !result.is_null() {
        // SAFETY: exclusive ownership of exactly one returned result is transferred.
        unsafe { drop(Box::from_raw(result)) };
    }
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_attrs_free(attrs: *mut IxeRegistryAttrs) {
    if !attrs.is_null() {
        // SAFETY: exclusive ownership of exactly one returned snapshot is transferred.
        unsafe { drop(Box::from_raw(attrs)) };
    }
}

/// # Safety
/// Handles must be live and of the declared type; input views must remain readable
/// for the call, and outputs must be writable. Free functions take exclusive
/// ownership of one allocation returned by this runtime, exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_registry_string_free(text: *mut c_char) {
    if !text.is_null() {
        // SAFETY: text was returned by this runtime and has not yet been freed.
        unsafe { drop(CString::from_raw(text)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    fn string_attrs(values: &[(&str, &str)]) -> Attrs {
        values
            .iter()
            .map(|(name, value)| ((*name).to_owned(), Attr::String((*value).to_owned())))
            .collect()
    }

    unsafe fn success(error: *mut c_char) {
        if !error.is_null() {
            // SAFETY: test consumes a newly returned error string once.
            let message = unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned();
            unsafe { ixe_registry_string_free(error) };
            panic!("{message}");
        }
    }

    #[test]
    fn snapshots_survive_registry_mutation_and_destruction() {
        // SAFETY: every view/handle below is retained through its last use.
        unsafe {
            let mut registry = ptr::null_mut();
            let source = r#"{"version":2,"flakes":[{"from":{"type":"indirect","id":"hello"},"to":{"type":"jj","url":"file:///repo"}}]}"#;
            success(ixe_registry_parse(
                IxeRegistryBytes::view(source),
                &mut registry,
            ));
            let mut entries = ptr::null_mut();
            success(ixe_registry_entries(registry, &mut entries));
            let mut entry = std::mem::MaybeUninit::uninit();
            success(ixe_registry_entries_get(entries, 0, entry.as_mut_ptr()));
            let entry = entry.assume_init();
            success(ixe_registry_remove(registry, entry.from));
            ixe_registry_free(registry);
            assert_eq!(
                entry.to.owned().unwrap(),
                string_attrs(&[("type", "jj"), ("url", "file:///repo")])
            );
            assert_eq!(ixe_registry_entries_len(entries), 1);
            ixe_registry_entries_free(entries);
        }
    }

    #[test]
    fn invalid_boundaries_leave_outputs_untouched_and_replace_is_atomic() {
        // SAFETY: invalid cases use null pointers, checked before dereferencing.
        unsafe {
            let mut registry = ptr::null_mut();
            let error = ixe_registry_parse(IxeRegistryBytes::view(""), &mut registry);
            assert!(!error.is_null());
            assert!(registry.is_null());
            ixe_registry_string_free(error);
            success(ixe_registry_new(&mut registry));
            let error = ixe_registry_replace(registry, ptr::null(), 1);
            assert!(!error.is_null());
            ixe_registry_string_free(error);
            let mut entries = ptr::null_mut();
            success(ixe_registry_entries(registry, &mut entries));
            assert_eq!(ixe_registry_entries_len(entries), 0);
            ixe_registry_entries_free(entries);
            ixe_registry_free(registry);
        }
    }

    #[test]
    fn concurrent_additions_use_shared_handles_and_keep_old_snapshots_alive() {
        let registry = IxeRegistry(Mutex::new(Arc::new(Registry::default())));
        let registry = &registry;
        let mut before = ptr::null_mut();
        // SAFETY: registry lives until every thread has joined and snapshot use ends.
        unsafe { success(ixe_registry_entries(registry, &mut before)) };
        std::thread::scope(|scope| {
            for lane in 0..4 {
                scope.spawn(move || {
                    for index in 0..8 {
                        let entry = EntrySnapshot::new(Entry {
                            from: string_attrs(&[
                                ("type", "indirect"),
                                ("id", &format!("lane{lane}_{index}")),
                            ]),
                            to: string_attrs(&[("type", "jj"), ("url", "file:///repo")]),
                            extra: Attrs::new(),
                            exact: false,
                        });
                        // SAFETY: shared registry is synchronized inside the API;
                        // each thread owns its borrowed input until the call returns.
                        unsafe { success(ixe_registry_add(registry, entry.view())) };
                    }
                });
            }
        });
        let mut after = ptr::null_mut();
        // SAFETY: both snapshots and the registry remain live until explicitly freed.
        unsafe {
            success(ixe_registry_entries(registry, &mut after));
            assert_eq!(ixe_registry_entries_len(before), 0);
            assert_eq!(ixe_registry_entries_len(after), 32);
            ixe_registry_entries_free(before);
            ixe_registry_entries_free(after);
        }
    }
}
