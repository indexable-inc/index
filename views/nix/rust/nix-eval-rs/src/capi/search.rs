//! Search evaluates slots in the current question and its existing JobMemo.
use super::*;
use crate::search::{Catalogue, Plan};
use crate::session::{Question, SearchScope};
use std::ptr;

#[cfg(test)]
thread_local! {static SEARCH_VISITS: std::cell::Cell<usize> = const {std::cell::Cell::new(0)};}

fn error(session: &mut IxeSession, message: impl Into<String>) -> i32 {
    session.fail(&EvalError::eval(ErrKind::Eval, message))
}

fn path_text(path: &[String]) -> Result<String, String> {
    let mut bytes = Vec::new();
    for (index, name) in path.iter().enumerate() {
        if index != 0 {
            bytes.push(b'.');
        }
        crate::print::print_attr_name(name, &mut bytes);
    }
    String::from_utf8(bytes).map_err(|e| e.to_string())
}

fn optional_root(
    session: &mut IxeSession,
    root: Slot,
    path: &[String],
) -> Result<Option<Slot>, i32> {
    let mut current = root;
    for name in path {
        let Value::Attrs(attrs) = force_slot(session, current)? else {
            return Err(error(
                session,
                "search root ancestor is not an attribute set",
            ));
        };
        let name = session.vm.intern(name);
        let Some(child) = attrs.get(&name).cloned() else {
            return Ok(None);
        };
        current = child;
    }
    Ok(Some(current))
}

fn catalogue(session: &mut IxeSession, root: Slot) -> Result<Catalogue, i32> {
    let Some(Question::SearchPackages { selection, scope }) =
        session.question.as_ref().map(|q| q.question.clone())
    else {
        return Err(session.bad("search requires a SearchPackages question in flight"));
    };
    if selection.apply.is_some() {
        return Err(session.bad("search does not support an apply expression"));
    }
    #[cfg(test)]
    SEARCH_VISITS.set(0);
    let mut roots = Vec::new();
    match scope {
        SearchScope::Selected => {
            let selected = super::command::select(
                session,
                root,
                &selection.attr_paths,
                selection.index_lists,
            )?;
            let path = super::command::attr_path(&selected.attr_path)
                .map_err(|why| error(session, why))?;
            roots.push((
                if selection.index_lists || selection.auto_call {
                    auto_call(session, selected.slot)?
                } else {
                    selected.slot
                },
                path,
                true,
            ));
        }
        SearchScope::FlakeDefaults => {
            for path in &selection.attr_paths {
                let parts = super::command::attr_path(path).map_err(|why| error(session, why))?;
                if let Some(value) = optional_root(session, root.clone(), &parts)? {
                    let recursive = parts.first().is_some_and(|name| name == "legacyPackages");
                    roots.push((value, parts, recursive));
                }
            }
            if roots.is_empty() {
                return Err(error(session, "no package roots exist in this flake"));
            }
        }
    }
    let type_name = session.vm.intern("type");
    let name_name = session.vm.intern("name");
    let meta_name = session.vm.intern("meta");
    let description_name = session.vm.intern("description");
    let recurse_name = session.vm.intern("recurseForDerivations");
    let max_depth = usize::try_from(session.vm.settings().max_call_depth)
        .map_err(|_| error(session, "max-call-depth is not addressable"))?;
    let mut result = Catalogue::default();
    enum Visit {
        Enter {
            slot: Slot,
            name: Option<String>,
            root: bool,
        },
        Exit {
            attrs: Rc<crate::value2::Attrs>,
            before: usize,
        },
    }
    for (slot, mut path, recursive) in roots {
        let root_depth = path.len();
        let mut active = BTreeSet::new();
        let mut completed_empty = BTreeMap::new();
        let mut pending = vec![Visit::Enter {
            slot,
            name: None,
            root: true,
        }];
        while let Some(visit) = pending.pop() {
            let (slot, name, is_root) = match visit {
                Visit::Enter { slot, name, root } => (slot, name, root),
                Visit::Exit { attrs, before } => {
                    let identity = Rc::as_ptr(&attrs) as usize;
                    active.remove(&identity);
                    if result.0.len() == before {
                        completed_empty.insert(identity, attrs);
                    }
                    path.pop();
                    continue;
                }
            };
            #[cfg(test)]
            SEARCH_VISITS.set(SEARCH_VISITS.get() + 1);
            if let Some(name) = name {
                path.push(name);
            }
            let value = force_slot(session, slot)?;
            let Value::Attrs(attrs) = value else {
                if is_root {
                    return Err(error(
                        session,
                        "search root is not a package or attribute set",
                    ));
                }
                path.pop();
                continue;
            };
            let is_derivation = if let Some(type_slot) = attrs.get(&type_name).cloned() {
                matches!(force_slot(session,type_slot)?,Value::Str(text) if text.as_str()==Some("derivation"))
            } else {
                false
            };
            if is_derivation {
                let name = attrs
                    .get(&name_name)
                    .cloned()
                    .ok_or_else(|| error(session, "search package has no name"))?;
                let name =
                    force_string_no_context(session, name, "while reading search package name")?;
                let description = if let Some(meta) = attrs.get(&meta_name).cloned() {
                    let Value::Attrs(meta) = force_slot(session, meta)? else {
                        return Err(error(
                            session,
                            "search package meta must be an attribute set",
                        ));
                    };
                    if let Some(description) = meta.get(&description_name).cloned() {
                        force_string_no_context(
                            session,
                            description,
                            "while reading search package meta.description",
                        )?
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                let rendered = path_text(&path).map_err(|why| error(session, why))?;
                result
                    .insert(rendered, &name, description)
                    .map_err(|why| error(session, why))?;
                if !is_root {
                    path.pop();
                }
                continue;
            }
            let descend = if is_root {
                true
            } else if recursive {
                match attrs.get(&recurse_name).cloned() {
                    Some(flag) => match force_slot(session, flag)? {
                        Value::Bool(flag) => flag,
                        _ => return Err(error(session, "recurseForDerivations must be a Boolean")),
                    },
                    None => false,
                }
            } else {
                false
            };
            if !descend {
                if !is_root {
                    path.pop();
                }
                continue;
            }
            if path.len().saturating_sub(root_depth) > max_depth {
                return Err(error(session, "search traversal exceeded max-call-depth"));
            }
            let identity = Rc::as_ptr(&attrs) as usize;
            if completed_empty.contains_key(&identity) {
                if !is_root {
                    path.pop();
                }
                continue;
            }
            if !active.insert(identity) {
                return Err(error(session, "cycle in recursive search package set"));
            }
            // Root Exit's path pop is harmless: no later sibling uses the root path.
            pending.push(Visit::Exit {
                attrs: attrs.clone(),
                before: result.0.len(),
            });
            let mut children: Vec<_> = attrs
                .iter()
                .filter(|(sym, _)| **sym != recurse_name)
                .map(|(sym, slot)| (session.vm.sym_name(*sym).to_owned(), slot.clone()))
                .collect();
            children.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            for (name, slot) in children.into_iter().rev() {
                pending.push(Visit::Enter {
                    slot,
                    name: Some(name),
                    root: false,
                });
            }
        }
    }
    Ok(result)
}

/// # Safety
/// Live exclusive session, valid root handle and writable owned-string output.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_search_execute(
    session: *mut IxeSession,
    root: u64,
    out: *mut *mut c_char,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null search output");
    }
    unsafe {
        *out = ptr::null_mut();
    }
    let Some(root) = session.get(root).cloned() else {
        return session.bad("unknown search root");
    };
    match catalogue(session, root)
        .and_then(|found| found.encode().map_err(|why| error(session, why)))
    {
        Ok(encoded) => out_string(encoded, out),
        Err(status) => status,
    }
}

