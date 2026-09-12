//! Production command policy over the existing incremental evaluation session.
//!
//! The cache owns canonical rendered answers. This module owns selection and
//! presentation, and never opens a second VM or imports an expression wrapper.

use super::*;
use crate::session::{Question, Selection};

const RAW: u32 = 1 << 0;
const JSON: u32 = 1 << 1;
const PRETTY: u32 = 1 << 2;
const FILE: u32 = 1 << 3;
const EXPR: u32 = 1 << 4;
const WRITE_TO: u32 = 1 << 5;
const FLAGS: u32 = RAW | JSON | PRETTY | FILE | EXPR | WRITE_TO;

struct EvalPlan {
    render: RenderMode,
    pretty: bool,
}

struct OutputTransform {
    transformed: Option<Vec<u8>>,
    append_newline: bool,
}

impl EvalPlan {
    fn new(flags: u32) -> Result<Self, String> {
        if flags & !FLAGS != 0 {
            return Err("unknown eval command flags".to_owned());
        }
        if flags & RAW != 0 && flags & JSON != 0 {
            return Err("--raw and --json are mutually exclusive".to_owned());
        }
        if flags & FILE != 0 && flags & EXPR != 0 {
            return Err("'--file' and '--expr' are exclusive".to_owned());
        }
        if flags & WRITE_TO != 0 {
            return Err("nix eval --write-to is not supported".to_owned());
        }
        Ok(Self {
            render: if flags & JSON != 0 {
                RenderMode::Json
            } else if flags & RAW != 0 {
                RenderMode::Raw
            } else {
                RenderMode::ValuePrinter
            },
            pretty: flags & PRETTY != 0,
        })
    }

    fn format(&self, answer: &[u8]) -> Result<OutputTransform, String> {
        let transformed = if self.render == RenderMode::Json && self.pretty {
            let value: serde_json::Value = serde_json::from_slice(answer)
                .map_err(|error| format!("invalid JSON evaluation answer: {error}"))?;
            Some(
                serde_json::to_vec_pretty(&value)
                    .map_err(|error| format!("cannot format JSON evaluation answer: {error}"))?,
            )
        } else {
            None
        };
        Ok(OutputTransform {
            transformed,
            append_newline: self.render != RenderMode::Raw,
        })
    }
}

/// Dots delimit unquoted components; quoted components use JSON escaping.
/// This is a selection grammar, not an expression to compile or evaluate.
pub(super) fn attr_path(path: &str) -> Result<Vec<String>, String> {
    if path.is_empty() {
        return Ok(Vec::new());
    }
    let mut components = Vec::new();
    let mut rest = path;
    loop {
        let component;
        if rest.starts_with('"') {
            let mut stream = serde_json::Deserializer::from_str(rest).into_iter::<String>();
            component = stream
                .next()
                .ok_or_else(|| format!("missing attribute in selection path '{path}'"))?
                .map_err(|error| {
                    format!("invalid quoted attribute in selection path '{path}': {error}")
                })?;
            rest = rest.split_at(stream.byte_offset()).1;
        } else {
            let end = rest.find('.').unwrap_or(rest.len());
            let (name, tail) = rest.split_at(end);
            component = name.to_owned();
            rest = tail;
            if component.is_empty() || component.contains('"') {
                return Err(format!("invalid attribute in selection path '{path}'"));
            }
        }
        components.push(component);
        if rest.is_empty() {
            return Ok(components);
        }
        rest = rest
            .strip_prefix('.')
            .ok_or_else(|| format!("expected '.' in selection path '{path}'"))?;
        if rest.is_empty() {
            return Err(format!(
                "missing attribute after '.' in selection path '{path}'"
            ));
        }
    }
}

fn evaluation_error(session: &mut IxeSession, message: impl Into<String>) -> i32 {
    session.fail(&EvalError::eval(ErrKind::Eval, message))
}

struct MissingSelection {
    attribute: String,
    path: String,
    attrs: std::rc::Rc<crate::value2::Attrs>,
}

fn missing_selection_error(session: &mut IxeSession, message: String) -> i32 {
    // Store the normal structured diagnostic, then preserve the command-level
    // missing-selection category across the ABI for callers such as help.
    let _ = evaluation_error(session, message);
    IXE_ERR_ATTR_PATH_NOT_FOUND
}

pub(super) struct Selected {
    pub(super) slot: Slot,
    pub(super) attr_path: String,
}

fn question_selection(question: &Question) -> Option<&Selection> {
    match question {
        Question::Select { selection, .. }
        | Question::Derivation { selection }
        | Question::DerivationSet { selection }
        | Question::DerivationPath { selection }
        | Question::Application { selection }
        | Question::SourcePosition { selection }
        | Question::SearchPackages { selection, .. }
        | Question::FlakeCheck { selection, .. }
        | Question::FlakeShow { selection, .. } => Some(selection),
        Question::Whole { .. } | Question::FlakeDocument => None,
    }
}

