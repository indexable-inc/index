//! Package source locations and their memo codec. Metadata is authoritative;
//! absent metadata never turns into a guessed attribute or expression position.

use super::*;
use crate::session::Question;

#[derive(Debug, PartialEq, Eq)]
struct SourcePosition {
    file: String,
    line: u32,
}

impl SourcePosition {
    fn parse(text: &str) -> Result<Self, String> {
        let invalid = || {
            format!(
                "invalid meta.position '{text}': expected an absolute filename and positive line number"
            )
        };
        let (file, line) = text.rsplit_once(':').ok_or_else(invalid)?;
        if !file.starts_with('/')
            || file == "/"
            || file.contains('\0')
            || line.is_empty()
            || !line.bytes().all(|byte| byte.is_ascii_digit())
        {
            return Err(invalid());
        }
        let line = line.parse::<u32>().map_err(|_| invalid())?;
        if line == 0 {
            return Err(invalid());
        }
        Ok(Self {
            file: file.to_owned(),
            line,
        })
    }

    fn encode(&self) -> String {
        format!("{}:{}", self.file, self.line)
    }
}

fn fail(session: &mut IxeSession, message: impl Into<String>) -> i32 {
    session.fail(&EvalError::eval(ErrKind::Eval, message))
}

fn attribute(session: &mut IxeSession, current: Slot, name: &str) -> Result<Slot, i32> {
    let value = force_slot(session, current)?;
    let Value::Attrs(attrs) = value else {
        return Err(fail(
            session,
            "meta.position requires a package and metadata attribute set",
        ));
    };
    let symbol = session.vm.intern(name);
    attrs.get(&symbol).cloned().ok_or_else(|| {
        fail(
            session,
            format!("cannot find package source location: missing '{name}' in meta.position"),
        )
    })
}

fn execute(session: &mut IxeSession, root: u64) -> Result<SourcePosition, i32> {
    require_question(session, IXE_QUESTION_SOURCE_POSITION, "source position")?;
    let Some(Question::SourcePosition { selection }) = session
        .question
        .as_ref()
        .map(|question| question.question.clone())
    else {
        return Err(session.bad("source position requires its own question in flight"));
    };
    if selection.apply.is_some() || selection.auto_call {
        return Err(session.bad("source position does not accept a result transformation"));
    }
    let root = session
        .get(root)
        .cloned()
        .ok_or_else(|| session.bad("unknown source position root handle"))?;
    let package =
        command::select(session, root, &selection.attr_paths, selection.index_lists)?.slot;
    let meta = attribute(session, package, "meta")?;
    let position = attribute(session, meta, "position")?;
    let value = force_slot(session, position)?;
    let Value::Str(text) = value else {
        return Err(fail(session, "meta.position must be a string"));
    };
    let text = text
        .as_str()
        .ok_or_else(|| fail(session, "meta.position must be UTF-8"))?;
    SourcePosition::parse(text).map_err(|error| fail(session, error))
}

/// # Safety
/// Session is live and exclusively borrowed; out is a writable pointer slot.
/// The answer is released with `ixe_string_free`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_source_position_execute(
    session: *mut IxeSession,
    root: u64,
    out: *mut *mut c_char,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null source position output");
    }
    // SAFETY: checked writable output supplied by caller.
    unsafe { *out = std::ptr::null_mut() };
    match execute(session, root) {
        Ok(position) => out_string(position.encode(), out),
        Err(status) => status,
    }
}

#[repr(C)]
pub struct IxeSourcePosition {
    pub file: *mut c_char,
    pub line: u32,
}