pub struct IxeSearchPlan(Plan);
pub struct IxeSearchCatalogue(Catalogue);
fn run(error: *mut *mut c_char, action: impl FnOnce() -> Result<(), String>) -> i32 {
    if error.is_null() {
        return IXE_ERR_BADCALL;
    }
    unsafe {
        *error = ptr::null_mut();
    }
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(action))
        .unwrap_or_else(|_| Err("search operation panicked".into()))
    {
        Ok(()) => IXE_OK,
        Err(why) => {
            out_string(why, error);
            IXE_ERR_BADCALL
        }
    }
}
unsafe fn bytes<'a>(text: *const u8, len: usize) -> Result<&'a [u8], String> {
    if len == 0 {
        return Ok(&[]);
    }
    if text.is_null() || len > isize::MAX as usize {
        return Err("invalid search byte span".into());
    }
    Ok(unsafe { std::slice::from_raw_parts(text, len) })
}
unsafe fn patterns(text: *const IxeBytes, len: usize) -> Result<Vec<String>, String> {
    if len == 0 {
        return Ok(Vec::new());
    }
    if text.is_null() || len > isize::MAX as usize / std::mem::size_of::<IxeBytes>() {
        return Err("invalid search pattern array".into());
    }
    unsafe { std::slice::from_raw_parts(text, len) }
        .iter()
        .map(|value| {
            std::str::from_utf8(unsafe { bytes(value.text, value.len)? })
                .map(str::to_owned)
                .map_err(|e| e.to_string())
        })
        .collect()
}
/// # Safety
/// Inputs readable for their lengths; output/error slots writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_search_plan_new(
    include: *const IxeBytes,
    include_len: usize,
    exclude: *const IxeBytes,
    exclude_len: usize,
    json: i32,
    colored: i32,
    out: *mut *mut IxeSearchPlan,
    error: *mut *mut c_char,
) -> i32 {
    run(error, || {
        if out.is_null() {
            return Err("null search plan output".into());
        }
        unsafe {
            *out = ptr::null_mut();
        }
        if !matches!(json, 0 | 1) || !matches!(colored, 0 | 1) {
            return Err("invalid search render flags".into());
        }
        let plan = Plan::new(
            &unsafe { patterns(include, include_len)? },
            &unsafe { patterns(exclude, exclude_len)? },
            json == 1,
            colored == 1,
        )?;
        unsafe {
            *out = Box::into_raw(Box::new(IxeSearchPlan(plan)));
        }
        Ok(())
    })
}
/// # Safety
/// Null or an owned live plan returned by plan_new, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_search_plan_free(plan: *mut IxeSearchPlan) {
    if !plan.is_null() {
        drop(unsafe { Box::from_raw(plan) });
    }
}
/// # Safety
/// Input readable for length; error slot writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_search_catalogue_decode(
    text: *const u8,
    len: usize,
    out: *mut *mut IxeSearchCatalogue,
    error: *mut *mut c_char,
) -> i32 {
    run(error, || {
        if out.is_null() {
            return Err("null search catalogue output".into());
        }
        // SAFETY: caller supplies a writable output slot.
        unsafe {
            *out = ptr::null_mut();
        }
        let catalogue = Catalogue::decode(unsafe { bytes(text, len)? })?;
        unsafe {
            *out = Box::into_raw(Box::new(IxeSearchCatalogue(catalogue)));
        }
        Ok(())
    })
}
/// # Safety
/// Null or an owned live catalogue returned by catalogue_decode, freed once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_search_catalogue_free(catalogue: *mut IxeSearchCatalogue) {
    if !catalogue.is_null() {
        drop(unsafe { Box::from_raw(catalogue) });
    }
}
/// # Safety
/// Plan and catalogue live; output/error slots writable and disjoint.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_search_plan_render(
    plan: *const IxeSearchPlan,
    catalogue: *const IxeSearchCatalogue,
    out: *mut *mut c_char,
    error: *mut *mut c_char,
) -> i32 {
    if out == error {
        return IXE_ERR_BADCALL;
    }
    run(error, || {
        if out.is_null() {
            return Err("null search render output".into());
        }
        unsafe {
            *out = ptr::null_mut();
        }
        let plan = unsafe { plan.as_ref() }.ok_or("null search plan")?;
        let catalogue = unsafe { catalogue.as_ref() }.ok_or("null search catalogue")?;
        let output = plan.0.render(&catalogue.0)?;
        if out_string(output, out) != 0 {
            return Err("search output contains a NUL byte".into());
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Selection;
    use std::ffi::CStr;

    fn evaluate(source: &str, paths: &[&str], scope: SearchScope) -> Result<Catalogue, String> {
        let _globals = crate::eval::globals_shared();
        let host = EmbedderHost::new(IxeHostVtable::empty()).map_err(|e| format!("{e:?}"))?;
        let mut session = IxeSession::new(host);
        let module = session
            .vm
            .compile(source, ".", crate::compile::Origin::String)
            .map_err(|e| format!("{e:?}"))?
            .module;
        let mut memo = crate::eval::JobMemo::default();
        let value =
            crate::session::run_to_value_with(&mut session.vm, &module, &session.host, &mut memo)
                .map_err(|e| format!("{e:?}"))?;
        let root = session.insert(Slot::value(value));
        let selection = Selection {
            attr_paths: paths.iter().map(|s| (*s).to_owned()).collect(),
            ..Selection::one("")
        };
        let question = Question::SearchPackages {
            selection: selection.clone(),
            scope,
        };
        session.question = Some(
            in_flight_question(
                &mut session,
                &selection,
                IXE_QUESTION_SEARCH_PACKAGES,
                question,
            )
            .map_err(|s| format!("question status {s}"))?,
        );
        let mut output = ptr::null_mut();
        // SAFETY: live exclusive session, valid root and writable output.
        let status = unsafe { ixe_search_execute(&mut session, root, &mut output) };
        if status != 0 {
            return Err(session
                .last_error
                .map_or_else(|| format!("status {status}"), |e| e.message));
        }
        // SAFETY: successful execution returns one owned NUL-terminated string.
        let found = Catalogue::decode(unsafe { CStr::from_ptr(output) }.to_bytes());
        unsafe {
            ixe_string_free(output);
        }
        found
    }

    #[test]
    fn default_roots_union_and_recursion_have_distinct_rules() -> Result<(), String> {
        let found = evaluate(
            r#"let p = {type="derivation";name="demo-2";drvPath=throw "must stay lazy";}; in {
            packages.system = { direct=p; nested={recurseForDerivations=true;hidden=p;}; };
            legacyPackages.system = { alias=p; group={recurseForDerivations=true;deep=p;}; ignored={hidden=throw "not visited";}; };
        }"#,
            &["packages.system", "legacyPackages.system"],
            SearchScope::FlakeDefaults,
        )?;
        let paths: Vec<_> = found.0.keys().map(String::as_str).collect();
        assert_eq!(
            paths,
            [
                "legacyPackages.system.alias",
                "legacyPackages.system.group.deep",
                "packages.system.direct"
            ]
        );
        let missing = evaluate(
            r#"{legacyPackages.system.one={type="derivation";name="one";};}"#,
            &["packages.system", "legacyPackages.system"],
            SearchScope::FlakeDefaults,
        )?;
        assert_eq!(missing.0.len(), 1);
        Ok(())
    }
    #[test]
    fn strict_metadata_and_recursion_fail_without_partial_catalogues() {
        for source in [
            r#"{ok={type="derivation";name="ok";}; bad={type="derivation";name="bad";meta.description=throw "metadata failed";};}"#,
            r#"{nested={recurseForDerivations="yes";};}"#,
            r#"rec {nested={recurseForDerivations=true;inherit nested;};}"#,
        ] {
            assert!(evaluate(source, &[""], SearchScope::Selected).is_err());
        }
    }
    #[test]
    fn shared_empty_subtrees_are_visited_linearly() -> Result<(), String> {
        let found = evaluate(
            r#"let mk = n: if n == 0 then {recurseForDerivations=true;} else
            let child=mk (n - 1); in {recurseForDerivations=true;a=child;b=child;}; in mk 30"#,
            &[""],
            SearchScope::Selected,
        )?;
        assert!(found.0.is_empty());
        assert!(
            SEARCH_VISITS.get() <= 61,
            "empty aliases were expanded repeatedly"
        );
        Ok(())
    }

    #[test]
    fn shared_package_subtrees_keep_every_alias_path() -> Result<(), String> {
        let found = evaluate(
            r#"let mk = n: if n == 0 then {type="derivation";name="leaf-1";} else
            let child=mk (n - 1); in {recurseForDerivations=true;a=child;b=child;}; in mk 4"#,
            &[""],
            SearchScope::Selected,
        )?;
        assert_eq!(found.0.len(), 16);
        assert!(found.0.contains_key("a.a.a.a"));
        assert!(found.0.contains_key("b.b.b.b"));
        Ok(())
    }

    #[test]
    fn selected_scope_and_default_scope_never_share_a_key() {
        let selection = Selection::one("packages.system");
        let selected = Question::SearchPackages {
            selection: selection.clone(),
            scope: SearchScope::Selected,
        };
        let defaults = Question::SearchPackages {
            selection,
            scope: SearchScope::FlakeDefaults,
        };
        assert_ne!(selected.fingerprint(), defaults.fingerprint());
        assert_ne!(
            selected.fingerprint(),
            Question::DerivationSet {
                selection: Selection::one("packages.system")
            }
            .fingerprint()
        );
    }
    #[test]
    fn plan_abi_validates_and_renders_owned_outputs() -> Result<(), String> {
        let pattern = IxeBytes {
            text: b"^".as_ptr(),
            len: 1,
        };
        let mut plan = ptr::null_mut();
        let mut error = ptr::null_mut();
        // SAFETY: live input, distinct writable output slots.
        assert_eq!(
            unsafe {
                ixe_search_plan_new(&pattern, 1, ptr::null(), 0, 1, 0, &mut plan, &mut error)
            },
            0
        );
        let mut catalogue = Catalogue::default();
        catalogue.insert("pkg".into(), "pkg-1", String::new())?;
        let canonical = catalogue.encode()?;
        let mut decoded = ptr::null_mut();
        assert_eq!(
            unsafe {
                ixe_search_catalogue_decode(
                    canonical.as_ptr(),
                    canonical.len(),
                    &mut decoded,
                    &mut error,
                )
            },
            0
        );
        drop(canonical);
        let mut output = ptr::null_mut();
        assert_eq!(
            unsafe { ixe_search_plan_render(plan, decoded, &mut output, &mut error,) },
            0
        );
        assert!(
            unsafe { CStr::from_ptr(output) }
                .to_bytes()
                .starts_with(b"{\"pkg\":")
        );
        unsafe {
            ixe_string_free(output);
            ixe_search_plan_free(plan);
            ixe_search_catalogue_free(decoded);
        }
        decoded = ptr::null_mut();
        assert_eq!(
            unsafe { ixe_search_catalogue_decode(b"{}".as_ptr(), 2, &mut decoded, &mut error) },
            4
        );
        assert!(!error.is_null());
        unsafe {
            ixe_string_free(error);
        }
        Ok(())
    }
}