pub(super) fn select(
    session: &mut IxeSession,
    root: Slot,
    paths: &[String],
    index_lists: bool,
) -> Result<Selected, i32> {
    let mut missing = None;
    'candidate: for path in paths {
        let components = attr_path(path).map_err(|error| session.fail(&EvalError::Parse(error)))?;
        let mut current = root.clone();
        for component in components {
            if index_lists {
                current = auto_call(session, current)?;
            }
            let value = force_slot(session, current)?;
            let index = if index_lists
                && !component.is_empty()
                && component.bytes().all(|byte| byte.is_ascii_digit())
            {
                Some(component.parse::<usize>().map_err(|_| {
                    evaluation_error(session, format!("list index '{component}' is too large"))
                })?)
            } else {
                None
            };
            current = match (value, index) {
                (Value::List(items), Some(index)) => {
                    items.get(index).cloned().ok_or_else(|| {
                        missing_selection_error(
                            session,
                            format!(
                                "list index {index} in selection path '{}' is out of range",
                                crate::terminal::terminal_text(path)
                            ),
                        )
                    })?
                }
                (Value::Attrs(attrs), None) => {
                    let symbol = session.vm.intern(&component);
                    match attrs.get(&symbol).cloned() {
                        Some(slot) => slot,
                        None => {
                            missing = Some(MissingSelection {
                                attribute: component,
                                path: path.clone(),
                                attrs,
                            });
                            continue 'candidate;
                        }
                    }
                }
                (value, index) => {
                    let expected = if index.is_some() { "list" } else { "set" };
                    return Err(evaluation_error(
                        session,
                        format!(
                            "the expression selected by the selection path '{path}' should be a {expected} but is {}",
                            crate::value2::type_name(&value)
                        ),
                    ));
                }
            };
        }
        // Existence chooses the candidate. A failing value is an error, never
        // an excuse to silently select a different candidate.
        force_slot(session, current.clone())?;
        return Ok(Selected {
            slot: current,
            attr_path: path.clone(),
        });
    }
    let Some(missing) = missing else {
        return Err(session.bad("attribute selection requires a candidate path"));
    };
    let suggestions = crate::suggestions::best_matches(
        &missing.attribute,
        missing
            .attrs
            .iter()
            .map(|(symbol, _)| session.vm.sym_name(*symbol)),
    );
    let mut message = format!(
        "attribute '{}' in selection path '{}' not found",
        crate::terminal::terminal_text(&missing.attribute),
        crate::terminal::terminal_text(&missing.path),
    );
    if !suggestions.is_empty() {
        message.push_str("\nDid you mean ");
        if suggestions.len() > 1 {
            message.push_str("one of ");
        }
        for (index, name) in suggestions.iter().enumerate() {
            if index > 0 {
                message.push_str(if index + 1 == suggestions.len() {
                    " or "
                } else {
                    ", "
                });
            }
            if name.is_empty() {
                message.push_str("\"\"");
            } else {
                message.push_str(&crate::terminal::terminal_text(name));
            }
        }
        message.push('?');
    }
    Err(missing_selection_error(session, message))
}

fn execute(session: &mut IxeSession, root: u64) -> Result<Vec<u8>, i32> {
    let Some(Question::Select { selection, render }) = session
        .question
        .as_ref()
        .map(|question| question.question.clone())
    else {
        return Err(session.bad("eval command requires a Select question in flight"));
    };
    let root = session
        .get(root)
        .cloned()
        .ok_or_else(|| session.bad("unknown eval command root handle"))?;
    let mut current = select(session, root, &selection.attr_paths, selection.index_lists)?.slot;
    if selection.auto_call {
        current = auto_call(session, current)?;
    }
    if let Some(apply) = selection.apply {
        let module = session
            .vm
            .compile(&apply.text, &apply.base, crate::compile::Origin::String)
            .map_err(|error| session.fail(&EvalError::from(error)))?
            .module;
        let function = {
            let mut fresh = crate::eval::JobMemo::default();
            let (vm, host, memo) = machine_and_host(session, &mut fresh);
            crate::session::run_to_value_with(vm, &module, host, memo)
                .map_err(|error| session.fail(&error))?
        };
        if !is_callable(session, &function) {
            return Err(evaluation_error(
                session,
                format!(
                    "attempt to call something which is not a function but {}",
                    crate::value2::type_name(&function)
                ),
            ));
        }
        current = Slot::pending(Slot::value(function), vec![current]);
    }
    let value = force_slot(session, current)?;
    let mut fresh = crate::eval::JobMemo::default();
    let (vm, host, memo) = machine_and_host(session, &mut fresh);
    crate::session::render_with(vm, host, value, render, memo).map_err(|error| session.fail(&error))
}