/// Decode a fresh or memoized answer into the same checked fields.
///
/// # Safety
/// Text is readable for len bytes; out and error are writable disjoint slots.
/// On success the file is released with `ixe_string_free`; on failure the error is.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_source_position_decode(
    text: *const u8,
    len: usize,
    out: *mut IxeSourcePosition,
    error: *mut *mut c_char,
) -> i32 {
    if out.is_null() || error.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller supplies writable disjoint output slots.
    unsafe {
        *out = IxeSourcePosition {
            file: std::ptr::null_mut(),
            line: 0,
        };
        *error = std::ptr::null_mut();
    }
    let parsed = (|| {
        if text.is_null() || len > isize::MAX as usize {
            return Err("invalid source position byte span".to_owned());
        }
        // SAFETY: addressable non-null span checked; caller guarantees allocation validity.
        let bytes = unsafe { std::slice::from_raw_parts(text, len) };
        let text =
            std::str::from_utf8(bytes).map_err(|_| "meta.position must be UTF-8".to_owned())?;
        SourcePosition::parse(text)
    })();
    match parsed {
        Ok(position) => {
            let mut file = std::ptr::null_mut();
            let status = out_string(position.file, &mut file);
            if status == IXE_OK {
                // SAFETY: caller supplies writable output.
                unsafe {
                    *out = IxeSourcePosition {
                        file,
                        line: position.line,
                    }
                };
            }
            status
        }
        Err(message) => {
            out_string(message, error);
            IXE_ERR_BADCALL
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Selection;

    #[test]
    fn location_codec_preserves_colons_spaces_and_large_lines() -> Result<(), String> {
        for (input, file, line) in [
            ("/a/package.nix:17", "/a/package.nix", 17),
            ("/a:b/file name.nix:0009", "/a:b/file name.nix", 9),
            ("/a.nix:4294967295", "/a.nix", u32::MAX),
        ] {
            let found = SourcePosition::parse(input)?;
            assert_eq!(found.file, file);
            assert_eq!(found.line, line);
            assert_eq!(SourcePosition::parse(&found.encode())?, found);
        }
        Ok(())
    }

    #[test]
    fn location_codec_rejects_missing_relative_and_invalid_lines() {
        for input in [
            "",
            "/a.nix",
            "a.nix:1",
            ":1",
            "/:1",
            "/a:0",
            "/a:-1",
            "/a:+1",
            "/a: 1",
            "/a:1x",
            "/a:4294967296",
            "/a\0b:1",
        ] {
            assert!(SourcePosition::parse(input).is_err(), "accepted {input:?}");
        }
    }

    fn evaluate(source: &str, selection: Selection, file: &str) -> Result<SourcePosition, String> {
        let _globals = crate::eval::globals_shared();
        let host =
            EmbedderHost::new(IxeHostVtable::empty()).map_err(|error| format!("{error:?}"))?;
        let mut session = IxeSession::new(host);
        let module = session
            .vm
            .compile(source, "/", crate::compile::Origin::File(file))
            .map_err(|error| format!("{error:?}"))?
            .module;
        let mut memo = crate::eval::JobMemo::default();
        let value =
            crate::session::run_to_value_with(&mut session.vm, &module, &session.host, &mut memo)
                .map_err(|error| format!("{error:?}"))?;
        let root = session.insert(Slot::value(value));
        let question = Question::SourcePosition {
            selection: selection.clone(),
        };
        session.question = Some(
            in_flight_question(
                &mut session,
                &selection,
                IXE_QUESTION_SOURCE_POSITION,
                question,
            )
            .map_err(|status| format!("question failed: {status}"))?,
        );
        execute(&mut session, root).map_err(|status| {
            session
                .last_error
                .map_or_else(|| format!("status {status}"), |error| error.message)
        })
    }

    #[test]
    fn real_attribute_origins_keep_source_identity() -> Result<(), String> {
        let source = r#"let
  attrs = { name = "located"; };
  pos = builtins.unsafeGetAttrPos "name" attrs;
in { package.meta.position = "${pos.file}:${toString pos.line}"; unused = throw "not selected"; }"#;
        for file in ["/first/package.nix", "/second/package.nix"] {
            let position = evaluate(source, Selection::one("package"), file)?;
            assert_eq!(position.file, file);
            assert_eq!(position.line, 2);
        }
        Ok(())
    }

    #[test]
    fn package_selection_precedes_metadata_and_never_falls_back() -> Result<(), String> {
        let mut selection = Selection::one("first");
        selection.attr_paths.push("second".to_owned());
        let missing = evaluate(
            r#"{ first = {}; second.meta.position = "/second.nix:3"; }"#,
            selection.clone(),
            "/flake.nix",
        );
        assert!(missing.is_err());
        let failed = evaluate(
            r#"{ first.meta.position = throw "selected metadata failure"; second.meta.position = "/second.nix:3"; }"#,
            selection.clone(),
            "/flake.nix",
        );
        assert!(failed.is_err_and(|error| error.contains("selected metadata failure")));
        let found = evaluate(
            r#"{ second.meta.position = "/second.nix:3"; }"#,
            selection,
            "/flake.nix",
        )?;
        assert_eq!(found.file, "/second.nix");
        assert_eq!(found.line, 3);
        Ok(())
    }

    #[test]
    fn metadata_type_errors_are_not_replaced_with_syntax_positions() {
        for source in [
            "{ package = 42; }",
            "{ package.meta = 42; }",
            "{ package.meta.position = 42; }",
            "{ package.meta.position = null; }",
        ] {
            assert!(evaluate(source, Selection::one("package"), "/real.nix").is_err());
        }
    }

    #[test]
    fn decoder_clears_outputs_on_failure_and_transfers_file_ownership() {
        let valid = b"/a:b.nix:8";
        let mut decoded = IxeSourcePosition {
            file: std::ptr::null_mut(),
            line: 0,
        };
        let mut error = std::ptr::null_mut();
        // SAFETY: all byte spans and output slots are live; returned strings are freed exactly once.
        unsafe {
            assert_eq!(
                ixe_source_position_decode(valid.as_ptr(), valid.len(), &mut decoded, &mut error),
                IXE_OK
            );
            assert!(error.is_null());
            assert_eq!(CStr::from_ptr(decoded.file).to_bytes(), b"/a:b.nix");
            assert_eq!(decoded.line, 8);
            ixe_string_free(decoded.file);
            assert_eq!(
                ixe_source_position_decode(std::ptr::null(), 0, &mut decoded, &mut error),
                IXE_ERR_BADCALL
            );
            assert!(decoded.file.is_null());
            assert_eq!(decoded.line, 0);
            assert!(!error.is_null());
            ixe_string_free(error);
        }
    }
}