/// # Safety
/// Non-null inputs must be readable for their lengths. Output pointers must
/// be writable and receive strings released with `ixe_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_eval_command_validate(
    flags: u32,
    installable: *const u8,
    installable_len: usize,
    render: *mut i32,
    error: *mut *mut c_char,
) -> i32 {
    if render.is_null() || error.is_null() || (installable.is_null() && installable_len != 0) {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: checked above; caller provides writable output slots.
    unsafe {
        *error = std::ptr::null_mut();
    }
    let plan = EvalPlan::new(flags).and_then(|plan| {
        // SAFETY: caller guarantees input lifetime and length.
        let installable = unsafe { borrow_str(installable, installable_len) }
            .map_err(|()| "installable is not UTF-8".to_owned())?
            .unwrap_or("");
        if installable.contains('^') {
            return Err("derivation output selection is not supported by nix eval".to_owned());
        }
        Ok(plan)
    });
    match plan {
        Ok(plan) => {
            // SAFETY: checked above; caller provides a writable output slot.
            unsafe {
                *render = match plan.render {
                    RenderMode::Raw => IXE_RENDER_RAW,
                    RenderMode::Json => IXE_RENDER_JSON,
                    _ => IXE_RENDER_VALUE_PRINTER,
                };
            }
            IXE_OK
        }
        Err(message) => {
            out_string(message, error);
            IXE_ERR_BADCALL
        }
    }
}

/// # Safety
/// `session` must be live. `out` must be writable. The returned string is
/// owned by the caller and released with `ixe_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_eval_command_execute(
    session: *mut IxeSession,
    root: u64,
    out: *mut *mut c_char,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null eval command output pointer");
    }
    // SAFETY: checked above; caller provides a writable output slot.
    unsafe {
        *out = std::ptr::null_mut();
    }
    match execute(session, root) {
        Ok(bytes) => out_bytes(&bytes, out),
        Err(status) => status,
    }
}

/// Select one candidate using only the selection retained in the cache question.
/// Force the selected value, including any final auto-application, to WHNF.
/// The returned handle is owned by the caller; the original root stays live.
///
/// # Safety
/// `session` must be live. Both output pointers must be writable. The selected
/// path is released with `ixe_string_free`, and the handle with `ixe_handle_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_question_select(
    session: *mut IxeSession,
    root: u64,
    out_handle: *mut u64,
    out_path: *mut *mut c_char,
) -> i32 {
    let session = session!(session);
    if out_handle.is_null() || out_path.is_null() {
        return session.bad("null question selection output pointer");
    }
    // SAFETY: checked writable slots are supplied by the caller.
    unsafe {
        *out_handle = 0;
        *out_path = std::ptr::null_mut();
    }
    let Some(selection) = session
        .question
        .as_ref()
        .and_then(|question| question_selection(&question.question))
        .cloned()
    else {
        return session.bad("question selection requires a selection-bearing question in flight");
    };
    let Some(root) = session.get(root).cloned() else {
        return session.bad("unknown question selection root handle");
    };
    match select(session, root, &selection.attr_paths, selection.index_lists) {
        Ok(selected) => {
            // The retained selection owns final application as well as the
            // attribute walk, including when the selected path is empty.
            let slot = if selection.auto_call {
                match auto_call(session, selected.slot) {
                    Ok(slot) => slot,
                    Err(status) => return status,
                }
            } else {
                selected.slot
            };
            // auto_call returns a pending application. Preserve select's
            // forced-result contract before the bridge inspects its type.
            if let Err(status) = force_slot(session, slot.clone()) {
                return status;
            }
            let status = out_string(selected.attr_path, out_path);
            if status != IXE_OK {
                return status;
            }
            let handle = session.insert(slot);
            // SAFETY: checked writable slot is supplied by the caller.
            unsafe {
                *out_handle = handle;
            }
            IXE_OK
        }
        Err(status) => status,
    }
}

/// Select exactly one entry in a derivation-set traversal. An absent entry
/// fails; the next requested path is never used as a substitute.
///
/// # Safety
/// `session` must be live and `out_handle` writable. The returned handle is
/// released with `ixe_handle_free`; the original root remains owned by its caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_question_select_at(
    session: *mut IxeSession,
    root: u64,
    index: usize,
    out_handle: *mut u64,
) -> i32 {
    let session = session!(session);
    if out_handle.is_null() {
        return session.bad("null indexed question selection output pointer");
    }
    // SAFETY: checked writable slot is supplied by the caller.
    unsafe {
        *out_handle = 0;
    }
    let Some(Question::DerivationSet { selection }) =
        session.question.as_ref().map(|question| &question.question)
    else {
        return session
            .bad("indexed question selection requires a DerivationSet question in flight");
    };
    let Some(path) = selection.attr_paths.get(index).cloned() else {
        return session.bad(format!(
            "derivation-set selection index {index} is out of range"
        ));
    };
    let index_lists = selection.index_lists;
    let Some(root) = session.get(root).cloned() else {
        return session.bad("unknown indexed question selection root handle");
    };
    match select(session, root, std::slice::from_ref(&path), index_lists) {
        Ok(selected) => {
            let handle = session.insert(selected.slot);
            // SAFETY: checked writable slot is supplied by the caller.
            unsafe {
                *out_handle = handle;
            }
            IXE_OK
        }
        Err(status) => status,
    }
}

/// Select an application and derive its expected type from that same selection.
/// Source expressions use derivations; flake app namespaces use explicit apps.
///
/// # Safety
/// `session` must be live and all output pointers writable. The returned handle
/// is released with `ixe_handle_free`, and both strings with `ixe_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_question_select_app(
    session: *mut IxeSession,
    root: u64,
    out_handle: *mut u64,
    out_path: *mut *mut c_char,
    out_expected_type: *mut *mut c_char,
) -> i32 {
    let session = session!(session);
    if out_handle.is_null() || out_path.is_null() || out_expected_type.is_null() {
        return session.bad("null application selection output pointer");
    }
    // SAFETY: checked writable slots are supplied by the caller.
    unsafe {
        *out_handle = 0;
        *out_path = std::ptr::null_mut();
        *out_expected_type = std::ptr::null_mut();
    }
    let Some(Question::Application { selection }) = session
        .question
        .as_ref()
        .map(|question| question.question.clone())
    else {
        return session.bad("application selection requires an Application question in flight");
    };
    let Some(root) = session.get(root).cloned() else {
        return session.bad("unknown application selection root handle");
    };
    let selected = match select(session, root, &selection.attr_paths, selection.index_lists) {
        Ok(selected) => selected,
        Err(status) => return status,
    };
    let components = match attr_path(&selected.attr_path) {
        Ok(components) => components,
        Err(error) => return session.fail(&EvalError::Parse(error)),
    };
    let expected = if !selection.index_lists
        && matches!(
            components.first().map(String::as_str),
            Some("apps" | "defaultApp")
        ) {
        "app"
    } else {
        "derivation"
    };
    let status = out_string(selected.attr_path, out_path);
    if status != IXE_OK {
        return status;
    }
    let status = out_string(expected.to_owned(), out_expected_type);
    if status != IXE_OK {
        return status;
    }
    let handle = session.insert(selected.slot);
    // SAFETY: checked writable slot is supplied by the caller.
    unsafe {
        *out_handle = handle;
    }
    IXE_OK
}

/// Describe final output without copying unchanged canonical bytes.
///
/// # Safety
/// `answer` must be readable for `answer_len` bytes (or null for zero bytes).
/// All output pointers must be writable. A non-null transformed string is
/// released with `ixe_string_free`; null means keep the input unchanged.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_eval_command_format(
    flags: u32,
    answer: *const u8,
    answer_len: usize,
    out: *mut *mut c_char,
    append_newline: *mut i32,
    error: *mut *mut c_char,
) -> i32 {
    if out.is_null()
        || append_newline.is_null()
        || error.is_null()
        || (answer.is_null() && answer_len != 0)
    {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: output slots checked; input is readable by the caller contract.
    let answer = unsafe {
        *out = std::ptr::null_mut();
        *append_newline = 0;
        *error = std::ptr::null_mut();
        if answer_len == 0 {
            &[]
        } else {
            slice::from_raw_parts(answer, answer_len)
        }
    };
    match EvalPlan::new(flags).and_then(|plan| plan.format(answer)) {
        Ok(transform) => {
            // SAFETY: checked writable slot is supplied by the caller.
            unsafe {
                *append_newline = i32::from(transform.append_newline);
            }
            match transform.transformed {
                Some(bytes) => out_bytes(&bytes, out),
                None => IXE_OK,
            }
        }
        Err(message) => {
            out_string(message, error);
            IXE_ERR_BADCALL
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct EvaluationFailure {
        status: i32,
        message: String,
    }

    fn evaluate_with_status(
        source: &str,
        selection: Selection,
    ) -> Result<Vec<u8>, EvaluationFailure> {
        let _globals = crate::eval::globals_shared();
        let host = EmbedderHost::new(IxeHostVtable::empty()).unwrap();
        let mut session = IxeSession::new(host);
        let module = session
            .vm
            .compile(source, ".", crate::compile::Origin::String)
            .unwrap()
            .module;
        let mut memo = crate::eval::JobMemo::default();
        let value =
            crate::session::run_to_value_with(&mut session.vm, &module, &session.host, &mut memo)
                .unwrap();
        let root = session.insert(Slot::value(value));
        let question = Question::Select {
            selection: selection.clone(),
            render: RenderMode::Json,
        };
        session.question = Some(
            in_flight_question(&mut session, &selection, IXE_QUESTION_SELECT, question).unwrap(),
        );
        execute(&mut session, root).map_err(|status| EvaluationFailure {
            status,
            message: session.last_error.unwrap().message,
        })
    }

    fn evaluate(source: &str, selection: Selection) -> Result<Vec<u8>, String> {
        evaluate_with_status(source, selection).map_err(|error| error.message)
    }

    #[test]
    fn missing_selection_has_its_own_status_and_does_not_force_suggestions() {
        let source = r#"{ foo = throw "suggestion was forced"; }"#;
        let missing = evaluate_with_status(source, Selection::one("foa")).unwrap_err();
        assert_eq!(missing.status, IXE_ERR_ATTR_PATH_NOT_FOUND);
        assert!(missing.message.contains("Did you mean foo?"));
        assert!(!missing.message.contains("suggestion was forced"));
        let thrown = evaluate_with_status(source, Selection::one("foo")).unwrap_err();
        assert_eq!(thrown.status, IXE_ERR_THROWN);
        assert!(thrown.message.contains("suggestion was forced"));
        let index = evaluate_with_status("{ items = []; }", Selection::one("items.0")).unwrap_err();
        assert_eq!(index.status, IXE_ERR_ATTR_PATH_NOT_FOUND);
    }

    #[test]
    fn suggestions_escape_control_names_and_candidate_success_discards_misses() {
        let source = r#"{ "c\nd" = throw "unforced"; "é" = throw "unforced"; answer = 42; }"#;
        let missing = evaluate_with_status(source, Selection::one("cd")).unwrap_err();
        assert_eq!(missing.status, IXE_ERR_ATTR_PATH_NOT_FOUND);
        assert!(missing.message.contains(r"c\nd"));
        assert!(!missing.message.contains("c\nd"));
        let selection = Selection {
            attr_paths: vec!["cd".to_owned(), "answer".to_owned()],
            ..Selection::one("")
        };
        assert_eq!(evaluate(source, selection).unwrap(), b"42");
    }

    #[test]
    fn command_selects_lazily_and_applies_in_the_same_session() {
        let mut selection = Selection::one("items.1.answer");
        selection.apply = Some(crate::session::Apply {
            text: "x: x + 1".to_owned(),
            base: ".".to_owned(),
        });
        assert_eq!(evaluate(
            "{ items = [ (throw \"unselected item\") { answer = 41; } ]; unused = throw \"unselected attribute\"; }",
            selection,
        ).unwrap(), b"42");
        assert_eq!(
            evaluate(
                "{ \"a.b\" = { \"\" = 7; }; }",
                Selection::one(r#""a.b"."""#)
            )
            .unwrap(),
            b"7"
        );
    }

    #[test]
    fn command_only_falls_through_missing_candidates() {
        let mut selection = Selection::one("missing");
        selection.attr_paths.push("answer".to_owned());
        assert_eq!(
            evaluate("{ answer = 42; }", selection.clone()).unwrap(),
            b"42"
        );
        assert!(
            evaluate(
                "{ missing = throw \"chosen failure\"; answer = 42; }",
                selection
            )
            .unwrap_err()
            .contains("chosen failure")
        );
    }

    #[test]
    fn derivation_selection_forces_final_auto_application() {
        let _globals = crate::eval::globals_shared();
        let host = EmbedderHost::new(IxeHostVtable::empty()).unwrap();
        let mut session = IxeSession::new(host);
        let module = session
            .vm
            .compile(
                "{ name, suffix ? \"default\" }: { value = name + suffix; unused = throw \"lazy field\"; }",
                ".",
                crate::compile::Origin::String,
            )
            .unwrap()
            .module;
        let mut memo = crate::eval::JobMemo::default();
        let value =
            crate::session::run_to_value_with(&mut session.vm, &module, &session.host, &mut memo)
                .unwrap();
        let root = session.insert(Slot::value(value));
        let mut selection = Selection::one("");
        selection.auto_call = true;
        selection.auto_args.push(crate::session::AutoArg {
            name: "name".to_owned(),
            value: crate::session::AutoArgValue::Str("called".to_owned()),
        });
        let question = Question::Derivation {
            selection: selection.clone(),
        };
        session.question = Some(
            in_flight_question(&mut session, &selection, IXE_QUESTION_DERIVATION, question)
                .unwrap(),
        );
        let mut handle = 0;
        let mut path = std::ptr::null_mut();
        // SAFETY: all pointers stay live; the returned path and handle are
        // released once. The type query intentionally does not force a value.
        unsafe {
            assert_eq!(
                ixe_question_select(&mut session, root, &mut handle, &mut path),
                IXE_OK
            );
            assert_eq!(ixe_value_type(&mut session, handle), IXE_TYPE_ATTRS);
            assert_eq!(CStr::from_ptr(path).to_bytes(), b"");
            ixe_string_free(path);
            ixe_handle_free(&mut session, handle);
            ixe_handle_free(&mut session, root);
        }
    }

    #[test]
    fn generic_selection_returns_the_retained_candidate_for_every_question_kind() {
        let _globals = crate::eval::globals_shared();
        let host = EmbedderHost::new(IxeHostVtable::empty()).unwrap();
        let mut session = IxeSession::new(host);
        let module = session
            .vm
            .compile(
                "{ \"a.b\" = { \"\" = 42; }; unused = throw \"not selected\"; }",
                ".",
                crate::compile::Origin::String,
            )
            .unwrap()
            .module;
        let mut memo = crate::eval::JobMemo::default();
        let value =
            crate::session::run_to_value_with(&mut session.vm, &module, &session.host, &mut memo)
                .unwrap();
        let root = session.insert(Slot::value(value));
        let selection = Selection {
            attr_paths: vec!["missing".to_owned(), r#""a.b"."""#.to_owned()],
            ..Selection::one("")
        };
        let cases = [
            (
                IXE_QUESTION_SELECT,
                Question::Select {
                    selection: selection.clone(),
                    render: RenderMode::Json,
                },
            ),
            (
                IXE_QUESTION_DERIVATION,
                Question::Derivation {
                    selection: selection.clone(),
                },
            ),
            (
                IXE_QUESTION_DERIVATION_SET,
                Question::DerivationSet {
                    selection: selection.clone(),
                },
            ),
            (
                IXE_QUESTION_DERIVATION_PATH,
                Question::DerivationPath {
                    selection: selection.clone(),
                },
            ),
            (
                IXE_QUESTION_APP,
                Question::Application {
                    selection: selection.clone(),
                },
            ),
            (
                IXE_QUESTION_SOURCE_POSITION,
                Question::SourcePosition {
                    selection: selection.clone(),
                },
            ),
            (
                IXE_QUESTION_FLAKE_SHOW,
                Question::FlakeShow {
                    selection: selection.clone(),
                    flags: 0,
                },
            ),
        ];
        for (kind, question) in cases {
            session.question =
                Some(in_flight_question(&mut session, &selection, kind, question).unwrap());
            let mut handle = 0;
            let mut path = std::ptr::null_mut();
            // SAFETY: session and output slots stay live. The returned path
            // and handle are released exactly once; root retains its owner.
            unsafe {
                assert_eq!(
                    ixe_question_select(&mut session, root, &mut handle, &mut path),
                    IXE_OK
                );
                assert_eq!(CStr::from_ptr(path).to_bytes(), br#""a.b"."""#);
                assert!(matches!(
                    force_handle(&mut session, handle),
                    Ok(Value::Int(42))
                ));
                ixe_string_free(path);
                ixe_handle_free(&mut session, handle);
            }
            assert!(session.get(root).is_some());
        }
        session.question = None;
        let mut handle = 99;
        let mut path = std::ptr::null_mut();
        // SAFETY: all pointers reference live values; rejection allocates nothing.
        unsafe {
            assert_eq!(
                ixe_question_select(&mut session, root, &mut handle, &mut path),
                IXE_ERR_BADCALL
            );
        }
        assert_eq!(handle, 0);
        assert!(path.is_null());
        assert!(question_selection(&Question::FlakeDocument).is_none());
        assert!(
            question_selection(&Question::Whole {
                render: RenderMode::Json
            })
            .is_none()
        );
    }

    #[test]
    fn derivation_set_selection_visits_each_path_without_fallback() {
        let _globals = crate::eval::globals_shared();
        let host = EmbedderHost::new(IxeHostVtable::empty()).unwrap();
        let mut session = IxeSession::new(host);
        let module = session
            .vm
            .compile(
                "{ provided }: { \"a.b\" = provided; other = 2; }",
                ".",
                crate::compile::Origin::String,
            )
            .unwrap()
            .module;
        let mut memo = crate::eval::JobMemo::default();
        let value =
            crate::session::run_to_value_with(&mut session.vm, &module, &session.host, &mut memo)
                .unwrap();
        let root = session.insert(Slot::value(value));
        let mut selection = Selection {
            attr_paths: vec![r#""a.b""#.to_owned(), "other".to_owned()],
            ..Selection::one("")
        };
        selection.auto_args.push(crate::session::AutoArg {
            name: "provided".to_owned(),
            value: crate::session::AutoArgValue::Str("first".to_owned()),
        });
        let question = Question::DerivationSet {
            selection: selection.clone(),
        };
        session.question = Some(
            in_flight_question(
                &mut session,
                &selection,
                IXE_QUESTION_DERIVATION_SET,
                question,
            )
            .unwrap(),
        );
        let mut handle = 0;
        // SAFETY: session, root and output slot stay live. Each successful
        // returned handle is freed once, while root retains its original owner.
        unsafe {
            assert_eq!(
                ixe_question_select_at(&mut session, root, 0, &mut handle),
                IXE_OK
            );
            assert!(
                matches!(force_handle(&mut session, handle), Ok(Value::Str(s)) if s.as_str() == Some("first"))
            );
            ixe_handle_free(&mut session, handle);
            assert_eq!(
                ixe_question_select_at(&mut session, root, 1, &mut handle),
                IXE_OK
            );
            assert!(matches!(
                force_handle(&mut session, handle),
                Ok(Value::Int(2))
            ));
            ixe_handle_free(&mut session, handle);
            assert_eq!(
                ixe_question_select_at(&mut session, root, 2, &mut handle),
                IXE_ERR_BADCALL
            );
            assert_eq!(handle, 0);
            assert_eq!(
                ixe_question_select_at(&mut session, root, 0, std::ptr::null_mut()),
                IXE_ERR_BADCALL
            );
            assert_eq!(
                ixe_question_select_at(std::ptr::null_mut(), root, 0, &mut handle),
                IXE_ERR_BADCALL
            );
        }
        selection.attr_paths = vec!["missing".to_owned(), "other".to_owned()];
        let question = Question::DerivationSet {
            selection: selection.clone(),
        };
        session.question = Some(
            in_flight_question(
                &mut session,
                &selection,
                IXE_QUESTION_DERIVATION_SET,
                question,
            )
            .unwrap(),
        );
        // SAFETY: all pointers refer to live values. The only allocated
        // handle is the successful second selection and is released here.
        unsafe {
            assert_eq!(
                ixe_question_select_at(&mut session, root, 0, &mut handle),
                IXE_ERR_ATTR_PATH_NOT_FOUND
            );
            assert_eq!(handle, 0);
            assert_eq!(
                ixe_question_select_at(&mut session, root, 1, &mut handle),
                IXE_OK
            );
            assert!(matches!(
                force_handle(&mut session, handle),
                Ok(Value::Int(2))
            ));
            ixe_handle_free(&mut session, handle);
        }
        for question in [
            Some(Question::Derivation {
                selection: selection.clone(),
            }),
            None,
        ] {
            session.question = question.map(|question| {
                in_flight_question(&mut session, &selection, IXE_QUESTION_DERIVATION, question)
                    .unwrap()
            });
            // SAFETY: all pointers are live; rejection allocates no handle.
            unsafe {
                assert_eq!(
                    ixe_question_select_at(&mut session, root, 0, &mut handle),
                    IXE_ERR_BADCALL
                );
            }
            assert_eq!(handle, 0);
        }
    }

    #[test]
    fn application_type_uses_the_actual_keyed_selection() {
        let _globals = crate::eval::globals_shared();
        let host = EmbedderHost::new(IxeHostVtable::empty()).unwrap();
        let mut session = IxeSession::new(host);
        let module = session.vm.compile(
            "{ apps.system.name = 1; defaultApp.system = 2; packages.system.name = 3; \"apps.nested\" = 4; }",
            ".", crate::compile::Origin::String,
        ).unwrap().module;
        let mut memo = crate::eval::JobMemo::default();
        let value =
            crate::session::run_to_value_with(&mut session.vm, &module, &session.host, &mut memo)
                .unwrap();
        let root = session.insert(Slot::value(value));
        let cases = [
            (false, r#""apps".system.name"#, "app", 1),
            (false, "defaultApp.system", "app", 2),
            (false, "packages.system.name", "derivation", 3),
            (false, r#""apps.nested""#, "derivation", 4),
            (true, "apps.system.name", "derivation", 1),
        ];
        for (index_lists, path, expected, value) in cases {
            let selection = Selection {
                attr_paths: vec!["absent.candidate".to_owned(), path.to_owned()],
                index_lists,
                ..Selection::one("")
            };
            let question = Question::Application {
                selection: selection.clone(),
            };
            session.question = Some(
                in_flight_question(&mut session, &selection, IXE_QUESTION_APP, question).unwrap(),
            );
            let mut handle = 0;
            let mut selected_path = std::ptr::null_mut();
            let mut expected_type = std::ptr::null_mut();
            // SAFETY: all pointers are live. Each returned handle and string
            // is released once, and the original root remains valid.
            unsafe {
                assert_eq!(
                    ixe_question_select_app(
                        &mut session,
                        root,
                        &mut handle,
                        &mut selected_path,
                        &mut expected_type,
                    ),
                    IXE_OK
                );
                assert_eq!(CStr::from_ptr(selected_path).to_bytes(), path.as_bytes());
                assert_eq!(
                    CStr::from_ptr(expected_type).to_bytes(),
                    expected.as_bytes()
                );
                assert!(
                    matches!(force_handle(&mut session, handle), Ok(Value::Int(found)) if found == value)
                );
                ixe_handle_free(&mut session, handle);
                ixe_string_free(selected_path);
                ixe_string_free(expected_type);
            }
        }
        let selection = Selection::one("apps.system.name");
        let mut handle = 0;
        let mut selected_path = std::ptr::null_mut();
        let mut expected_type = std::ptr::null_mut();
        for question in [
            Some(Question::Derivation {
                selection: selection.clone(),
            }),
            None,
        ] {
            session.question = question.map(|question| {
                in_flight_question(&mut session, &selection, IXE_QUESTION_DERIVATION, question)
                    .unwrap()
            });
            // SAFETY: output slots and session are live; rejection allocates nothing.
            unsafe {
                assert_eq!(
                    ixe_question_select_app(
                        &mut session,
                        root,
                        &mut handle,
                        &mut selected_path,
                        &mut expected_type,
                    ),
                    IXE_ERR_BADCALL
                );
            }
            assert_eq!(handle, 0);
            assert!(selected_path.is_null());
            assert!(expected_type.is_null());
        }
    }

    #[test]
    fn command_auto_arguments_and_flake_index_policy_are_explicit() {
        let mut selection = Selection::one("answer");
        selection.auto_args.push(crate::session::AutoArg {
            name: "input".to_owned(),
            value: crate::session::AutoArgValue::Str("provided".to_owned()),
        });
        assert_eq!(
            evaluate("{ input }: { answer = input; }", selection).unwrap(),
            b"\"provided\""
        );
        let mut selection = Selection::one("items.0");
        selection.index_lists = false;
        assert!(
            evaluate("{ items = [ 42 ]; }", selection.clone())
                .unwrap_err()
                .contains("should be a set")
        );
        assert_eq!(
            evaluate("{ items = { \"0\" = 42; }; }", selection).unwrap(),
            b"42"
        );
    }

    #[test]
    fn command_header_flags_match_rust_values() {
        let header = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("include/ixe-command.h"),
        )
        .unwrap();
        let mut names = std::collections::BTreeSet::new();
        for line in header
            .lines()
            .map(str::trim)
            .filter_map(|line| line.strip_prefix("#define").map(str::trim))
            .filter(|line| line.starts_with("IXE_EVAL_"))
        {
            let mut fields = line.split_whitespace();
            let name = fields.next().unwrap();
            let value = fields.next().unwrap();
            assert!(
                fields.next().is_none(),
                "unexpected flag declaration: {line}"
            );
            assert!(names.insert(name), "duplicate command flag {name}");
            let expected = match name {
                "IXE_EVAL_RAW" => RAW,
                "IXE_EVAL_JSON" => JSON,
                "IXE_EVAL_PRETTY" => PRETTY,
                "IXE_EVAL_FILE" => FILE,
                "IXE_EVAL_EXPR" => EXPR,
                "IXE_EVAL_WRITE_TO" => WRITE_TO,
                name => panic!("unrecognized command flag {name}"),
            };
            let value = value
                .strip_suffix('u')
                .or_else(|| value.strip_suffix('U'))
                .unwrap_or(value);
            assert_eq!(value.parse::<u32>().unwrap(), expected);
        }
        assert_eq!(names.len(), 6);
    }

    #[test]
    fn command_ffi_validates_flags_and_formats_owned_output() {
        let mut render = -1;
        let mut error = std::ptr::null_mut();
        let mut output = std::ptr::null_mut();
        let mut append_newline = -1;
        // SAFETY: input slices and output slots are live for each call. Every
        // returned allocation is freed once before its pointer is reused.
        unsafe {
            assert_eq!(
                ixe_eval_command_validate(JSON | PRETTY, b"".as_ptr(), 0, &mut render, &mut error,),
                IXE_OK
            );
            assert_eq!(render, IXE_RENDER_JSON);
            assert!(error.is_null());
            assert_eq!(
                ixe_eval_command_validate(RAW | JSON, b"".as_ptr(), 0, &mut render, &mut error,),
                IXE_ERR_BADCALL
            );
            assert!(
                CStr::from_ptr(error)
                    .to_str()
                    .unwrap()
                    .contains("mutually exclusive")
            );
            ixe_string_free(error);
            assert_eq!(
                ixe_eval_command_validate(
                    0,
                    b"flake#pkg^out".as_ptr(),
                    13,
                    &mut render,
                    &mut error,
                ),
                IXE_ERR_BADCALL
            );
            assert!(!error.is_null());
            ixe_string_free(error);
            assert_eq!(
                ixe_eval_command_format(
                    JSON | PRETTY,
                    b"42".as_ptr(),
                    2,
                    &mut output,
                    &mut append_newline,
                    &mut error,
                ),
                IXE_OK
            );
            assert!(error.is_null());
            assert_eq!(CStr::from_ptr(output).to_bytes(), b"42");
            assert_eq!(append_newline, 1);
            ixe_string_free(output);
            for flags in [RAW, JSON, 0] {
                assert_eq!(
                    ixe_eval_command_format(
                        flags,
                        b"42".as_ptr(),
                        2,
                        &mut output,
                        &mut append_newline,
                        &mut error,
                    ),
                    IXE_OK
                );
                assert!(
                    output.is_null(),
                    "unchanged output allocated under flags {flags}"
                );
                assert!(error.is_null());
                assert_eq!(append_newline, i32::from(flags != RAW));
            }
            assert_eq!(
                ixe_eval_command_format(
                    RAW,
                    std::ptr::null(),
                    1,
                    &mut output,
                    &mut append_newline,
                    &mut error,
                ),
                IXE_ERR_BADCALL
            );
            assert_eq!(
                ixe_eval_command_execute(std::ptr::null_mut(), 0, &mut output,),
                IXE_ERR_BADCALL
            );
        }
    }

    #[test]
    fn selection_paths_preserve_quoted_dots_and_empty_names() {
        assert_eq!(attr_path(r#"a."b.c"."""#).unwrap(), ["a", "b.c", ""]);
        assert_eq!(
            attr_path(r#""escaped\"quote".0"#).unwrap(),
            ["escaped\"quote", "0"]
        );
        assert!(attr_path("").unwrap().is_empty());
        for invalid in ["a.", ".a", "a..b", "\"unfinished", "\"a\"b"] {
            assert!(attr_path(invalid).is_err(), "accepted {invalid}");
        }
    }

    #[test]
    fn output_policy_preserves_raw_bytes_and_controls_json_layout() {
        let bytes = [0xff, b'\n'];
        let raw = EvalPlan::new(RAW).unwrap().format(&bytes).unwrap();
        assert!(raw.transformed.is_none());
        assert!(!raw.append_newline);
        for flags in [0, JSON] {
            let unchanged = EvalPlan::new(flags).unwrap().format(b"42").unwrap();
            assert!(unchanged.transformed.is_none());
            assert!(unchanged.append_newline);
        }
        let pretty = EvalPlan::new(JSON | PRETTY)
            .unwrap()
            .format(b"{\"a\":1}")
            .unwrap();
        assert_eq!(pretty.transformed.unwrap(), b"{\n  \"a\": 1\n}");
        assert!(pretty.append_newline);
        assert!(
            EvalPlan::new(JSON | PRETTY)
                .unwrap()
                .format(b"broken")
                .is_err()
        );
        for flags in [RAW | JSON, FILE | EXPR, WRITE_TO, 1 << 31] {
            assert!(EvalPlan::new(flags).is_err());
        }
    }
}
