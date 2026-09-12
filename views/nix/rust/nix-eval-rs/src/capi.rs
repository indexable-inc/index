//! The C ABI cppnix calls. Deliberately tiny for M1: evaluate a source
//! string, get back a status and a malloc'd string the caller frees. The
//! full handle-table API from the architecture plan replaces this when
//! values outgrow strings; the status contract below is the part meant to
//! survive.

use crate::eval::EvalError;
use crate::host::Host;
use crate::vm::ErrKind;
use std::borrow::Cow;
use std::ffi::{CStr, CString, c_char, c_void};
use std::slice;

/// Status contract, kept in step with ixe.h and the C++ bridge:
/// 0 value produced; 1 evaluation error (cppnix should throw EvalError);
/// 2 unimplemented construct (cppnix should throw with the marker the
/// harnesses grep, "rust-eval unimplemented"); 3 parse error; 4 bad call;
/// 5 `builtins.throw` (ThrownError); 6 failed assert (AssertionError);
/// 8 import from derivation disabled (IFDError); 9 a top-level auto-call
/// met a formal with neither a default nor an argument
/// (MissingArgumentError); 10 candidate attribute selection failed
/// (AttrPathNotFound).
///
/// 5 and 6 exist because the exception class is not recoverable from the
/// message: cppnix reports a throw as ThrownError under "while calling the
/// 'throw' builtin", and collapsing every failure into EvalError loses the
/// distinction a reader (and the corpus differ) reads the class from.
const IXE_OK: i32 = 0;
const IXE_ERR_EVAL: i32 = 1;
const IXE_ERR_UNIMPLEMENTED: i32 = 2;
const IXE_ERR_PARSE: i32 = 3;
const IXE_ERR_BADCALL: i32 = 4;
const IXE_ERR_THROWN: i32 = 5;
const IXE_ERR_ASSERT: i32 = 6;
const IXE_ERR_IFD: i32 = 8;
/// A top-level auto-call met a formal with neither a default nor an
/// argument: cppnix's `MissingArgumentError`.
const IXE_ERR_MISSING_ARGUMENT: i32 = 9;
/// A complete candidate selection failed; distinct from an optional field miss.
const IXE_ERR_ATTR_PATH_NOT_FOUND: i32 = 10;

/// Every string this ABI hands out is NUL-terminated, so a NUL inside the
/// payload would truncate it. Substituting is not a caller error and must not
/// be reported as one. Unreachable in practice, because the evaluator refuses
/// to build a NUL-bearing Nix string at all -- but that is a property of
/// another module, so it is held by a test here
/// (`the_evaluator_refuses_nul_bearing_strings`) rather than by this comment.
/// If that test ever fails, this substitution stops being dead code and starts
/// deciding what a user sees.
///
/// Both producers go through here rather than each spelling the replacement:
/// `ixe_attrs_names` packs names back to back separated by NUL, so a name
/// carrying one would not merely truncate, it would split into two names.
fn without_nul(s: &str) -> Cow<'_, str> {
    if s.contains('\0') {
        Cow::Owned(s.replace('\0', "\u{2400}"))
    } else {
        Cow::Borrowed(s)
    }
}

fn out_string(s: String, out: *mut *mut c_char) -> i32 {
    let Ok(c) = CString::new(without_nul(&s).into_owned()) else {
        return IXE_ERR_BADCALL;
    };
    // SAFETY: out is non-null, checked by the sole caller before dispatch.
    unsafe { *out = c.into_raw() };
    IXE_OK
}

/// [`out_string`] for a string value's raw bytes: a C string carries any
/// byte but NUL, so a non-UTF-8 Nix string crosses this ABI intact. NUL
/// bytes get `without_nul`'s exact substitution (U+2400), so the two exits
/// repair identically; see `without_nul` for why the substitution is
/// believed dead.
fn out_bytes(s: &[u8], out: *mut *mut c_char) -> i32 {
    let mut replaced = Vec::with_capacity(s.len() + 1);
    for &c in s {
        if c == 0 {
            replaced.extend_from_slice("\u{2400}".as_bytes());
        } else {
            replaced.push(c);
        }
    }
    replaced.push(0);
    // No interior NUL by construction, so this cannot fail; the branch is
    // kept over unwrap because the workspace denies panicking paths.
    let Ok(c) = CString::from_vec_with_nul(replaced) else {
        return IXE_ERR_BADCALL;
    };
    // SAFETY: out is non-null, checked by the sole caller before dispatch.
    unsafe { *out = c.into_raw() };
    IXE_OK
}

/// Where a failure happened, as the C ABI hands it over.
///
/// `line == 0` means there is no position, which is a real answer and not a
/// missing one: an error raised with none of the user's source on the frame
/// stack has nowhere to point, and cppnix prints no `at ...` line for those
/// either. `file` is a malloc'd path the caller frees with
/// `ixe_string_free`, or null when the source was a string with no file
/// behind it (`--expr`), which is a different answer from an empty path.
///
/// Fixed layout and plain integers rather than three out-parameters: the
/// position is one fact and travels with the message, and a caller that can
/// take the message without the position is a caller that can print half an
/// error.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IxePos {
    pub file: *mut c_char,
    pub line: u32,
    pub column: u32,
}

impl IxePos {
    /// The "nowhere" value, which is what a caller sees for every failure
    /// this evaluator cannot place.
    const NONE: IxePos = IxePos {
        file: std::ptr::null_mut(),
        line: 0,
        column: 0,
    };
}

/// Write a position through a caller's slot, tolerating a null slot.
///
/// Always writes, including the "nowhere" value, so a caller reading the slot
/// after an early return sees no position rather than whatever its own
/// variable happened to hold -- the same reason `out_token` is cleared up
/// front.
///
/// # Safety
/// `out`, when non-null, must be writable.
unsafe fn write_pos(out: *mut IxePos, pos: Option<&crate::vm::SrcPos>) {
    if out.is_null() {
        return;
    }
    let value = match pos {
        None => IxePos::NONE,
        Some(pos) => IxePos {
            file: match pos.file.as_deref().and_then(|p| CString::new(p).ok()) {
                Some(path) => path.into_raw(),
                None => std::ptr::null_mut(),
            },
            line: pos.line,
            column: pos.column,
        },
    };
    // SAFETY: caller contract; out is a writable slot.
    unsafe { *out = value };
}

/// Evaluate UTF-8 Nix source. On any return, *out (when non-null) is a
/// malloc'd C string: the rendered value for status 0, the message
/// otherwise. Free with ixe_string_free.
///
/// `file` is the path the source came from, or null when it did not come
/// from one. It is what `__curPos` reports; cppnix answers `null` for a
/// string origin rather than naming a file, so the two cases are different
/// arguments and not one with an empty default.
///
/// `out_token` receives the refusal token when the status is
/// `IXE_ERR_UNIMPLEMENTED`, and null otherwise. Static storage, exactly as
/// `ixe_session_refusal_token` returns: not owned by the caller, not to be
/// freed. May itself be null when the caller does not want the token.
///
/// This parameter is not optional decoration. Without it, this call -- which
/// is the one `nix-instantiate --eval` takes for a whole expression, and the
/// one the result cache serves -- had no way to report which kind of refusal
/// it was, so every refusal on the commonest path in the fleet was counted
/// as `unrecorded`. The handle API's refusals carried their tokens fine, so
/// the census looked healthy from `nix eval` and reported one unnamed bucket
/// from nix-instantiate (ENG-12819).
///
/// `out_pos` receives where the failure happened, or the "nowhere" value
/// when it has no place; see [`IxePos`]. May be null when the caller does not
/// want it.
///
/// # Safety
/// `src` must point to `src_len` readable bytes; `file` to `file_len`
/// readable bytes or be null; `out` must be a valid non-null pointer to write
/// one pointer through; `out_token` and `out_pos`, when non-null, likewise.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_eval_expr(
    host: *const IxeHostVtable,
    src: *const u8,
    src_len: usize,
    base_dir: *const u8,
    base_dir_len: usize,
    file: *const u8,
    file_len: usize,
    out: *mut *mut c_char,
    out_token: *mut *const c_char,
    out_pos: *mut IxePos,
) -> i32 {
    if src.is_null() || out.is_null() {
        return IXE_ERR_BADCALL;
    }
    // Cleared before anything can fail, so a caller reading it after a status
    // this function returns early with sees "no token" rather than whatever
    // its own variable happened to hold.
    if !out_token.is_null() {
        // SAFETY: caller contract; out_token is a writable pointer slot.
        unsafe { *out_token = std::ptr::null() };
    }
    // SAFETY: caller contract; out_pos is null or a writable slot.
    unsafe { write_pos(out_pos, None) };
    let base = if base_dir.is_null() || base_dir_len == 0 {
        ".".to_owned()
    } else {
        // SAFETY: caller contract; base_dir points to base_dir_len bytes.
        let b = unsafe { slice::from_raw_parts(base_dir, base_dir_len) };
        match std::str::from_utf8(b) {
            Ok(s) => s.to_owned(),
            Err(_) => return IXE_ERR_BADCALL,
        }
    };
    // SAFETY: caller contract above.
    let bytes = unsafe { slice::from_raw_parts(src, src_len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        // cppnix accepts arbitrary bytes inside string literals; this
        // pipeline is &str end to end, so non-UTF-8 source is a coverage
        // gap to report, not a caller error. Tokened like any other refusal:
        // raised before anything is compiled, it used to be the one kind that
        // reached the census with no name at all.
        if !out_token.is_null() {
            // SAFETY: caller contract; out_token is a writable slot.
            unsafe { *out_token = token_cstr(crate::refusal::RefusalToken::NonUtf8Source) };
        }
        let rc = out_string("non-UTF-8 source".to_owned(), out);
        return if rc == IXE_OK {
            IXE_ERR_UNIMPLEMENTED
        } else {
            rc
        };
    };
    let file = match unsafe { borrow_str(file, file_len) } {
        Ok(None) => None,
        Ok(Some(path)) => Some(path.to_owned()),
        Err(()) => {
            let rc = out_string("source file path is not UTF-8".to_owned(), out);
            return if rc == IXE_OK { IXE_ERR_BADCALL } else { rc };
        }
    };
    // The host is read here and not once per process, which is the whole
    // difference between this and the `ixe_set_*` surface it replaces: this
    // call answers out of the vtable it was handed and nothing installed
    // earlier or later can change that. A null vtable is the standalone
    // embedding -- `std::fs` reads, no store, nowhere to warn.
    // SAFETY: caller contract above; read once and copied.
    let vtable = (unsafe { host.as_ref() })
        .copied()
        .unwrap_or_else(IxeHostVtable::empty);
    let host = match EmbedderHost::new(vtable) {
        Ok(host) => host,
        Err(why) => {
            let rc = out_string(why.to_owned(), out);
            return if rc == IXE_OK { IXE_ERR_BADCALL } else { rc };
        }
    };
    if let Err(why) =
        require_host_cache_identity(&host, &settings_for(&host), eval_cache_dir().is_some())
    {
        let status = out_string(why.to_owned(), out);
        return if status == IXE_OK {
            IXE_ERR_BADCALL
        } else {
            status
        };
    }
    // The on-disk cache, when configured and openable, goes behind the
    // machine: its imports compile through it, and `evaluate_once` finds the
    // store for the result memo there too.
    let (store, unopened) =
        crate::session::open_cache(eval_cache_dir().as_deref(), cache_max_bytes());
    let mut vm = crate::vm::Vm::with_modules(
        settings_for(&host),
        store.map_or_else(
            crate::modcache::ModuleCache::in_memory,
            crate::modcache::ModuleCache::persistent,
        ),
    );
    if let Some(interrupt) = host.interrupt() {
        vm.set_interrupt(interrupt);
    }
    let mut warnings: Vec<crate::readset::Complaint> = unopened.into_iter().collect();
    let (answer, more) = crate::session::evaluate_once(
        &mut vm,
        &host,
        text,
        &base,
        match &file {
            Some(path) => crate::compile::Origin::File(path),
            None => crate::compile::Origin::String,
        },
        true,
        verify_rate(),
    );
    warnings.extend(more);
    // Damaged store entries are a miss with a reason, and the reason has to
    // reach somebody. stderr rather than the returned string: the string is
    // the expression's value, and a cache complaint is not part of it.
    // The severity is the prefix, not a word inside the sentence: a reader
    // grepping for failures, and anything filtering a journal on priority,
    // both key on the label rather than the prose.
    for complaint in warnings {
        eprintln!("rust-eval: {complaint}");
    }
    let (status, msg) = match answer {
        Ok(v) => (IXE_OK, v.to_string()),
        Err(EvalError::Unimplemented(refusal)) => {
            if !out_token.is_null() {
                // SAFETY: caller contract; out_token is a writable slot, and
                // `token_cstr` hands back storage that lives for the process.
                unsafe { *out_token = token_cstr(refusal.token) };
            }
            (IXE_ERR_UNIMPLEMENTED, refusal.detail)
        }
        Err(EvalError::Eval(kind, msg, pos)) => {
            // SAFETY: caller contract; out_pos is null or a writable slot.
            unsafe { write_pos(out_pos, pos.as_ref()) };
            (status_of_kind(kind), msg)
        }
        Err(EvalError::Parse(msg)) => (IXE_ERR_PARSE, msg),
    };
    let rc = out_string(msg, out);
    if rc == IXE_OK { status } else { rc }
}

/// Set the call-depth ceiling for subsequent `ixe_eval_expr` calls, mirroring
/// cppnix's `max-call-depth`. Without it an unbounded recursion in this VM is
/// not a SIGSEGV the OS stops but a heap allocation loop: `(x: x x) (x: x x)`
/// reached 67 GB before being killed (ENG-12432). That is a property of the
/// machine rather than of this function, and
/// `vm::tests::self_application_fails_instead_of_allocating_forever` is what
/// holds it; if the VM ever moved its frames back onto the host stack, that
/// test would be the thing to change and this setter would become optional.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_max_call_depth(depth: u32) {
    crate::eval::set_max_call_depth(depth);
}

/// Tell the evaluator what `builtins.nixVersion` should report. Taken from
/// the embedder because the alternative is a second copy of the version
/// number in this crate, which would drift from the binary it is linked into.
///
/// # Safety
/// `v` must point to `v_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_set_nix_version(v: *const u8, v_len: usize) -> i32 {
    if v.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller contract; v points to v_len bytes.
    let bytes = unsafe { slice::from_raw_parts(v, v_len) };
    let Ok(s) = std::str::from_utf8(bytes) else {
        return IXE_ERR_BADCALL;
    };
    match crate::eval::set_nix_version(s) {
        Ok(()) => IXE_OK,
        // The embedder is changing a setting that is fixed for the process.
        // Reported rather than ignored: carrying on with the first value
        // computes answers for a configuration nobody asked for, and stores
        // them under a key that says otherwise (ENG-12541).
        Err(conflict) => {
            set_last_setting_conflict(conflict.to_string());
            IXE_ERR_BADCALL
        }
    }
}

/// Tell the evaluator what `builtins.currentSystem` should report. From
/// `settings.thisSystem`, which `--system` moves, so it cannot be worked out
/// from this crate's own build target.
///
/// # Safety
/// `v` must point to `v_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_set_current_system(v: *const u8, v_len: usize) -> i32 {
    if v.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller contract; v points to v_len bytes.
    let bytes = unsafe { slice::from_raw_parts(v, v_len) };
    let Ok(s) = std::str::from_utf8(bytes) else {
        return IXE_ERR_BADCALL;
    };
    match crate::eval::set_current_system(s) {
        Ok(()) => IXE_OK,
        // The embedder is changing a setting that is fixed for the process.
        // Reported rather than ignored: carrying on with the first value
        // computes answers for a configuration nobody asked for, and stores
        // them under a key that says otherwise (ENG-12541).
        Err(conflict) => {
            set_last_setting_conflict(conflict.to_string());
            IXE_ERR_BADCALL
        }
    }
}

/// Supply the immutable build identity of external host callbacks and answer policy.
/// Nonempty UTF-8; identical repeated calls succeed, conflicting calls report through
/// `ixe_take_setting_conflict` and leave the first identity intact.
///
/// # Safety
/// `identity` must point to `identity_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_set_host_build_identity(
    identity: *const u8,
    identity_len: usize,
) -> i32 {
    if identity.is_null() || identity_len == 0 {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller contract guarantees a readable nonempty byte slice.
    let bytes = unsafe { slice::from_raw_parts(identity, identity_len) };
    let Ok(identity) = std::str::from_utf8(bytes) else {
        return IXE_ERR_BADCALL;
    };
    match crate::eval::set_host_build_identity(identity) {
        Ok(()) => IXE_OK,
        Err(conflict) => {
            set_last_setting_conflict(conflict.to_string());
            IXE_ERR_BADCALL
        }
    }
}

/// Tell the evaluator what `~/...` expands to. From cppnix's `getHome()`,
/// which is `$HOME` checked against the `passwd` entry and the directory's
/// owner, so it cannot be worked out from the environment here.
///
/// # Safety
/// `v` must point to `v_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_set_home_dir(v: *const u8, v_len: usize) -> i32 {
    if v.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller contract; v points to v_len bytes.
    let bytes = unsafe { slice::from_raw_parts(v, v_len) };
    let Ok(s) = std::str::from_utf8(bytes) else {
        return IXE_ERR_BADCALL;
    };
    match crate::eval::set_home_dir(s) {
        Ok(()) => IXE_OK,
        // The embedder is changing a setting that is fixed for the process.
        // Reported rather than ignored: carrying on with the first value
        // computes answers for a configuration nobody asked for, and stores
        // them under a key that says otherwise (ENG-12541).
        Err(conflict) => {
            set_last_setting_conflict(conflict.to_string());
            IXE_ERR_BADCALL
        }
    }
}

/// Tell the evaluator which store directory derivations are built under.
///
/// Taken from the embedder's `state.store->storeDir` because the store is the
/// embedder's. Without it `builtins.derivationStrict` refuses rather than
/// assuming `/nix/store`: the directory is hashed into every path it computes,
/// so assuming wrong yields a wrong path that nothing downstream can tell from
/// a right one.
///
/// # Safety
/// `dir` must point to `dir_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_set_store_dir(dir: *const u8, dir_len: usize) -> i32 {
    if dir.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: caller contract; dir points to dir_len bytes.
    let bytes = unsafe { slice::from_raw_parts(dir, dir_len) };
    let Ok(s) = std::str::from_utf8(bytes) else {
        return IXE_ERR_BADCALL;
    };
    match crate::eval::set_store_dir(s) {
        Ok(()) => IXE_OK,
        // The embedder is changing a setting that is fixed for the process.
        // Reported rather than ignored: carrying on with the first value
        // computes answers for a configuration nobody asked for, and stores
        // them under a key that says otherwise (ENG-12541).
        Err(conflict) => {
            set_last_setting_conflict(conflict.to_string());
            IXE_ERR_BADCALL
        }
    }
}

/// Tell the evaluator that `pure-eval` is on. Non-zero for on.
///
/// Which host questions that forbids is [`crate::purity::verdict`]'s, one row
/// per question with the cppnix line each was read off. The short version:
/// pure eval forbids impure *inputs*, not the question channel, so a fetch
/// pinned with `sha256` and a tree with a locked input are both served, and
/// an unpinned fetch fails with cppnix's own message.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_pure_eval(on: i32) {
    crate::eval::set_pure_eval(on != 0);
}

/// Tell the evaluator that `restrict-eval` is on. Non-zero for on.
///
/// A separate entry point from [`ixe_set_pure_eval`] because the two settings
/// forbid different questions. One flag standing for `restrictEval ||
/// pureEval` is what this replaces, and it refused every host question under
/// either -- which made no flake evaluable on this backend, since a flake is
/// evaluated under pure eval by default.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_restrict_eval(on: i32) {
    crate::eval::set_restrict_eval(on != 0);
}

/// Tell the evaluator whether `builtins.traceVerbose` traces, mirroring
/// cppnix's `trace-verbose`.
///
/// A setting and not a hook: cppnix chooses between two different primops
/// with it, and the one it picks when the setting is off does not force the
/// message at all. So the flag decides values, not output, and
/// `eval::Settings` carries it into the memo key.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_trace_verbose(on: i32) {
    crate::eval::set_trace_verbose(on != 0);
}

/// Tell the evaluator whether `builtins.warn` aborts after warning, mirroring
/// cppnix's `abort-on-warn`.
///
/// Also a value-deciding setting: with it on an expression that warns has no
/// value at all. Left unforwarded, the Rust backend would answer where cppnix
/// dies.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_abort_on_warn(on: i32) {
    crate::eval::set_abort_on_warn(on != 0);
}

/// Tell the evaluator whether cppnix's `ca-derivations` experimental feature
/// is enabled.
///
/// Value-deciding, like the two settings above: with it off,
/// `__contentAddressed = true` is cppnix's feature-is-disabled error, and
/// with it on the same derivation is a floating-CA `.drv`. `eval::Settings`
/// carries it into the memo key.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_ca_derivations(on: i32) {
    crate::eval::set_ca_derivations(on != 0);
}

/// Tell the evaluator whether `allow-import-from-derivation` is on.
///
/// In the memo key: a witness recorded with it on holds realisations the
/// verifier serves by validity without reaching the host's
/// `realiseContextCheck`, which refuses them with it off.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_allow_import_from_derivation(on: i32) {
    crate::eval::set_allow_import_from_derivation(on != 0);
}

/// Tell the evaluator the `allowed-uris` list, one entry per
/// newline-terminated line.
///
/// In the memo key: a fetch served by validity never reaches `checkURI`, so a
/// witness recorded under a wider list must not hit under a narrower one.
///
/// # Safety
/// `uris` must point to `uris_len` readable bytes of UTF-8.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_set_allowed_uris(uris: *const u8, uris_len: usize) {
    // SAFETY: the caller promises `uris_len` readable bytes at `uris`.
    let bytes = unsafe { std::slice::from_raw_parts(uris, uris_len) };
    crate::eval::set_allowed_uris(String::from_utf8_lossy(bytes).into_owned());
}

/// Tell the evaluator whether the evaluation runs under `--repair`. Nothing
/// is served from the memo under repair: repair means redo.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_repair(on: i32) {
    crate::eval::set_repair(on != 0);
}

/// Tell the evaluator whether cppnix's `blake3-hashes` experimental feature
/// is enabled.
///
/// Value-deciding for every hash parser: with it off cppnix raises
/// `MissingExperimentalFeature`, and with it on the same expression computes
/// a Blake3 hash (`libutil/hash.cc:25-29,468-473`). `eval::Settings` carries
/// it into the memo key.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_blake3_hashes(on: i32) {
    crate::eval::set_blake3_hashes(on != 0);
}

/// Tell the evaluator cppnix's `lint-url-literals` level: 0 ignore, 1 warn,
/// 2 fatal.
///
/// Value-deciding at `fatal` only: cppnix's parser then rejects a URL
/// literal, so the compiler here must reject the same text (`compile.rs`,
/// mirroring `parser.y:372-380`). At `warn` cppnix prints a diagnostic this
/// backend does not, which is tier-2 warning text; the level still crosses
/// so the backend knows the setting rather than assuming it.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_lint_url_literals(level: i32) {
    crate::eval::set_lint_url_literals(crate::eval::Diagnose::from_c(level));
}

/// Tell the evaluator cppnix's `lint-short-path-literals` level: 0 ignore,
/// 1 warn, 2 fatal. See [`ixe_set_lint_url_literals`].
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_lint_short_path_literals(level: i32) {
    crate::eval::set_lint_short_path_literals(crate::eval::Diagnose::from_c(level));
}

/// Tell the evaluator cppnix's `lint-absolute-path-literals` level: 0
/// ignore, 1 warn, 2 fatal. Covers `~/x` literals too, as cppnix's `HPATH`
/// rule does (`parser.y:461-466`). See [`ixe_set_lint_url_literals`].
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_lint_absolute_path_literals(level: i32) {
    crate::eval::set_lint_absolute_path_literals(crate::eval::Diagnose::from_c(level));
}

/// Tell the evaluator whether cppnix's `pipe-operators` experimental
/// feature is enabled.
///
/// Value-deciding at parse time: with it off `a |> f` is cppnix's
/// feature-is-disabled error, and with it on the same text is `f a`
/// (`parser.y:287-295`). `eval::Settings` carries it into the memo key.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_pipe_operators(on: i32) {
    crate::eval::set_pipe_operators(on != 0);
}

/// Tell the evaluator whether cppnix's `parse-toml-timestamps` experimental
/// feature is enabled.
///
/// Value-deciding: with it off a TOML date or time is `error: while parsing
/// TOML: Dates and times are not supported`, and with it on the same
/// document evaluates to `{ _type = "timestamp"; value = "..."; }` sets
/// (`primops.cc`, `prim_fromTOML`). `eval::Settings` carries it into the
/// memo key.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_parse_toml_timestamps(on: i32) {
    crate::eval::set_parse_toml_timestamps(on != 0);
}

/// Set the language capabilities. Unknown bits are rejected without changing settings.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_builtin_features(flags: u32) -> i32 {
    let Some(features) = crate::eval::BuiltinFeatures::from_bits(flags) else {
        return IXE_ERR_BADCALL;
    };
    crate::eval::set_builtin_features(features);
    IXE_OK
}

/// Export the owned language documentation without creating a store or evaluation session.
/// Both returned strings use `ixe_string_free`.
///
/// # Safety
/// `out` and `error` must be distinct writable pointer slots.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_language_docs(out: *mut *mut c_char, error: *mut *mut c_char) -> i32 {
    if out.is_null() || error.is_null() || out == error {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: writable distinct output slots required by the caller contract.
    unsafe {
        *out = std::ptr::null_mut();
        *error = std::ptr::null_mut();
    }
    match crate::builtin_catalogue::language_docs() {
        Ok(docs) => out_string(docs, out),
        Err(why) => {
            let _ = out_string(why.to_string(), error);
            IXE_ERR_BADCALL
        }
    }
}

/// The reason the most recent settings call was refused.
///
/// A separate slot rather than the per-session error channel, because these
/// setters are process-global and are called before any session exists.
static LAST_SETTING_CONFLICT: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

fn set_last_setting_conflict(message: String) {
    let mut slot = LAST_SETTING_CONFLICT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = Some(message);
}

/// Take the reason the most recent settings call was refused, or null if
/// there is none. Ownership transfers; free with `ixe_string_free`.
///
/// # Safety
/// Nothing to uphold; the returned pointer is this crate's to hand over.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_take_setting_conflict() -> *mut c_char {
    let taken = LAST_SETTING_CONFLICT
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    match taken.and_then(|text| CString::new(text).ok()) {
        Some(c) => c.into_raw(),
        None => std::ptr::null_mut(),
    }
}

/// How often the evaluation cache checks itself, as one in this many. 0 is
/// off, which is the default.
static VERIFY_RATE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

fn verify_rate() -> u32 {
    VERIFY_RATE.load(std::sync::atomic::Ordering::Relaxed)
}

/// Have the evaluation cache check one hit in `rate` by evaluating anyway, and
/// one record in `rate` by looking it up again. 0 turns it off; 1 checks
/// everything.
///
/// A cache cannot be checked by reading its answers, because its answers are
/// by construction whatever it was told to say. ENG-12541 -- a memo key blind
/// to the store directory, so a cache shared between two stores served paths
/// for the wrong one -- was found by reading the code, and would have been
/// found in production by a one-in-twenty check. This is that check.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_cache_verify_rate(rate: u32) {
    VERIFY_RATE.store(rate, std::sync::atomic::Ordering::Relaxed);
}

static EVAL_CACHE_MAX_BYTES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn cache_max_bytes() -> u64 {
    EVAL_CACHE_MAX_BYTES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Cap the on-disk evaluation cache at `bytes`. After each result is
/// published the store is swept, least recently used entries first, until it
/// fits (`Store::sweep_to_cap`). 0 turns the sweep off and the cache grows
/// without bound. Eviction can only cause a later miss, never a different
/// answer, so this is not part of the memo key. Meaningless without
/// [`ixe_set_eval_cache_dir`].
#[unsafe(no_mangle)]
pub extern "C" fn ixe_set_eval_cache_max_bytes(bytes: u64) {
    EVAL_CACHE_MAX_BYTES.store(bytes, std::sync::atomic::Ordering::Relaxed);
}

/// Where the on-disk evaluation cache lives, or `None` for no cache.
///
/// Process-global, like the call-depth ceiling and the version string, because
/// the embedder configures the evaluator once before using it and the C ABI
/// has no handle to hang it off yet.
static EVAL_CACHE_DIR: std::sync::Mutex<Option<std::path::PathBuf>> = std::sync::Mutex::new(None);

fn eval_cache_dir() -> Option<std::path::PathBuf> {
    EVAL_CACHE_DIR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Point the evaluator at an on-disk cache of compiled modules and evaluation
/// results. Passing an empty path turns it off.
///
/// Opt-in: without this the evaluator keeps its caches in memory and a new
/// process starts cold, which is the behaviour every release so far has had.
/// With it, a second run finds the first run's work, and an edit invalidates
/// by content rather than by anything having to notice it.
///
/// # Safety
/// `path` must point to `path_len` readable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_set_eval_cache_dir(path: *const u8, path_len: usize) {
    let value = if path.is_null() || path_len == 0 {
        None
    } else {
        // SAFETY: caller contract above.
        let bytes = unsafe { slice::from_raw_parts(path, path_len) };
        match std::str::from_utf8(bytes) {
            Ok(text) => Some(std::path::PathBuf::from(text)),
            // A path this crate cannot read is not a reason to abort an
            // evaluation; it is a reason not to cache.
            Err(_) => None,
        }
    };
    let mut slot = EVAL_CACHE_DIR
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    *slot = value;
}

/// How the embedder answers "what store path does this path copy to".
///
/// Pointer and length rather than an owned string: the callee keeps the
/// buffer and the evaluator copies it before returning, so neither side frees
/// the other's allocation.
pub type CopyToStoreFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    root: *const u8,
    root_len: usize,
    path: *const u8,
    path_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// `builtins.storePath`: the same rooted input and buffer contract as a copy,
/// under its own name, as `ixe_store_path_fn` is in `ixe.h`.
pub type StorePathFn = CopyToStoreFn;

/// How the embedder stores a text blob, for `builtins.toFile`.
///
/// Same buffer discipline as [`CopyToStoreFn`]. `references` is the NUL-
/// terminated encoding `encode_entries` uses for its fields: each store path
/// followed by a NUL, which is unambiguous because a store path cannot contain
/// one.
///
/// The embedder owns the read-only decision: cppnix computes the path without
/// writing under `settings.readOnlyMode` and writes otherwise, and this
/// evaluator cannot see that setting (ENG-12479 put `ensurePath`'s equivalent
/// branch on the same side). ENG-12607.
pub type StoreTextFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    name: *const u8,
    name_len: usize,
    contents: *const u8,
    contents_len: usize,
    references: *const u8,
    references_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// How the embedder writes `.drv`s, for `builtins.derivationStrict`: a batch
/// of them, not one.
///
/// The request is prepared by [`crate::store_batch`]: an accounting count,
/// a set of lazy input sources, then one AddMultipleToStore stream. Rust
/// validates each derivation, owns its references and content address, and
/// encodes its metadata and NAR. The embedder materialises the lazy sources
/// and forwards the store stream. Zero means every derivation is a store
/// object with its temporary root; anything else is the failure text, and
/// nothing is promised about which entries were written.
///
/// A batch because the evaluator answers the question itself. The path is a
/// function of the bytes, so the store is not consulted per derivation but
/// told, once, before anything could observe the object: the derivations an
/// evaluation writes are queued on the host and handed over here at the
/// first build, validity check, read or `toFile` reference that names one,
/// and when the scheduler settles the finished evaluation
/// ([`EmbedderHost::flush_derivations`], `Host::settle`). cppnix asks its
/// store one round
/// trip per `derivationStrict` (a NixOS closure is 23k of them, at 600 us
/// each over the daemon socket); this is one round trip per batch.
///
/// Leaving this NULL is not an error and is cppnix's `readOnlyMode`: a
/// derivation still evaluates and reports the path it would have been
/// written to, only the file is missing. `nix build` needs the hook;
/// `nix eval` does not. It is a hook of its own rather than `store_text`
/// with a loop because leaving `store_text` NULL means `toFile` refuses.
pub type WriteDrvsFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    batch: *const u8,
    batch_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// How the embedder performs a filtered copy into the store, for
/// `builtins.path`.
///
/// One opaque buffer rather than a dozen arguments, because the request has a
/// variable-length tail. The encoding is NUL-terminated fields, which is
/// unambiguous for the reason `references` is: a filesystem path cannot
/// contain a NUL.
///
/// 1. the accessor root (empty for ambient, otherwise its mount point);
/// 2. the path relative to that accessor;
/// 3. the store object's name;
/// 4. `"nar"` or `"flat"`;
/// 5. the expected SHA-256 as SRI, or empty for "no `sha256` attribute" -- a
///    present one is rendered from a parsed hash and is never empty;
/// 6. `"inherit-references"` or `"own-references"`;
/// 7. `"unfiltered"` (copy everything, cppnix's `defaultPathFilter`) or
///    `"filtered"`;
/// 8. then, when filtered, an accessor-relative path and a type (`regular`,
///    `directory`, `symlink`, `unknown`) per accepted entry.
///
/// Same buffer discipline as [`CopyToStoreFn`], and the same division of
/// labour: the embedder owns the store and the read-only decision. What it
/// must not do is re-decide the filter -- see
/// [`crate::task::NeedPath::StoreFiltered`].
pub type StoreFilteredFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    request: *const u8,
    request_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// The wire form of a filtered copy. Public so the probe encodes and the
/// bridge decodes one description rather than two.
#[must_use]
pub fn encode_filtered_copy(request: &crate::task::FilteredCopy) -> Vec<u8> {
    let mut out = Vec::new();
    let mut field = |s: &str| {
        out.extend_from_slice(s.as_bytes());
        out.push(0);
    };
    field(request.root.root.wire_name());
    field(request.root.accessor_path());
    field(&request.name);
    field(request.method.as_str());
    field(request.expected_sha256.as_deref().unwrap_or(""));
    // A word rather than an empty-or-not field, because the two states are
    // both meaningful and a reader that mistook one for the other would land
    // the copy on a well-formed wrong store path. See
    // [`crate::task::FilteredCopy::inherit_references`].
    field(if request.inherit_references {
        "inherit-references"
    } else {
        "own-references"
    });
    match &request.accepted {
        None => field("unfiltered"),
        Some(list) => {
            field("filtered");
            for e in list {
                field(request.root.with_path(e.path.clone()).accessor_path());
                field(e.file_type.as_str());
            }
        }
    }
    out
}

/// How the embedder fetches a URL into the store, for `builtins.fetchurl`
/// and `builtins.fetchTarball`. The request is NUL-terminated fields, for
/// the reason [`StoreFilteredFn`]'s is -- neither a URL nor a store path
/// name can contain a NUL:
///
/// 1. the URL, already through `resolvePseudoUrl`;
/// 2. the store object's name, already defaulted and already validated;
/// 3. `"file"` (ingest flat, cppnix's `fetchurl`) or `"tarball"` (unpack and
///    ingest as a NAR, cppnix's `fetchTarball`);
/// 4. the expected SHA-256 as SRI, or empty for "no `sha256` attribute".
///
/// Same buffer discipline as [`CopyToStoreFn`]. The whole of the fetch is
/// the embedder's -- see [`crate::task::NeedPath::Fetch`] for what the
/// answer has to be.
pub type FetchFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    request: *const u8,
    request_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// The wire form of a fetch. Public so the probe encodes and the bridge
/// decodes one description rather than two.
#[must_use]
pub fn encode_fetch(request: &crate::task::FetchRequest) -> Vec<u8> {
    let mut out = Vec::new();
    let mut field = |s: &str| {
        out.extend_from_slice(s.as_bytes());
        out.push(0);
    };
    field(&request.url);
    field(&request.name);
    field(request.kind.as_str());
    field(request.expected_sha256.as_deref().unwrap_or(""));
    out
}

/// How the embedder fetches a tree, for `builtins.fetchTree` and
/// `builtins.fetchGit`. NUL-terminated fields, for the reason
/// [`StoreFilteredFn`]'s are:
///
/// 1. the fetcher, `"fetchTree"` or `"fetchGit"`;
/// 2. then a name, a one-letter type tag (`s`, `b`, `i`) and a value per
///    input attribute, in name order.
///
/// The answer is not a store path but the JSON of the attribute set cppnix's
/// `emitTreeAttrs` builds. Same buffer discipline as [`CopyToStoreFn`]. See
/// [`crate::task::NeedPath::FetchTree`] for the division of labour.
pub type FetchTreeFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    request: *const u8,
    request_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// The wire form of a tree fetch. Public so the probe encodes and the bridge
/// decodes one description rather than two.
#[must_use]
pub fn encode_fetch_tree(request: &crate::task::FetchTreeRequest) -> Vec<u8> {
    let mut out = Vec::new();
    let mut field = |s: &str| {
        out.extend_from_slice(s.as_bytes());
        out.push(0);
    };
    field(request.fetcher.as_str());
    for (name, value) in &request.attrs {
        field(name);
        field(value.tag());
        field(&value.text());
    }
    out
}

/// How the embedder locks a flake. See `ixe.h` for the wire form.
///
/// Same three-outcome contract and same buffer discipline as
/// [`FetchTreeFn`], and for the same reason: locking under the read-set
/// tracker cannot be served, so "this embedder will not serve this" needs to
/// be a status of its own rather than a failure.
pub type LockFlakeFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    flake_ref: *const u8,
    flake_ref_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// The success answer: one JSON object with three string fields.
///
/// `overrides` is a *string* holding a JSON document rather than a nested
/// object, and that is deliberate. The bridge already serialised that document
/// attribute by attribute for a reason (`printValueAsJSON` collapses any
/// attrset carrying an `outPath`), and a read set digests it byte for byte;
/// re-parsing and re-serialising it here would put key ordering and number
/// formatting between the bytes the embedder produced and the bytes the digest
/// covers, which is a way for a memo key to move without the lock moving.
fn decode_flake_call(answer: &str) -> Result<crate::host::FlakeCall, crate::host::StoreError> {
    let doc: serde_json::Value = serde_json::from_str(answer).map_err(|e| {
        crate::host::StoreError::Failed(format!("the embedder's flake answer is not JSON: {e}"))
    })?;
    let field = |name: &str| -> Result<String, crate::host::StoreError> {
        doc.get(name)
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                crate::host::StoreError::Failed(format!(
                    "the embedder's flake answer has no string '{name}'"
                ))
            })
    };
    Ok(crate::host::FlakeCall {
        source: field("source")?,
        lock_file: field("lockFile")?,
        overrides: field("overrides")?,
    })
}

/// How the embedder parses a flake reference, for `builtins.parseFlakeRef`.
/// The request is the reference string; the answer is one JSON object of
/// string, integer and Boolean fields -- `fetchers::attrsToJSON` over
/// `FlakeRef::toAttrs`, the three shapes `fetchers::Attr` holds.
///
/// Same buffer discipline and three-outcome contract as [`LockFlakeFn`]. The
/// flakes feature gate lives behind this hook, where cppnix checks it: the
/// primop is registered unconditionally and the call raises the
/// feature-is-disabled error, so the hook does the same.
pub type ParseFlakeRefFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    flake_ref: *const u8,
    flake_ref_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// How the embedder prints a flake reference, for `builtins.flakeRefToString`.
/// The request is a name, a one-letter type tag (`s`, `b`, `i`) and a value
/// per attribute, NUL-terminated in name order -- [`FetchTreeFn`]'s encoding
/// without its leading fetcher field. The answer is the reference string.
/// Same contract and gate as [`ParseFlakeRefFn`].
pub type FlakeRefToStringFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    request: *const u8,
    request_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// The wire form of a flake ref's attribute set. Public so the probe encodes
/// and the bridge decodes one description rather than two.
#[must_use]
pub fn encode_flake_ref_attrs(
    attrs: &std::collections::BTreeMap<String, crate::task::TreeAttr>,
) -> Vec<u8> {
    let mut out = Vec::new();
    let mut field = |s: &str| {
        out.extend_from_slice(s.as_bytes());
        out.push(0);
    };
    for (name, value) in attrs {
        field(name);
        field(value.tag());
        field(&value.text());
    }
    out
}

/// How the embedder resolves a search path. See `ixe.h` for the encoding.
pub type FindFileFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    entries: *const u8,
    entries_len: usize,
    name: *const u8,
    name_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// Pairs, NUL-terminated field by field. Chosen over a length-prefixed
/// encoding because neither half can contain a NUL -- both are a path or a
/// prefix of one -- and because the C side can build it with a plain append.
fn encode_entries(entries: &[crate::task::SearchPathEntry]) -> Vec<u8> {
    let mut buf = Vec::new();
    for e in entries {
        buf.extend_from_slice(e.prefix.as_bytes());
        buf.push(0);
        buf.extend_from_slice(e.path.as_bytes());
        buf.push(0);
    }
    buf
}

/// The inverse. A trailing partial field is a malformed answer and is
/// reported as one rather than silently dropped, since a dropped entry is a
/// search path that quietly stops finding something.
fn decode_entries(buf: &[u8]) -> Result<Vec<crate::task::SearchPathEntry>, String> {
    let mut out = Vec::new();
    let mut fields = buf.split(|b| *b == 0);
    while let (Some(prefix), Some(path)) = (fields.next(), fields.next()) {
        // The split after a trailing NUL yields one empty tail field, which
        // is the end of a well-formed buffer rather than another entry.
        if prefix.is_empty() && path.is_empty() && fields.next().is_none() {
            break;
        }
        let decode = |b: &[u8]| {
            std::str::from_utf8(b)
                .map(str::to_owned)
                .map_err(|_| "search path entry is not valid UTF-8".to_owned())
        };
        out.push(crate::task::SearchPathEntry {
            prefix: decode(prefix)?,
            path: decode(path)?,
        });
    }
    Ok(out)
}

/// How the embedder reports the default search path.
pub type NixPathFn =
    unsafe extern "C" fn(ctx: *mut c_void, out: *mut *const u8, out_len: *mut usize) -> i32;

/// The callee's buffer as bytes. Copied immediately, because the contract in
/// `ixe.h` is that it need only outlive the call.
fn read_answer_bytes(out: *const u8, out_len: usize) -> Vec<u8> {
    if out.is_null() {
        return Vec::new();
    }
    // SAFETY: the ABI's contract -- on a non-null `out` the callee has
    // written `out_len` readable bytes.
    unsafe { std::slice::from_raw_parts(out, out_len) }.to_vec()
}

/// Where a `builtins.trace` line goes. Same contract as `ixe_warn_fn`.
pub type TraceFn = unsafe extern "C" fn(ctx: *mut c_void, message: *const u8, message_len: usize);

/// How the embedder reports that the running evaluation should stop.
///
/// Returns non-zero when interrupted. No buffer contract because there is
/// nothing to say: the evaluator supplies cppnix's own message.
pub type InterruptedFn = unsafe extern "C" fn(ctx: *mut c_void) -> i32;

/// How the embedder makes a store path present, for `builtins.appendContext`.
///
/// Same buffer contract as [`CopyToStoreFn`]; the only difference is that
/// success carries nothing, so `out` is written only on failure.
pub type EnsurePathFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    path: *const u8,
    path_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// How the embedder says which of a list of store paths it holds now:
/// `Store::queryValidPaths`, one round trip for the whole list. Paths in and
/// out are newline-separated; see `ixe_valid_paths_fn` in `ixe.h`.
pub type ValidPathsFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    paths: *const u8,
    paths_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// How the embedder says which of a list of store objects are sealed
/// (content-addressed) and every symlink under each; see
/// `ixe_sealed_paths_fn` in `ixe.h`. Objects in are newline-separated
/// `<hash>-<name>` lines; each line out is a sealed object followed by
/// (object-relative path, target) pairs, all tab-separated.
pub type SealedPathsFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    paths: *const u8,
    paths_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// How the embedder allows reads through store paths the verifier served by
/// validity (`ixe_allow_paths_fn`): `allowPath` per NUL-terminated path, the
/// form the live copy, fetch and tree hooks use. The closure form for
/// realised outputs is the realise protocol's own third phase.
pub type AllowPathsFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    paths: *const u8,
    paths_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// How the embedder realises a string context, for import from derivation.
///
/// This is cppnix's `EvalState::realiseContext` (`primops.cc:72`) behind one
/// call. The request is the context, NUL-terminated, one field per element,
/// each rendered the way cppnix's `NixStringContextElem::to_string` renders
/// it -- `!<output>!<drv>`, `=<drv>`, or a bare store path -- so the embedder
/// parses them back with `NixStringContextElem::parse` and neither side
/// invents a second spelling. A NUL cannot occur in a store path, so the
/// framing is unambiguous.
///
/// Success writes the rewrite map `realiseContext` returns: NUL-terminated
/// fields, an even number of them, alternating `from` and `to`. Empty for
/// every input-addressed derivation, which is the common case; non-empty
/// only under `ca-derivations`, where a downstream placeholder has to become
/// the path the build actually produced. Failure writes the message.
///
/// Everything policy-shaped lives on the far side of this call:
/// `allow-import-from-derivation`, `trace-import-from-derivation`, the
/// `isValidPath` check on each element, `buildPaths`, and the closure copy
/// for a store the evaluator is not talking to directly. The evaluator asks
/// only "make these valid"; it does not know whether it is allowed to.
///
/// Same buffer discipline as [`CopyToStoreFn`]. On failure, `error_class`
/// distinguishes a disabled import from derivation from an ordinary store
/// error so the embedder can reconstruct cppnix's exception class.
///
/// The callback writes a plain `i32`, never this enum: C may store any
/// integer in an enum-typed cell, and a Rust enum holding a discriminant it
/// does not define is undefined behaviour before anything reads it. The
/// values are decoded through [`IxeRealiseErrorClass::decode`], which
/// refuses what the header does not name.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IxeRealiseErrorClass {
    Other = 0,
    ImportFromDerivation = 1,
}

impl IxeRealiseErrorClass {
    /// The class an embedder answered, or the integer it answered that the
    /// header does not define. The wire value is the discriminant.
    pub fn decode(raw: i32) -> Result<Self, i32> {
        match raw {
            x if x == Self::Other as i32 => Ok(Self::Other),
            x if x == Self::ImportFromDerivation as i32 => Ok(Self::ImportFromDerivation),
            other => Err(other),
        }
    }
}

/// What the check phase of a realise answered, as `ixe.h` names it. Decoded
/// from the hook's status the way [`IxeRealiseErrorClass`] is, refusing what
/// the header does not name.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IxeRealiseCheck {
    /// The context needs a build: phase 2 follows.
    Build = 0,
    /// The checks refused, with a message and an [`IxeRealiseErrorClass`].
    Failed = 1,
    /// Every element is already valid: the answer is the empty rewrite map.
    Nothing = 2,
}

impl IxeRealiseCheck {
    /// The status an embedder answered, or the integer it answered that the
    /// header does not define. The wire value is the discriminant.
    pub fn decode(raw: i32) -> Result<Self, i32> {
        match raw {
            x if x == Self::Build as i32 => Ok(Self::Build),
            x if x == Self::Failed as i32 => Ok(Self::Failed),
            x if x == Self::Nothing as i32 => Ok(Self::Nothing),
            other => Err(other),
        }
    }
}

/// The three phases of a realise (ENG-13150). Both routes queue the build on
/// the session's dispatcher; the blocking route waits for its answer.
/// The full protocol -- who runs on which thread, the answer
/// encoding, and why supplying `realise_build` is the embedder's consent to
/// a worker-thread call -- is written once, beside `ixe_realise_check_fn` in
/// `ixe.h`. One signature shape per phase rather than one alias three ways,
/// so a vtable field cannot be filled with a phase it does not name.
///
/// Phase 1 answers an [`IxeRealiseCheck`] status; on `Failed` it also writes
/// the error class, as a plain int the evaluator decodes and refuses when
/// the header does not name it, rather than an enum cell C may have filled
/// with anything.
pub type RealiseCheckFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    request: *const u8,
    request_len: usize,
    error_class: *mut i32,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// Phase 2: the build, on a worker thread. See [`RealiseCheckFn`].
pub type RealiseBuildFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    requests: *const IxeRealiseRequest,
    count: usize,
    results: *mut IxeRealiseResult,
) -> i32;

/// One checked context borrowed for a batch build call.
#[repr(C)]
pub struct IxeRealiseRequest {
    pub data: *const u8,
    pub len: usize,
}

/// One batch result. The host owns its bytes until the next build call.
#[repr(C)]
pub struct IxeRealiseResult {
    pub status: i32,
    pub data: *const u8,
    pub len: usize,
}

/// Phase 3: allow-list registration at delivery. See [`RealiseCheckFn`].
pub type RealiseAllowFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    outputs: *const u8,
    outputs_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// The realise request bytes: the context, one NUL-terminated field per
/// element. One spelling for the blocking hook and all three phases of the
/// threaded one, because the check phase and the build phase parsing two
/// different renderings of one context is exactly the drift the split must
/// not introduce.
fn encode_realise_request<'a>(
    context: impl IntoIterator<Item = &'a crate::value2::ContextElem>,
) -> Vec<u8> {
    let mut encoded = Vec::new();
    for e in context {
        // `display_base_name` and NOT `display`, and the difference is the
        // whole call. The embedder reads these back with
        // `NixStringContextElem::parse`, whose leaves are `StorePath{s}` --
        // and a `StorePath` is constructed from `<hash>-<name>` with no store
        // directory, so a full `/nix/store/...` path throws
        // `BadStorePath` there. `display_base_name` is this crate's
        // transcription of cppnix's `to_string`, which is `parse`'s inverse;
        // `display` is the error rendering, which is not.
        //
        // The witness codec in `readset` goes the other way and keys on
        // `display`, deliberately: a read set has to stay unambiguous across
        // two evaluators configured with different store directories, which
        // a base name is not.
        encoded.extend_from_slice(e.display_base_name().as_bytes());
        encoded.push(0);
    }
    encoded
}

/// Decode a rewrite map: NUL-terminated fields, an even number of them,
/// alternating from and to.
fn parse_rewrite_fields(
    answer: &[u8],
) -> Result<std::collections::BTreeMap<String, String>, crate::host::StoreError> {
    let mut fields: Vec<&[u8]> = answer.split(|b| *b == 0).collect();
    // A trailing NUL after the last field leaves an empty tail, which is not
    // a field. An empty map is the whole answer for an input-addressed
    // derivation, so this has to survive the zero-field case too.
    if fields.last().is_some_and(|f| f.is_empty()) {
        fields.pop();
    }
    if !fields.len().is_multiple_of(2) {
        return Err(crate::host::StoreError::Failed(format!(
            "realise hook returned {} rewrite fields, which is not a whole              number of from/to pairs",
            fields.len()
        )));
    }
    let mut map = std::collections::BTreeMap::new();
    for pair in fields.chunks_exact(2) {
        let (Some(from), Some(to)) = (pair.first(), pair.get(1)) else {
            continue;
        };
        map.insert(
            String::from_utf8_lossy(from).into_owned(),
            String::from_utf8_lossy(to).into_owned(),
        );
    }
    Ok(map)
}

/// How the embedder reports a warning. No answer and no failure: a logger
/// that dropped the message has still done all this hook promises.
pub type WarnFn = unsafe extern "C" fn(ctx: *mut c_void, message: *const u8, message_len: usize);

/// How the embedder reads a file. See `ixe.h` for the encoding.
pub type ReadFileFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    root: *const u8,
    root_len: usize,
    path: *const u8,
    path_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

pub type ImportFn = ReadFileFn;

/// How the embedder answers `builtins.pathExists`: the same status and buffer
/// contract as a file read, with `1` or `0` as the successful answer.
pub type PathExistsFn = ReadFileFn;

/// How the embedder lists a directory. See `ixe.h` for the encoding.
pub type ReadDirFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    root: *const u8,
    root_len: usize,
    path: *const u8,
    path_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// How the embedder says what a path is. See `ixe.h` for the encoding.
pub type FileTypeFn = unsafe extern "C" fn(
    ctx: *mut c_void,
    root: *const u8,
    root_len: usize,
    path: *const u8,
    path_len: usize,
    out: *mut *const u8,
    out_len: *mut usize,
) -> i32;

/// Everything the evaluator can ask of the world outside it, as one struct of
/// function pointers plus the context they are answered out of.
///
/// # Why this is a parameter and not a set of globals
///
/// It used to be fourteen `static RwLock<Option<Fn>>` slots, installed by
/// `ixe_set_copy_to_store` and thirteen siblings. Every one of them was
/// process-wide, so two evaluations in one process shared one store, one
/// logger, one resolver and one accessor, and the second to install won for
/// both -- including retroactively, for an evaluation already running. Three
/// changes fenced that with guards (#165, #170, #171) rather than removing
/// it, because the ABI gave the embedder nowhere else to put the answer.
///
/// A vtable handed over at session creation removes it: the set a session
/// answers with is fixed when the session is made, is reachable only through
/// that session, and cannot be moved by anyone afterwards. There is no
/// installation step left to race.
///
/// # The context pointer is the point of the exercise
///
/// Each function takes `ctx` first, so the embedder carries per-session state
/// on the pointer it supplied rather than in a thread-local it has to set and
/// clear around every call. The C++ bridge did the latter -- a `currentState`
/// thread-local plus fifteen `thread_local std::string` answer buffers, all
/// nulled by a destructor whose ordering nothing enforced. Those are one
/// heap object hanging off `ctx` now.
///
/// `ctx` is opaque to this crate: it is stored, passed back, and never
/// dereferenced here. It may be null when the embedder needs no state.
///
/// # Every pointer is optional, and absence is not failure
///
/// A null hook means "this embedding cannot answer that", which the evaluator
/// reports as unimplemented rather than guessing -- see
/// [`crate::host::StoreError::NoStore`] and
/// [`crate::host::LookupError::NoResolver`]. A standalone embedder that has
/// no store supplies none of the store hooks and evaluates everything that
/// does not need one.
///
/// The seven read hooks are the exception: they are all-or-nothing, because
/// [`crate::purity`] can only honour `pure-eval` and `restrict-eval` when
/// *every* read goes through an accessor that applies the allow list. Five of
/// six is a state the evaluator would have to describe as both honoured and
/// not, so a session refuses to be created with one. See
/// [`IxeHostVtable::path_reads`].
///
/// # Layout
///
/// `#[repr(C)]`, and the field order below is the ABI. It is mirrored in
/// `ixe.h` as `IxeHostVtable`; the two are kept in step by hand, like the
/// status values and the render enum.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeHostVtable {
    /// Passed back to every function below and otherwise untouched.
    pub ctx: *mut c_void,
    pub copy_to_store: Option<CopyToStoreFn>,
    pub store_path: Option<StorePathFn>,
    pub store_text: Option<StoreTextFn>,
    pub write_derivations: Option<WriteDrvsFn>,
    pub store_filtered: Option<StoreFilteredFn>,
    pub fetch: Option<FetchFn>,
    pub fetch_tree: Option<FetchTreeFn>,
    pub lock_flake: Option<LockFlakeFn>,
    pub parse_flake_ref: Option<ParseFlakeRefFn>,
    pub flake_ref_to_string: Option<FlakeRefToStringFn>,
    pub ensure_path: Option<EnsurePathFn>,
    pub valid_paths: Option<ValidPathsFn>,
    pub sealed_paths: Option<SealedPathsFn>,
    pub allow_paths: Option<AllowPathsFn>,
    /// All three or none; enforced by [`IxeHostVtable::async_realise`] at
    /// session creation. None is a host with no store behind it.
    pub realise_check: Option<RealiseCheckFn>,
    pub realise_build: Option<RealiseBuildFn>,
    pub realise_allow: Option<RealiseAllowFn>,
    pub find_file: Option<FindFileFn>,
    pub nix_path: Option<NixPathFn>,
    pub warn: Option<WarnFn>,
    pub trace: Option<TraceFn>,
    pub interrupted: Option<InterruptedFn>,
    pub import_source: Option<ImportFn>,
    pub read_file: Option<ReadFileFn>,
    pub path_exists: Option<PathExistsFn>,
    pub dir_exists: Option<PathExistsFn>,
    pub read_dir: Option<ReadDirFn>,
    pub file_type: Option<FileTypeFn>,
    pub file_type_resolved: Option<FileTypeFn>,
}

/// The seven filesystem reads, present together or not at all.
#[derive(Clone, Copy)]
struct PathReadFns {
    import_source: ImportFn,
    read_file: ReadFileFn,
    path_exists: PathExistsFn,
    dir_exists: PathExistsFn,
    read_dir: ReadDirFn,
    file_type: FileTypeFn,
    file_type_resolved: FileTypeFn,
}

impl IxeHostVtable {
    /// A vtable that answers nothing: `std::fs` reads, no store, no
    /// resolver, nowhere to warn.
    ///
    /// What a caller that passes null gets. Written out rather than derived
    /// so that adding a field to the struct is a compile error here, which is
    /// the one place that would otherwise silently keep working while
    /// answering `None` for something a caller expected to be able to set.
    #[must_use]
    pub const fn empty() -> Self {
        IxeHostVtable {
            ctx: std::ptr::null_mut(),
            copy_to_store: None,
            store_path: None,
            store_text: None,
            write_derivations: None,
            store_filtered: None,
            fetch: None,
            fetch_tree: None,
            lock_flake: None,
            parse_flake_ref: None,
            flake_ref_to_string: None,
            ensure_path: None,
            valid_paths: None,
            sealed_paths: None,
            allow_paths: None,
            realise_check: None,
            realise_build: None,
            realise_allow: None,
            find_file: None,
            nix_path: None,
            warn: None,
            trace: None,
            interrupted: None,
            import_source: None,
            read_file: None,
            path_exists: None,
            dir_exists: None,
            read_dir: None,
            file_type: None,
            file_type_resolved: None,
        }
    }

    /// Any host callback can affect answers or their replay policy.
    fn has_callbacks(&self) -> bool {
        let Self {
            ctx: _,
            copy_to_store,
            store_path,
            store_text,
            write_derivations,
            store_filtered,
            fetch,
            fetch_tree,
            lock_flake,
            parse_flake_ref,
            flake_ref_to_string,
            ensure_path,
            valid_paths,
            sealed_paths,
            allow_paths,
            realise_check,
            realise_build,
            realise_allow,
            find_file,
            nix_path,
            warn,
            trace,
            interrupted,
            import_source,
            read_file,
            path_exists,
            dir_exists,
            read_dir,
            file_type,
            file_type_resolved,
        } = self;
        copy_to_store.is_some()
            || store_path.is_some()
            || store_text.is_some()
            || write_derivations.is_some()
            || store_filtered.is_some()
            || fetch.is_some()
            || fetch_tree.is_some()
            || lock_flake.is_some()
            || parse_flake_ref.is_some()
            || flake_ref_to_string.is_some()
            || ensure_path.is_some()
            || valid_paths.is_some()
            || sealed_paths.is_some()
            || allow_paths.is_some()
            || realise_check.is_some()
            || realise_build.is_some()
            || realise_allow.is_some()
            || find_file.is_some()
            || nix_path.is_some()
            || warn.is_some()
            || trace.is_some()
            || interrupted.is_some()
            || import_source.is_some()
            || read_file.is_some()
            || path_exists.is_some()
            || dir_exists.is_some()
            || read_dir.is_some()
            || file_type.is_some()
            || file_type_resolved.is_some()
    }

    /// The seven read hooks as a group, or the reason the group is malformed.
    ///
    /// `Ok(None)` is a legitimate answer -- an embedder with no accessor,
    /// which reads with `std::fs` and cannot honour either purity setting.
    /// `Err` is the partial set, which is a caller bug: see the type's docs
    /// for why there is no honest way to run with six of seven.
    fn path_reads(&self) -> Result<Option<PathReadFns>, &'static str> {
        let supplied = [
            self.import_source.is_some(),
            self.read_file.is_some(),
            self.path_exists.is_some(),
            self.dir_exists.is_some(),
            self.read_dir.is_some(),
            self.file_type.is_some(),
            self.file_type_resolved.is_some(),
        ];
        let (
            Some(import_source),
            Some(read_file),
            Some(path_exists),
            Some(dir_exists),
            Some(read_dir),
            Some(file_type),
            Some(file_type_resolved),
        ) = (
            self.import_source,
            self.read_file,
            self.path_exists,
            self.dir_exists,
            self.read_dir,
            self.file_type,
            self.file_type_resolved,
        )
        else {
            return if supplied.iter().any(|p| *p) {
                Err(
                    "rust-eval: the host vtable supplies some of the seven filesystem \
                     read hooks and not all of them, which would leave one question \
                     reading outside this process's access control while the purity \
                     table said the setting was being honoured",
                )
            } else {
                Ok(None)
            };
        };
        Ok(Some(PathReadFns {
            import_source,
            read_file,
            path_exists,
            dir_exists,
            read_dir,
            file_type,
            file_type_resolved,
        }))
    }

    /// The three-phase realise group, or the reason it is malformed.
    ///
    /// All three or none, for the reason [`IxeHostVtable::path_reads`]
    /// refuses a partial set: the phases are a protocol, and a vtable with
    /// two of three would either skip the checks the evaluation thread owes
    /// or build outputs no one ever registers in the allow list -- both
    /// silent.
    fn async_realise(&self) -> Result<Option<AsyncRealiseFns>, &'static str> {
        let supplied = [
            self.realise_check.is_some(),
            self.realise_build.is_some(),
            self.realise_allow.is_some(),
        ];
        let (Some(check), Some(build), Some(allow)) =
            (self.realise_check, self.realise_build, self.realise_allow)
        else {
            return if supplied.iter().any(|p| *p) {
                Err(
                    "rust-eval: the host vtable supplies some of the three async \
                     realise hooks and not all of them; the phases are a protocol \
                     (check on the evaluation thread, build on a worker, allow at \
                     delivery) and a partial set would skip one of them silently",
                )
            } else {
                Ok(None)
            };
        };
        Ok(Some(AsyncRealiseFns {
            check,
            build,
            allow,
        }))
    }
}

/// The resolved three-phase realise group. See `ixe.h` for the protocol.
#[derive(Clone, Copy)]
struct AsyncRealiseFns {
    check: RealiseCheckFn,
    build: RealiseBuildFn,
    allow: RealiseAllowFn,
}

/// A [`Host`] that answers out of an [`IxeHostVtable`].
///
/// `Clone`, and cheaply: a session hands a clone to its recording host rather
/// than lending a reference, which is what keeps `MemoScope` from borrowing
/// the session that owns it. It used to be `Copy` -- the struct was pointers
/// only -- until the threaded realise path gave it in-flight state; the
/// `Arc` below is what a clone now shares, so a ticket minted through one
/// clone is collectable through any other and the "hand a copy to the
/// recorder" shape keeps working unchanged.
///
/// Anything the vtable does not supply falls through to
/// [`crate::host::RealFs`], which reads the real filesystem and has no store.
#[derive(Clone)]
pub struct EmbedderHost {
    vtable: IxeHostVtable,
    /// Resolved once, at construction, so a read does not re-derive the
    /// group on every question.
    reads: Option<PathReadFns>,
    /// The three-phase realise group, resolved once like `reads`.
    async_realise: Option<AsyncRealiseFns>,
    /// Builds begun and not yet collected. Shared across clones (see the
    /// type's doc) and behind a `Mutex` because the trait hands out `&self`;
    /// uncontended in practice, since only the evaluation thread ever calls
    /// `begin` or `collect` -- the workers talk back through their channels,
    /// never through this map.
    ifd: std::sync::Arc<IfdInflight>,
    /// Files the embedder handed over by content through `findFile`
    /// answers; see [`crate::host::VirtualFiles`]. Shared by the clones of
    /// this host, which are one session's.
    virtual_files: crate::host::VirtualFiles,
    /// Derivations answered and not yet handed to the embedder's store.
    /// Shared by the clones for the reason `ifd` is: the recording host in a
    /// memo scope is a clone, and a derivation it queues is flushed by
    /// whichever clone crosses the boundary next. See
    /// [`EmbedderHost::flush_derivations`].
    drv_writes: std::sync::Arc<std::sync::Mutex<DrvWrites>>,
}

use crate::store_batch::PendingDerivation as PendingDrv;

/// The write-behind state of one host: what is queued, and whether a flush
/// has failed.
///
/// `paths` is the set of `expected` paths in `pending`, kept beside the
/// vector so a read can ask "does this name a pending derivation" in one
/// lookup rather than a scan (a NixOS evaluation lists 139k directories).
/// `failed` is sticky: a batch the store refused is not retried, because the
/// derivations in it were already answered as written to the evaluation
/// that asked, and the only honest continuation is to fail every later
/// store question and the settle of every later evaluation, so no result
/// that depends on them can be returned or memoised.
#[derive(Default)]
struct DrvWrites {
    pending: Vec<PendingDrv>,
    paths: std::collections::BTreeSet<String>,
    failed: Option<String>,
}

/// Queue length that forces a flush on its own, so an evaluation that never
/// crosses the boundary (one `nix build` of a whole NixOS closure is a single
/// force) holds a bounded number of ATerms: 4096 of them is ~16 MB.
const DRV_WRITE_BATCH: usize = 4096;

/// What arrives at [`EmbedderHost::collect`] for one realise question: the
/// worker's answer bytes, or the outcome the check phase settled on the
/// evaluation thread without a build. Both are filed under the ticket at
/// `begin`, so a question is checked exactly once whichever way it goes.
enum Realised {
    Nothing,
    Built(Vec<u8>),
    Failed(crate::host::StoreError),
}

/// What the check phase settled, short of a refusal.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Checked {
    Build,
    Nothing,
}

/// A build hook's status and answer bytes, as the outcome they mean.
fn built(rc: i32, answer: Vec<u8>) -> Realised {
    if rc == 0 {
        Realised::Built(answer)
    } else {
        Realised::Failed(crate::host::StoreError::Failed(
            String::from_utf8_lossy(&answer).into_owned(),
        ))
    }
}

/// Tickets and the build dispatcher shared by this session's host clones.
#[derive(Default)]
struct IfdInflight {
    next: std::sync::atomic::AtomicU64,
    inflight: std::sync::Mutex<std::collections::HashMap<u64, std::sync::mpsc::Receiver<Realised>>>,
    dispatcher: std::sync::Mutex<Option<IfdDispatcher>>,
}

struct IfdRequest {
    encoded: Vec<u8>,
    answer: std::sync::mpsc::Sender<Realised>,
}

/// One build dispatcher per session. The store owns concurrency within a
/// batch, so max-jobs and max-substitution-jobs govern all its IFD roots.
struct IfdDispatcher {
    queue: std::sync::Arc<(std::sync::Mutex<IfdQueue>, std::sync::Condvar)>,
    worker: Option<std::thread::JoinHandle<()>>,
}

#[derive(Default)]
struct IfdQueue {
    pending: std::collections::VecDeque<IfdRequest>,
    ready: bool,
    closed: bool,
}

impl Drop for IfdDispatcher {
    fn drop(&mut self) {
        {
            let mut queue = self.queue.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            queue.closed = true;
            queue.pending.clear();
        }
        self.queue.1.notify_one();
        // The callback borrows the embedder's ctx. Join before session_free
        // returns and lets the embedder release that object. Queued requests
        // are discarded; an active callback observes normal store interrupts.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

const IFD_BATCH_SIZE: usize = 256;

fn run_ifd_batches(
    ctx: SendCtx,
    build: RealiseBuildFn,
    shared: std::sync::Arc<(std::sync::Mutex<IfdQueue>, std::sync::Condvar)>,
) {
    loop {
        let batch: Vec<_> = {
            let queue = shared.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut queue = shared.1.wait_while(queue, |q| !q.closed && (!q.ready || q.pending.is_empty()))
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if queue.closed {
                break;
            }
            let count = queue.pending.len().min(IFD_BATCH_SIZE);
            let batch = queue.pending.drain(..count).collect();
            queue.ready = !queue.pending.is_empty();
            batch
        };
        let requests: Vec<_> = batch.iter().map(|request| IxeRealiseRequest {
            data: request.encoded.as_ptr(),
            len: request.encoded.len(),
        }).collect();
        const MISSING: &[u8] = b"realise build hook did not fill a result slot";
        let mut results: Vec<_> = batch.iter().map(|_| IxeRealiseResult {
            status: 1,
            data: MISSING.as_ptr(),
            len: MISSING.len(),
        }).collect();
        let started = std::time::Instant::now();
        // SAFETY: requests borrow this batch; results has exactly the same
        // length. The ctx contract and returned-buffer lifetime are in ixe.h.
        let rc = unsafe { build(ctx.0, requests.as_ptr(), requests.len(), results.as_mut_ptr()) };
        if crate::perf::replay_trace() {
            eprintln!("ixe slow: Realise batch={} work_ns={} rc={rc}", batch.len(), started.elapsed().as_nanos());
        }
        for (request, result) in batch.into_iter().zip(results) {
            let realised = if rc == 0 {
                built(result.status, read_answer_bytes(result.data, result.len))
            } else {
                Realised::Failed(crate::host::StoreError::Failed(format!(
                    "realise build batch failed with status {rc}"
                )))
            };
            // A dropped receiver means this evaluation no longer needs it.
            let _ = request.answer.send(realised);
        }
    }
}

/// One `(ctx, bytes...) -> (status, answer)` crossing.
///
/// Every hook below has this shape: hand the embedder some borrowed bytes,
/// get back a status and a pointer into a buffer the embedder owns, and copy
/// the buffer before returning. One helper rather than fourteen copies of it,
/// which is also fourteen fewer places for the copy to be forgotten.
macro_rules! ask_embedder {
    ($f:expr, $ctx:expr $(, $arg:expr)* $(,)?) => {{
        let mut out: *const u8 = std::ptr::null();
        let mut out_len: usize = 0;
        // SAFETY: every `$arg` is borrowed from a local that outlives the
        // call, `out` and `out_len` are locals, and the ABI's contract in
        // `ixe.h` is that the callee's buffer outlives the call -- it is read
        // by `read_answer_bytes` before this block ends.
        let rc = unsafe { $f($ctx $(, $arg)*, &raw mut out, &raw mut out_len) };
        (rc, read_answer_bytes(out, out_len))
    }};
}

impl EmbedderHost {
    fn enqueue_ifd(
        &self,
        build: RealiseBuildFn,
        encoded: Vec<u8>,
        answer: std::sync::mpsc::Sender<Realised>,
    ) -> Result<(), crate::host::StoreError> {
        let mut dispatcher = self.ifd.dispatcher.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if dispatcher.is_none() {
            let queue = std::sync::Arc::new((
                std::sync::Mutex::new(IfdQueue::default()), std::sync::Condvar::new(),
            ));
            let worker_queue = queue.clone();
            let ctx = SendCtx(self.vtable.ctx);
            let worker = std::thread::Builder::new()
                .name("nix-eval-ifd".into())
                .spawn(move || run_ifd_batches(ctx, build, worker_queue))
                .map_err(|why| crate::host::StoreError::Failed(format!(
                    "could not start the build dispatcher: {why}"
                )))?;
            *dispatcher = Some(IfdDispatcher {
                queue, worker: Some(worker),
            });
        }
        let dispatcher = dispatcher.as_ref()
            .ok_or_else(|| crate::host::StoreError::Failed("build dispatcher is closed".into()))?;
        if dispatcher.worker.as_ref().is_some_and(|worker| worker.is_finished()) {
            return Err(crate::host::StoreError::Failed("build dispatcher stopped before answering".into()));
        }
        dispatcher.queue.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending.push_back(IfdRequest { encoded, answer });
        Ok(())
    }

    fn dispatch_ifd(&self) {
        let dispatcher = self.ifd.dispatcher.lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(dispatcher) = dispatcher.as_ref() {
            let mut queue = dispatcher.queue.0.lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if dispatcher.worker.as_ref().is_some_and(|worker| worker.is_finished()) {
                for request in queue.pending.drain(..) {
                    let _ = request.answer.send(Realised::Failed(crate::host::StoreError::Failed(
                        "build dispatcher stopped before answering".into()
                    )));
                }
                return;
            }
            if !queue.pending.is_empty() {
                queue.ready = true;
                dispatcher.queue.1.notify_one();
            }
        }
    }

    /// Build a host from the embedder's vtable, or say why the vtable is
    /// malformed.
    fn new(vtable: IxeHostVtable) -> Result<Self, &'static str> {
        Ok(EmbedderHost {
            reads: vtable.path_reads()?,
            async_realise: vtable.async_realise()?,
            vtable,
            ifd: std::sync::Arc::default(),
            virtual_files: crate::host::VirtualFiles::default(),
            drv_writes: std::sync::Arc::default(),
        })
    }

    /// Whether a plain filesystem read reaches the world through the
    /// embedder's accessor. Folded into [`crate::eval::Settings`] when the
    /// session is created, because it changes what `pure-eval` answers.
    fn path_reads(&self) -> crate::purity::PathReads {
        match self.reads {
            Some(_) => crate::purity::PathReads::ThroughEmbedder,
            None => crate::purity::PathReads::Direct,
        }
    }

    /// Hand every derivation answered since the last flush to the embedder's
    /// store, as one batch.
    ///
    /// Why the write is deferred at all: `Host::write_derivation` is asked
    /// once per `builtins.derivationStrict` and its answer, the store path,
    /// is a function of the ATerm the evaluator already holds. Asking the
    /// store per derivation made every one a daemon round trip (measured on
    /// the real home-manager closure: 23k writes at 600 us each, the largest
    /// single cost of an edited-config evaluation, and most of them for
    /// derivations the store already held). So the host answers from the
    /// bytes and queues the write.
    ///
    /// Why it is flushed where it is: a queued derivation is invisible to the
    /// store, and the evaluation must never observe that. Every question
    /// whose answer could -- a build (`realise`, blocking or begun), a
    /// validity or sealing check, `ensure_path` or `builtins.storePath` of
    /// the path, a read of the path, a `toFile` that references it -- flushes
    /// first. The crossing back to the embedder is ONE place: the scheduler
    /// settles every finished evaluation through [`Host::settle`]
    /// (`eval::settle_job`), so a forced handle, a render, a memo
    /// verification and a derivation-set walk are all covered by
    /// construction, because the embedder may build what it was just handed.
    /// (A flush at each entry point was tried first and missed `ixe_render`,
    /// whose deep force is where `nix-instantiate --eval --strict` builds its
    /// derivations.) A question about a path that is not pending does not
    /// flush: the check is one set lookup, and a NixOS evaluation asks 139k
    /// directory listings.
    ///
    /// A refused batch poisons the host (`DrvWrites::failed`): this returns
    /// the failure, every later `write_derivation` returns it, every store
    /// question that consults the queue (`settle_writes_if`) routes here
    /// and returns it, and the question-answer path asks before recording, so
    /// a result whose derivations are not all in the store is neither
    /// returned quietly nor memoised.
    fn flush_derivations(&self) -> Result<(), crate::host::StoreError> {
        // No hook: nothing was ever queued (`write_derivation` answers
        // `NoStore` before it gets that far).
        let Some(f) = self.vtable.write_derivations else {
            return Ok(());
        };
        let mut writes = self
            .drv_writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(why) = &writes.failed {
            return Err(crate::host::StoreError::Failed(why.clone()));
        }
        if writes.pending.is_empty() {
            return Ok(());
        }
        let pending = std::mem::take(&mut writes.pending);
        writes.paths.clear();
        let batch = crate::store_batch::encode(&pending).map_err(|error| {
            let why = format!(
                "preparing {} derivation(s) for the store: {error}",
                pending.len()
            );
            writes.failed = Some(why.clone());
            crate::host::StoreError::Failed(why)
        })?;
        // The lock is held across the call on purpose: a second flush from
        // another clone must not interleave, and the embedder's writer does
        // not call back into this host.
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, batch.as_ptr(), batch.len());
        crate::perf::note_drv_write_flush(pending.len() as u64);
        if rc == IXE_OK {
            return Ok(());
        }
        let why = format!(
            "writing {} derivation(s) to the store: {}",
            pending.len(),
            text_of(answer)
        );
        writes.failed = Some(why.clone());
        Err(crate::host::StoreError::Failed(why))
    }

    /// Flush if `path` is, or lies under, a derivation answered and not yet
    /// written. `path` is an ambient (store-directory) path; a mounted root
    /// is a tree the evaluator reads directly and is never a `.drv`.
    fn settle_writes_under(&self, path: &str) -> Result<(), crate::host::StoreError> {
        self.settle_writes_if(|writes| {
            store_object_of(path).is_some_and(|object| writes.paths.contains(object))
        })
    }

    /// Flush when `pending` says the queue holds what the caller is about to
    /// ask the store about, and always once a batch has failed: the failure
    /// is sticky, and a store question after it must return it rather than
    /// proceed as if the queue were simply empty. The `paths` emptiness test
    /// keeps the common case (nothing queued) to one lock and one branch.
    fn settle_writes_if(
        &self,
        pending: impl FnOnce(&DrvWrites) -> bool,
    ) -> Result<(), crate::host::StoreError> {
        let settle = {
            let writes = self
                .drv_writes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            writes.failed.is_some() || (!writes.paths.is_empty() && pending(&writes))
        };
        if settle {
            self.flush_derivations()
        } else {
            Ok(())
        }
    }

    /// [`EmbedderHost::settle_writes_under`] for a path value: only an
    /// ambient path can name a store object.
    fn settle_writes_for(&self, path: &crate::value2::PathValue) -> Result<(), String> {
        match path.root {
            crate::value2::Root::Ambient => self
                .settle_writes_under(&path.path)
                .map_err(|error| error.to_string()),
            crate::value2::Root::Mounted(_) => Ok(()),
        }
    }

    /// Flush if any of `references` is a pending derivation: a `toFile`
    /// that references a `.drv` is registered with that reference, and the
    /// store refuses a reference it does not hold.
    fn settle_writes_referenced(
        &self,
        references: &[String],
    ) -> Result<(), crate::host::StoreError> {
        self.settle_writes_if(|writes| references.iter().any(|r| writes.paths.contains(r)))
    }

    /// How this host's evaluations find out they have been interrupted, or
    /// `None` when the embedder cannot be asked.
    fn interrupt(&self) -> Option<crate::vm::InterruptHook> {
        let f = self.vtable.interrupted?;
        let ctx = self.vtable.ctx;
        // SAFETY: the ABI's contract -- the embedder's function takes the
        // context it supplied, returns an int, and does not unwind. `ctx` is
        // opaque here and is only handed back.
        Some(Box::new(move || unsafe { f(ctx) } != 0))
    }

    /// Phase 1, on the calling thread: the embedder's validity checks (and
    /// their read-set recording) and its allow-import-from-derivation
    /// refusal, for a context this host is about to realise. Both routes
    /// come through here, so a question is checked exactly once.
    fn check_realise(
        &self,
        fns: AsyncRealiseFns,
        encoded: &[u8],
    ) -> Result<Checked, crate::host::StoreError> {
        let mut error_class = IxeRealiseErrorClass::Other as i32;
        let (rc, answer) = ask_embedder!(
            fns.check,
            self.vtable.ctx,
            encoded.as_ptr(),
            encoded.len(),
            &raw mut error_class
        );
        match IxeRealiseCheck::decode(rc) {
            Ok(IxeRealiseCheck::Build) => Ok(Checked::Build),
            Ok(IxeRealiseCheck::Nothing) => Ok(Checked::Nothing),
            Ok(IxeRealiseCheck::Failed) => {
                let message = String::from_utf8_lossy(&answer).into_owned();
                Err(match IxeRealiseErrorClass::decode(error_class) {
                    Ok(IxeRealiseErrorClass::Other) => crate::host::StoreError::Failed(message),
                    Ok(IxeRealiseErrorClass::ImportFromDerivation) => {
                        crate::host::StoreError::ImportFromDerivation(message)
                    }
                    // A protocol fault, not a store fault: reported as a
                    // failure with the integer, never coerced into a class
                    // it did not name.
                    Err(other) => crate::host::StoreError::Failed(format!(
                        "realise check hook failed with error class {other}, which \
                         ixe.h does not define: {message}"
                    )),
                })
            }
            Err(other) => Err(crate::host::StoreError::Failed(format!(
                "realise check hook answered status {other}, which ixe.h does not define"
            ))),
        }
    }

    /// Turn a realise question's settled outcome into its result,
    /// running phase 3 -- the allow-list registration -- on the calling
    /// thread, which is the collecting thread, which is the evaluation
    /// thread. That placement is the point: the allow list is a plain set
    /// the evaluation thread reads on every file access, and it runs before
    /// the answer is delivered, so no read through a built output can
    /// precede its registration.
    fn finish_realise(
        &self,
        fns: AsyncRealiseFns,
        realised: Realised,
    ) -> Result<std::collections::BTreeMap<String, String>, crate::host::StoreError> {
        let answer = match realised {
            Realised::Nothing => return Ok(std::collections::BTreeMap::new()),
            Realised::Failed(why) => return Err(why),
            Realised::Built(answer) => answer,
        };
        let (rewrites, outputs) = split_build_answer(&answer)?;
        let (rc, message) =
            ask_embedder!(fns.allow, self.vtable.ctx, outputs.as_ptr(), outputs.len());
        if rc != 0 {
            return Err(crate::host::StoreError::Failed(
                String::from_utf8_lossy(&message).into_owned(),
            ));
        }
        Ok(rewrites)
    }
}

/// The embedder's context pointer, crossing into the build worker.
///
/// # Safety
///
/// `*mut c_void` is `!Send` because Rust cannot know what it points at. Here
/// the embedder does: supplying `realise_build` in the vtable is its written
/// agreement (`ixe.h`, beside `ixe_realise_check_fn`) that this function may
/// be called with this context from a thread the evaluator owns, concurrently
/// with the other hooks on the evaluation thread. The pointer is otherwise
/// opaque -- the worker only hands it back.
struct SendCtx(*mut c_void);
// SAFETY: see the type's doc; the agreement is the embedder's, made by
// supplying the hook this wrapper is only ever used to call.
unsafe impl Send for SendCtx {}

/// Split a `realise_build` answer at its empty-field separator: the rewrite
/// pairs before it, the output store paths (returned raw, NUL-terminated,
/// ready for `realise_allow`) after it.
fn split_build_answer(
    answer: &[u8],
) -> Result<(std::collections::BTreeMap<String, String>, &[u8]), crate::host::StoreError> {
    let mut rest = answer;
    let mut rewrites = Vec::new();
    loop {
        let Some(cut) = rest.iter().position(|b| *b == 0) else {
            return Err(crate::host::StoreError::Failed(
                "realise build hook answered with no separator between the \
                 rewrites and the outputs"
                    .to_owned(),
            ));
        };
        let field = rest.get(..cut).unwrap_or_default();
        rest = rest.get(cut + 1..).unwrap_or_default();
        if field.is_empty() {
            break;
        }
        rewrites.extend_from_slice(field);
        rewrites.push(0);
    }
    Ok((parse_rewrite_fields(&rewrites)?, rest))
}

/// The bytes of a borrowed string, as the pointer/length pair the ABI takes.
fn bytes_of(s: &str) -> (*const u8, usize) {
    (s.as_ptr(), s.len())
}

/// Store paths each followed by a NUL, the encoding the allow hooks take
/// (`ixe_allow_paths_fn`, `ixe_realise_allow_fn`); unambiguous because a
/// store path cannot contain one.
fn nul_terminated(paths: &[String]) -> Vec<u8> {
    let mut encoded = Vec::new();
    for path in paths {
        encoded.extend_from_slice(path.as_bytes());
        encoded.push(0);
    }
    encoded
}

/// Store paths packed the way [`StoreTextFn`] documents: each followed by a
/// NUL, which is unambiguous because a store path cannot contain one.
fn pack_references(references: &[String]) -> Vec<u8> {
    let mut refs = Vec::new();
    for r in references {
        refs.extend_from_slice(r.as_bytes());
        refs.push(0);
    }
    refs
}

impl Host for EmbedderHost {
    fn settle(&self) -> Result<(), crate::host::StoreError> {
        self.flush_derivations()
    }

    fn import_source(
        &self,
        path: &crate::value2::PathValue,
    ) -> Result<crate::host::ImportedSource, String> {
        self.settle_writes_for(path)?;
        self.virtual_files.import_source_or(path, || {
            let Some(reads) = self.reads else {
                return crate::host::RealFs.import_source(path);
            };
            let answer = ask_about_path_bytes(reads.import_source, self.vtable.ctx, path)?;
            decode_imported_source(&answer)
        })
    }

    fn get_env(&self, name: &str) -> Option<String> {
        crate::host::RealFs.get_env(name)
    }

    fn copy_to_store(
        &self,
        path: &crate::value2::PathValue,
    ) -> Result<String, crate::host::StoreError> {
        let f = self
            .vtable
            .copy_to_store
            .ok_or(crate::host::StoreError::NoStore)?;
        let accessor_path = path.accessor_path();
        let (r, r_len) = bytes_of(path.root.wire_name());
        let (p, len) = bytes_of(accessor_path);
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, r, r_len, p, len);
        store_answer(rc, answer)
    }

    fn store_path(
        &self,
        path: &crate::value2::PathValue,
    ) -> Result<crate::host::StorePathResult, crate::host::StoreError> {
        let f = self
            .vtable
            .store_path
            .ok_or(crate::host::StoreError::NoStore)?;
        if let crate::value2::Root::Ambient = path.root {
            self.settle_writes_under(&path.path)?;
        }
        let accessor_path = path.accessor_path();
        let (r, r_len) = bytes_of(path.root.wire_name());
        let (p, len) = bytes_of(accessor_path);
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, r, r_len, p, len);
        let answer = store_answer(rc, answer)?;
        let Some((store_path, visible_path)) = answer.split_once('\0') else {
            return Err(crate::host::StoreError::Failed(
                "builtins.storePath answer has no store-path separator".to_owned(),
            ));
        };
        if store_path.is_empty() || visible_path.is_empty() || visible_path.contains('\0') {
            return Err(crate::host::StoreError::Failed(
                "builtins.storePath answer is malformed".to_owned(),
            ));
        }
        Ok(crate::host::StorePathResult {
            path: visible_path.to_owned(),
            store_path: store_path.to_owned(),
        })
    }

    fn store_text(
        &self,
        name: &str,
        contents: &str,
        references: &[String],
    ) -> Result<String, crate::host::StoreError> {
        let f = self
            .vtable
            .store_text
            .ok_or(crate::host::StoreError::NoStore)?;
        self.settle_writes_referenced(references)?;
        let refs = pack_references(references);
        let (n, n_len) = bytes_of(name);
        let (c, c_len) = bytes_of(contents);
        let (rc, answer) = ask_embedder!(
            f,
            self.vtable.ctx,
            n,
            n_len,
            c,
            c_len,
            refs.as_ptr(),
            refs.len(),
        );
        store_answer(rc, answer)
    }

    /// Answered here, from the bytes, and queued for the store; see
    /// [`EmbedderHost::flush_derivations`] for why and for when the store
    /// hears about it. Rust validates the canonical ATerm once and retains
    /// its reference set beside the bytes until ingestion. Memo replay takes
    /// this same route, so cached bytes cannot bypass validation.
    fn write_derivation(&self, name: &str, aterm: &str) -> Result<String, crate::host::StoreError> {
        if self.vtable.write_derivations.is_none() {
            return Err(crate::host::StoreError::NoStore);
        }
        let Some(store_dir) = crate::eval::store_dir() else {
            return Err(crate::host::StoreError::Failed(format!(
                "derivation '{name}': no store directory was set for this evaluator"
            )));
        };
        let drv =
            PendingDrv::new(store_dir, name, aterm).map_err(crate::host::StoreError::Failed)?;
        let expected = drv.expected.clone();
        let full = {
            let mut writes = self
                .drv_writes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(why) = &writes.failed {
                return Err(crate::host::StoreError::Failed(why.clone()));
            }
            if writes.paths.insert(expected.clone()) {
                writes.pending.push(drv);
                crate::perf::note_drv_write_deferred();
            }
            writes.pending.len() >= DRV_WRITE_BATCH
        };
        if full {
            self.flush_derivations()?;
        }
        Ok(expected)
    }

    fn ensure_path(&self, path: &str) -> Result<(), crate::host::StoreError> {
        let f = self
            .vtable
            .ensure_path
            .ok_or(crate::host::StoreError::NoStore)?;
        self.settle_writes_under(path)?;
        let (p, len) = bytes_of(path);
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, p, len);
        match rc {
            0 => Ok(()),
            _ => Err(crate::host::StoreError::Failed(text_of(answer))),
        }
    }

    fn valid_paths(
        &self,
        paths: &[String],
    ) -> Result<std::collections::BTreeSet<String>, crate::host::StoreError> {
        let f = self
            .vtable
            .valid_paths
            .ok_or(crate::host::StoreError::NoStore)?;
        // A pending derivation answers this wrongly, so the queue is flushed
        // when any asked path is (or lies under) one; a question about
        // anything else -- the copy memo's candidate, a fetched tree -- does
        // not wait for the writes. Measured before this test (l2ba1, l2ca1):
        // the copy memo's 1006 validity asks spent 1.9 s, p50 65 us, p99 31
        // ms, and the tail was the flushes they forced.
        self.settle_writes_if(|writes| {
            paths.iter().any(|path| {
                store_object_of(path).is_some_and(|object| writes.paths.contains(object))
            })
        })?;
        let request = paths.join("\n");
        let (p, len) = bytes_of(&request);
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, p, len);
        if rc != 0 {
            return Err(crate::host::StoreError::Failed(text_of(answer)));
        }
        // Strict, as `sealed_paths` is: each line is authorisation to serve a
        // row by validity, so a line that is not one of the paths asked about
        // (a trailing `\r` included: split on `\n` alone) refuses the whole
        // answer and every row asks.
        let text = text_of(answer);
        let mut present = std::collections::BTreeSet::new();
        for line in text.split('\n').filter(|line| !line.is_empty()) {
            if !paths.iter().any(|asked| asked == line) {
                return Err(crate::host::StoreError::Failed(format!(
                    "valid_paths: {line:?} was not asked about"
                )));
            }
            present.insert(line.to_owned());
        }
        Ok(present)
    }

    fn sealed_paths(
        &self,
        objects: &[String],
    ) -> Result<crate::host::Links, crate::host::StoreError> {
        let f = self
            .vtable
            .sealed_paths
            .ok_or(crate::host::StoreError::NoStore)?;
        self.flush_derivations()?;
        let request = objects.join("\n");
        let (p, len) = bytes_of(&request);
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, p, len);
        if rc != 0 {
            return Err(crate::host::StoreError::Failed(text_of(answer)));
        }
        // Strict, and one bad line refuses the whole answer: a line is
        // authorisation to replay reads without asking, so anything the
        // contract does not spell -- an object not asked about, an odd field,
        // an empty or non-relative link path -- is a host this decoder does
        // not understand, and every row asks. Split on `\n` alone: `lines()`
        // would strip a trailing `\r` and rename a link.
        let text = text_of(answer);
        let mut links = crate::host::Links::new();
        for line in text.split('\n').filter(|line| !line.is_empty()) {
            let fields: Vec<&str> = line.split('\t').collect();
            let [object, pairs @ ..] = fields.as_slice() else {
                unreachable!("split yields at least one field");
            };
            if !objects.iter().any(|asked| asked == object) || pairs.len() % 2 != 0 {
                return Err(crate::host::StoreError::Failed(format!(
                    "sealed_paths: malformed line for {object:?}"
                )));
            }
            let mut object_links = Vec::with_capacity(pairs.len() / 2);
            for pair in pairs.chunks_exact(2) {
                let &[path, target] = pair else {
                    unreachable!("chunks_exact(2) yields pairs");
                };
                let canonical = !path.is_empty()
                    && path
                        .split('/')
                        .all(|c| !c.is_empty() && c != "." && c != "..");
                if !canonical {
                    return Err(crate::host::StoreError::Failed(format!(
                        "sealed_paths: link path {path:?} under {object:?} is not object-relative"
                    )));
                }
                object_links.push(crate::host::Symlink {
                    path: path.to_owned(),
                    target: target.to_owned(),
                });
            }
            links.insert((*object).to_owned(), object_links);
        }
        Ok(links)
    }

    fn allow_paths(&self, paths: &[String]) -> Result<(), crate::host::StoreError> {
        // `allowPath` per path, as the live copy, fetch and tree hooks do.
        let f = self
            .vtable
            .allow_paths
            .ok_or(crate::host::StoreError::NoStore)?;
        let encoded = nul_terminated(paths);
        let (rc, message) = ask_embedder!(f, self.vtable.ctx, encoded.as_ptr(), encoded.len());
        if rc != 0 {
            return Err(crate::host::StoreError::Failed(
                String::from_utf8_lossy(&message).into_owned(),
            ));
        }
        Ok(())
    }

    fn allow_closures(&self, outputs: &[String]) -> Result<(), crate::host::StoreError> {
        // Phase 3 of the realise protocol on its own: the allow-list
        // registration, in the encoding `finish_realise` hands it --
        // NUL-terminated store paths -- and with its closure semantics.
        let Some(fns) = self.async_realise else {
            return Err(crate::host::StoreError::NoStore);
        };
        let encoded = nul_terminated(outputs);
        let (rc, message) =
            ask_embedder!(fns.allow, self.vtable.ctx, encoded.as_ptr(), encoded.len());
        if rc != 0 {
            return Err(crate::host::StoreError::Failed(
                String::from_utf8_lossy(&message).into_owned(),
            ));
        }
        Ok(())
    }

    fn realise(
        &self,
        context: &[crate::value2::ContextElem],
    ) -> Result<std::collections::BTreeMap<String, String>, crate::host::StoreError> {
        // No hooks is a host with no store behind it, which refuses by name
        // rather than reading a path nothing built.
        let Some(fns) = self.async_realise else {
            return Err(crate::host::StoreError::NoStore);
        };
        // A build reads `.drv`s, and the check phase ensures drv-deep
        // elements: everything queued lands first.
        self.flush_derivations()?;
        let started = std::time::Instant::now();
        let encoded = encode_realise_request(context);
        let checked = self.check_realise(fns, &encoded)?;
        let outcome = match checked {
            Checked::Nothing => "nothing",
            Checked::Build => "build",
        };
        let realised = match checked {
            Checked::Nothing => Realised::Nothing,
            Checked::Build => {
                let (tx, rx) = std::sync::mpsc::channel();
                self.enqueue_ifd(fns.build, encoded, tx)?;
                self.dispatch_ifd();
                rx.recv().map_err(|_| crate::host::StoreError::Failed(
                    "build dispatcher stopped before answering".into()
                ))?
            }
        };
        let answer = self.finish_realise(fns, realised);
        if crate::perf::replay_trace() {
            // One line per question, the instrument for a slow record: what
            // the evaluation asked to be built, whether the check phase
            // settled it without a build, and what it cost.
            eprintln!(
                "ixe question: Realise elements={} outcome={outcome} ns={} first={}",
                context.len(),
                started.elapsed().as_nanos(),
                context
                    .first()
                    .map_or(String::new(), |elem| format!("{elem:?}"))
            );
        }
        answer
    }

    fn store_filtered(
        &self,
        request: &crate::task::FilteredCopy,
    ) -> Result<String, crate::host::StoreError> {
        let f = self
            .vtable
            .store_filtered
            .ok_or(crate::host::StoreError::NoStore)?;
        let encoded = encode_filtered_copy(request);
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, encoded.as_ptr(), encoded.len());
        // The trace line for this ask is the recorder's
        // (`readset::store_filtered_trace`, `served=walk` for a copy that
        // reached here), one per ask whichever route answered.
        store_answer(rc, answer)
    }

    fn fetch(
        &self,
        request: &crate::task::FetchRequest,
    ) -> Result<String, crate::host::StoreError> {
        let f = self.vtable.fetch.ok_or(crate::host::StoreError::NoStore)?;
        let encoded = encode_fetch(request);
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, encoded.as_ptr(), encoded.len());
        store_answer(rc, answer)
    }

    fn fetch_tree(
        &self,
        request: &crate::task::FetchTreeRequest,
    ) -> Result<String, crate::host::StoreError> {
        let f = self
            .vtable
            .fetch_tree
            .ok_or(crate::host::StoreError::NoStore)?;
        let encoded = encode_fetch_tree(request);
        let started = std::time::Instant::now();
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, encoded.as_ptr(), encoded.len());
        let answer = three_way(rc, answer).map(|(text, _)| text);
        if crate::perf::replay_trace() {
            // The edit arm spends ~1.5 s on 22 of these (~66 ms each) with
            // every input locked; this names the fetcher and the cost so the
            // slow ones can be told from the cheap ones.
            eprintln!(
                "ixe question: FetchTree fetcher={} ns={} -> {}",
                request.fetcher.as_str(),
                started.elapsed().as_nanos(),
                answer.as_deref().map_or("error", |text| head(text, 200))
            );
        }
        answer
    }

    fn lock_flake(
        &self,
        flake_ref: &str,
    ) -> Result<crate::host::FlakeCall, crate::host::StoreError> {
        let f = self
            .vtable
            .lock_flake
            .ok_or(crate::host::StoreError::NoStore)?;
        let (p, len) = bytes_of(flake_ref);
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, p, len);
        let (text, _) = three_way(rc, answer)?;
        decode_flake_call(&text)
    }

    fn parse_flake_ref(&self, flake_ref: &str) -> Result<String, crate::host::StoreError> {
        let f = self
            .vtable
            .parse_flake_ref
            .ok_or(crate::host::StoreError::NoStore)?;
        let (p, len) = bytes_of(flake_ref);
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, p, len);
        three_way(rc, answer).map(|(text, _)| text)
    }

    fn flake_ref_to_string(
        &self,
        attrs: &std::collections::BTreeMap<String, crate::task::TreeAttr>,
    ) -> Result<String, crate::host::StoreError> {
        let f = self
            .vtable
            .flake_ref_to_string
            .ok_or(crate::host::StoreError::NoStore)?;
        let encoded = encode_flake_ref_attrs(attrs);
        let (rc, answer) = ask_embedder!(f, self.vtable.ctx, encoded.as_ptr(), encoded.len());
        three_way(rc, answer).map(|(text, _)| text)
    }

    fn find_file(
        &self,
        entries: &[crate::task::SearchPathEntry],
        name: &str,
    ) -> Result<crate::value2::PathValue, crate::host::LookupError> {
        let f = self
            .vtable
            .find_file
            .ok_or(crate::host::LookupError::NoResolver)?;
        let encoded = encode_entries(entries);
        let (n, n_len) = bytes_of(name);
        let (rc, answer) = ask_embedder!(
            f,
            self.vtable.ctx,
            encoded.as_ptr(),
            encoded.len(),
            n,
            n_len,
        );
        match rc {
            // A success whose payload does not decode is the embedder
            // breaking the protocol, not this backend declining: `Failed`,
            // never `Unsupported`, which is reserved for an explicit
            // IXE_ERR_UNIMPLEMENTED answer.
            IXE_OK => {
                let (path, contents) =
                    decode_find_file_answer(&answer).map_err(crate::host::LookupError::Failed)?;
                // Bytes for a path this evaluator cannot read off the world:
                // held by this host, and served to every later question
                // about the path, the atomic import included.
                if let Some(contents) = contents {
                    self.virtual_files.insert(path.as_ref(), &contents);
                }
                Ok(path)
            }
            // The status the C side uses for a thrown error everywhere else,
            // so "not found" stays catchable by `builtins.tryEval` as it is
            // in cppnix.
            IXE_ERR_THROWN => Err(crate::host::LookupError::NotFound(text_of(answer))),
            // The status the C side uses for an unimplemented construct, so a
            // resolved path this evaluator cannot read scores `unimplemented`
            // rather than a mismatch.
            IXE_ERR_UNIMPLEMENTED => Err(crate::host::LookupError::Unsupported(text_of(answer))),
            _ => Err(crate::host::LookupError::Failed(text_of(answer))),
        }
    }

    fn nix_path(&self) -> Result<Vec<crate::task::SearchPathEntry>, crate::host::LookupError> {
        let f = self
            .vtable
            .nix_path
            .ok_or(crate::host::LookupError::NoResolver)?;
        let (rc, bytes) = ask_embedder!(f, self.vtable.ctx);
        if rc != IXE_OK {
            return Err(crate::host::LookupError::Failed(text_of(bytes)));
        }
        decode_entries(&bytes).map_err(crate::host::LookupError::Failed)
    }

    fn warn(&self, message: &str) {
        if let Some(f) = self.vtable.warn {
            // SAFETY: the ABI's contract -- `message` is live for the call and
            // the callee must not retain it.
            unsafe { f(self.vtable.ctx, message.as_ptr(), message.len()) };
        }
    }

    fn trace(&self, message: &str) {
        if let Some(f) = self.vtable.trace {
            // SAFETY: as `warn` above.
            unsafe { f(self.vtable.ctx, message.as_ptr(), message.len()) };
        }
    }

    /// The asynchronous path across the C ABI, for exactly one question:
    /// [`crate::host::Slow::Realise`], the import-from-derivation build
    /// (ENG-13150).
    ///
    /// Only that one, deliberately. `Fetch`, `FetchTree` and `Flake` reach
    /// the embedder's fetchers, tarball cache and flake registry, none of
    /// which has been audited for a call from a second thread; answering
    /// `None` sends them down the synchronous path unchanged, and widening
    /// this match is a decision that starts with that audit, not with the
    /// match.
    ///
    /// An embedder that never agreed to a worker-thread call is never given
    /// one: the thread runs only `realise_build`, and supplying that hook is
    /// the agreement, written down beside `ixe_realise_check_fn` in `ixe.h`.
    /// The check phase runs here, on the calling thread, before anything is
    /// spawned; the allow phase runs in [`Host::collect`], on the collecting
    /// thread. Both touch evaluator-side state that is single-threaded, and
    /// keeping them on this side of the spawn is the fix for the two
    /// structures the thread-safety audit found unsafe (the access allow
    /// list, the read-set tracker) rather than a lock around either.
    ///
    /// The session owns one dispatcher, which batches pending contexts into
    /// one store request. A Worker per root multiplies both substitution
    /// budgets and downloads of shared dependencies. The store now owns
    /// concurrency and deduplication across every root in the batch.
    fn begin(&self, question: &crate::host::Slow<'_>) -> Option<crate::host::Ticket> {
        let &crate::host::Slow::Realise(context) = question else {
            return None;
        };
        let fns = self.async_realise?;
        // The build thread reads `.drv`s this evaluation wrote. A failed flush
        // declines the ticket; the blocking route then asks again and reports
        // the poisoned host's failure.
        self.flush_derivations().ok()?;
        // Nothing to build: only opaque or drv-deep elements. The blocking
        // route answers this from the check phase alone, so a thread would
        // cost more than it hides.
        if !context
            .iter()
            .any(|e| matches!(e, crate::value2::ContextElem::Built { .. }))
        {
            return None;
        }
        let encoded = encode_realise_request(context);
        // Phase 1, on this thread. Whatever it settles without a build --
        // nothing to build, or a refusal with its class -- is filed ready
        // under the ticket, so `collect` delivers it as it would a build's
        // answer and the check never runs a second time.
        let (checked, check_ns) = crate::perf::timed(|| self.check_realise(fns, &encoded));
        let trace = crate::perf::replay_trace();
        if trace {
            // The check runs on the evaluation thread before the ticket
            // exists, so no question counter sees it; this line does.
            eprintln!(
                "ixe slow: Realise check_ns={check_ns} build={}",
                matches!(checked, Ok(Checked::Build))
            );
        }
        let settled = match checked {
            Ok(Checked::Build) => None,
            Ok(Checked::Nothing) => Some(Realised::Nothing),
            Err(refused) => Some(Realised::Failed(refused)),
        };
        let id = self
            .ifd
            .next
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .wrapping_add(1);
        let (tx, rx) = std::sync::mpsc::channel();
        if let Some(settled) = settled {
            drop(tx.send(settled));
        } else {
            let ready = tx.clone();
            if let Err(why) = self.enqueue_ifd(fns.build, encoded, tx) {
                let _ = ready.send(Realised::Failed(why));
            }
        }
        let mut inflight = self
            .ifd
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inflight.insert(id, rx);
        Some(crate::host::Ticket(id))
    }

    fn collect(&self, ticket: crate::host::Ticket, block: bool) -> Option<crate::host::SlowAnswer> {
        let fns = self.async_realise?;
        // The scheduler has advanced all runnable roots before collecting.
        // Releasing here lets the first wave share a Worker too.
        self.dispatch_ifd();
        let mut inflight = self
            .ifd
            .inflight
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let rx = inflight.remove(&ticket.0)?;
        // Out of the map before waiting, as `ThreadedHost::collect` does and
        // for its reason: holding the lock across a blocking `recv` would
        // make one slow build block every other collect.
        drop(inflight);
        let received = if block {
            Some(rx.recv().unwrap_or_else(|_| Realised::Failed(
                crate::host::StoreError::Failed("build dispatcher stopped before answering".into())
            )))
        } else {
            match rx.try_recv() {
                Ok(answer) => Some(answer),
                Err(std::sync::mpsc::TryRecvError::Empty) => None,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => Some(Realised::Failed(
                    crate::host::StoreError::Failed("build dispatcher stopped before answering".into())
                )),
            }
        };
        let Some(realised) = received else {
            // Only a live, empty channel reaches here. A stopped dispatcher
            // produces an explicit failure above, even for a polling collect.
            if !block {
                let mut inflight = self
                    .ifd
                    .inflight
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                inflight.insert(ticket.0, rx);
            }
            return None;
        };
        Some(crate::host::SlowAnswer::Realise(
            self.finish_realise(fns, realised),
        ))
    }

    fn read_file(&self, path: &crate::value2::PathValue) -> Result<String, String> {
        self.settle_writes_for(path)?;
        self.virtual_files.read_file_or(path, || {
            let Some(reads) = self.reads else {
                return crate::host::RealFs.read_file(path);
            };
            // With an accessor the read goes through it, which for cppnix is
            // `rootFS` -- so `pure-eval` and `restrict-eval` are enforced
            // there and their `RestrictedPathError` text comes back as the
            // error. The `RealFs` branch above is the standalone embedding
            // and cannot do either (ENG-12792).
            ask_about_path(reads.read_file, self.vtable.ctx, path)
        })
    }

    fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
        self.settle_writes_for(path)?;
        self.virtual_files.read_file_bytes_or(path, || {
            let Some(reads) = self.reads else {
                return crate::host::RealFs.read_file_bytes(path);
            };
            // The same hook as `read_file`: the embedder already answers in
            // bytes, and `text_of` on that answer was where `hashFile`'s
            // digest went wrong (ENG-13146).
            ask_about_path_bytes(reads.read_file, self.vtable.ctx, path)
        })
    }

    fn read_dir(
        &self,
        path: &crate::value2::PathValue,
    ) -> Result<Vec<(String, crate::host::FileType)>, String> {
        self.settle_writes_for(path)?;
        let Some(reads) = self.reads else {
            return crate::host::RealFs.read_dir(path);
        };
        let accessor_path = path.accessor_path();
        let (r, r_len) = bytes_of(path.root.wire_name());
        let (p, len) = bytes_of(accessor_path);
        let (rc, bytes) = ask_embedder!(reads.read_dir, self.vtable.ctx, r, r_len, p, len);
        if rc != IXE_OK {
            return Err(text_of(bytes));
        }
        decode_dir_entries(&bytes)
    }

    fn path_exists_checked(&self, path: &crate::value2::PathValue) -> Result<bool, String> {
        self.settle_writes_for(path)?;
        self.virtual_files.path_exists_checked_or(path, || {
            let Some(reads) = self.reads else {
                return crate::host::RealFs.path_exists_checked(path);
            };
            match ask_about_path(reads.path_exists, self.vtable.ctx, path)?.as_str() {
                "1" => Ok(true),
                "0" => Ok(false),
                other => Err(format!("pathExists answer is neither 0 nor 1: {other:?}")),
            }
        })
    }

    fn dir_exists_checked(&self, path: &crate::value2::PathValue) -> Result<bool, String> {
        self.settle_writes_for(path)?;
        self.virtual_files.dir_exists_checked_or(path, || {
            let Some(reads) = self.reads else {
                return crate::host::RealFs.dir_exists_checked(path);
            };
            match ask_about_path(reads.dir_exists, self.vtable.ctx, path)?.as_str() {
                "1" => Ok(true),
                "0" => Ok(false),
                other => Err(format!(
                    "directory-existence answer is neither 0 nor 1: {other:?}"
                )),
            }
        })
    }

    fn file_type(
        &self,
        path: &crate::value2::PathValue,
    ) -> Result<Option<crate::host::FileType>, String> {
        self.settle_writes_for(path)?;
        self.virtual_files.file_type_or(path, || {
            let Some(reads) = self.reads else {
                return crate::host::RealFs.file_type(path);
            };
            decode_maybe_file_type(&ask_about_path(reads.file_type, self.vtable.ctx, path)?)
        })
    }

    fn file_type_resolved(
        &self,
        path: &crate::value2::PathValue,
    ) -> Result<crate::host::FileType, String> {
        self.settle_writes_for(path)?;
        self.virtual_files.file_type_resolved_or(path, || {
            let Some(reads) = self.reads else {
                return crate::host::RealFs.file_type_resolved(path);
            };
            decode_file_type(&ask_about_path(
                reads.file_type_resolved,
                self.vtable.ctx,
                path,
            )?)
        })
    }
}

/// The answer bytes as a string. Lossy, because a hook that answered with
/// invalid UTF-8 has still said something and dropping it would leave the
/// evaluator reporting a failure with no text.
fn text_of(bytes: Vec<u8>) -> String {
    String::from_utf8_lossy(&bytes).into_owned()
}

/// The first `at_most` bytes of `text`, cut back to a character boundary so
/// the slice is valid (`text.get(..n)` is `None` inside a multi-byte
/// character, and a trace that fell back to the whole text on that would
/// print an unbounded line).
fn head(text: &str, at_most: usize) -> &str {
    let mut cut = at_most.min(text.len());
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    &text[..cut]
}

/// The store object `path` names, when it lies in the evaluator's store
/// directory: `/nix/store/<hash>-<name>` for `/nix/store/<hash>-<name>/x`.
/// `None` for a path anywhere else and when no store directory is set.
fn store_object_of(path: &str) -> Option<&str> {
    let store_dir = crate::eval::store_dir()?;
    let rest = path.strip_prefix(store_dir)?.strip_prefix('/')?;
    let object_len = rest.find('/').unwrap_or(rest.len());
    if object_len == 0 {
        return None;
    }
    Some(&path[..store_dir.len() + 1 + object_len])
}

/// A `findFile` success: `root NUL path`, or `root NUL path NUL contents`
/// when the embedder resolved into an accessor this evaluator cannot read
/// and hands the bytes over instead. Contents only make sense for an ambient
/// path -- a mounted root is a real accessor -- and are refused with one.
fn decode_find_file_answer(
    bytes: &[u8],
) -> Result<(crate::value2::PathValue, Option<String>), String> {
    let mut fields = bytes.splitn(3, |byte| *byte == 0);
    let (Some(root), Some(path)) = (fields.next(), fields.next()) else {
        return Err("findFile answer has no root separator".to_owned());
    };
    let root =
        std::str::from_utf8(root).map_err(|_| "findFile answer root is not UTF-8".to_owned())?;
    let path =
        std::str::from_utf8(path).map_err(|_| "findFile answer path is not UTF-8".to_owned())?;
    let rooted = crate::value2::PathValue::from_wire(root, path)?;
    let contents = fields
        .next()
        .map(|c| {
            std::str::from_utf8(c)
                .map(str::to_owned)
                .map_err(|_| "findFile answer contents are not UTF-8".to_owned())
        })
        .transpose()?;
    if contents.is_some() && !root.is_empty() {
        return Err(format!(
            "findFile answer carries contents for a mounted path under '{root}', which is a real accessor"
        ));
    }
    Ok((rooted, contents))
}

fn decode_imported_source(bytes: &[u8]) -> Result<crate::host::ImportedSource, String> {
    // A source root is empty or absolute, so this tag cannot be a source
    // response. Preserve the existing source wire shape verbatim.
    if let Some(bytes) = bytes.strip_prefix(b"derivation\0") {
        let value: serde_json::Value = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid imported derivation: {error}"))?;
        let object = value.as_object()
            .ok_or_else(|| "imported derivation is not an object".to_owned())?;
        if object.len() != 3 {
            return Err("imported derivation must contain path, name and outputs".to_owned());
        }
        let text = |key: &str| -> Result<String, String> {
            object.get(key).and_then(serde_json::Value::as_str).map(str::to_owned)
                .ok_or_else(|| format!("imported derivation {key} is not a string"))
        };
        let path = text("path")?;
        crate::value2::PathValue::from_wire("", &path)?;
        let outputs = object.get("outputs").and_then(serde_json::Value::as_object)
            .ok_or_else(|| "imported derivation outputs is not an object".to_owned())?
            .iter().map(|(name, value)| {
                value.as_str().map(|path| (name.clone(), path.to_owned()))
                    .ok_or_else(|| format!("imported derivation output {name} is not a string"))
            }).collect::<Result<_, _>>()?;
        return Ok(crate::host::ImportedSource::Derivation(crate::host::ImportedDerivation {
            path,
            name: text("name")?,
            outputs,
        }));
    }
    let Some(first) = bytes.iter().position(|byte| *byte == 0) else {
        return Err("import answer has no root separator".to_owned());
    };
    let Some(second_relative) = bytes
        .get(first + 1..)
        .and_then(|tail| tail.iter().position(|byte| *byte == 0))
    else {
        return Err("import answer has no path separator".to_owned());
    };
    let second = first + 1 + second_relative;
    let root = std::str::from_utf8(bytes.get(..first).unwrap_or_default())
        .map_err(|_| "import answer root is not UTF-8".to_owned())?;
    let path = std::str::from_utf8(bytes.get(first + 1..second).unwrap_or_default())
        .map_err(|_| "import answer path is not UTF-8".to_owned())?;
    let text = std::str::from_utf8(bytes.get(second + 1..).unwrap_or_default())
        .map_err(|_| "imported source is not UTF-8".to_owned())?;
    Ok(crate::host::ImportedSource::Nix {
        path: crate::value2::PathValue::from_wire(root, path)?,
        text: text.to_owned(),
    })
}

/// The two-outcome store contract: zero is the answer, anything else is the
/// failure text.
fn store_answer(rc: i32, answer: Vec<u8>) -> Result<String, crate::host::StoreError> {
    let answer = text_of(answer);
    if rc == IXE_OK {
        Ok(answer)
    } else {
        Err(crate::host::StoreError::Failed(answer))
    }
}

/// The three-outcome store contract, which the tree fetchers and the flake
/// lock use: zero is the answer, `IXE_ERR_UNIMPLEMENTED` is "this embedder
/// will not serve this", anything else is a failure. The second is separate
/// because a decline has to surface as unimplemented rather than as a Nix
/// error -- as an error it would score a mismatch against a cpp arm that
/// answers fine.
///
/// The `bool` in the success case is unused by both callers today and is
/// carried so the shape stays a `Result` the `?` operator threads.
fn three_way(rc: i32, answer: Vec<u8>) -> Result<(String, bool), crate::host::StoreError> {
    let answer = text_of(answer);
    match rc {
        IXE_OK => Ok((answer, true)),
        IXE_ERR_UNIMPLEMENTED => Err(crate::host::StoreError::Unsupported(answer)),
        _ => Err(crate::host::StoreError::Failed(answer)),
    }
}

/// Ask the embedder a one-path question whose answer is a string, and copy
/// the answer out of its buffer before returning.
fn ask_about_path(
    f: ReadFileFn,
    ctx: *mut c_void,
    path: &crate::value2::PathValue,
) -> Result<String, String> {
    let accessor_path = path.accessor_path();
    let (r, r_len) = bytes_of(path.root.wire_name());
    let (p, len) = bytes_of(accessor_path);
    let (rc, answer) = ask_embedder!(f, ctx, r, r_len, p, len);
    let answer = text_of(answer);
    if rc == IXE_OK {
        Ok(answer)
    } else {
        Err(answer)
    }
}

/// [`ask_about_path`] without the text decoding on success: the raw answer
/// bytes, for `Host::read_file_bytes`. The failure side stays text, because
/// an error is a message however the contents were going to be read.
fn ask_about_path_bytes(
    f: ReadFileFn,
    ctx: *mut c_void,
    path: &crate::value2::PathValue,
) -> Result<Vec<u8>, String> {
    let accessor_path = path.accessor_path();
    let (r, r_len) = bytes_of(path.root.wire_name());
    let (p, len) = bytes_of(accessor_path);
    let (rc, answer) = ask_embedder!(f, ctx, r, r_len, p, len);
    if rc == IXE_OK {
        Ok(answer)
    } else {
        Err(text_of(answer))
    }
}

/// `name\0type\0` per entry, the same pair shape `decode_entries` uses for
/// the search path and unambiguous for the same reason: neither half can
/// contain a NUL.
///
/// A trailing partial pair is a malformed answer and is reported as one. A
/// dropped entry would be a directory that quietly lost a file, which
/// `builtins.readDir` callers turn into a module that silently stops being
/// imported.
fn decode_dir_entries(buf: &[u8]) -> Result<Vec<(String, crate::host::FileType)>, String> {
    let mut out = Vec::new();
    let mut fields = buf.split(|b| *b == 0);
    while let (Some(name), Some(kind)) = (fields.next(), fields.next()) {
        // The split after a trailing NUL yields one empty tail field, which
        // is the end of a well-formed buffer rather than another entry.
        if name.is_empty() && kind.is_empty() && fields.next().is_none() {
            break;
        }
        let name = std::str::from_utf8(name)
            .map_err(|_| "a directory entry name is not valid UTF-8".to_owned())?;
        let kind = std::str::from_utf8(kind)
            .map_err(|_| "a directory entry type is not valid UTF-8".to_owned())?;
        out.push((name.to_owned(), decode_file_type(kind)?));
    }
    Ok(out)
}

/// cppnix's four spellings, which are corpus-visible through
/// `builtins.readDir` and `builtins.readFileType` (`primops.cc:2480`). A
/// fifth is a malformed answer rather than an `Unknown`, because `Unknown` is
/// a real answer cppnix gives and swallowing a typo into it would turn a
/// broken hook into a plausible-looking directory listing.
fn decode_file_type(name: &str) -> Result<crate::host::FileType, String> {
    match name {
        "regular" => Ok(crate::host::FileType::Regular),
        "directory" => Ok(crate::host::FileType::Directory),
        "symlink" => Ok(crate::host::FileType::Symlink),
        "unknown" => Ok(crate::host::FileType::Unknown),
        other => Err(format!("'{other}' is not a file type this evaluator knows")),
    }
}

/// The four spellings plus `absent`, which is cppnix's `maybeLstat` answering
/// nullopt. Only the non-resolving hook may say it; see [`Host::file_type`]
/// for why absence is a value there and a failure everywhere else.
///
/// `builtins.readDir` keeps [`decode_file_type`] and so cannot: an entry the
/// directory listed and the accessor cannot see is a broken hook, not a file
/// with no type.
fn decode_maybe_file_type(name: &str) -> Result<Option<crate::host::FileType>, String> {
    if name == "absent" {
        return Ok(None);
    }
    decode_file_type(name).map(Some)
}

/// This evaluation's perf counters, as one line of `key=value` pairs.
///
/// The caller owns the string and frees it with [`ixe_string_free`].
///
/// # Why the embedder prints and the VM does not
///
/// The evaluator performs no IO, which is the property that makes a recorded
/// read set complete and the memo table sound. A perf module that decided for
/// itself whether to print, by reading an environment variable or opening a
/// file, would be the same defect as `getEnv` reaching `std::env` behind
/// `Host`'s back -- a bug this crate has already had once and fixed. So the
/// counters accumulate and this hands them over; whether anyone looks is the
/// embedder's business.
///
/// Nothing here is reachable from a Nix program, and none of it is in the memo
/// key. Two evaluations that differ only in their counters are the same
/// evaluation.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_perf_snapshot() -> *mut c_char {
    // Null when the counters were compiled out, so the embedder records
    // nothing and the stats block is absent rather than a page of zeros.
    //
    // The block being absent when the rust arm did not run was designed in
    // from the start; this case was not, and it is the same ambiguity one
    // layer down. Built `--no-default-features`, every counter is a no-op and
    // this rendered `compiles=0 questions=0 interns=0 ...`, which reads as
    // "the evaluator did no work" rather than "this build cannot count".
    // Found by finally running the on/off pair for real (ENG-12859).
    if !cfg!(feature = "perf") {
        return std::ptr::null_mut();
    }
    let line = crate::perf::render(&crate::perf::counters());
    match CString::new(line) {
        Ok(s) => s.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Zero the counters. An embedder measuring one evaluation calls this first:
/// without it a second evaluation in one process reports the sum of both,
/// which is a plausible-looking wrong number rather than an obvious one.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_perf_reset() {
    crate::perf::reset();
}

/// Free a string returned through `out` by ixe_eval_expr.
///
/// # Safety
/// `s` must be a pointer previously returned via ixe_eval_expr's out
/// parameter, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_string_free(s: *mut c_char) {
    if !s.is_null() {
        // SAFETY: ownership round-trip of a CString::into_raw pointer.
        drop(unsafe { CString::from_raw(s) });
    }
}

// -- the handle table -------------------------------------------------------
//
// `ixe_eval_expr` above answers one question -- "render this whole
// expression" -- and cannot answer any other. `nix eval` asks three more:
// select an attribute path, choose an output format, and do the first without
// forcing what it did not select. Rendering a whole value and then picking
// text out of it would force every sibling, which is the property the Nix
// language is built around, so the value itself has to cross.
//
// It crosses as a handle: an opaque integer naming a lazy cell (a `Slot`)
// inside one session's table. The alternative, handing out raw pointers into
// the value graph, would put `Rc` lifetimes in the embedder's hands, and the
// embedder is C++.
//
// Ownership, in one place:
//
//   * A session owns a VM, a handle table, and the last error message. It is
//     created with `ixe_session_new` and destroyed with `ixe_session_free`.
//   * Every call that yields a handle transfers ownership of that handle to
//     the caller. Free it with `ixe_handle_free`, or let `ixe_session_free`
//     take all of them at once.
//   * A handle belongs to the session that made it. Handles carry that
//     session's serial in their high bits, so one used with another session
//     is rejected rather than silently naming a different value.
//   * Handles are never zero; zero is the "no handle" value.
//   * Every string written through an out parameter is owned by the caller
//     and freed with `ixe_string_free`, exactly as `ixe_eval_expr`'s is.

use crate::session::RenderMode;
use crate::value2::{Slot, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// A value was asked for by a name or an index it does not have. Separate
/// from `IXE_ERR_EVAL` because cppnix reports a missing attribute in a
/// selection path with its own message and the embedder builds it.
const IXE_ERR_MISSING: i32 = 7;

/// What `ixe_value_type` reports. Negative values are not Nix types: they say
/// the question could not be asked.
const IXE_TYPE_UNKNOWN_HANDLE: i32 = -1;
const IXE_TYPE_UNFORCED: i32 = -2;
const IXE_TYPE_INT: i32 = 0;
const IXE_TYPE_FLOAT: i32 = 1;
const IXE_TYPE_BOOL: i32 = 2;
const IXE_TYPE_NULL: i32 = 3;
const IXE_TYPE_STRING: i32 = 4;
const IXE_TYPE_PATH: i32 = 5;
const IXE_TYPE_LIST: i32 = 6;
const IXE_TYPE_ATTRS: i32 = 7;
const IXE_TYPE_FUNCTION: i32 = 8;

/// Render modes, kept in step with ixe.h.
const IXE_RENDER_PLAIN: i32 = 0;
const IXE_RENDER_JSON: i32 = 1;
const IXE_RENDER_RAW: i32 = 2;
const IXE_RENDER_VALUE_PRINTER: i32 = 3;
const IXE_RENDER_XML: i32 = 4;
const IXE_RENDER_PLAIN_LAZY: i32 = 5;

/// Handle layout: the session's serial on top, the table index underneath.
/// Splitting the word is what makes "handle from another session" a detected
/// error rather than a silent hit on an unrelated value, which is the failure
/// this would otherwise have: two sessions both start their indices at 1.
const HANDLE_INDEX_BITS: u32 = 40;
const HANDLE_INDEX_MASK: u64 = (1u64 << HANDLE_INDEX_BITS) - 1;

/// Serial of the next session. Wraps after 2^24 sessions in one process, at
/// which point a stale handle from the first could be accepted by the
/// 16,777,217th; a `nix` invocation makes one session, so the wrap is a
/// theoretical note rather than a live hazard.
static NEXT_SESSION_SERIAL: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// One embedder's evaluation: a VM, the values it produced, and why the last
/// call failed.
///
/// The caches are built per `ixe_session_eval` call rather than held here,
/// because they borrow the on-disk store and a session that owned both would
/// be self-referential. A command evaluates once, so this costs one store
/// open per command, which is three path joins (see `session::evaluate_once`).
/// Explicit owner of decoded witness reuse, independent of every VM and host.
/// Like evaluator sessions, this Rc-based handle is confined to its creating thread.
pub struct IxeEvalCache {
    retained: crate::readset::retained::Shared,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct IxeEvalCacheStats {
    pub memory_hits: u64,
    pub disk_loads: u64,
    pub retained_bytes: u64,
    pub entries: u64,
    pub evictions: u64,
}

pub struct IxeSession {
    retained: Option<crate::readset::retained::Shared>,
    vm: crate::vm::Vm,
    source_root: crate::value2::Root,
    /// The question this session is answering, while it is answering one.
    ///
    /// Present between `ixe_session_eval_question` and
    /// `ixe_session_question_answer`, which is exactly the window in which
    /// the embedder's walk and render are producing the answer that gets
    /// memoised. While it is present every force in this session goes through
    /// its recorder, which is what makes the recorded read set cover the walk
    /// and not just the first evaluation.
    memo: Option<MemoScope>,
    serial: u64,
    next_index: u64,
    handles: BTreeMap<u64, Slot>,
    /// The message belonging to the most recent non-zero status, and where in
    /// the source it happened when it happened somewhere.
    ///
    /// One field and not two, so the two cannot come apart. A position beside
    /// the message in its own slot would go stale the moment a failure path
    /// set only the message -- and the reader cannot tell a stale position
    /// from a right one, because both name a real line of a real file.
    last_error: Option<SessionError>,
    /// The token for `last_error`, when it was a refusal.
    ///
    /// Beside the message rather than inside it: the message is prose a user
    /// reads and is free to be reworded, while this is what a census groups
    /// by, and putting the token in the text would make every reword a
    /// silent reset of the population. Cleared with the message.
    last_refusal: Option<crate::refusal::RefusalToken>,
    /// The memo row the last `ixe_session_eval_question` served an answer
    /// from, for `ixe_session_question_reject`. Cleared by the next question.
    served: Option<(crate::readset::EvalId, ix_kernel::Key)>,
    /// The question in flight, between `ixe_session_eval_question` and the
    /// call that ends it (`ixe_session_question_answer`, `_reject`, a served
    /// answer, or a failure), for the two applications the key names and
    /// therefore allows while it is open: [`ixe_question_apply`] and
    /// [`ixe_auto_call`]. Every terminal path clears it, so a stale one
    /// cannot be applied after its question is over.
    question: Option<InFlightQuestion>,
    /// Damaged-store complaints, drained by the embedder.
    warnings: Vec<String>,
    /// Everything outside this crate that this session can ask, taken once
    /// from the vtable it was created with.
    ///
    /// Per session and not per process, which is the property the whole hook
    /// surface was rebuilt for: two sessions in one process answer out of
    /// two vtables, and neither can move the other's. `Clone`, so the
    /// recording host in `memo` owns a clone rather than borrowing this field
    /// -- a borrow would make the session self-referential. Clones share the
    /// in-flight build table (see [`EmbedderHost`]), so which clone begins a
    /// build and which collects it does not matter.
    host: EmbedderHost,
}

/// A session's pending failure: what to say and where it happened.
struct SessionError {
    message: String,
    pos: Option<crate::vm::SrcPos>,
}

impl From<String> for SessionError {
    /// A failure with no source behind it, which is every one the C ABI
    /// itself raises: a bad call is the embedder's mistake, not a line of the
    /// user's Nix.
    fn from(message: String) -> SessionError {
        SessionError { message, pos: None }
    }
}

impl IxeSession {
    fn new(host: EmbedderHost) -> Self {
        let serial =
            NEXT_SESSION_SERIAL.fetch_add(1, std::sync::atomic::Ordering::Relaxed) & 0xFF_FFFF;
        // The compile cache is the session's for its whole life, so its store
        // opens here rather than per evaluation: a session evaluates many
        // expressions and every one of them imports through it. The result
        // memo (`QuestionCache`) still opens per question, over the same
        // directory.
        let (store, unopened) =
            crate::session::open_cache(eval_cache_dir().as_deref(), cache_max_bytes());
        let mut vm = crate::vm::Vm::with_modules(
            settings_for(&host),
            store.map_or_else(
                crate::modcache::ModuleCache::in_memory,
                crate::modcache::ModuleCache::persistent,
            ),
        );
        if let Some(interrupt) = host.interrupt() {
            vm.set_interrupt(interrupt);
        }
        IxeSession {
            retained: None,
            vm,
            source_root: crate::value2::Root::Ambient,
            memo: None,
            question: None,
            serial,
            next_index: 1,
            handles: BTreeMap::new(),
            last_error: None,
            last_refusal: None,
            served: None,
            warnings: unopened
                .into_iter()
                .map(|complaint| complaint.to_string())
                .collect(),
            host,
        }
    }

    fn insert(&mut self, slot: Slot) -> u64 {
        let index = self.next_index;
        self.next_index += 1;
        self.handles.insert(index, slot);
        (self.serial << HANDLE_INDEX_BITS) | index
    }

    fn get(&self, handle: u64) -> Option<&Slot> {
        if handle >> HANDLE_INDEX_BITS != self.serial {
            return None;
        }
        self.handles.get(&(handle & HANDLE_INDEX_MASK))
    }

    fn fail(&mut self, error: &crate::eval::EvalError) -> i32 {
        let (status, message) = match error {
            EvalError::Unimplemented(refusal) => (IXE_ERR_UNIMPLEMENTED, refusal.detail.clone()),
            EvalError::Parse(message) => (IXE_ERR_PARSE, message.clone()),
            EvalError::Eval(kind, message, _) => (status_of_kind(*kind), message.clone()),
        };
        self.last_refusal = match error {
            EvalError::Unimplemented(refusal) => Some(refusal.token),
            _ => None,
        };
        self.last_error = Some(SessionError {
            message,
            pos: error.pos().cloned(),
        });
        status
    }

    fn bad(&mut self, message: impl Into<String>) -> i32 {
        self.last_refusal = None;
        self.last_error = Some(SessionError::from(message.into()));
        IXE_ERR_BADCALL
    }
}

/// What the question in flight allows the embedder to apply.
///
/// Both halves are in the memo key (`Selection::apply`, `Selection::auto_args`)
/// and both are built from the key's own bytes, which is what makes applying
/// them sound where `ixe_apply` on an injected value would not be.
struct InFlightQuestion {
    /// The exact typed request hashed into the memo key.
    question: crate::session::Question,
    /// The `IXE_QUESTION_*` kind the question was asked as. Each accessor
    /// that reads a value "under the question in flight" (`ixe_derivation_set`
    /// for derivation sets, `ixe_flake_document`) checks it: a document filed under
    /// a select question's key would be served for that question later.
    kind: i32,
    /// For `IXE_QUESTION_FLAKE_DOCUMENT`: the directory the file's path
    /// literals resolved against, under the file's root. The document spells
    /// every path relative to it (`flake_doc::flake_document`).
    flake_dir: Option<crate::value2::PathValue>,
    apply: Option<crate::session::Apply>,
    /// `--arg`/`--argstr` by name, each a cell shared by every formal it is
    /// bound to: cppnix builds one `Bindings` and every auto-call reads from
    /// it, so an argument's expression runs at most once.
    auto_args: Vec<(String, Slot)>,
}

/// One question in flight: where it is filed, what it is filed under, and the
/// recording that will become its read set.
struct MemoScope {
    cache: crate::session::QuestionCache,
    identity: crate::readset::EvalId,
    /// Live while the embedder produces the answer.
    recorder: crate::readset::RecordingHost<EmbedderHost>,
    /// The evaluation memo every force of this question shares
    /// (`eval::Job::memo`). One question is one recording, and the embedder
    /// answers it through many `ixe_force` calls; with one memo per force,
    /// each re-asked the directories, imports and store paths the last one
    /// had (measured: 15.6 listings per directory, `goals/rust-eval.md`
    /// 2026-09-04). Dies with the scope, which is what makes sharing sound.
    memo: crate::eval::JobMemo,
    /// The answer the cache gave, when this occasion is a sampled check of
    /// it. The check runs quiet and the served answer is what the caller is
    /// told to use, so the two halves agree with `session::evaluate`.
    verifying: Option<crate::readset::EvalResult>,
}

/// Refuse to build a value into a session that is in the middle of answering
/// a question.
///
/// # This is the invariant the flake memo key rests on
///
/// Between [`ixe_session_eval_question`] and [`ixe_session_question_answer`]
/// every force goes through the recorder (`machine_and_host`), and what the
/// recorder collects becomes the read set of a row filed under the identity
/// that question was keyed with. A value injected here is in no key: the
/// module digest describes the source, and the argument fingerprint describes
/// the arguments the question was *told about*. So an injected value's reads
/// would be recorded into a row that does not name it, and the next run with
/// a different injected value would replay those reads, get the same answers,
/// and be served this run's result.
///
/// That is precisely how the flake path would have been unsound, and it is
/// why the arguments now cross on the question call rather than through
/// `ixe_apply` afterwards: one list both keys and builds, and there is no
/// arrangement of embedder code that keys on one thing and applies another.
/// ENG-12915.
///
/// Enforced rather than documented because the previous version of this rule
/// *was* documented -- `mayBeMemoised` in `src/libcmd/rust-eval-session.cc`
/// refused to ask a question at all when the evaluand had arguments -- and a
/// rule living in the embedder is a rule the next embedder does not have.
fn refuse_injection_during_a_question(session: &mut IxeSession, what: &str) -> Option<i32> {
    session.memo.as_ref()?;
    Some(session.bad(format!(
        "{what} while a question is in flight. A value built this way is not \
         in the memo key, but every force after it is recorded into the read \
         set of a row that is -- so the row would later be served for a \
         different value. Pass it in the argument list of \
         ixe_session_eval_question, which keys on it. ENG-12915."
    )))
}

/// Split a session into the machine and the host to drive it with.
///
/// A method returning the host cannot be called while `&mut session.vm` is
/// held, so the split is written out. Which host it is matters: a force that
/// went through `RealFs` while a question was in flight would leave its reads
/// out of the read set, and the memo would then serve that answer again
/// without ever re-checking the files it depended on. That is a wrong answer
/// rather than a slow one, and it would appear only on the second run.
///
/// The memo travels with the recorder for the same reason: a question's
/// forces share the scope's memo, and a drive outside any question gets
/// `fresh`, which the caller made for this one drive and drops with it.
fn machine_and_host<'s>(
    session: &'s mut IxeSession,
    fresh: &'s mut crate::eval::JobMemo,
) -> (
    &'s mut crate::vm::Vm,
    &'s dyn Host,
    &'s mut crate::eval::JobMemo,
) {
    let IxeSession { vm, memo, host, .. } = session;
    let (host, memo): (&dyn Host, &mut crate::eval::JobMemo) = match memo {
        Some(scope) => (&scope.recorder, &mut scope.memo),
        None => (host, fresh),
    };
    (vm, host, memo)
}

/// Borrow a session from a caller's pointer, or return `IXE_ERR_BADCALL`.
/// A null session has nowhere to record a message, which is why this is the
/// one failure with no retrievable text.
/// The C status an evaluation error class reports. One table, used by the
/// expression entry point and by every session failure, so a new class cannot
/// be mapped in one and forgotten in the other.
fn status_of_kind(kind: ErrKind) -> i32 {
    match kind {
        ErrKind::Eval => IXE_ERR_EVAL,
        ErrKind::Thrown => IXE_ERR_THROWN,
        ErrKind::Assertion => IXE_ERR_ASSERT,
        ErrKind::ImportFromDerivation => IXE_ERR_IFD,
        ErrKind::MissingArgument => IXE_ERR_MISSING_ARGUMENT,
    }
}

macro_rules! session {
    ($ptr:expr) => {
        match unsafe { $ptr.as_mut() } {
            Some(session) => session,
            None => return IXE_ERR_BADCALL,
        }
    };
}

mod command;
mod flake_check;
mod flake_show;
mod persistent;
mod search;
mod source_position;

/// The settings this session evaluates under: the process configuration,
/// with the one field that is not process state taken from the host.
///
/// `path_reads` is a property of who answers a file read, which is the host
/// and not the process. It reaches [`crate::eval::Settings`] because it
/// changes what `pure-eval` answers and therefore has to be in the memo key
/// (ENG-12792); it reaches it *here* because a session is the first place
/// that knows both halves.
fn settings_for(host: &EmbedderHost) -> crate::eval::Settings {
    let mut settings = crate::eval::Settings::current();
    settings.path_reads = host.path_reads();
    settings
}

fn require_host_cache_identity(
    host: &EmbedderHost,
    settings: &crate::eval::Settings,
    persistent: bool,
) -> Result<(), &'static str> {
    if persistent
        && host.vtable.has_callbacks()
        && !settings
            .host_build_identity
            .as_deref()
            .is_some_and(|identity| !identity.is_empty())
    {
        return Err("persistent external-host caching requires a host build identity");
    }
    Ok(())
}

fn require_session_cache_identity(session: &IxeSession) -> Result<(), &'static str> {
    require_host_cache_identity(
        &session.host,
        session.vm.settings(),
        eval_cache_dir().is_some() || session.vm.modules().store().is_some(),
    )
}

/// Create an evaluation session that answers through `host`.
///
/// `host` is copied, so the caller may free the struct as soon as this
/// returns -- but everything it points at, including `ctx` and any buffer a
/// hook writes into, must outlive the session.
///
/// Returns null for a malformed vtable or persistent external-host caching
/// without a build identity; [`ixe_take_setting_conflict`] carries the reason. A null `host` is also refused: a session with no host
/// is not a useful object, and accepting one would put the "which embedder
/// answers this" question back where it was.
///
/// # Safety
/// `host` must point to a readable [`IxeHostVtable`], or be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_session_new(host: *const IxeHostVtable) -> *mut IxeSession {
    // SAFETY: caller contract above; read once and copied.
    let Some(vtable) = (unsafe { host.as_ref() }).copied() else {
        set_last_setting_conflict(
            "rust-eval: ixe_session_new needs a host vtable and was given null".to_owned(),
        );
        return std::ptr::null_mut();
    };
    match EmbedderHost::new(vtable) {
        Ok(host) => {
            if let Err(why) =
                require_host_cache_identity(&host, &settings_for(&host), eval_cache_dir().is_some())
            {
                set_last_setting_conflict(why.to_owned());
                return std::ptr::null_mut();
            }
            Box::into_raw(Box::new(IxeSession::new(host)))
        }
        Err(why) => {
            set_last_setting_conflict(why.to_owned());
            std::ptr::null_mut()
        }
    }
}

/// A session with a host that answers nothing, for tests about everything
/// else.
///
/// The all-null vtable is the same object the standalone embedding gets, so a
/// test written against it exercises the fall-through to
/// [`crate::host::RealFs`] rather than a test-only code path.
/// An expression this crate refuses, named once.
///
/// Four tests need "something that refuses", and the subject keeps being
/// implemented out from under them: `/tmp/${"x"}` (path interpolation), then
/// `~/nope` (home paths), then `1_000` (underscore digit separators,
/// ENG-13119), then `1 |> builtins.length` (pipe operators). Each landing
/// meant finding every copy, and the third one was found by three tests
/// failing rather than by the comment that told the next person to look. One
/// name means the next landing is an edit here.
///
/// This was `REFUSED_EXPRESSION` while the compiler still had reachable
/// refusals; the pipe operators were the last, so the current subject is a
/// runtime one: `genericClosure` refuses list-typed keys by name because
/// cppnix's comparison switch has no case for them. The token must stay
/// different from `store-unavailable`, which
/// `two_refusal_kinds_report_two_tokens` uses as its second row.
#[cfg(test)]
const REFUSED_EXPRESSION: &str = "builtins.genericClosure { startSet = [ { key = [ 1 ]; } { key = [ 2 ]; } ]; \
     operator = x: [ ]; }";

/// The token [`REFUSED_EXPRESSION`] refuses with.
#[cfg(test)]
const REFUSED_EXPRESSION_TOKEN: &str = "unordered-comparison";

#[cfg(test)]
fn session_without_embedder() -> *mut IxeSession {
    let vtable = IxeHostVtable::empty();
    // SAFETY: points at a live local for the duration of the call, which is
    // all `ixe_session_new` needs -- it copies the struct.
    unsafe { ixe_session_new(&raw const vtable) }
}

/// Create an explicitly bounded witness owner. Zero in either budget disables
/// retention. No VM values, handles, host pointers, or mutable inputs are retained.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_eval_cache_new(max_bytes: u64, max_entries: usize) -> *mut IxeEvalCache {
    Box::into_raw(Box::new(IxeEvalCache {
        retained: std::rc::Rc::new(std::cell::RefCell::new(
            crate::readset::retained::Retained::new(max_bytes, max_entries),
        )),
    }))
}

/// Release the owner's reference. Attached sessions retain their own reference.
/// # Safety
/// `cache` must be null or a live handle from ixe_eval_cache_new, on its creating
/// thread. A non-null handle must be freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_eval_cache_free(cache: *mut IxeEvalCache) {
    if !cache.is_null() {
        // SAFETY: ownership follows the caller contract above.
        drop(unsafe { Box::from_raw(cache) });
    }
}

/// Snapshot counters and accounted retained payload bytes; not process RSS.
/// # Safety
/// A non-null `cache` must be live on its creating thread; `output` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_eval_cache_stats(
    cache: *const IxeEvalCache,
    output: *mut IxeEvalCacheStats,
) -> i32 {
    if output.is_null() {
        return IXE_ERR_BADCALL;
    }
    // SAFETY: output is writable under the caller contract.
    unsafe {
        *output = IxeEvalCacheStats::default();
    }
    // SAFETY: a non-null cache follows the caller contract.
    let Some(cache) = (unsafe { cache.as_ref() }) else {
        return IXE_ERR_BADCALL;
    };
    let stats = cache.retained.borrow().stats();
    // SAFETY: output is writable under the caller contract.
    unsafe {
        *output = IxeEvalCacheStats {
            memory_hits: stats.memory_hits,
            disk_loads: stats.disk_loads,
            retained_bytes: stats.retained_bytes,
            entries: stats.entries,
            evictions: stats.evictions,
        };
    }
    IXE_OK
}

/// Attach shared witness ownership to an idle session; null detaches it.
/// # Safety
/// Both non-null pointers must be live on this thread. Attachment retains its
/// own reference, so the cache handle may subsequently be freed before the session.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_session_set_eval_cache(
    session: *mut IxeSession,
    cache: *const IxeEvalCache,
) -> i32 {
    let session = session!(session);
    if session.question.is_some() || session.memo.is_some() {
        return session.bad("cannot replace evaluation cache ownership during a question");
    }
    // SAFETY: non-null cache pointers follow the caller contract.
    session.retained = unsafe { cache.as_ref() }.map(|cache| cache.retained.clone());
    IXE_OK
}

/// Destroy a session and every handle it issued.
///
/// # Safety
/// `session` must come from `ixe_session_new` and not have been freed. Any
/// handle it issued is dangling afterwards.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_session_free(session: *mut IxeSession) {
    if !session.is_null() {
        // SAFETY: ownership round-trip of a Box::into_raw pointer.
        drop(unsafe { Box::from_raw(session) });
    }
}

/// How many refusal tokens exist.
///
/// With `ixe_refusal_token_at`, this is how a caller builds a histogram with a
/// denominator instead of one that only has rows for what it happened to see.
/// A census that counts only observed kinds cannot tell "no refusals of this
/// kind" from "this kind is not in my table", and the flip criterion is read
/// per kind, so the difference is the whole measurement.
///
/// It also keeps the vocabulary in one place. The C++ command layer raises
/// refusals of its own, before the evaluator is reached; those tokens are in
/// this same list rather than a second one maintained by hand, because two
/// lists drift the moment either side gains a kind.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_refusal_token_count() -> usize {
    crate::refusal::RefusalToken::ALL.len()
}

/// The name of token `index`, or null when `index` is out of range.
///
/// Static storage, as `ixe_session_refusal_token` returns: do not free it.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_refusal_token_at(index: usize) -> *const c_char {
    match crate::refusal::RefusalToken::ALL.get(index) {
        None => std::ptr::null(),
        Some(token) => token_cstr(*token),
    }
}

/// Which layer raises token `index`: 0 the evaluator, 1 the command layer, 2
/// either. Negative when `index` is out of range.
///
/// Lets a reader say "the evaluator refused nothing" without having to know
/// which name prefixes mean what, and makes moving a refusal between the two
/// layers a visible change rather than a silent reclassification.
#[unsafe(no_mangle)]
pub extern "C" fn ixe_refusal_token_raised_by(index: usize) -> i32 {
    use crate::refusal::RaisedBy;
    match crate::refusal::RefusalToken::ALL.get(index) {
        None => -1,
        Some(token) => match token.raised_by() {
            RaisedBy::Evaluator => 0,
            RaisedBy::CommandLayer => 1,
            RaisedBy::Either => 2,
            // 3 and not 2: a sentinel is not raised by anybody, and a
            // consumer checking "is this the command layer's" must not be
            // told yes for the name that means nobody said.
            RaisedBy::Sentinel => 3,
        },
    }
}

/// The refusal token for the most recent status, or null when the last
/// failure was not a refusal.
///
/// Static storage, not owned by the caller and not to be freed: a token is one
/// of a fixed set of names that live for the life of the process, which is
/// what lets a caller use it as a map key without copying. Unlike
/// `ixe_session_take_error` this does not clear, so it can be read before or
/// after the message, in either order.
///
/// Exists because the message is prose and the token is not. Counting
/// refusals across a fleet by slicing the message text made two refusals of
/// one kind look like two kinds whenever they interpolated different names,
/// and made rewording an error silently reset the census.
///
/// # Safety
/// `session` must be a live session pointer, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_session_refusal_token(session: *mut IxeSession) -> *const c_char {
    let Some(session) = (unsafe { session.as_mut() }) else {
        return std::ptr::null();
    };
    match session.last_refusal {
        None => std::ptr::null(),
        Some(token) => token_cstr(token),
    }
}

/// The NUL-terminated spelling of a token, with a lifetime of the process.
///
/// The `as_str` names are Rust string literals and so not NUL-terminated;
/// rather than allocate one per call and hand the caller a free obligation
/// for a value that never changes, intern them once here.
fn token_cstr(token: crate::refusal::RefusalToken) -> *const c_char {
    use std::collections::BTreeMap;
    use std::sync::{Mutex, OnceLock};
    static INTERNED: OnceLock<Mutex<BTreeMap<&'static str, &'static CStr>>> = OnceLock::new();
    let map = INTERNED.get_or_init(|| Mutex::new(BTreeMap::new()));
    let Ok(mut map) = map.lock() else {
        return std::ptr::null();
    };
    let name = token.as_str();
    let entry = map.entry(name).or_insert_with(|| {
        // Tokens are `[a-z0-9-]+` by `token_names_are_safe_as_a_bare_key`, so
        // this cannot fail; leaking one small allocation per distinct token
        // for the life of the process is what buys the caller a pointer with
        // no ownership attached.
        match CString::new(name) {
            Ok(owned) => Box::leak(owned.into_boxed_c_str()),
            Err(_) => c"unrecorded",
        }
    });
    entry.as_ptr()
}

/// Take the message belonging to the most recent non-zero status from this
/// session, or null if there is none. Ownership transfers; free with
/// `ixe_string_free`. Taking it clears it, so a message is never read twice
/// and attributed to the wrong call.
///
/// `out_pos` receives where the failure happened, or the "nowhere" value when
/// it has no place; see [`IxePos`]. It is taken here rather than through an
/// accessor of its own because a message and its position are one fact: two
/// calls could be made in either order, and the order that reads the position
/// after taking the message reads nothing, silently, in a shape whose output
/// still looks like an error message.
///
/// # Safety
/// `session` must be a live session pointer, or null; `out_pos`, when
/// non-null, must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_session_take_error(
    session: *mut IxeSession,
    out_pos: *mut IxePos,
) -> *mut c_char {
    // SAFETY: caller contract; out_pos is null or a writable slot.
    unsafe { write_pos(out_pos, None) };
    let Some(session) = (unsafe { session.as_mut() }) else {
        return std::ptr::null_mut();
    };
    let Some(SessionError { message, pos }) = session.last_error.take() else {
        return std::ptr::null_mut();
    };
    // SAFETY: caller contract; out_pos is null or a writable slot.
    unsafe { write_pos(out_pos, pos.as_ref()) };
    match CString::new(message.replace('\0', "\u{2400}")) {
        Ok(text) => text.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Take the next warning about a damaged cache entry, or null when there are
/// none left. Same ownership as `ixe_session_take_error`.
///
/// Warnings rather than errors: a cache entry that will not load is a slower
/// evaluation, not a wrong one, so it must not replace the answer -- but it
/// still has to reach somebody, and the embedder owns where that is.
///
/// # Safety
/// `session` must be a live session pointer, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_session_take_warning(session: *mut IxeSession) -> *mut c_char {
    let Some(session) = (unsafe { session.as_mut() }) else {
        return std::ptr::null_mut();
    };
    if session.warnings.is_empty() {
        return std::ptr::null_mut();
    }
    let warning = session.warnings.remove(0);
    match CString::new(warning.replace('\0', "\u{2400}")) {
        Ok(text) => text.into_raw(),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Evaluate UTF-8 Nix source and hand back a handle to the result in weak
/// head normal form, **without memoising anything**.
///
/// The compile cache still applies; the result cache cannot, and this is the
/// shape that showed why. A memo row is filed under the question that was
/// asked, and this call has not been told one: the caller has a live value
/// and has not yet said which part of it it wants or how. Everything reached
/// through the returned handle is therefore evaluated from cold on every run,
/// however warm `eval-cache-dir` is.
///
/// For a caller that does know its question -- which is every command in this
/// tree, `nix eval` and `nix build` included -- use
/// [`ixe_session_eval_question`], which states it up front and can serve.
/// This one stays for an embedder that genuinely wants to explore a value it
/// cannot describe in advance, and for the tests of the handle table itself.
///
/// Weak head, not normal: `{ a = 1; b = throw "x"; }` succeeds here and the
/// throw only happens if something asks for `b`. That is the whole point of
/// the handle -- selecting `a` from a large set must not enter its siblings.
///
/// On a non-zero return `*out` is left alone and the message is available
/// from `ixe_session_take_error`.
///
/// `file` is the path the source was read from, or null when it was not read
/// from one (`--expr`). It is what `__curPos` reports, and cppnix answers
/// `null` for the second case rather than inventing a name, so the two are
/// different arguments and not one with an empty default.
///
/// # Safety
/// `session` must be live; `src` must point to `src_len` readable bytes;
/// `base_dir` to `base_dir_len` readable bytes or be null; `file` to
/// `file_len` readable bytes or be null; `out` must be a valid non-null
/// pointer to write one handle through.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_session_eval(
    session: *mut IxeSession,
    src: *const u8,
    src_len: usize,
    base_dir: *const u8,
    base_dir_len: usize,
    file: *const u8,
    file_len: usize,
    out: *mut u64,
) -> i32 {
    let session = session!(session);
    if src.is_null() || out.is_null() {
        return session.bad("null source or output pointer");
    }
    // Like `ixe_apply` and the other injections: a value built from a source
    // the question's key never saw, then forced inside the question, would
    // record its reads under that question's row (review, 2026-09-04).
    if let Some(status) = refuse_injection_during_a_question(session, "ixe_session_eval") {
        return status;
    }
    // Re-take the process configuration for this evaluation. The embedder may
    // have called a setter since `ixe_session_new`, and a session outlives any
    // one call; within the evaluation below the settings then hold still,
    // which is what lets the memo key describe the run it labels (ENG-12939).
    session.vm.reload_settings_from_process();
    if let Err(why) = require_session_cache_identity(session) {
        return session.bad(why);
    }

    let base = match unsafe { borrow_str(base_dir, base_dir_len) } {
        Ok(None) => ".".to_owned(),
        Ok(Some(text)) => text.to_owned(),
        Err(()) => return session.bad("base directory is not UTF-8"),
    };
    // SAFETY: caller contract; src points to src_len bytes.
    let bytes = unsafe { slice::from_raw_parts(src, src_len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        // The token as well as the message: `last_error` alone left this the
        // one refusal the handle API reported without a kind, and a census
        // row nobody can name is worse than a missing one.
        session.last_error = Some("non-UTF-8 source".to_owned().into());
        session.last_refusal = Some(crate::refusal::RefusalToken::NonUtf8Source);
        return IXE_ERR_UNIMPLEMENTED;
    };

    let file = match unsafe { borrow_str(file, file_len) } {
        Ok(None) => None,
        Ok(Some(path)) => Some(path.to_owned()),
        Err(()) => return session.bad("source file path is not UTF-8"),
    };
    let origin = match &file {
        Some(path) => crate::compile::Origin::File(path),
        None => crate::compile::Origin::String,
    };
    eval_uncached(
        session,
        text,
        &base,
        origin,
        &crate::session::Arguments::none(),
        out,
    )
}

// -- the whole question -----------------------------------------------------
//
// `ixe_session_eval` above hands back a live value and cannot memoise,
// because a memo row is filed under a question and it has not been told one.
// That was read for a long time as a property of handle walks (ENG-12470) and
// it is not: it is a property of *this call*. Every command in this tree
// knows its whole question before it opens a session -- which attribute path,
// and which bytes it wants at the end of it -- and only the handle table in
// between does not. Saying it up front is what lets the same table be a way
// of selecting *within* a memoised answer instead of a way around the cache.
//
// The protocol is two calls, because the work in the middle is the embedder's:
//
//   1. `ixe_session_eval_question` says whether the cache already has this
//      question's answer. If it does, that is the whole exchange.
//   2. otherwise it hands back a root handle with recording live, the caller
//      walks and renders as before, and `ixe_session_question_answer` files
//      what it produced under the read set that walk generated.
//
// Every force between the two goes through the recorder (`machine_and_host`),
// so the read set covers the walk and the render and not just the first
// evaluation. That is also what carries the `.drv` write: it leaves the
// evaluator as a `NeedPath::WriteDrv`, is recorded as the store question it
// is, and is therefore re-performed when a later run replays the read set
// (ENG-12801).

/// Question kinds, kept in step with ixe.h.
pub(crate) const IXE_QUESTION_SELECT: i32 = 0;
pub(crate) const IXE_QUESTION_DERIVATION: i32 = 1;
pub(crate) const IXE_QUESTION_APP: i32 = 2;
pub(crate) const IXE_QUESTION_FLAKE_SHOW: i32 = 3;
pub(crate) const IXE_QUESTION_DERIVATION_PATH: i32 = 4;
pub(crate) const IXE_QUESTION_DERIVATION_SET: i32 = 5;
pub(crate) const IXE_QUESTION_FLAKE_DOCUMENT: i32 = 6;
pub(crate) const IXE_QUESTION_SOURCE_POSITION: i32 = 7;
pub(crate) const IXE_QUESTION_SEARCH_PACKAGES: i32 = 8;
pub(crate) const IXE_QUESTION_FLAKE_CHECK: i32 = 9;

/// Argument kinds, kept in step with ixe.h.
const IXE_ARG_JSON: i32 = 0;
const IXE_ARG_INTERNAL_PRIMOP: i32 = 1;

pub(crate) const IXE_AUTO_ARG_EXPR: i32 = 0;
pub(crate) const IXE_AUTO_ARG_STRING: i32 = 1;

/// A counted byte string the embedder owns for the length of the call.
///
/// A struct rather than another pointer/length pair in the parameter list,
/// because the question call now carries two *lists* of them and C has no way
/// to spell a list of pairs without one.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeBytes {
    pub text: *const u8,
    pub len: usize,
}

/// One value to apply to the source before the question is asked of it.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeArgument {
    /// `IXE_ARG_JSON` or `IXE_ARG_INTERNAL_PRIMOP`.
    pub kind: i32,
    /// The document, or the primop's name.
    pub text: IxeBytes,
}

/// One `--arg`/`--argstr`: what a function met on the walk is applied to.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeAutoArg {
    /// The formal it binds.
    pub name: IxeBytes,
    /// `IXE_AUTO_ARG_EXPR` (parsed under the working directory, evaluated
    /// lazily) or `IXE_AUTO_ARG_STRING` (the bytes, no context).
    pub kind: i32,
    pub text: IxeBytes,
}

/// Read a counted string, or say which field was not UTF-8.
///
/// # Safety
/// `bytes.text` must point to `bytes.len` readable bytes, or be null.
unsafe fn bytes_str<'a>(bytes: &IxeBytes) -> Result<&'a str, ()> {
    // SAFETY: caller contract; an empty string is spelled by a zero length,
    // which `borrow_str` reads as absent.
    match unsafe { borrow_str(bytes.text, bytes.len) } {
        Ok(Some(text)) => Ok(text),
        Ok(None) => Ok(""),
        Err(()) => Err(()),
    }
}

/// Decode the embedder's argument array.
///
/// # Safety
/// `args` must point to `args_len` readable [`IxeArgument`]s, or be null when
/// `args_len` is zero; every `text` within must be readable for its length.
unsafe fn decode_arguments(
    session: &mut IxeSession,
    args: *const IxeArgument,
    args_len: usize,
) -> Result<crate::session::Arguments, i32> {
    if args_len == 0 {
        return Ok(crate::session::Arguments::none());
    }
    if args.is_null() {
        return Err(session.bad("null argument array with a non-zero length"));
    }
    // SAFETY: caller contract.
    let raw = unsafe { slice::from_raw_parts(args, args_len) };
    let mut out = Vec::with_capacity(args_len);
    for (n, argument) in raw.iter().enumerate() {
        // SAFETY: caller contract.
        let Ok(text) = (unsafe { bytes_str(&argument.text) }) else {
            return Err(session.bad(format!("argument {n} is not UTF-8")));
        };
        out.push(match argument.kind {
            IXE_ARG_JSON => crate::session::Argument::Json(text.to_owned()),
            IXE_ARG_INTERNAL_PRIMOP => crate::session::Argument::InternalPrimop(text.to_owned()),
            other => return Err(session.bad(format!("unknown argument kind {other}"))),
        });
    }
    Ok(crate::session::Arguments::new(out))
}

/// Decode the embedder's candidate attribute paths.
///
/// # Safety
/// As [`decode_arguments`], for `paths`.
unsafe fn decode_attr_paths(
    session: &mut IxeSession,
    paths: *const IxeBytes,
    paths_len: usize,
) -> Result<Vec<String>, i32> {
    if paths_len == 0 || paths.is_null() {
        // Refused rather than read as "the root", which is how a caller with
        // nothing to select spells it: one empty path. Two spellings for one
        // walk would be either two keys for one answer, which costs hits, or
        // one key normalised from both -- and then the key would claim to
        // describe a ladder that resolves nothing, which is a different walk.
        // `Question::Whole` is the variant for a caller with no selection.
        return Err(session.bad(
            "a Select or Derivation question needs at least one attribute path; \
             the root itself is one empty path",
        ));
    }
    // SAFETY: caller contract.
    let raw = unsafe { slice::from_raw_parts(paths, paths_len) };
    let mut out = Vec::with_capacity(paths_len);
    for (n, path) in raw.iter().enumerate() {
        // SAFETY: caller contract.
        let Ok(text) = (unsafe { bytes_str(path) }) else {
            return Err(session.bad(format!("attribute path {n} is not UTF-8")));
        };
        out.push(text.to_owned());
    }
    Ok(out)
}

/// Build the values `arguments` describes and apply `root` to them in order.
///
/// One application per argument, which is what a curried call is in the IR
/// too, and each result is lazy: `call-flake.nix` has not run when this
/// returns, so a flake's `allNodes` is built by whatever the selection asks
/// for and by nothing else.
///
/// **The list applied here is the same value that went into the memo key**,
/// a few lines above in [`ixe_session_eval_question`]. That is the whole of
/// the soundness argument for memoising an evaluand with arguments: there is
/// no arrangement of embedder code that keys on one list and applies another,
/// because the embedder never gets to apply anything --
/// [`refuse_injection_during_a_question`] holds the other side of it.
fn apply_arguments(
    session: &mut IxeSession,
    root: Value,
    arguments: &crate::session::Arguments,
) -> Result<Slot, i32> {
    let mut current = Slot::value(root);
    for argument in arguments.as_slice() {
        let value = match argument {
            crate::session::Argument::Json(text) => json_document_value(session, text)?,
            crate::session::Argument::InternalPrimop(name) => internal_primop_value(session, name)?,
        };
        // The function is forced and checked before the call, as `ixe_apply`
        // does and as cppnix's `forceFunction` does: applying an argument to
        // a non-function is an error cppnix raises at the call, and deferring
        // it into a cell nobody forces would turn a type error into silence.
        let f = force_slot(session, current)?;
        if !is_callable(session, &f) {
            let what = crate::value2::type_name(&f);
            return Err(session.bad(format!(
                "attempt to apply an argument to something which is not a function but {what}"
            )));
        }
        current = Slot::pending(Slot::value(f), vec![Slot::value(value)]);
    }
    Ok(current)
}

/// What `ixe_session_eval_question` decided, written through `out_mode`.
///
/// An explicit mode rather than "is the answer pointer null": there are three
/// outcomes and two of them hand back an answer, so a caller inferring the
/// mode from which pointers are set would have to get a two-way test right to
/// distinguish them. Getting it wrong in the quiet direction means silently
/// skipping the verification, which is the one outcome nothing else would
/// notice.
const IXE_SERVE_EVALUATE: i32 = 0;
const IXE_SERVE_ANSWER: i32 = 1;
const IXE_SERVE_VERIFY: i32 = 2;

/// Evaluate `src` for one whole question, serving it from `eval-cache-dir`
/// when that cache has it.
///
/// Writes one of the `IXE_SERVE_*` values through `out_mode`:
///
///   * `IXE_SERVE_ANSWER`: `*out_answer` is the memoised answer and `*out_root`
///     is zero. The caller is done and must not walk anything.
///   * `IXE_SERVE_EVALUATE`: `*out_root` is a handle to the expression and
///     `*out_answer` is null. In weak head normal form when `args` is empty,
///     and an unforced application of it to those arguments when it is not --
///     lazily, so `call-flake.nix`'s body has not run. The caller does its
///     walk and reports the result to [`ixe_session_question_answer`].
///   * `IXE_SERVE_VERIFY`: both are set. This occasion is one of the sampled
///     checks of a memoised answer, so the caller does the work anyway,
///     reports it for comparison, and then uses `*out_answer` -- the served
///     one -- so that a command's output never depends on whether the sampler
///     happened to pick it.
///
/// A non-zero status is a failure exactly as [`ixe_session_eval`]'s is, with
/// the message on the session. There is then no question in flight and
/// [`ixe_session_question_answer`] is a no-op, so a caller may call it
/// unconditionally.
///
/// Only successful answers are memoised. A failure on this path can be raised
/// by the bridge rather than by the evaluator -- a missing attribute carries
/// the sibling names it suggests, a refusal carries a token -- and none of
/// those round-trip through the `(status, text)` pair a row holds. Storing a
/// failure this cannot reproduce faithfully would be worse than re-evaluating
/// it, so failures stay cold. ENG-12857.
///
/// # Safety
/// `session` must be live; `src` must point to `src_len` readable bytes;
/// `base_dir`, `file` and `attr_path` to their lengths in readable bytes, or
/// be null; `out_mode`, `out_root` and `out_answer` must each be a valid
/// non-null pointer to write one value through.
#[unsafe(no_mangle)]
#[allow(clippy::too_many_arguments)]
pub unsafe extern "C" fn ixe_session_eval_question(
    session: *mut IxeSession,
    src: *const u8,
    src_len: usize,
    base_dir: *const u8,
    base_dir_len: usize,
    file: *const u8,
    file_len: usize,
    args: *const IxeArgument,
    args_len: usize,
    kind: i32,
    attr_paths: *const IxeBytes,
    attr_paths_len: usize,
    index_lists: i32,
    apply: *const u8,
    apply_len: usize,
    auto_args: *const IxeAutoArg,
    auto_args_len: usize,
    auto_call: i32,
    render: i32,
    out_mode: *mut i32,
    out_root: *mut u64,
    out_answer: *mut *mut c_char,
) -> i32 {
    let session = session!(session);
    if src.is_null() || out_mode.is_null() || out_root.is_null() || out_answer.is_null() {
        return session.bad("null source or output pointer");
    }
    // A question that starts ends the last one: its applications, and its
    // recording and memo if the embedder never answered it, belong to the
    // question they were keyed with and to nothing after it. Left in place,
    // a stale scope would record this question's reads under the old
    // identity and hand its memo to a new recording (review, 2026-09-04).
    abandon_question(session);
    // Re-take the process configuration for this evaluation. The embedder may
    // have called a setter since `ixe_session_new`, and a session outlives any
    // one call; within the evaluation below the settings then hold still,
    // which is what lets the memo key describe the run it labels (ENG-12939).
    session.vm.reload_settings_from_process();

    // Cleared before anything can fail, so a caller reading them after an
    // early return sees "nothing" rather than whatever its variables held.
    // SAFETY: all three checked non-null above.
    unsafe {
        *out_mode = IXE_SERVE_EVALUATE;
        *out_root = 0;
        *out_answer = std::ptr::null_mut();
    }

    if let Err(why) = require_session_cache_identity(session) {
        return session.bad(why);
    }

    let base = match unsafe { borrow_str(base_dir, base_dir_len) } {
        Ok(None) => ".".to_owned(),
        Ok(Some(text)) => text.to_owned(),
        Err(()) => return session.bad("base directory is not UTF-8"),
    };
    // SAFETY: caller contract; src points to src_len bytes.
    let bytes = unsafe { slice::from_raw_parts(src, src_len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        session.last_error = Some("non-UTF-8 source".to_owned().into());
        session.last_refusal = Some(crate::refusal::RefusalToken::NonUtf8Source);
        return IXE_ERR_UNIMPLEMENTED;
    };
    let file = match unsafe { borrow_str(file, file_len) } {
        Ok(None) => None,
        Ok(Some(path)) => Some(path.to_owned()),
        Err(()) => return session.bad("source file path is not UTF-8"),
    };
    // SAFETY: caller contract.
    let attr_paths = match unsafe { decode_attr_paths(session, attr_paths, attr_paths_len) } {
        Ok(paths) => paths,
        Err(status) => return status,
    };
    // `nix eval --apply`, parsed under the working directory as cppnix parses
    // it (`rootPath(".")`); the directory is in the key beside the text.
    // Present when the pointer is, not when the text is: `--apply ''` is an
    // expression that fails to parse, and reading it as "no apply" would
    // serve the unapplied answer for it.
    let apply = if apply.is_null() {
        None
    } else {
        // SAFETY: caller contract; apply points to apply_len bytes.
        let bytes = unsafe { slice::from_raw_parts(apply, apply_len) };
        let Ok(text) = std::str::from_utf8(bytes) else {
            return session.bad("--apply expression is not UTF-8");
        };
        let base = match working_directory(session) {
            Ok(base) => base,
            Err(status) => return status,
        };
        Some(crate::session::Apply {
            text: text.to_owned(),
            base,
        })
    };
    // SAFETY: caller contract.
    let auto_args = match unsafe { decode_auto_args(session, auto_args, auto_args_len) } {
        Ok(auto_args) => auto_args,
        Err(status) => return status,
    };
    let selection = crate::session::Selection {
        attr_paths,
        index_lists: index_lists != 0,
        apply,
        auto_args,
        auto_call: auto_call != 0,
    };
    // SAFETY: caller contract.
    let arguments = match unsafe { decode_arguments(session, args, args_len) } {
        Ok(arguments) => arguments,
        Err(status) => return status,
    };
    let question = match kind {
        IXE_QUESTION_SELECT => {
            let Some(render) = render_mode_of(render) else {
                return session.bad(format!("unknown render mode {render}"));
            };
            crate::session::Question::Select {
                selection: selection.clone(),
                render,
            }
        }
        IXE_QUESTION_DERIVATION => crate::session::Question::Derivation {
            selection: selection.clone(),
        },
        IXE_QUESTION_DERIVATION_SET => crate::session::Question::DerivationSet {
            selection: selection.clone(),
        },
        IXE_QUESTION_APP => crate::session::Question::Application {
            selection: selection.clone(),
        },
        IXE_QUESTION_SOURCE_POSITION => crate::session::Question::SourcePosition {
            selection: selection.clone(),
        },
        IXE_QUESTION_SEARCH_PACKAGES => crate::session::Question::SearchPackages {
            selection: selection.clone(),
            scope: match render {
                0 => crate::session::SearchScope::Selected,
                1 => crate::session::SearchScope::FlakeDefaults,
                _ => return session.bad("unknown search scope"),
            },
        },
        IXE_QUESTION_FLAKE_CHECK => crate::session::Question::FlakeCheck {
            selection: selection.clone(),
            options: match crate::flake_check::Options::from_flags(render) {
                Ok(options) => options,
                Err(why) => return session.bad(why),
            },
        },
        IXE_QUESTION_FLAKE_SHOW => crate::session::Question::FlakeShow {
            selection: selection.clone(),
            flags: render,
        },
        IXE_QUESTION_DERIVATION_PATH => crate::session::Question::DerivationPath {
            selection: selection.clone(),
        },
        IXE_QUESTION_FLAKE_DOCUMENT => crate::session::Question::FlakeDocument,
        other => return session.bad(format!("unknown question kind {other}")),
    };
    // The arguments' cells, built before anything can be served: an `--arg`
    // that does not parse fails the command whether or not the cache holds
    // an answer, as it does under cppnix, which parses them first.
    let mut in_flight = match in_flight_question(session, &selection, kind, question.clone()) {
        Ok(in_flight) => in_flight,
        Err(status) => return status,
    };
    // After the question is known to be well-formed: a call refused above
    // leaves no question in flight, as the API says.
    let source_root = session.source_root.clone();
    if matches!(source_root, crate::value2::Root::Mounted(_)) {
        for path in std::iter::once(base.as_str()).chain(file.as_deref()) {
            if let Err(error) = crate::value2::PathValue::try_new(source_root.clone(), path) {
                return session.bad(error);
            }
        }
    }
    let origin = match (&file, &source_root) {
        (Some(path), crate::value2::Root::Mounted(mount_point)) => {
            crate::compile::Origin::MountedFile { path, mount_point }
        }
        (Some(path), crate::value2::Root::Ambient) => crate::compile::Origin::File(path),
        (None, crate::value2::Root::Ambient) => crate::compile::Origin::String,
        (None, crate::value2::Root::Mounted(_)) => {
            return session.bad("mounted source has no file");
        }
    };
    if matches!(question, crate::session::Question::FlakeDocument) {
        in_flight.flake_dir = Some(match &file {
            Some(path) => crate::value2::PathValue::normalized(origin.root(), path).parent(),
            None => crate::value2::PathValue::normalized(origin.root(), &base),
        });
    }
    // In flight from here: everything above was validation, and a question
    // that never got past it must leave no state for the accessors to find.
    // Every failure below this line goes through `abandon_question`.
    session.question = Some(in_flight);
    // No cache, or one that will not open: evaluate the way `ixe_session_eval`
    // does. A cache is an optimisation and the expression is still owed an
    // answer.
    let Some(dir) = eval_cache_dir() else {
        return eval_uncached(session, text, &base, origin, &arguments, out_root);
    };
    let mut cache =
        match crate::session::QuestionCache::open(&dir, verify_rate(), cache_max_bytes()) {
            Ok(cache) => cache.with_retained(session.retained.clone()),
            Err(reason) => {
                session
                    .warnings
                    .push(format!("warning: {reason}; evaluating without it"));
                return eval_uncached(session, text, &base, origin, &arguments, out_root);
            }
        };

    // Compile first: the module digest is half the key, and the compile cache
    // is the part of the saving a cold process gets even on a memo miss. The
    // machine's cache, opened with the session: the one its imports go
    // through.
    let compiled = {
        let outcome = session.vm.compile(text, &base, origin);
        let corruption = session.vm.modules_mut().take_corruption();
        for message in corruption {
            cache.complain(crate::readset::Complaint::warning(message));
        }
        match outcome {
            Ok(compiled) => compiled,
            Err(error) => {
                let failure = crate::session::compile_failure(&error);
                drain_cache(session, &mut cache);
                abandon_question(session);
                return fail_with(session, &failure);
            }
        }
    };
    let module_id = *compiled.id.hash();
    let identity = crate::readset::EvalId::of(
        &module_id,
        &crate::eval::Settings::current(),
        &arguments,
        &question,
    );

    session.served = None;
    let verifying = match cache.serve(&identity, &session.host, session.vm.settings()) {
        crate::session::Served::Answer(result) => {
            session.served = cache.last_served();
            drain_cache(session, &mut cache);
            // SAFETY: checked non-null above.
            unsafe { *out_mode = IXE_SERVE_ANSWER };
            return serve_answer(session, &result, out_answer);
        }
        crate::session::Served::Evaluate { verifying } => verifying,
    };
    // A served failure cannot happen on this path, because only successes are
    // recorded here. Handled rather than asserted: if one ever appears -- a
    // row written by some future caller under this same key scheme -- the
    // honest thing is to hand it back, not to check it against a walk whose
    // failure has a different shape.
    let verifying = match verifying {
        Some(result) if result.status != crate::session::OK => {
            drain_cache(session, &mut cache);
            // SAFETY: checked non-null above.
            unsafe { *out_mode = IXE_SERVE_ANSWER };
            return serve_answer(session, &result, out_answer);
        }
        other => other,
    };

    // A sampled whole-question verifier must execute imported subtrees too.
    session.vm.set_import_cache_enabled(verifying.is_none());
    let served_text = verifying.as_ref().map(|result| result.value.clone());
    let recorder = if verifying.is_some() {
        // Quiet: this run is a check of an answer the cache already gave, and
        // `settle` replays that answer's emissions, so a reader must not also
        // see this copy.
        crate::readset::RecordingHost::quiet(session.host.clone())
    } else {
        crate::readset::RecordingHost::new(session.host.clone())
    };
    // Filtered copies from the cache's own memo where sound, on both paths:
    // a verification checks the whole-evaluation answer, and the copy memo
    // confirms every path it serves with the store, so nothing it can say
    // would pass a check that the walk would fail. Realisations with nothing
    // to build likewise: the recorder confirms every path they stand on with
    // the store before answering without the build.
    let recorder = recorder
        .with_copy_memo(cache.copy_memo())
        .realising_by_validity(
            session.vm.settings().ca_derivations,
            session.vm.settings().allow_import_from_derivation,
        );
    session.memo = Some(MemoScope {
        cache,
        identity,
        recorder,
        memo: crate::eval::JobMemo::default(),
        verifying,
    });

    let module = compiled.module;
    let mut fresh = crate::eval::JobMemo::default();
    let (vm, host, memo) = machine_and_host(session, &mut fresh);
    match crate::session::run_to_value_with(vm, &module, host, memo) {
        Ok(value) => {
            // Inside the scope, deliberately: applying `call-flake.nix` to a
            // lock file forces its outer lambdas, and any world read that
            // causes belongs in this question's read set.
            let root = match apply_arguments(session, value, &arguments) {
                Ok(root) => root,
                Err(status) => {
                    abandon_question(session);
                    return status;
                }
            };
            let handle = session.insert(root);
            // SAFETY: checked non-null above.
            unsafe { *out_root = handle };
            if let Some(text) = served_text {
                // SAFETY: checked non-null above.
                unsafe { *out_mode = IXE_SERVE_VERIFY };
                let rc = out_string(text, out_answer);
                if rc != IXE_OK {
                    return rc;
                }
            }
            drain_scope(session);
            IXE_OK
        }
        Err(error) => {
            // The expression itself failed, so there is no walk to do and
            // nothing to file. Drop the scope before reporting, or every
            // later force in this session would keep recording into a read
            // set nobody will ever settle.
            abandon_question(session);
            session.fail(&error)
        }
    }
}

/// File the answer the caller produced for the question in flight, or compare
/// it against the one that was served.
///
/// `status` is zero when the caller produced an answer and the status it is
/// about to raise otherwise. A non-zero status abandons the question without
/// filing anything, which is what keeps a failure this cannot faithfully
/// reproduce out of the table.
///
/// Calling this with no question in flight is not an error and does nothing.
/// A caller that was served, or one running without `eval-cache-dir`, calls
/// it on the same line as one that was not, so the two shapes read the same
/// at the call site and neither can be forgotten in one branch.
///
/// # Safety
/// `session` must be live; `answer` must point to `answer_len` readable bytes
/// or be null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_session_question_answer(
    session: *mut IxeSession,
    status: i32,
    answer: *const u8,
    answer_len: usize,
) -> i32 {
    let session = session!(session);
    // The answer ends the question, filed or not, cached or not.
    drain_scope(session);
    session.vm.set_import_cache_enabled(true);
    session.question = None;
    let Some(mut scope) = session.memo.take() else {
        return IXE_OK;
    };
    if status != IXE_OK {
        drain_cache_of(&mut session.warnings, &mut scope.cache);
        return IXE_OK;
    }
    let value = match unsafe { borrow_str(answer, answer_len) } {
        Ok(None) => String::new(),
        Ok(Some(text)) => text.to_owned(),
        Err(()) => {
            drain_cache_of(&mut session.warnings, &mut scope.cache);
            return session.bad("answer is not UTF-8");
        }
    };
    let read_set = scope.recorder.take();
    let result = crate::readset::EvalResult {
        status: crate::session::OK.to_owned(),
        value,
        emissions: scope.recorder.take_emissions(),
        token: None,
        pos: None,
    };
    let interrupted = session.vm.interrupted();
    let settings = session.vm.settings().clone();
    let host = session.host.clone();
    let MemoScope {
        cache,
        identity,
        verifying,
        ..
    } = &mut scope;
    cache.settle(
        identity,
        &host,
        &settings,
        &read_set,
        &result,
        verifying.as_ref(),
        interrupted,
    );
    drain_cache_of(&mut session.warnings, cache);
    IXE_OK
}

/// The embedder could not use the answer the last `ixe_session_eval_question`
/// served: forget that row and its witness, so that asking again evaluates
/// and no later process is served it either. `why` is the embedder's reason,
/// reported through the warnings. An error when nothing was served, or when
/// the store refused the removal: an answer that cannot be forgotten must not
/// be silently served twice.
///
/// # Safety
/// `session` must be live; `why` must point to `why_len` readable bytes or be
/// null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_session_question_reject(
    session: *mut IxeSession,
    why: *const u8,
    why_len: usize,
) -> i32 {
    let session = session!(session);
    session.question = None;
    let why = match unsafe { borrow_str(why, why_len) } {
        Ok(text) => text.unwrap_or("no reason given").to_owned(),
        Err(()) => return session.bad("rejection reason is not UTF-8"),
    };
    let Some((identity, key)) = session.served.take() else {
        return session.bad("no served answer to reject");
    };
    let Some(dir) = eval_cache_dir() else {
        return session.bad("a served answer was rejected but no evaluation cache is configured");
    };
    let mut cache =
        match crate::session::QuestionCache::open(&dir, verify_rate(), cache_max_bytes()) {
            Ok(cache) => cache.with_retained(session.retained.clone()),
            Err(reason) => {
                return session.bad(format!(
                    "cannot reopen the evaluation cache to forget a rejected answer: {reason}"
                ));
            }
        };
    let forgotten = cache.forget(&identity, key, &why);
    drain_cache(session, &mut cache);
    match forgotten {
        Ok(()) => IXE_OK,
        Err(reason) => session.bad(format!(
            "could not forget a rejected memoised answer: {reason}"
        )),
    }
}

/// Evaluate to a root handle with no result cache: `ixe_session_eval`'s body,
/// and `ixe_session_eval_question`'s fallback when the result cache is not
/// there, shared so the fallback cannot drift from the thing it falls back to.
fn eval_uncached(
    session: &mut IxeSession,
    text: &str,
    base: &str,
    origin: crate::compile::Origin<'_>,
    arguments: &crate::session::Arguments,
    out_root: *mut u64,
) -> i32 {
    // Through the machine's own compile cache, whatever it is: the result memo
    // is what failed to open or was never configured, and that has already
    // been complained about once.
    let host = session.host.clone();
    let (answer, warnings) =
        crate::session::evaluate_value_once(&mut session.vm, &host, text, base, origin);
    // Labelled on the way in, so `ixe_session_take_warning` keeps handing the
    // embedder one string and the severity still survives to the log line.
    session
        .warnings
        .extend(warnings.into_iter().map(|complaint| complaint.to_string()));
    match answer {
        Ok(value) => {
            // The same application the cached path performs, so a caller with
            // no `eval-cache-dir` gets the same value out of the same call.
            // Written here rather than left to the caller because there is no
            // longer a way for the caller to do it: the arguments cross on
            // this call precisely so that they cannot be applied anywhere the
            // key cannot see.
            let root = match apply_arguments(session, value, arguments) {
                Ok(root) => root,
                Err(status) => {
                    abandon_question(session);
                    return status;
                }
            };
            let handle = session.insert(root);
            // SAFETY: both callers checked it.
            unsafe { *out_root = handle };
            IXE_OK
        }
        // No question survives a failed evaluation (`ixe_session_eval` has
        // none in flight, and clearing nothing is free).
        Err(error) => {
            abandon_question(session);
            session.fail(&error)
        }
    }
}

/// Hand a memoised result to the caller: the text on success, the exception
/// it stands for otherwise.
fn serve_answer(
    session: &mut IxeSession,
    result: &crate::readset::EvalResult,
    out_answer: *mut *mut c_char,
) -> i32 {
    // A served question is over: nothing is walked, so nothing may be
    // applied under its key.
    session.question = None;
    crate::perf::note_memo_served();
    if result.status == crate::session::OK {
        // SAFETY: the callers checked it.
        return out_string(result.value.clone(), unsafe { &mut *out_answer });
    }
    fail_with(session, result)
}

/// Raise the failure a result stands for, with its token where it has one.
fn fail_with(session: &mut IxeSession, result: &crate::readset::EvalResult) -> i32 {
    match crate::session::error_of(result) {
        Some(error) => {
            let status = session.fail(&error);
            session.last_refusal = result.token.or(session.last_refusal);
            status
        }
        // `error_of` answers `None` only for a successful result, which the
        // one caller has already ruled out. Reported rather than ignored,
        // because a status of zero with no answer written is the shape a
        // caller cannot detect.
        None => session.bad("internal: a successful result reached the failure path"),
    }
}

/// Move whatever the cache complained about into the session's warning queue.
fn drain_cache(session: &mut IxeSession, cache: &mut crate::session::QuestionCache) {
    drain_cache_of(&mut session.warnings, cache);
}

/// The same, for a caller that already holds the cache apart from the session.
fn drain_cache_of(warnings: &mut Vec<String>, cache: &mut crate::session::QuestionCache) {
    warnings.extend(
        cache
            .take_complaints()
            .into_iter()
            .map(|complaint| complaint.to_string()),
    );
}

/// Drain the in-flight question's complaints without ending it.
fn drain_scope(session: &mut IxeSession) {
    let IxeSession {
        memo, warnings, vm, ..
    } = session;
    warnings.extend(
        vm.modules_mut()
            .take_corruption()
            .into_iter()
            .map(|message| format!("warning: {message}")),
    );
    if let Some(scope) = memo {
        drain_cache_of(warnings, &mut scope.cache);
    }
}

/// End the question in flight without filing anything.
fn abandon_question(session: &mut IxeSession) {
    drain_scope(session);
    session.vm.set_import_cache_enabled(true);
    session.question = None;
    if let Some(mut scope) = session.memo.take() {
        drain_cache_of(&mut session.warnings, &mut scope.cache);
    }
}

/// The render mode an `IXE_RENDER_*` value names.
fn render_mode_of(mode: i32) -> Option<RenderMode> {
    match mode {
        IXE_RENDER_PLAIN => Some(RenderMode::Plain),
        IXE_RENDER_JSON => Some(RenderMode::Json),
        IXE_RENDER_RAW => Some(RenderMode::Raw),
        IXE_RENDER_VALUE_PRINTER => Some(RenderMode::ValuePrinter),
        IXE_RENDER_XML => Some(RenderMode::Xml),
        IXE_RENDER_PLAIN_LAZY => Some(RenderMode::PlainLazy),
        _ => None,
    }
}

/// Force a handle to weak head normal form. Idempotent: the cell memoises,
/// so a second call is free and a failed force raises the same error again
/// rather than re-running it.
///
/// # Safety
/// `session` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_force(session: *mut IxeSession, handle: u64) -> i32 {
    let session = session!(session);
    force_handle(session, handle).map_or_else(|status| status, |_| IXE_OK)
}

/// Force `handle` and return its value, or the status to hand back.
fn force_handle(session: &mut IxeSession, handle: u64) -> Result<Value, i32> {
    let Some(slot) = session.get(handle).cloned() else {
        return Err(session.bad("unknown handle"));
    };
    force_slot(session, slot)
}

/// Force a slot that is not (yet) in the handle table.
///
/// The body of [`force_handle`], shared with [`apply_arguments`], which
/// builds a chain of pending applications and has no reason to give each
/// intermediate step a handle the embedder will never see.
fn force_slot(session: &mut IxeSession, slot: Slot) -> Result<Value, i32> {
    if let Some(value) = slot.peek() {
        return Ok(value);
    }
    session.vm.start_force(slot);
    let mut fresh = crate::eval::JobMemo::default();
    let (vm, host, memo) = machine_and_host(session, &mut fresh);
    match crate::eval::drive_with(vm, host, memo) {
        Ok(value) => Ok(value),
        Err(error) => Err(session.fail(&crate::eval::map_vm_error(error))),
    }
}

/// cppnix's `forceFunction` (`eval.cc:2505`): a lambda, a primop, or a set
/// carrying `__functor`.
fn is_callable(session: &mut IxeSession, f: &Value) -> bool {
    match f {
        Value::Closure(_) | Value::Builtin(_) => true,
        Value::Attrs(m) => {
            let functor = session.vm.intern("__functor");
            m.contains_key(&functor)
        }
        _ => false,
    }
}

/// The type of an already-forced handle, as one of the `IXE_TYPE_*` values.
/// Reports `IXE_TYPE_UNFORCED` rather than forcing, so a caller cannot
/// accidentally enter a thunk by asking what something is.
///
/// # Safety
/// `session` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_value_type(session: *mut IxeSession, handle: u64) -> i32 {
    let Some(session) = (unsafe { session.as_mut() }) else {
        return IXE_TYPE_UNKNOWN_HANDLE;
    };
    let Some(slot) = session.get(handle) else {
        return IXE_TYPE_UNKNOWN_HANDLE;
    };
    match slot.peek() {
        None => IXE_TYPE_UNFORCED,
        Some(Value::Int(_)) => IXE_TYPE_INT,
        Some(Value::Float(_)) => IXE_TYPE_FLOAT,
        Some(Value::Bool(_)) => IXE_TYPE_BOOL,
        Some(Value::Null) => IXE_TYPE_NULL,
        Some(Value::Str(_)) => IXE_TYPE_STRING,
        Some(Value::Path(_)) => IXE_TYPE_PATH,
        Some(Value::List(_)) => IXE_TYPE_LIST,
        Some(Value::Attrs(_)) => IXE_TYPE_ATTRS,
        Some(Value::Closure(_) | Value::Builtin(_)) => IXE_TYPE_FUNCTION,
    }
}

/// Release one handle. Freeing an unknown handle is a no-op, so double frees
/// are quiet rather than fatal. Dropping the slot also releases any dynamic
/// attribute-origin records reachable only through this handle.
///
/// # Safety
/// `session` must be live.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_handle_free(session: *mut IxeSession, handle: u64) {
    let Some(session) = (unsafe { session.as_mut() }) else {
        return;
    };
    if handle >> HANDLE_INDEX_BITS == session.serial {
        session.handles.remove(&(handle & HANDLE_INDEX_MASK));
    }
}

/// Number of attributes in a forced attribute set.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_attrs_len(
    session: *mut IxeSession,
    handle: u64,
    out: *mut usize,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    match force_handle(session, handle) {
        Err(status) => status,
        Ok(Value::Attrs(map)) => {
            // SAFETY: out is non-null, checked above.
            unsafe { *out = map.len() };
            IXE_OK
        }
        Ok(other) => {
            let what = crate::value2::type_name(&other);
            session.bad(format!("expected a set but found {what}"))
        }
    }
}

/// Every attribute name of a forced set, in one crossing.
///
/// `*out` receives one buffer of `*out_len` bytes holding the names back to
/// back, each NUL-terminated including the last, in the order cppnix prints
/// and serialises attribute sets in: sorted by name, not by the interner's
/// symbol ids. An empty set yields a null pointer and a length of zero.
/// Ownership transfers; free with `ixe_names_free`, which takes the length
/// because this is a byte buffer and not a C string.
///
/// This is bulk rather than an index accessor because an index accessor
/// cannot answer cheaply. The names live in a map ordered by symbol id, so
/// producing "the name at index i" in cppnix's order means materialising and
/// sorting the whole list, and there is nowhere to keep that between calls; a
/// caller enumerating a set therefore paid it once per name. The bridge
/// enumerates exactly when an attribute is missing and it wants did-you-mean
/// candidates, so on nixpkgs' 25,442-name top level a typo cost 42 seconds
/// against cppnix's 2, for an answer both arms printed identically. ENG-12913.
///
/// Enumerating does not force: the names of a set are known once the set
/// itself is, and nothing about a sibling's value is read here.
///
/// # Safety
/// `session` must be live; `out` and `out_len` must be valid non-null
/// pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_attrs_names(
    session: *mut IxeSession,
    handle: u64,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    let session = session!(session);
    if out.is_null() || out_len.is_null() {
        return session.bad("null output pointer");
    }
    let map = match force_handle(session, handle) {
        Err(status) => return status,
        Ok(Value::Attrs(map)) => map,
        Ok(other) => {
            let what = crate::value2::type_name(&other);
            return session.bad(format!("expected a set but found {what}"));
        }
    };
    // Borrowed from the interner, not copied: sorting `&str` compares the same
    // bytes an owned sort would and allocates nothing per name.
    let mut names: Vec<&str> = map.keys().map(|sym| session.vm.sym_name(*sym)).collect();
    names.sort_unstable();

    let mut blob: Vec<u8> = Vec::with_capacity(names.iter().map(|n| n.len() + 1).sum());
    for name in names {
        blob.extend_from_slice(without_nul(name).as_bytes());
        blob.push(0);
    }
    let len = blob.len();
    // SAFETY: both pointers are non-null, checked above.
    unsafe {
        *out_len = len;
        *out = if len == 0 {
            // An empty set has no buffer at all rather than a dangling
            // zero-length one, so a caller that forgets to check the length
            // faults instead of reading whatever the allocator left.
            std::ptr::null_mut()
        } else {
            Box::into_raw(blob.into_boxed_slice()).cast::<c_char>()
        };
    }
    IXE_OK
}

/// Release a buffer from `ixe_attrs_names` or `ixe_get_string_context`.
/// Freeing null is a no-op.
///
/// Separate from `ixe_string_free` because the two own different shapes: that
/// one round-trips a `CString`, this one a boxed slice whose length cannot be
/// recovered with `strlen` -- the buffer has a NUL after every name, so
/// `strlen` would report the first name's length and free the wrong extent.
///
/// # Safety
/// `names` must be a pointer from either producing function and `len` the
/// length it reported, or null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_names_free(names: *mut c_char, len: usize) {
    if names.is_null() {
        return;
    }
    // SAFETY: ownership round-trip of a Box<[u8]> from either producer.
    drop(unsafe { Box::from_raw(std::ptr::slice_from_raw_parts_mut(names.cast::<u8>(), len)) });
}

/// Select one attribute by name, without forcing it or any sibling.
///
/// Returns `IXE_ERR_MISSING` when the set has no such attribute, which the
/// embedder turns into cppnix's "attribute ... in selection path ... not
/// found". `*out` receives a handle to the attribute's cell, still lazy.
///
/// # Safety
/// `session` must be live; `name` must point to `name_len` readable bytes;
/// `out` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_attrs_select(
    session: *mut IxeSession,
    handle: u64,
    name: *const u8,
    name_len: usize,
    out: *mut u64,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    let name = match unsafe { borrow_str(name, name_len) } {
        Ok(Some(text)) => text.to_owned(),
        Ok(None) => String::new(),
        Err(()) => return session.bad("attribute name is not UTF-8"),
    };
    let map = match force_handle(session, handle) {
        Err(status) => return status,
        Ok(Value::Attrs(map)) => map,
        Ok(other) => {
            let what = crate::value2::type_name(&other);
            return session.bad(format!("expected a set but found {what}"));
        }
    };
    let sym = session.vm.intern(&name);
    let Some(slot) = map.get(&sym).cloned() else {
        session.last_error = Some(name.into());
        return IXE_ERR_MISSING;
    };
    let handle = session.insert(slot);
    // SAFETY: out is non-null, checked above.
    unsafe { *out = handle };
    IXE_OK
}

/// Number of elements in a forced list.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_list_len(
    session: *mut IxeSession,
    handle: u64,
    out: *mut usize,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    match force_handle(session, handle) {
        Err(status) => status,
        Ok(Value::List(items)) => {
            // SAFETY: out is non-null, checked above.
            unsafe { *out = items.len() };
            IXE_OK
        }
        Ok(other) => {
            let what = crate::value2::type_name(&other);
            session.bad(format!("expected a list but found {what}"))
        }
    }
}

/// One element of a list, unforced, by position. An attribute path may index
/// a list (`nix eval -f x.nix 'xs.1'`), which is why this exists beside
/// `ixe_attrs_select`.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_list_at(
    session: *mut IxeSession,
    handle: u64,
    index: usize,
    out: *mut u64,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    let items = match force_handle(session, handle) {
        Err(status) => return status,
        Ok(Value::List(items)) => items,
        Ok(other) => {
            let what = crate::value2::type_name(&other);
            return session.bad(format!("expected a list but found {what}"));
        }
    };
    let Some(slot) = items.get(index).cloned() else {
        session.last_error = Some(format!("index {index}").into());
        return IXE_ERR_MISSING;
    };
    let handle = session.insert(slot);
    // SAFETY: out is non-null, checked above.
    unsafe { *out = handle };
    IXE_OK
}

/// Read a forced integer.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_get_int(session: *mut IxeSession, handle: u64, out: *mut i64) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    match force_handle(session, handle) {
        Err(status) => status,
        Ok(Value::Int(n)) => {
            // SAFETY: out is non-null, checked above.
            unsafe { *out = n };
            IXE_OK
        }
        Ok(other) => {
            let what = crate::value2::type_name(&other);
            session.bad(format!("expected an integer but found {what}"))
        }
    }
}

/// Read a forced float.
///
/// Exists because `ixe_value_type` reports `IXE_TYPE_FLOAT` and nothing could
/// then read one: a caller that met a float had to render it and parse the
/// text back. The printer is not a round-tripping format, and rather than
/// assert that about another module, the per-kind round-trip table compares
/// what this returns against exactly what the printer produced -- so the two
/// are checked against each other on every run instead of one of them being
/// described in a comment. A tag with no accessor is a hole that table now
/// refuses to leave open.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_get_float(
    session: *mut IxeSession,
    handle: u64,
    out: *mut f64,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    match force_handle(session, handle) {
        Err(status) => status,
        Ok(Value::Float(x)) => {
            // SAFETY: out is non-null, checked above.
            unsafe { *out = x };
            IXE_OK
        }
        // Not widened from an integer: Nix keeps the two apart, `1` and `1.0`
        // are different values, and silently promoting here would hide that
        // from a caller that asked which one it had.
        Ok(other) => {
            let what = crate::value2::type_name(&other);
            session.bad(format!("expected a float but found {what}"))
        }
    }
}

/// Read a forced Boolean, as 0 or 1.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_get_bool(session: *mut IxeSession, handle: u64, out: *mut i32) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    match force_handle(session, handle) {
        Err(status) => status,
        Ok(Value::Bool(b)) => {
            // SAFETY: out is non-null, checked above.
            unsafe { *out = i32::from(b) };
            IXE_OK
        }
        Ok(other) => {
            let what = crate::value2::type_name(&other);
            session.bad(format!("expected a Boolean but found {what}"))
        }
    }
}

/// Read a forced string or path as bytes. Ownership of `*out` transfers.
///
/// A string carrying context is refused rather than handed over without it.
/// The bytes of `"${./f}"` are a store path and the context is the record
/// that the value depends on that path; a caller given only the bytes holds
/// something that looks complete and has quietly lost the dependency, which
/// is the same shape as an exception class that survives evaluation and not
/// memoisation -- right on the first look, wrong later, and nothing in
/// between says so.
///
/// This bare-string entry point deliberately continues to reject context.
/// Callers that need it read the bytes with `ixe_render(IXE_RENDER_RAW)` and
/// the dependency records separately with `ixe_get_string_context`; making
/// the lossy call succeed would still be a silent dependency drop.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_get_string(
    session: *mut IxeSession,
    handle: u64,
    out: *mut *mut c_char,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    match force_handle(session, handle) {
        Err(status) => status,
        Ok(Value::Str(text)) if text.has_context() => session.bad(format!(
            "the string has {} context element(s) and this call cannot carry them; \
             reading it as bare bytes would drop the store paths it depends on \
             (ENG-12492)",
            text.context_set().len()
        )),
        // Named rather than reached through a conversion, so a caller
        // dropping a context is visible in the source.
        Ok(Value::Str(text)) => out_bytes(text.bytes(), out),
        Ok(Value::Path(text)) => out_string(text.to_string(), out),
        Ok(other) => {
            let what = crate::value2::type_name(&other);
            session.bad(format!("expected a string but found {what}"))
        }
    }
}

/// Return a forced string's context in cppnix's wire spelling.
///
/// The buffer has one NUL-terminated `NixStringContextElem::parse` input per
/// element. It uses the same codec as the realise hook, so application
/// programs and realised strings cannot disagree about a dependency's kind.
///
/// # Safety
/// `session` must be live; `out` and `out_len` must be valid non-null pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_get_string_context(
    session: *mut IxeSession,
    handle: u64,
    out: *mut *mut c_char,
    out_len: *mut usize,
) -> i32 {
    let session = session!(session);
    if out.is_null() || out_len.is_null() {
        return session.bad("null output pointer");
    }
    let context = match force_handle(session, handle) {
        Err(status) => return status,
        Ok(Value::Str(text)) => {
            let context = text.context_set();
            encode_realise_request(&context)
        }
        Ok(other) => {
            let what = crate::value2::type_name(&other);
            return session.bad(format!("expected a string but found {what}"));
        }
    };
    let len = context.len();
    unsafe {
        *out_len = len;
        *out = if len == 0 {
            std::ptr::null_mut()
        } else {
            Box::into_raw(context.into_boxed_slice()).cast::<c_char>()
        };
    }
    IXE_OK
}

/// Render a handle's value to the bytes a command prints. `mode` is one of
/// the `IXE_RENDER_*` values. Ownership of `*out` transfers.
///
/// Rendering forces deeply, which is what every one of these output modes
/// does in cppnix too, so this is where a `throw` in a selected subtree
/// finally happens.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_render(
    session: *mut IxeSession,
    handle: u64,
    mode: i32,
    out: *mut *mut c_char,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    let mode = match mode {
        IXE_RENDER_PLAIN => RenderMode::Plain,
        IXE_RENDER_JSON => RenderMode::Json,
        IXE_RENDER_RAW => RenderMode::Raw,
        IXE_RENDER_VALUE_PRINTER => RenderMode::ValuePrinter,
        IXE_RENDER_XML => RenderMode::Xml,
        IXE_RENDER_PLAIN_LAZY => RenderMode::PlainLazy,
        other => return session.bad(format!("unknown render mode {other}")),
    };
    let value = match force_handle(session, handle) {
        Err(status) => return status,
        Ok(value) => value,
    };
    let mut fresh = crate::eval::JobMemo::default();
    let (vm, host, memo) = machine_and_host(session, &mut fresh);
    match crate::session::render_with(vm, host, value, mode, memo) {
        // Raw bytes, as cppnix writes them: only NUL cannot cross a C string.
        Ok(text) => out_bytes(&text, out),
        Err(error) => session.fail(&error),
    }
}

// -- building values the embedder supplies ----------------------------------
//
// Everything above hands values *out*. These three hand them in, which is
// what a command needs when the program it wants evaluated is a function of
// data cppnix computed: `nix eval <flake>#attr` evaluates cppnix's own
// `call-flake.nix` applied to a lock file, an overrides set and an internal
// primop, none of which the VM can produce for itself because locking a flake
// is IO and policy that stays in the embedder.
//
// Deliberately three general calls and not one `ixe_call_flake`. Nothing here
// knows what a flake is: one applies a function to an argument, one decodes a
// JSON document, one looks up an internal primop by name. Each is the handle
// API's version of something cppnix already has (`callFunction`,
// `parseJSON`, `internalPrimOps`), and a flake is what the *bridge* builds
// out of them.
//
// None of these can reach the memo table. `ixe_session_eval_question` is the
// only call that files a result, keyed on the source it was handed; a handle
// produced here is applied and forced through `force_handle`, which outside
// a question drives the VM with no recording host attached, and inside one
// is turned away by `refuse_injection_during_a_question`. So an injected
// value cannot be keyed on -- and cannot silently answer for a different one.

// The escape a JSON document uses to say "this string is a store path".
//
// JSON has no way to carry string context, and a store path handed to the
// evaluator without its own `Opaque` element is a value that prints
// correctly and has lost the dependency it stands for -- a derivation built
// from it ends up with one fewer input, silently. So a document written for
// [`ixe_alloc_json`] spells such a string `{"__storePath": "/nix/store/…"}`,
// and this is the same rule [`crate::eval::store_path_string`] applies to a
// fetched tree's `outPath`.
//
// An object carrying this key alongside any other key, or with a non-string
// value under it, is an error rather than a plain attribute set: an escape
// that quietly falls back to its literal reading is an escape that loses
// context on the day someone mistypes it.
//
// **It is honoured on this path and nowhere else, and that is a store
// integrity rule rather than a scoping preference.** A Nix program that
// could write `builtins.fromJSON ''{"__storePath": "/nix/store/..."}''` and
// get back a string carrying that path as context would be forging a
// dependency: it could name any store path as an input of a derivation
// without the evaluator ever having produced it. `bi_from_json` therefore
// calls `json_to_value`, which has never heard of this key, and the two
// decoders share only the scalar rule. `the_store_path_escape_is_not_reachable_from_user_json`
// is the guard, and it checks a value rather than the source: the shared
// helper could be swapped in without changing which function calls which.
// `STORE_PATH_ESCAPE` and `json_value_with_store_paths` moved to
// `primops_pure`, beside the `builtins.fromJSON` reader whose scalar rule
// they share. Two callers need them now: this one, and `builtins.getFlake`,
// which decodes the same overrides document from inside the VM. A copy on
// each side is two chances to disagree about what a flake input's `outPath`
// carries, and a string that lost its `Opaque` element is a derivation input
// that has silently vanished.

/// Build a value from a JSON document and hand back a handle to it.
///
/// The document's strings become Nix strings, its objects attribute sets and
/// its arrays lists, exactly as `builtins.fromJSON` reads them -- with one
/// addition the embedder needs and JSON cannot express: an object of the form
/// `{"__storePath": "/nix/store/…"}` becomes a string carrying that path as
/// its own context. See [`crate::primops_pure::STORE_PATH_ESCAPE`].
///
/// The result is already a value, not a thunk: there is nothing to defer,
/// since the whole document is data.
///
/// # Safety
/// `session` must be live; `json` must point to `json_len` readable bytes;
/// `out` must be a valid non-null pointer to write one handle through.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_alloc_json(
    session: *mut IxeSession,
    json: *const u8,
    json_len: usize,
    out: *mut u64,
) -> i32 {
    let session = session!(session);
    if json.is_null() || out.is_null() {
        return session.bad("null document or output pointer");
    }
    if let Some(status) = refuse_injection_during_a_question(session, "ixe_alloc_json") {
        return status;
    }
    // SAFETY: caller contract; json points to json_len bytes.
    let bytes = unsafe { slice::from_raw_parts(json, json_len) };
    let Ok(text) = std::str::from_utf8(bytes) else {
        return session.bad("JSON document is not UTF-8");
    };
    match json_document_value(session, text) {
        Ok(value) => {
            let handle = session.insert(Slot::value(value));
            // SAFETY: out is non-null, checked above.
            unsafe { *out = handle };
            IXE_OK
        }
        Err(status) => status,
    }
}

/// Decode one JSON document into a value, in `ixe_alloc_json`'s dialect.
///
/// The body of [`ixe_alloc_json`], shared with [`apply_arguments`]: a
/// document handed over as an argument and one handed over as a handle must
/// become the same value, including the `__storePath` escape, or the memo key
/// would be digesting text that means something else by the time it is
/// applied.
fn json_document_value(session: &mut IxeSession, text: &str) -> Result<Value, i32> {
    let doc: serde_json::Value = match serde_json::from_str(text) {
        Ok(doc) => doc,
        Err(e) => return Err(session.bad(format!("malformed JSON document: {e}"))),
    };
    // Through the same mapping every other VM failure takes, so a malformed
    // escape reports as the evaluation error it is rather than as a bad call
    // by the embedder.
    crate::primops_pure::json_value_with_store_paths(&mut session.vm, &doc)
        .map_err(|error| session.fail(&crate::eval::map_vm_error(error)))
}

/// A handle to one of cppnix's internal primops, by its registered name.
///
/// cppnix's `state.internalPrimOps` (`eval.cc:608`): a primop declared
/// `.internal = true` is in neither the `builtins` set nor the global scope,
/// so no program can name it, and the flake machinery reaches it through that
/// map instead. This is the same map with the same one member today,
/// `fetchFinalTree`, and the same rule decides membership -- `Gate::Never` in
/// the owned builtin catalogue, which is also what keeps the name out of `builtins`.
///
/// A name that is registered ordinarily is refused rather than served: it is
/// reachable as `builtins.<name>` and handing it over here as well would be a
/// second way to name the same thing, differing in whether the gate applies.
///
/// # Safety
/// `session` must be live; `name` must point to `name_len` readable bytes;
/// `out` must be a valid non-null pointer to write one handle through.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_internal_primop(
    session: *mut IxeSession,
    name: *const u8,
    name_len: usize,
    out: *mut u64,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    if let Some(status) = refuse_injection_during_a_question(session, "ixe_internal_primop") {
        return status;
    }
    let name = match unsafe { borrow_str(name, name_len) } {
        Ok(Some(name)) => name.to_owned(),
        Ok(None) => return session.bad("empty internal primop name"),
        Err(()) => return session.bad("internal primop name is not UTF-8"),
    };
    let value = match internal_primop_value(session, &name) {
        Ok(value) => value,
        Err(status) => return status,
    };
    let handle = session.insert(Slot::value(value));
    // SAFETY: out is non-null, checked above.
    unsafe { *out = handle };
    IXE_OK
}

/// Look one of cppnix's internal primops up by name, or say why not.
///
/// The body of [`ixe_internal_primop`], shared with [`apply_arguments`] so
/// that an argument naming an internal primop and a handle naming one are the
/// same lookup with the same two refusals -- rather than two spellings that
/// can come to disagree about which names exist.
fn internal_primop_value(session: &mut IxeSession, name: &str) -> Result<Value, i32> {
    if crate::builtin_catalogue::gate_of(name) != Some(crate::builtin_catalogue::Gate::Never) {
        return Err(session.bad(format!(
            "'{name}' is not one of cppnix's internal primops; an ordinarily \
             registered primop is reachable as builtins.{name}"
        )));
    }
    let Some(index) = crate::builtins::global_index(name) else {
        // A `Gate::Never` name this crate has no body for. Not a bad call by
        // the embedder, so it is a refusal with a token: the histogram is
        // where a missing internal primop should show up.
        session.last_error = Some(
            format!("cppnix's internal primop '{name}' is not implemented by this evaluator")
                .into(),
        );
        session.last_refusal = Some(crate::refusal::RefusalToken::UnimplementedBuiltin);
        return Err(IXE_ERR_UNIMPLEMENTED);
    };
    Ok(crate::builtins::mk_value(index))
}

/// Apply a function to one argument, lazily, and hand back a handle to the
/// result. cppnix's `EvalState::callFunction`, minus the forcing.
///
/// The function is forced, because "is this a function" is a question with an
/// answer now and cppnix answers it now: `forceFunction` runs before the call
/// in every caller. The *argument* is not forced and the *result* is not
/// computed -- the handle names a `PendingApply` cell, which is the same
/// thing `map`, `genList` and `mapAttrs` build, so applying a function to a
/// large structure costs nothing until something looks.
///
/// Lazy rather than eager because the handle API's whole reason for existing
/// is that selection must not force what it did not select: `nix eval
/// <flake>#lib.version` applies `call-flake.nix` to its three arguments and
/// then enters exactly `lib` and `version`. A caller wanting cppnix's eager
/// behaviour calls `ixe_force` on the result.
///
/// Curry by calling again: `f a b` is two applications, which is what the IR
/// does too.
///
/// # What an auto-call would still need
///
/// cppnix's `findAlongAttrPath` auto-calls a function it meets partway along
/// an attribute path, using the formals' defaults and any `--arg` overrides
/// (`autoCallFunction`, `eval.cc`). That is two things, and this is one of
/// them: the application. The other is reading the function's formals -- their
/// names, and which of them have defaults -- so the caller can build the set
/// to apply. The handle API cannot answer that today; `ixe_value_type` says
/// only `IXE_TYPE_FUNCTION`. (`ixe_flake_document` reads a lambda's formals
/// in-process for the one document that needs their names, and is a reader
/// of a whole file, not an accessor a caller can point at a function.)
///
/// Written down because there are two callers wanting one semantic: flake
/// output selection, which is served, and `-A` over a function root such as a
/// bare nixpkgs, which still refuses by name
/// (`auto-calling the function reached at ...`). Whoever closes the second
/// adds a formals accessor and builds the argument set from it -- through this
/// call, not beside it. Deciding *which* arguments is `autoCallFunction`'s
/// rule and belongs wherever the command layer keeps `--arg`, not here.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer to write
/// one handle through.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_apply(
    session: *mut IxeSession,
    func: u64,
    arg: u64,
    out: *mut u64,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    if let Some(status) = refuse_injection_during_a_question(session, "ixe_apply") {
        return status;
    }
    // Looked up before the function is forced, so a bad argument handle is
    // reported as one rather than after an evaluation that may itself fail.
    let Some(argument) = session.get(arg).cloned() else {
        return session.bad("unknown argument handle");
    };
    let f = match force_handle(session, func) {
        Err(status) => return status,
        Ok(value) => value,
    };
    // cppnix's `forceFunction` (`eval.cc:2505`), which passes a `__functor`
    // set as well as a lambda or a primop. Checked here rather than left to
    // the force of the result: cppnix raises at the call, and a lazy cell
    // nobody forces would otherwise turn a type error into silence.
    if !is_callable(session, &f) {
        let what = crate::value2::type_name(&f);
        return session.bad(format!(
            "attempt to call something which is not a function but {what}"
        ));
    }
    let handle = session.insert(Slot::pending(Slot::value(f), vec![argument]));
    // SAFETY: out is non-null, checked above.
    unsafe { *out = handle };
    IXE_OK
}

/// Apply the question's `--apply` expression to a value, lazily.
///
/// The one application allowed while a question is in flight, because it is
/// the one the key names: the expression's text and the directory it is
/// parsed under are in the question's fingerprint, so a row filed for this
/// question is filed for this application too. The expression is compiled
/// and forced through the question's recorder, so what it reads is in the
/// row's read set like every other force between the question and its answer.
///
/// # Safety
/// `out` must be a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_question_apply(
    session: *mut IxeSession,
    arg: u64,
    out: *mut u64,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    let Some(apply) = session.question.as_ref().and_then(|q| q.apply.clone()) else {
        return session.bad("ixe_question_apply with no --apply in the question in flight");
    };
    let Some(argument) = session.get(arg).cloned() else {
        return session.bad("unknown argument handle");
    };
    let module = match session
        .vm
        .compile(&apply.text, &apply.base, crate::compile::Origin::String)
    {
        Ok(compiled) => compiled.module,
        Err(error) => return session.fail(&crate::eval::EvalError::from(error)),
    };
    let f = {
        let mut fresh = crate::eval::JobMemo::default();
        let (vm, host, memo) = machine_and_host(session, &mut fresh);
        match crate::session::run_to_value_with(vm, &module, host, memo) {
            Ok(value) => value,
            Err(error) => return session.fail(&error),
        }
    };
    if !is_callable(session, &f) {
        // The user's expression, not the embedder's call: cppnix's
        // `callFunction` type error, in its words and class.
        let what = crate::value2::type_name(&f);
        return session.fail(&crate::eval::EvalError::eval(
            ErrKind::Eval,
            format!("attempt to call something which is not a function but {what}"),
        ));
    }
    let handle = session.insert(Slot::pending(Slot::value(f), vec![argument]));
    // SAFETY: out is non-null, checked above.
    unsafe { *out = handle };
    IXE_OK
}

/// The working directory, as the base every command-line expression is
/// parsed under (cppnix's `rootPath(".")`).
///
/// Refused rather than transliterated when it is not UTF-8: a lossy spelling
/// would put two directories under one key.
fn working_directory(session: &mut IxeSession) -> Result<String, i32> {
    let dir = match std::env::current_dir() {
        Ok(dir) => dir,
        Err(error) => return Err(session.bad(format!("working directory: {error}"))),
    };
    dir.into_os_string().into_string().map_err(|_| {
        session.last_error = Some("the working directory is not UTF-8".to_owned().into());
        session.last_refusal = Some(crate::refusal::RefusalToken::NonUtf8Boundary);
        IXE_ERR_UNIMPLEMENTED
    })
}

/// Decode the `--arg`/`--argstr` list of a question call.
///
/// # Safety
/// `args` must point to `args_len` readable [`IxeAutoArg`]s, or be null when
/// `args_len` is zero.
unsafe fn decode_auto_args(
    session: &mut IxeSession,
    args: *const IxeAutoArg,
    args_len: usize,
) -> Result<Vec<crate::session::AutoArg>, i32> {
    if args_len == 0 {
        return Ok(Vec::new());
    }
    if args.is_null() {
        return Err(session.bad("null auto-argument array with a non-zero length"));
    }
    // SAFETY: caller contract.
    let raw = unsafe { slice::from_raw_parts(args, args_len) };
    let mut out = Vec::with_capacity(args_len);
    for (n, argument) in raw.iter().enumerate() {
        // SAFETY: caller contract.
        let Ok(name) = (unsafe { bytes_str(&argument.name) }) else {
            return Err(session.bad(format!("auto-argument {n}'s name is not UTF-8")));
        };
        // SAFETY: caller contract.
        let Ok(text) = (unsafe { bytes_str(&argument.text) }) else {
            return Err(session.bad(format!("auto-argument {n} is not UTF-8")));
        };
        let value = match argument.kind {
            IXE_AUTO_ARG_EXPR => crate::session::AutoArgValue::Expr {
                text: text.to_owned(),
                base: working_directory(session)?,
            },
            IXE_AUTO_ARG_STRING => crate::session::AutoArgValue::Str(text.to_owned()),
            other => return Err(session.bad(format!("unknown auto-argument kind {other}"))),
        };
        out.push(crate::session::AutoArg {
            name: name.to_owned(),
            value,
        });
    }
    Ok(out)
}

/// Build the cells a question's applications read from.
///
/// An `--arg` is compiled now and evaluated when a formal first reads it,
/// under the root environment, which is where cppnix's `mkThunk_` puts it
/// (`MixEvalArgs::getAutoArgs`); a `--argstr` is a string value. One cell per
/// name, shared by every formal that takes it.
fn in_flight_question(
    session: &mut IxeSession,
    selection: &crate::session::Selection,
    kind: i32,
    question: crate::session::Question,
) -> Result<InFlightQuestion, i32> {
    let mut auto_args = Vec::with_capacity(selection.auto_args.len());
    for crate::session::AutoArg { name, value } in &selection.auto_args {
        let slot = match value {
            crate::session::AutoArgValue::Expr { text, base } => {
                let module = match session
                    .vm
                    .compile(text, base, crate::compile::Origin::String)
                {
                    Ok(compiled) => compiled.module,
                    Err(error) => return Err(session.fail(&crate::eval::EvalError::from(error))),
                };
                let entry = module.entry;
                Slot::thunk(module, entry, Rc::new(crate::value2::EnvNode::Root))
            }
            crate::session::AutoArgValue::Str(text) => {
                Slot::value(Value::Str(crate::value2::NixStr::from(text.as_str())))
            }
        };
        auto_args.push((name.clone(), slot));
    }
    Ok(InFlightQuestion {
        question,
        kind,
        flake_dir: None,
        apply: selection.apply.clone(),
        auto_args,
    })
}

/// The question in flight is one of `kind`, or the accessor `who` is refused:
/// no question at all, or a question of another kind whose memo row the
/// accessor's answer would otherwise be filed under.
fn require_question(session: &mut IxeSession, kind: i32, who: &str) -> Result<(), i32> {
    match session.question.as_ref() {
        Some(question) if question.kind == kind => Ok(()),
        Some(question) => {
            let found = question.kind;
            Err(session.bad(format!(
                "{who} under a question of kind {found}, not {kind}"
            )))
        }
        None => Err(session.bad(format!("{who} with no question in flight"))),
    }
}

/// The cell bound to `name` by the question in flight, if any.
fn auto_arg_named(session: &IxeSession, name: &str) -> Option<Slot> {
    session
        .question
        .as_ref()
        .and_then(|q| q.auto_args.iter().find(|(n, _)| n == name))
        .map(|(_, cell)| cell.clone())
}

/// cppnix's `autoCallFunction` (`eval.cc`) on one cell, under the question's
/// `--arg`/`--argstr`.
///
/// A set with a `__functor` is applied to itself and the result treated the
/// same way; a lambda with formals is applied to the set its formals select
/// from the arguments -- every argument under an ellipsis, otherwise each
/// formal's own or its default, and a formal with neither is the
/// missing-argument error in cppnix's words; anything else is handed back
/// untouched. The application is lazy, as [`ixe_apply`]'s is: the caller
/// forces where cppnix forces.
fn auto_call(session: &mut IxeSession, slot: Slot) -> Result<Slot, i32> {
    let mut current = slot;
    loop {
        let value = force_slot(session, current.clone())?;
        match &value {
            Value::Attrs(attrs) => {
                let functor = session.vm.intern("__functor");
                let Some(f) = attrs.get(&functor).cloned() else {
                    return Ok(current);
                };
                let applied = Slot::pending(f, vec![current.clone()]);
                // cppnix forces the functor's result before looking at it
                // again, so a functor that fails does so here.
                force_slot(session, applied.clone())?;
                current = applied;
            }
            Value::Closure(closure) => {
                let Some(unit) = closure.module.units.get(closure.unit as usize) else {
                    return Err(session.bad("internal: bad closure unit"));
                };
                let Some(crate::ir::Param::Formals {
                    fields, ellipsis, ..
                }) = &unit.param
                else {
                    return Ok(current);
                };
                let mut map = BTreeMap::new();
                if *ellipsis {
                    // Everything on offer: an ellipsis accepts what it does
                    // not name.
                    let given: Vec<(String, Slot)> = session
                        .question
                        .as_ref()
                        .map(|q| q.auto_args.clone())
                        .unwrap_or_default();
                    for (name, cell) in given {
                        map.insert(session.vm.intern(&name), cell);
                    }
                } else {
                    for formal in fields {
                        let name = closure
                            .module
                            .symbols
                            .get(formal.sym as usize)
                            .cloned()
                            .unwrap_or_default();
                        match (auto_arg_named(session, &name), formal.default) {
                            (Some(cell), _) => {
                                map.insert(session.vm.intern(&name), cell);
                            }
                            (None, Some(_)) => {}
                            (None, None) => {
                                let pos = (formal.pos != crate::ir::NO_POS)
                                    .then(|| closure.module.line_col(formal.pos))
                                    .flatten()
                                    .map(|(line, column)| crate::vm::SrcPos {
                                        file: match &closure.module.origin {
                                            crate::ir::SrcOrigin::File(path) => {
                                                Some(Rc::from(path.as_str()))
                                            }
                                            crate::ir::SrcOrigin::String => None,
                                        },
                                        line,
                                        column,
                                    });
                                return Err(session.fail(&crate::eval::EvalError::Eval(
                                    ErrKind::MissingArgument,
                                    format!(
                                        "cannot evaluate a function that has an argument without a value ('{name}')\n\
                                         Nix attempted to evaluate a function as a top level expression; in\n\
                                         this case it must have its arguments supplied either by default\n\
                                         values, or passed explicitly with '--arg' or '--argstr'. See\n\
                                         https://nix.dev/manual/nix/stable/language/syntax.html#functions."
                                    ),
                                    pos,
                                )));
                            }
                        }
                    }
                }
                let argument = Slot::value(Value::Attrs(Rc::new(crate::value2::Attrs::new(map))));
                return Ok(Slot::pending(current, vec![argument]));
            }
            _ => return Ok(current),
        }
    }
}

/// Auto-call a value the way cppnix's `findAlongAttrPath`, `processExpr` and
/// `getDerivations` do, under the `--arg`/`--argstr` of the question in
/// flight, and hand back the result: an application when the value took one
/// (lazy; force it where cppnix does), the value itself otherwise.
///
/// Refused outside a question: the arguments are in the key of the question
/// that carried them, and applying them under any other would put a value in
/// a row whose key does not name it.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer to write
/// one handle through.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_auto_call(session: *mut IxeSession, value: u64, out: *mut u64) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    if session.question.is_none() {
        return session.bad("ixe_auto_call with no question in flight");
    }
    let Some(slot) = session.get(value).cloned() else {
        return session.bad("unknown value handle");
    };
    let called = match auto_call(session, slot) {
        Ok(called) => called,
        Err(status) => return status,
    };
    let handle = session.insert(called);
    // SAFETY: out is non-null, checked above.
    unsafe { *out = handle };
    IXE_OK
}

/// One record of a derivation-set answer: what `nix-build` builds.
struct FoundDerivation {
    drv_path: String,
    output_name: String,
}

/// One step of the derivation walk, on an explicit stack so a set nested as
/// deep as `max-call-depth` allows cannot exhaust this thread's stack the
/// way recursion would.
enum Visit {
    /// `getDerivations`: auto-call, then the derivation test, then the
    /// walk of a set or a list.
    Walk { slot: Slot, depth: u32 },
    /// One attribute of a set: `getDerivation`, then descend only into a set
    /// that asks for it (`recurseForDerivations`).
    Attr { slot: Slot, depth: u32 },
    /// One element of a list: `getDerivation`, then `getDerivations` on
    /// anything that was not one.
    Elem { slot: Slot, depth: u32 },
}

/// The names the derivation walk reads, interned once.
struct DerivationWalkNames {
    type_: crate::value2::Sym,
    name: crate::value2::Sym,
    drv_path: crate::value2::Sym,
    output_name: crate::value2::Sym,
    recurse: crate::value2::Sym,
    combine: crate::value2::Sym,
}

/// cppnix's `getDerivation` (`get-drvs.cc`) on one cell: record the value if
/// it is a derivation not yet seen, and say whether the caller should look
/// inside it (`false` for any derivation, seen or not).
fn get_derivation(
    session: &mut IxeSession,
    names: &DerivationWalkNames,
    done: &mut BTreeSet<usize>,
    found: &mut Vec<FoundDerivation>,
    slot: &Slot,
) -> Result<(bool, Value), i32> {
    let value = force_slot(session, slot.clone())?;
    let Value::Attrs(attrs) = &value else {
        return Ok((true, value));
    };
    let Some(type_slot) = attrs.get(&names.type_).cloned() else {
        return Ok((true, value));
    };
    // `isDerivation`: a `type` that is not the string "derivation" -- or
    // not a string -- is not one, without complaint.
    let is_derivation = matches!(
        force_slot(session, type_slot)?,
        Value::Str(s) if s.as_str() == Some("derivation")
    );
    if !is_derivation {
        return Ok((true, value));
    }
    // `Done`: the set's identity, so `rec { x = derivation ..; y = x; }`
    // is one derivation.
    if !done.insert(Rc::as_ptr(attrs) as usize) {
        return Ok((false, value));
    }
    let Some(name_slot) = attrs.get(&names.name).cloned() else {
        return Err(session.fail(&crate::eval::EvalError::eval(
            ErrKind::Eval,
            "derivation name missing",
        )));
    };
    force_string_no_context(
        session,
        name_slot,
        "while evaluating the 'name' attribute of a derivation",
    )?;
    let drv_path = match attrs.get(&names.drv_path).cloned() {
        Some(slot) => match force_slot(session, slot)? {
            Value::Str(s) => s.as_str().unwrap_or_default().to_owned(),
            Value::Path(p) => AsRef::<str>::as_ref(&*p).to_owned(),
            other => {
                return Err(session.fail(&crate::eval::EvalError::eval(
                    ErrKind::Eval,
                    format!(
                        "cannot coerce {} to a string: while evaluating the 'drvPath' attribute of a derivation",
                        crate::value2::type_name(&other)
                    ),
                )));
            }
        },
        None => {
            return Err(session.fail(&crate::eval::EvalError::eval(
                ErrKind::Eval,
                "derivation does not contain a 'drvPath' attribute",
            )));
        }
    };
    let output_name = match attrs.get(&names.output_name).cloned() {
        Some(slot) => force_string_no_context(
            session,
            slot,
            "while evaluating the output name of a derivation",
        )?,
        None => String::new(),
    };
    found.push(FoundDerivation {
        drv_path,
        output_name,
    });
    Ok((false, value))
}

/// cppnix's `getDerivations` (`get-drvs.cc`), from `root`, in its order.
///
/// Auto-called at every level, attributes in name order and only those that
/// spell a path component, a set entered when it carries
/// `recurseForDerivations = true` or when its parent carries
/// `_combineChannels`, every element of a list, and each derivation once
/// however many names reach it. A derivation's `name` is forced as cppnix's
/// `queryName` forces it, so a name that fails fails the walk here too.
fn derivation_set(session: &mut IxeSession, root: Slot) -> Result<Vec<FoundDerivation>, i32> {
    let max_depth = session.vm.settings().max_call_depth;
    let names = DerivationWalkNames {
        type_: session.vm.intern("type"),
        name: session.vm.intern("name"),
        drv_path: session.vm.intern("drvPath"),
        output_name: session.vm.intern("outputName"),
        recurse: session.vm.intern("recurseForDerivations"),
        combine: session.vm.intern("_combineChannels"),
    };
    let mut found = Vec::new();
    let mut done: BTreeSet<usize> = BTreeSet::new();
    let mut stack = vec![Visit::Walk {
        slot: root,
        depth: 0,
    }];
    while let Some(visit) = stack.pop() {
        match visit {
            Visit::Walk { slot, depth } => {
                // `addCallDepth`, with cppnix's message.
                if depth > max_depth {
                    return Err(session.fail(&crate::eval::EvalError::eval(
                        ErrKind::Eval,
                        "stack overflow; max-call-depth exceeded",
                    )));
                }
                let called = auto_call(session, slot)?;
                let (look_inside, value) =
                    get_derivation(session, &names, &mut done, &mut found, &called)?;
                if !look_inside {
                    continue;
                }
                match &value {
                    Value::Attrs(attrs) => {
                        let combine = attrs.contains_key(&names.combine);
                        let mut children: Vec<(String, Slot)> = attrs
                            .iter()
                            .map(|(sym, child)| {
                                (session.vm.sym_name(*sym).to_owned(), child.clone())
                            })
                            .filter(|(name, _)| is_attr_path_component(name))
                            .collect();
                        children.sort_unstable_by(|a, b| a.0.cmp(&b.0));
                        // Reversed onto the stack, so the first name pops
                        // first and its descent runs before its sibling.
                        for (_, child) in children.into_iter().rev() {
                            stack.push(if combine {
                                Visit::Walk {
                                    slot: child,
                                    depth: depth + 1,
                                }
                            } else {
                                Visit::Attr { slot: child, depth }
                            });
                        }
                    }
                    Value::List(items) => {
                        for item in items.iter().rev() {
                            stack.push(Visit::Elem {
                                slot: item.clone(),
                                depth,
                            });
                        }
                    }
                    _ => {
                        return Err(session.fail(&crate::eval::EvalError::eval(
                            ErrKind::Eval,
                            "expression does not evaluate to a derivation (or a set or list of those)",
                        )));
                    }
                }
            }
            Visit::Attr { slot, depth } => {
                let (look_inside, value) =
                    get_derivation(session, &names, &mut done, &mut found, &slot)?;
                if !look_inside {
                    continue;
                }
                let Value::Attrs(attrs) = &value else {
                    continue;
                };
                let Some(flag) = attrs.get(&names.recurse).cloned() else {
                    continue;
                };
                let recurse = match force_slot(session, flag)? {
                    Value::Bool(b) => b,
                    other => {
                        return Err(session.fail(&crate::eval::EvalError::eval(
                            ErrKind::Eval,
                            format!(
                                "expected a Boolean but found {}: while evaluating the attribute `recurseForDerivations`",
                                crate::value2::type_name(&other)
                            ),
                        )));
                    }
                };
                if recurse {
                    stack.push(Visit::Walk {
                        slot,
                        depth: depth + 1,
                    });
                }
            }
            Visit::Elem { slot, depth } => {
                let (look_inside, _) =
                    get_derivation(session, &names, &mut done, &mut found, &slot)?;
                if look_inside {
                    stack.push(Visit::Walk {
                        slot,
                        depth: depth + 1,
                    });
                }
            }
        }
    }
    Ok(found)
}

/// cppnix's `forceStringNoCtx`: the string, refusing one that carries a
/// store-path context, in cppnix's words.
fn force_string_no_context(
    session: &mut IxeSession,
    slot: Slot,
    what: &str,
) -> Result<String, i32> {
    match force_slot(session, slot)? {
        Value::Str(s) => {
            let text = s.as_str().unwrap_or_default().to_owned();
            if s.has_context() {
                return Err(session.fail(&crate::eval::EvalError::eval(
                    ErrKind::Eval,
                    format!("the string '{text}' is not allowed to refer to a store path: {what}"),
                )));
            }
            Ok(text)
        }
        other => Err(session.fail(&crate::eval::EvalError::eval(
            ErrKind::Eval,
            format!(
                "expected a string but found {}: {what}",
                crate::value2::type_name(&other)
            ),
        ))),
    }
}

/// cppnix's `isAttrPathComponent` (`get-drvs.cc`): `[A-Za-z_][A-Za-z0-9_+-]*`.
fn is_attr_path_component(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '+')
}

/// Encode the records of a derivation-set answer.
///
/// Netstrings, two per record: `<len>:<drvPath>,<len>:<outputName>,`. Every
/// field is length-delimited, so a name carrying a tab or a newline is one
/// field of that length and not two records; a decoder that reaches the end
/// mid-field has a malformed answer, not a short one. Records concatenate,
/// which is what lets one answer hold the walks of several `-A` paths.
fn encode_derivation_set(found: &[FoundDerivation]) -> String {
    let mut out = String::new();
    for record in found {
        for field in [&record.drv_path, &record.output_name] {
            out.push_str(&field.len().to_string());
            out.push(':');
            out.push_str(field);
            out.push(',');
        }
    }
    out
}

/// Walk `root` for derivations the way `nix-build` does (cppnix's
/// `getDerivations`, `get-drvs.cc`), under the question in flight, and hand
/// back the records found, encoded as [`encode_derivation_set`] says. The
/// caller frees the string with `ixe_string_free`.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer to write
/// one string through.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_derivation_set(
    session: *mut IxeSession,
    root: u64,
    out: *mut *mut c_char,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    if let Err(status) =
        require_question(session, IXE_QUESTION_DERIVATION_SET, "ixe_derivation_set")
    {
        return status;
    }
    let Some(slot) = session.get(root).cloned() else {
        return session.bad("unknown value handle");
    };
    let found = match derivation_set(session, slot) {
        Ok(found) => found,
        Err(status) => return status,
    };
    out_string(encode_derivation_set(&found), out)
}

/// cppnix's `readFlake` over `root`, under the question in flight: the
/// document [`crate::flake_doc::flake_document`] describes, as one JSON
/// string the caller frees with `ixe_string_free`.
///
/// Under the question, so every force the reader performs -- and it forces
/// only what `forceTrivialValue` would -- lands in the row's read set, and a
/// later run is served the document without touching the file.
///
/// # Safety
/// `session` must be live; `out` must be a valid non-null pointer to write
/// one string through.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_flake_document(
    session: *mut IxeSession,
    root: u64,
    out: *mut *mut c_char,
) -> i32 {
    let session = session!(session);
    if out.is_null() {
        return session.bad("null output pointer");
    }
    if let Err(status) =
        require_question(session, IXE_QUESTION_FLAKE_DOCUMENT, "ixe_flake_document")
    {
        return status;
    }
    let Some(slot) = session.get(root).cloned() else {
        return session.bad("unknown value handle");
    };
    let Some(flake_dir) = session.question.as_ref().and_then(|q| q.flake_dir.clone()) else {
        return session.bad("ixe_flake_document under a document question with no flake directory");
    };
    let document = {
        let mut fresh = crate::eval::JobMemo::default();
        let (vm, host, memo) = machine_and_host(session, &mut fresh);
        crate::flake_doc::flake_document(vm, host, memo, slot, &flake_dir)
    };
    match document {
        Ok(document) => out_string(document, out),
        Err(error) => session.fail(&error),
    }
}

/// Set the root used for file-backed question sources. Its identity is part
/// of the compiled module and therefore of every result-cache key.
///
/// # Safety
/// `session` must be live and `root` must name `root_len` readable bytes,
/// or be null for the ambient filesystem.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_session_set_source_root(
    session: *mut IxeSession,
    root: *const u8,
    root_len: usize,
) -> i32 {
    let session = session!(session);
    if session.question.is_some() || session.memo.is_some() {
        return session.bad("cannot change source root during a question");
    }
    // SAFETY: caller provides the bytes described above.
    let name = match unsafe { borrow_str(root, root_len) } {
        Ok(name) => name.unwrap_or(""),
        Err(()) => return session.bad("source root is not UTF-8"),
    };
    match crate::value2::Root::from_wire_name(name) {
        Ok(root) => {
            session.source_root = root;
            IXE_OK
        }
        Err(error) => session.bad(error),
    }
}

/// Borrow an optional UTF-8 string from a pointer and length.
///
/// # Safety
/// `p` must point to `len` readable bytes, or be null.
unsafe fn borrow_str<'a>(p: *const u8, len: usize) -> Result<Option<&'a str>, ()> {
    if p.is_null() || len == 0 {
        return Ok(None);
    }
    // SAFETY: caller contract above.
    let bytes = unsafe { slice::from_raw_parts(p, len) };
    std::str::from_utf8(bytes).map(Some).map_err(|_| ())
}

/// Whether an entry point that configures the evaluator can change what an
/// evaluation answers, and if it can, where that is accounted for.
///
/// Every entry here is a claim somebody made deliberately. The gate below
/// checks the list against the source, so a new one cannot ship without an
/// answer.
#[cfg(test)]
#[derive(Debug)]
enum Accounting {
    /// Folded into the memo key through [`crate::eval::Settings`], so two
    /// values of it cannot share a cached answer.
    InKey,
    /// Cannot change an answer, for the stated reason. The reason is the
    /// point: "it does not matter" written down is reviewable, and the same
    /// words unwritten are how `store_dir` stayed out of the key while being
    /// hashed into every `outPath` (ENG-12541).
    CannotChangeAnAnswer(&'static str),
}

/// One row per configuring entry point. See [`MUTATING_PREFIXES`] for what
/// the gate counts as one.
#[cfg(test)]
const SETTER_ACCOUNTING: &[(&str, Accounting)] = &[
    ("ixe_set_max_call_depth", Accounting::InKey),
    ("ixe_set_nix_version", Accounting::InKey),
    ("ixe_set_host_build_identity", Accounting::InKey),
    ("ixe_set_store_dir", Accounting::InKey),
    ("ixe_set_current_system", Accounting::InKey),
    ("ixe_set_pure_eval", Accounting::InKey),
    ("ixe_set_restrict_eval", Accounting::InKey),
    ("ixe_set_builtin_features", Accounting::InKey),
    ("ixe_set_home_dir", Accounting::InKey),
    // Not a setter, and in this table because it carries what the fourteen
    // deleted `ixe_set_*` hook installers used to: the host vtable. Only one
    // thing about that vtable can change an answer -- whether the six
    // filesystem reads go through the embedder's accessor, which decides what
    // `pure-eval` refuses -- and `settings_for` folds exactly that into
    // `Settings::path_reads`. Everything else the vtable carries is a hook
    // whose answers are recorded and replayed, which is the argument the
    // deleted rows made one at a time.
    ("ixe_session_new", Accounting::InKey),
    (
        "ixe_session_set_eval_cache",
        Accounting::CannotChangeAnAnswer(
            "attaches only bounded decoded dependency witnesses, which are replayed \
             against the current host before any result can be served. No VM value \
             or host context is retained, and replacement during a question is refused.",
        ),
    ),
    (
        "ixe_set_eval_cache_dir",
        Accounting::CannotChangeAnAnswer(
            "this is the cache itself. Keying the cache on its own location \
             would give every directory a private cache and defeat the point; \
             what stops it changing an answer is every other setting being in \
             the key, plus the tests in tests/cache_semantics.rs.",
        ),
    ),
    (
        "ixe_set_cache_verify_rate",
        Accounting::CannotChangeAnAnswer(
            "how often the cache checks itself against a fresh evaluation. It \
             changes what gets *reported*, never what gets returned: on \
             agreement the served answer is returned unchanged, and on \
             disagreement the served answer is still returned and the \
             difference is complained about, because a verifier that silently \
             substituted its own result would hide the bug it exists to find.",
        ),
    ),
    (
        "ixe_set_eval_cache_max_bytes",
        Accounting::CannotChangeAnAnswer(
            "the cache's byte cap. A sweep removes completed entries, and every \
             entry is Policy::Keyed, so removing one can only make the next \
             lookup a miss that re-performs; nothing it removes was ever the \
             answer, only a way to reach it.",
        ),
    ),
    // The trace family's two halves, and they are opposite kinds of thing.
    // The vtable's `warn` and `trace` say where a line goes, which cannot
    // change a value; these two decide whether the expression has a value at
    // all, which is why they are the only members of the family in the key.
    ("ixe_set_trace_verbose", Accounting::InKey),
    ("ixe_set_abort_on_warn", Accounting::InKey),
    ("ixe_set_ca_derivations", Accounting::InKey),
    ("ixe_set_blake3_hashes", Accounting::InKey),
    // Host policy the witness verifier would otherwise serve across: a
    // realisation or fetch served by validity never reaches the host check
    // that enforces these, so the key carries them.
    ("ixe_set_allow_import_from_derivation", Accounting::InKey),
    ("ixe_set_allowed_uris", Accounting::InKey),
    (
        "ixe_set_repair",
        Accounting::CannotChangeAnAnswer(
            "it turns the memo off: nothing is served under --repair, so no answer is shared across it",
        ),
    ),
    // The three parser lints and the pipe-operators feature are compile-time
    // settings: each decides what a module compiles to (a fatal lint makes
    // the linted literal a compile error, the feature makes `|>` a call
    // rather than an error), so each is in the key. Only fatal-ness of a
    // lint is folded -- `warn` and `ignore` compile identically; the
    // fingerprint says why.
    ("ixe_set_lint_url_literals", Accounting::InKey),
    ("ixe_set_lint_short_path_literals", Accounting::InKey),
    ("ixe_set_lint_absolute_path_literals", Accounting::InKey),
    ("ixe_set_pipe_operators", Accounting::InKey),
    ("ixe_set_parse_toml_timestamps", Accounting::InKey),
    // The three value-building entry points. They are in this table because
    // the scan was widened to catch them, and the scan was widened because
    // "does this change what an evaluation answers" is a question a new
    // entry point should have to answer even when its name is not `set`.
    //
    // The answer is the same for all three and it is enforced rather than
    // promised: each refuses with `IXE_ERR_BADCALL` while a question is in
    // flight (`refuse_injection_during_a_question`), so a value built through
    // one of them is never inside the window whose forces are recorded and
    // whose result is filed. Outside that window nothing is recorded and
    // nothing recorded can be served in its place.
    //
    // The previous justification here was that `force_handle` drives against
    // a host with no recording wrapper. That was true only because
    // `mayBeMemoised` kept the one caller with arguments away from the memo
    // table; `force_handle` goes through `machine_and_host` like everything
    // else, so it records whenever a question is open. The refusal is what
    // makes the row true on its own terms. ENG-12915.
    //
    // `a_value_cannot_be_injected_while_a_question_is_in_flight` and
    // `no_memo_entry_is_written_while_forcing_a_handle` are the guards.
    (
        "ixe_alloc_json",
        Accounting::CannotChangeAnAnswer(
            "builds a value in one session's handle table from a document the \
             embedder supplies. Refused while a question is in flight, so it \
             cannot reach a memo row; a document that must reach one is passed \
             as an argument to `ixe_session_eval_question`, which digests it \
             into the key.",
        ),
    ),
    (
        "ixe_internal_primop",
        Accounting::CannotChangeAnAnswer(
            "hands back a value naming a builtin this crate already has, by a \
             name no program can write. It adds no behaviour -- the same body \
             runs whether it is reached from here or from the table -- and it \
             is refused while a question is in flight, where the name would \
             instead be an `IXE_ARG_INTERNAL_PRIMOP` argument and in the key.",
        ),
    ),
    (
        "ixe_apply",
        Accounting::CannotChangeAnAnswer(
            "applies one handle to another, outside any question. Refused \
             while one is in flight: the arguments an evaluand is applied to \
             are part of what its answer depends on, so they cross on the \
             question call and are keyed on there.",
        ),
    ),
];

#[cfg(test)]
mod setter_accounting_tests {
    use super::{Accounting, SETTER_ACCOUNTING};

    /// Every `ixe_set_*` in this file is accounted for, and every row in the
    /// table names one that exists.
    ///
    /// Read from the source rather than from a list somebody keeps in step,
    /// because a list somebody keeps in step is the thing that fell behind:
    /// `store_dir`, `nix_version` and the call-depth ceiling were all added as
    /// settings and none reached the memo key (ENG-12541).
    /// The verbs that change what the evaluator will do.
    /// The entry points that must carry a row: everything that changes what
    /// the evaluator will do, and everything that puts a value into a
    /// session. Prefixes where there is a verb, whole names where there is
    /// not.
    ///
    /// `ixe_alloc_`, `ixe_apply` and `ixe_internal_primop` were added when
    /// the value-building surface landed, which is part of the residual
    /// weakness the comment below names: a mutator whose name follows no
    /// convention escapes a scan keyed on conventions. It is smaller now and
    /// still not closed.
    const MUTATING_PREFIXES: &[&str] = &[
        "ixe_set_",
        "ixe_add_",
        "ixe_clear_",
        "ixe_alloc_",
        "ixe_apply",
        "ixe_internal_primop",
        // Not a verb, and the one entry point that hands the evaluator its
        // whole view of the world outside it. It was `ixe_set_*` fourteen
        // times over until the host became a per-session vtable; the question
        // this table asks did not stop applying just because the spelling
        // changed.
        "ixe_session_new",
        "ixe_session_set_eval_cache",
    ];

    #[test]
    fn every_capi_setter_is_accounted_for() {
        let source = include_str!("capi.rs");
        let mut found: Vec<&str> = Vec::new();
        for line in source.lines() {
            let line = line.trim_start();
            let Some(rest) = line
                .strip_prefix("pub extern \"C\" fn ")
                .or_else(|| line.strip_prefix("pub unsafe extern \"C\" fn "))
            else {
                continue;
            };
            let Some(name) = rest.split('(').next() else {
                continue;
            };
            // Every mutating verb, not just `ixe_set_`. The scan used to key
            // on that one prefix, and the virtual-file registration (since
            // folded into the findFile answer) -- which decided what
            // `<nix/fetchurl.nix>` evaluates to -- sailed past it unasked,
            // because it was spelled `add`. A gate that keys on a naming
            // convention only covers the surfaces that follow it.
            //
            // Known residual weakness, stated rather than papered over: a
            // mutator named outside these verbs still escapes. The complete
            // fix is to require every `ixe_*` export to be either accounted
            // for or declared a read-only accessor, which forces a decision
            // on any new entry point whatever it is called. That is a bigger
            // table than this change should carry; ENG-12546 follow-up.
            if MUTATING_PREFIXES.iter().any(|p| name.starts_with(p)) {
                found.push(name);
            }
        }
        assert!(
            found.len() >= 8,
            "the scanner found only {found:?}; it has stopped matching the \
             declarations it is meant to enumerate, which would let this pass \
             while finding nothing"
        );

        let declared: Vec<&str> = SETTER_ACCOUNTING.iter().map(|(name, _)| *name).collect();
        for name in &found {
            assert!(
                declared.contains(name),
                "{name} is a new evaluator setting with no entry in \
                 SETTER_ACCOUNTING. Say whether it belongs in the memo key \
                 (crate::eval::Settings) or write down why it cannot change an \
                 answer."
            );
        }
        for name in &declared {
            assert!(
                found.contains(name),
                "SETTER_ACCOUNTING names {name}, which no longer exists"
            );
        }
    }

    /// A justification has to say something. An empty string, or a couple of
    /// words, is the shape of a row added to silence the gate above.
    #[test]
    fn every_exemption_carries_a_written_reason() {
        for (name, accounting) in SETTER_ACCOUNTING {
            if let Accounting::CannotChangeAnAnswer(reason) = accounting {
                assert!(
                    reason.split_whitespace().count() >= 10,
                    "{name}'s exemption is too thin to review: {reason:?}"
                );
            }
        }
    }

    /// Everything marked `InKey` really is. The list of settings lives in
    /// `crate::eval::Settings`, whose `fingerprint` is held field by field by
    /// `eval::tests::every_setting_is_in_the_memo_key`; this checks the two
    /// lists are the same length, so a setter marked `InKey` against a
    /// `Settings` that has no such field is caught here.
    #[test]
    fn the_in_key_setters_match_the_settings_struct() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let in_key = SETTER_ACCOUNTING
            .iter()
            .filter(|(_, a)| matches!(a, Accounting::InKey))
            .count();
        // Destructuring names every field, so a field added to `Settings`
        // fails to compile here until the count below is revisited.
        let crate::eval::Settings {
            store_dir: _,
            nix_version: _,
            host_build_identity: _,
            current_system: _,
            max_call_depth: _,
            pure_eval: _,
            restrict_eval: _,
            builtin_features: _,
            path_reads: _,
            trace_verbose: _,
            abort_on_warn: _,
            home_dir: _,
            ca_derivations: _,
            blake3_hashes: _,
            lint_url_literals: _,
            lint_short_path_literals: _,
            lint_absolute_path_literals: _,
            pipe_operators: _,
            parse_toml_timestamps: _,
            allow_import_from_derivation: _,
            allowed_uris: _,
            repair: _,
        } = crate::eval::Settings::current();
        assert_eq!(
            in_key, 21,
            "the number of settings in the memo key changed; update this \
             count and check `Settings` gained or lost the matching field"
        );
    }
}

/// The C ABI of the seven filesystem reads, exercised the way the bridge calls
/// it: raw pointers, out parameters, a partial set refused.
///
/// # No guard, and that is the change worth noticing
///
/// These hooks used to be process globals, so every test here held
/// `globals_moving()` and cleared the hooks in a `Drop` -- because clearing on
/// the last line is skipped by a panic, and the first failure in this module
/// otherwise left a fake filesystem installed for every later test in the
/// process. That happened: one real failure here, then
/// `virtual_file_tests::a_registered_file_is_read_from_memory_and_looks_like_a_file`
/// failing for no reason of its own, three times over.
///
/// A fake filesystem is a local value now. Nothing installs it, nothing has to
/// remove it, and no other test in the process can see it, so the guard, the
/// `Drop` and the exclusion they bought are all gone.
#[cfg(test)]
mod path_read_tests {
    use super::{
        EmbedderHost, FileTypeFn, FindFileFn, IXE_OK, ImportFn, IxeHostVtable, PathExistsFn,
        ReadDirFn, ReadFileFn, decode_dir_entries, decode_file_type, decode_imported_source, settings_for,
    };
    use crate::host::{FileType, Host};
    use std::ffi::c_void;

    unsafe extern "C" fn fake_store_path(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        _path: *const u8,
        _path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        const ANSWER: &[u8] = b"/nix/store/00000000000000000000000000000000-source\0/nix/store/00000000000000000000000000000000-source/sub";
        unsafe {
            *out = ANSWER.as_ptr();
            *out_len = ANSWER.len();
        }
        0
    }

    /// Answers for one fixed path and reports every other as missing, in
    /// cppnix's own wording, which is what the real accessor produces.
    unsafe extern "C" fn fake_read_file(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        path: *const u8,
        path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        // SAFETY: the ABI's contract -- `path_len` readable bytes at `path`.
        let asked =
            unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(path, path_len)) };
        let (answer, rc): (&'static str, i32) = if asked == "/bridged/hello.nix" {
            ("{ a = 1; }", 0)
        } else {
            (
                "access to absolute path '/nope' is forbidden in pure evaluation mode",
                1,
            )
        };
        // SAFETY: the caller copies the bytes before returning, so a 'static
        // buffer outlives the call by construction.
        unsafe {
            *out = answer.as_ptr();
            *out_len = answer.len();
        }
        rc
    }

    unsafe extern "C" fn fake_import(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        _path: *const u8,
        _path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        const ANSWER: &[u8] = b"\0/bridged/hello.nix\0{ a = 1; }";
        unsafe {
            *out = ANSWER.as_ptr();
            *out_len = ANSWER.len();
        }
        0
    }

    unsafe extern "C" fn fake_path_exists(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        path: *const u8,
        path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        // SAFETY: as above.
        let asked =
            unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(path, path_len)) };
        let answer: &'static str = if asked == "/bridged/hello.nix" {
            "1"
        } else {
            "0"
        };
        unsafe {
            *out = answer.as_ptr();
            *out_len = answer.len();
        }
        0
    }

    unsafe extern "C" fn fake_read_dir(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        _path: *const u8,
        _path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        const ANSWER: &[u8] = b"hello.nix\0regular\0sub\0directory\0";
        // SAFETY: as above.
        unsafe {
            *out = ANSWER.as_ptr();
            *out_len = ANSWER.len();
        }
        0
    }

    unsafe extern "C" fn fake_file_type(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        _path: *const u8,
        _path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        const ANSWER: &str = "regular";
        // SAFETY: as above.
        unsafe {
            *out = ANSWER.as_ptr();
            *out_len = ANSWER.len();
        }
        0
    }

    /// The resolving half of the pair, and deliberately disagreeing with
    /// `fake_file_type` on one path. A symlink to a directory is `symlink`
    /// under `lstat` and `directory` under `stat`, and that is the whole
    /// difference between `builtins.readFileType` and the directory test
    /// inside an `import`. A test where the two hooks answer the same thing
    /// cannot tell which one `resolve_import` asked.
    unsafe extern "C" fn fake_file_type_resolved(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        path: *const u8,
        path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        // SAFETY: as above; the caller passes a live buffer of `path_len`.
        let asked = unsafe { std::slice::from_raw_parts(path, path_len) };
        let answer: &'static str = if asked == b"/bridged/link-to-dir" {
            "directory"
        } else {
            "regular"
        };
        // SAFETY: as above.
        unsafe {
            *out = answer.as_ptr();
            *out_len = answer.len();
        }
        0
    }

    unsafe extern "C" fn fake_import_derivation(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        _path: *const u8,
        _path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        const ANSWER: &[u8] = b"derivation\0{\"path\":\"/nix/store/11111111111111111111111111111111-demo.drv\",\"name\":\"demo\",\"outputs\":{\"out\":\"/nix/store/22222222222222222222222222222222-demo\",\"dev\":\"/downstream-placeholder\"}}";
        // SAFETY: the callback's live out parameters receive a static buffer.
        unsafe {
            *out = ANSWER.as_ptr();
            *out_len = ANSWER.len();
        }
        0
    }

    #[test]
    fn a_typed_drv_import_preserves_output_wrappers_and_contexts() {
        let _held = crate::eval::globals_shared();
        let host = EmbedderHost::new(IxeHostVtable {
            import_source: Some(fake_import_derivation as ImportFn),
            ..bridged()
        }).expect("complete read hooks");
        let expression = r#"
          let d = import /nix/store/11111111111111111111111111111111-demo.drv;
          in assert d.type == "derivation" && d.name == "demo";
             assert d.outputName == "dev" && d.outPath == "/downstream-placeholder";
             assert map (x: x.outputName) d.all == [ "dev" "out" ];
             assert d.out.dev.out.outputName == "out";
             assert builtins.attrNames d == [ "all" "dev" "drvPath" "name" "out" "outPath" "outputName" "type" ];
             assert builtins.getContext d.drvPath == {
               "/nix/store/11111111111111111111111111111111-demo.drv" = { allOutputs = true; };
             };
             assert builtins.getContext d.outPath == {
               "/nix/store/11111111111111111111111111111111-demo.drv" = { outputs = [ "dev" ]; };
             };
             assert builtins.getContext d.out.outPath == {
               "/nix/store/11111111111111111111111111111111-demo.drv" = { outputs = [ "out" ]; };
             };
             true
        "#;
        assert_eq!(crate::eval::render_with(&crate::eval::settings_with_store(), &host, expression), "true");
    }

    #[test]
    fn a_malformed_typed_drv_import_is_not_reparsed_as_source() {
        for bytes in [
            b"derivation\0{}".as_slice(),
            b"derivation\0{\"path\":\"/x.drv\",\"name\":\"x\",\"outputs\":{\"out\":42}}".as_slice(),
            b"derivation\0{\"path\":\"relative.drv\",\"name\":\"x\",\"outputs\":{}}".as_slice(),
        ] {
            assert!(decode_imported_source(bytes).is_err());
        }
        assert!(matches!(decode_imported_source(b"\0/a.drv\0{ answer = 42; }"),
            Ok(crate::host::ImportedSource::Nix { .. })));
    }

    /// A vtable whose seven reads are all answered by the fakes above.
    fn bridged() -> IxeHostVtable {
        IxeHostVtable {
            import_source: Some(fake_import as ImportFn),
            read_file: Some(fake_read_file as ReadFileFn),
            path_exists: Some(fake_path_exists as PathExistsFn),
            dir_exists: Some(fake_path_exists as PathExistsFn),
            read_dir: Some(fake_read_dir as ReadDirFn),
            file_type: Some(fake_file_type as FileTypeFn),
            file_type_resolved: Some(fake_file_type_resolved as FileTypeFn),
            ..IxeHostVtable::empty()
        }
    }

    /// Reports every path missing, in the accessor's own wording.
    unsafe extern "C" fn denies_everything(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        _path: *const u8,
        _path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        const ANSWER: &str = "the embedder was asked and said no";
        // SAFETY: a 'static buffer outlives the call by construction.
        unsafe {
            *out = ANSWER.as_ptr();
            *out_len = ANSWER.len();
        }
        1
    }

    unsafe extern "C" fn denies_existence(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        _path: *const u8,
        _path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        const ANSWER: &str = "0";
        unsafe {
            *out = ANSWER.as_ptr();
            *out_len = ANSWER.len();
        }
        0
    }

    unsafe extern "C" fn missing_mount(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        _path: *const u8,
        _path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        const ANSWER: &str = "mounted path root '/nix/store/gone' disappeared";
        unsafe {
            *out = ANSWER.as_ptr();
            *out_len = ANSWER.len();
        }
        1
    }

    /// The bytes a `findFile` answer carries are served by the host that
    /// received them, to every later question about the path: existence,
    /// kind, contents and the atomic import. Every read hook here refuses,
    /// so an answer can only have come from those bytes.
    ///
    /// This is the seam that was missing twice: first `EmbedderHost` did not
    /// consult the registry at all (`import <nix/fetchurl.nix>` failed with
    /// `path '/fetchurl.nix' does not exist`, the one `lang-diff` mismatch),
    /// then the registry was process-global and the atomic import bypassed
    /// it (the same import refused under pure evaluation on the real
    /// configuration). A second host from the same vtable sees nothing:
    /// the bytes are the session's.
    #[test]
    fn find_file_contents_are_served_by_the_host_that_received_them() {
        let _held = crate::eval::globals_shared();
        let vtable = IxeHostVtable {
            import_source: Some(denies_everything as ImportFn),
            read_file: Some(denies_everything as ReadFileFn),
            path_exists: Some(denies_existence as PathExistsFn),
            dir_exists: Some(denies_existence as PathExistsFn),
            read_dir: Some(denies_everything as ReadDirFn),
            file_type: Some(denies_everything as FileTypeFn),
            file_type_resolved: Some(denies_everything as FileTypeFn),
            find_file: Some(answers_fetchurl as FindFileFn),
            ..IxeHostVtable::empty()
        };
        let Ok(fs) = EmbedderHost::new(vtable) else {
            unreachable!("a complete set of read hooks has to be accepted")
        };
        let rooted = crate::value2::PathValue::ambient("/fetchurl.nix");
        assert!(
            !fs.path_exists_checked(&rooted)
                .expect("existence question failed"),
            "absent before the lookup, or this proves nothing"
        );
        assert_eq!(
            fs.find_file(&[], "nix/fetchurl.nix").ok(),
            Some(rooted.clone())
        );
        assert_eq!(
            fs.read_file(&rooted).ok().as_deref(),
            Some("{ registered = true; }")
        );
        assert!(
            fs.path_exists_checked(&rooted)
                .expect("existence question failed")
        );
        assert_eq!(fs.dir_exists_checked(&rooted), Ok(false));
        // Regular and not a directory, or `resolve_import` appends
        // `/default.nix` and turns this into a second missing path.
        assert!(matches!(
            fs.file_type(&rooted),
            Ok(Some(crate::host::FileType::Regular))
        ));
        assert!(matches!(
            fs.file_type_resolved(&rooted),
            Ok(crate::host::FileType::Regular)
        ));
        assert_eq!(
            fs.resolve_import(&rooted).ok().as_deref(),
            Some("/fetchurl.nix")
        );
        let imported = fs
            .import_source(&rooted)
            .expect("the atomic import answers from the bytes");
        assert_eq!(
            imported,
            crate::host::ImportedSource::Nix {
                path: rooted.clone(), text: "{ registered = true; }".to_owned(),
            }
        );

        let Ok(other) = EmbedderHost::new(vtable) else {
            unreachable!("a complete set of read hooks has to be accepted")
        };
        assert!(
            !other
                .path_exists_checked(&rooted)
                .expect("existence question failed"),
            "a second host from the same vtable has not been handed the bytes"
        );
    }

    /// `findFile` answering `/fetchurl.nix` with its bytes, as the C++
    /// bridge does for cppnix's in-memory corepkgs.
    unsafe extern "C" fn answers_fetchurl(
        _ctx: *mut c_void,
        _entries: *const u8,
        _entries_len: usize,
        _name: *const u8,
        _name_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        const ANSWER: &[u8] = b"\0/fetchurl.nix\0{ registered = true; }";
        // SAFETY: a 'static buffer outlives the call by construction.
        unsafe {
            *out = ANSWER.as_ptr();
            *out_len = ANSWER.len();
        }
        IXE_OK
    }

    /// The whole seam in one pass: build a host from the vtable, ask each
    /// read through it, and confirm the answer came from the hook rather than
    /// from `std::fs`. `/bridged/hello.nix` does not exist on any disk, so an
    /// answer for it can only have come through the ABI.
    #[test]
    fn the_reads_come_back_through_the_c_abi() {
        let Ok(fs) = EmbedderHost::new(bridged()) else {
            unreachable!("a complete set of read hooks has to be accepted")
        };
        assert_eq!(
            fs.read_file(&crate::value2::ambient_path("/bridged/hello.nix"))
                .ok()
                .as_deref(),
            Some("{ a = 1; }")
        );
        assert!(
            fs.path_exists_checked(&crate::value2::ambient_path("/bridged/hello.nix"))
                .expect("existence question failed")
        );
        assert!(
            !fs.path_exists_checked(&crate::value2::ambient_path("/bridged/absent.nix"))
                .expect("existence question failed")
        );
        assert_eq!(
            fs.file_type(&crate::value2::ambient_path("/bridged/hello.nix"))
                .ok(),
            Some(Some(FileType::Regular))
        );
        assert_eq!(
            fs.read_dir(&crate::value2::ambient_path("/bridged")).ok(),
            Some(vec![
                ("hello.nix".to_owned(), FileType::Regular),
                ("sub".to_owned(), FileType::Directory),
            ])
        );
        // An `import` is the *resolving* kind question and then `read_file`.
        assert_eq!(
            fs.file_type_resolved(&crate::value2::ambient_path("/bridged/hello.nix"))
                .ok(),
            Some(FileType::Regular)
        );
        assert_eq!(
            fs.resolve_import(&crate::value2::ambient_path("/bridged/hello.nix"))
                .ok()
                .map(|p| p.to_string())
                .as_deref(),
            Some("/bridged/hello.nix")
        );
        // The discriminating pair. `lstat` says regular and `stat` says
        // directory for this path, so an `import` that appends
        // `/default.nix` asked the resolving hook and one that does not asked
        // the plain one. Asking the plain one is ENG-12871.
        assert_eq!(
            fs.file_type(&crate::value2::ambient_path("/bridged/link-to-dir"))
                .ok(),
            Some(Some(FileType::Regular))
        );
        assert_eq!(
            fs.resolve_import(&crate::value2::ambient_path("/bridged/link-to-dir"))
                .ok()
                .map(|p| p.to_string())
                .as_deref(),
            Some("/bridged/link-to-dir/default.nix")
        );

        // A refusal arrives as the embedder's own text, not as this crate's.
        let Err(denied) = fs.read_file(&crate::value2::ambient_path("/bridged/absent.nix")) else {
            unreachable!("a read under pure eval must be refused")
        };
        assert!(
            denied.contains("forbidden in pure evaluation mode"),
            "the accessor's own wording did not come back: {denied}"
        );

        // And a host without them is a plain std::fs miss on the same path,
        // in the same process and at the same moment -- which is the property
        // the process globals could not have.
        let Ok(standalone) = EmbedderHost::new(IxeHostVtable::empty()) else {
            unreachable!("an empty vtable is a legitimate standalone embedding")
        };
        assert_eq!(
            standalone
                .read_file(&crate::value2::ambient_path("/bridged/hello.nix"))
                .err()
                .as_deref(),
            Some("path '/bridged/hello.nix' does not exist")
        );
        // The bridged host still answers, so the two did not share anything.
        assert_eq!(
            fs.read_file(&crate::value2::ambient_path("/bridged/hello.nix"))
                .ok()
                .as_deref(),
            Some("{ a = 1; }")
        );
    }

    #[test]
    fn an_existence_hook_error_is_not_a_negative_answer() {
        let vtable = IxeHostVtable {
            path_exists: Some(missing_mount as PathExistsFn),
            dir_exists: Some(missing_mount as PathExistsFn),
            ..bridged()
        };
        let Ok(host) = EmbedderHost::new(vtable) else {
            unreachable!("a complete read vtable is valid")
        };
        let answer = host.path_exists_checked(&crate::value2::ambient_path("/gone"));
        assert!(matches!(answer, Err(message) if message.contains("disappeared")));
        let answer = host.dir_exists_checked(&crate::value2::ambient_path("/gone/"));
        assert!(matches!(answer, Err(message) if message.contains("disappeared")));
    }

    #[test]
    fn store_path_keeps_visible_subpath_and_store_object_context_separate() {
        let vtable = IxeHostVtable {
            store_path: Some(fake_store_path),
            ..IxeHostVtable::empty()
        };
        let Ok(host) = EmbedderHost::new(vtable) else {
            unreachable!("storePath hook is valid")
        };
        let mount = "/nix/store/00000000000000000000000000000000-source";
        let path = crate::value2::PathValue::new(
            crate::value2::Root::mounted(mount),
            format!("{mount}/sub"),
        );
        assert_eq!(
            host.store_path(&path),
            Ok(crate::host::StorePathResult {
                path: format!("{mount}/sub"),
                store_path: mount.to_owned(),
            })
        );
    }

    /// A partial set is refused outright, and the session it was offered to
    /// is never created.
    ///
    /// This is what keeps [`crate::purity::PathReads`] a single fact. Three
    /// hooks supplied would mean `readFile` honouring the allow list while
    /// `readDir` walked around it, with the table saying the setting was
    /// being honoured for both.
    #[test]
    fn a_partial_set_of_read_hooks_is_refused() {
        let missing_read_dir = IxeHostVtable {
            read_dir: None,
            ..bridged()
        };
        assert!(
            EmbedderHost::new(missing_read_dir).is_err(),
            "a null among the reads has to be refused"
        );
        let missing_resolved = IxeHostVtable {
            file_type_resolved: None,
            ..bridged()
        };
        assert!(
            EmbedderHost::new(missing_resolved).is_err(),
            "the resolving kind hook is one of the set, not an optional extra"
        );
        // And it is refused at the ABI, not merely internally: a session
        // built on a partial set would be one whose purity table lies.
        // SAFETY: a live, readable vtable.
        let session = unsafe { super::ixe_session_new(&raw const missing_read_dir) };
        assert!(session.is_null(), "a partial set must not yield a session");
        let why = super::ixe_take_setting_conflict();
        assert!(!why.is_null(), "a refusal has to say why");
        // SAFETY: non-null, allocated by this crate just above.
        unsafe { super::ixe_string_free(why) };
    }

    /// The per-evaluation re-take of the process settings must not forget who
    /// answers a read.
    ///
    /// `path_reads` sits in `Settings` because it is in the memo key, but it
    /// is the one field there that is NOT process state, and
    /// `Settings::current()` cannot know it. The first version of this change
    /// let `reload_settings_from_process` overwrite it with `Direct`, so a
    /// session created with the bridge's full set of read hooks refused every
    /// read under `pure-eval` -- reporting "this evaluator has no embedder to
    /// read through" on a run that had one. Nothing in the crate caught it;
    /// `nix eval --expr 'builtins.readFile ./flake.nix'` against the linked
    /// binary did.
    #[test]
    fn re_taking_the_process_settings_keeps_the_host_that_answers_reads() {
        // The reload reads the process settings, so they are held still.
        let _globals = crate::eval::globals_shared();
        let vtable = bridged();
        // SAFETY: points at a live local; `ixe_session_new` copies it.
        let session = unsafe { super::ixe_session_new(&raw const vtable) };
        assert!(
            !session.is_null(),
            "a complete set of read hooks is accepted"
        );
        // SAFETY: non-null, just created here, not freed until below.
        let live = unsafe { &mut *session };
        assert_eq!(
            live.vm.settings().path_reads,
            crate::purity::PathReads::ThroughEmbedder,
            "a session built with read hooks must start out reading through them"
        );
        live.vm.reload_settings_from_process();
        assert_eq!(
            live.vm.settings().path_reads,
            crate::purity::PathReads::ThroughEmbedder,
            "re-taking the process settings dropped the embedder, so every read \
             under pure-eval is now refused for want of a host this session has"
        );
        // SAFETY: from `ixe_session_new`, not freed before now.
        unsafe { super::ixe_session_free(session) };
    }

    /// Supplying the hooks is what flips the purity table's six rows, and
    /// what puts a different value in the memo key.
    ///
    /// Read off two hosts rather than off the process before and after an
    /// install, which is what this could only be when the hooks were global.
    #[test]
    fn the_read_hooks_move_the_verdict_and_the_memo_key() {
        // `settings_for` reads the process settings, which is the point --
        // the fingerprint compared below is of the whole snapshot -- so they
        // are held still for the two reads.
        let _globals = crate::eval::globals_shared();
        let pure = crate::purity::Purity {
            pure_eval: true,
            restrict_eval: false,
        };
        let question =
            crate::task::NeedPath::Contents(crate::value2::ambient_path("/bridged/hello.nix"));
        let Ok(standalone) = EmbedderHost::new(IxeHostVtable::empty()) else {
            unreachable!("an empty vtable is a legitimate standalone embedding")
        };
        let Ok(embedded) = EmbedderHost::new(bridged()) else {
            unreachable!("a complete set of read hooks has to be accepted")
        };
        assert_eq!(
            crate::purity::verdict(&question, pure, standalone.path_reads()),
            crate::purity::Verdict::Refuse
        );
        assert_eq!(
            crate::purity::verdict(&question, pure, embedded.path_reads()),
            crate::purity::Verdict::Ask
        );
        assert_ne!(
            settings_for(&standalone).fingerprint(),
            settings_for(&embedded).fingerprint(),
            "a standalone run and a bridged one share a memo key, so a witness \
             recorded by one can be served to the other"
        );
    }

    /// A malformed answer is a failure and not a plausible one. Both halves
    /// matter: a half pair would silently drop a directory entry, and a
    /// misspelled type would land in `Unknown`, which is a type cppnix really
    /// returns.
    #[test]
    fn a_malformed_directory_listing_is_refused_rather_than_trimmed() {
        assert!(decode_dir_entries(b"a\0regular\0b\0").is_err());
        assert!(decode_dir_entries(b"a\0rgular\0").is_err());
        assert_eq!(
            decode_dir_entries(b"a\0regular\0").ok(),
            Some(vec![("a".to_owned(), FileType::Regular)])
        );
        assert_eq!(decode_dir_entries(b"").ok(), Some(Vec::new()));
        for (text, want) in [
            ("regular", FileType::Regular),
            ("directory", FileType::Directory),
            ("symlink", FileType::Symlink),
            ("unknown", FileType::Unknown),
        ] {
            assert_eq!(decode_file_type(text).ok(), Some(want));
        }
        assert!(decode_file_type("").is_err());
    }
}

/// The threaded import-from-derivation path (ENG-13150): the three-phase
/// vtable group, exercised the way the scheduler drives it -- `begin` on the
/// evaluation thread, the build on a worker, `collect` back on the
/// evaluation thread.
#[cfg(test)]
mod async_realise_tests {
    use super::{
        EmbedderHost, IxeHostVtable, IxeRealiseCheck, IxeRealiseErrorClass, RealiseAllowFn,
        RealiseBuildFn, RealiseCheckFn,
    };
    use crate::host::{Host, Slow, SlowAnswer};
    use crate::value2::ContextElem;
    use std::ffi::c_void;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// What the fake embedder records: which thread ran what, and what the
    /// allow phase was handed. A local value per test, reached through the
    /// vtable's `ctx`, exactly as the C++ host state is.
    #[derive(Default)]
    struct FakeEmbedder {
        checks: AtomicU32,
        build_calls: AtomicU32,
        active_builds: AtomicU32,
        max_active_builds: AtomicU32,
        batch_sizes: Mutex<Vec<usize>>,
        build_protocol_fault: AtomicU32,
        /// Set by the test to let the build finish; the build spins on it,
        /// so "not ready yet" is a state the test enters deterministically
        /// rather than by winning a race.
        release_build: AtomicBool,
        build_thread: Mutex<Option<std::thread::ThreadId>>,
        allow_thread: Mutex<Option<std::thread::ThreadId>>,
        allowed: Mutex<Vec<u8>>,
        check_status: AtomicU32,
    }

    fn embedder_of(ctx: *mut c_void) -> &'static FakeEmbedder {
        // SAFETY: every test passes a pointer to a `FakeEmbedder` that
        // outlives the host built over it.
        unsafe { &*ctx.cast::<FakeEmbedder>() }
    }

    /// Answers the status the test set, and on `Failed` the disabled-IFD
    /// class, so a test can see the class survive the crossing.
    unsafe extern "C" fn fake_check(
        ctx: *mut c_void,
        _request: *const u8,
        _request_len: usize,
        error_class: *mut i32,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        let fake = embedder_of(ctx);
        fake.checks.fetch_add(1, Ordering::SeqCst);
        // SAFETY: a 'static buffer outlives the call by construction, and
        // `error_class` is the caller's local.
        unsafe {
            *out = b"declined".as_ptr();
            *out_len = 8;
            *error_class = IxeRealiseErrorClass::ImportFromDerivation as i32;
        }
        fake.check_status.load(Ordering::SeqCst) as i32
    }

    /// Rewrites `from -> to`, separator, one output. Waits for the test's
    /// release first, and records which thread it ran on.
    unsafe extern "C" fn fake_build(
        ctx: *mut c_void,
        requests: *const super::IxeRealiseRequest,
        count: usize,
        results: *mut super::IxeRealiseResult,
    ) -> i32 {
        let fake = embedder_of(ctx);
        let active = fake.active_builds.fetch_add(1, Ordering::SeqCst) + 1;
        fake.max_active_builds.fetch_max(active, Ordering::SeqCst);
        fake.batch_sizes.lock().unwrap_or_else(|e| e.into_inner()).push(count);
        fake.build_calls.fetch_add(1, Ordering::SeqCst);
        while !fake.release_build.load(Ordering::SeqCst) {
            std::thread::yield_now();
        }
        *fake.build_thread.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(std::thread::current().id());
        const ANSWER: &[u8] = b"from\0to\0\0/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-out\0";
        let fault = fake.build_protocol_fault.load(Ordering::SeqCst);
        if fault != 0 {
            fake.active_builds.fetch_sub(1, Ordering::SeqCst);
            // 1 fails the callback; 2 succeeds but omits every result slot.
            return if fault == 1 { 7 } else { 0 };
        }
        // SAFETY: the caller provides count live request and result slots.
        let requests = unsafe { std::slice::from_raw_parts(requests, count) };
        let results = unsafe { std::slice::from_raw_parts_mut(results, count) };
        for (request, result) in requests.iter().zip(results) {
            // SAFETY: each request buffer stays live for the callback.
            let encoded = unsafe { std::slice::from_raw_parts(request.data, request.len) };
            let failed = encoded.windows(4).any(|part| part == b"fail");
            let answer: &[u8] = if failed { b"one build failed" } else { ANSWER };
            *result = super::IxeRealiseResult {
                status: i32::from(failed),
                data: answer.as_ptr(),
                len: answer.len(),
            };
        }
        fake.active_builds.fetch_sub(1, Ordering::SeqCst);
        0
    }

    unsafe extern "C" fn fake_allow(
        ctx: *mut c_void,
        outputs: *const u8,
        outputs_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        let fake = embedder_of(ctx);
        *fake.allow_thread.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(std::thread::current().id());
        // SAFETY: the ABI's contract -- `outputs_len` readable bytes.
        fake.allowed
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(unsafe { std::slice::from_raw_parts(outputs, outputs_len) });
        // SAFETY: as `fake_check`.
        unsafe {
            *out = b"".as_ptr();
            *out_len = 0;
        }
        0
    }

    fn threaded(fake: &FakeEmbedder) -> IxeHostVtable {
        IxeHostVtable {
            ctx: std::ptr::from_ref(fake).cast_mut().cast(),
            realise_check: Some(fake_check as RealiseCheckFn),
            realise_build: Some(fake_build as RealiseBuildFn),
            realise_allow: Some(fake_allow as RealiseAllowFn),
            ..IxeHostVtable::empty()
        }
    }

    fn built_context() -> Vec<ContextElem> {
        vec![ContextElem::Built {
            drv: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-x.drv".into(),
            output: "out".into(),
        }]
    }

    #[test]
    fn callback_failure_and_missing_result_slot_never_allow_outputs() {
        for (fault, expected) in [
            (1, "realise build batch failed with status 7"),
            (2, "realise build hook did not fill a result slot"),
        ] {
            let fake = Box::leak(Box::new(FakeEmbedder::default()));
            fake.release_build.store(true, Ordering::SeqCst);
            fake.build_protocol_fault.store(fault, Ordering::SeqCst);
            let host = EmbedderHost::new(threaded(fake)).expect("complete host");
            let result = host.realise(&built_context());
            assert!(matches!(result, Err(crate::host::StoreError::Failed(message)) if message == expected));
            assert!(fake.allowed.lock().unwrap_or_else(|e| e.into_inner()).is_empty());
        }
    }

    #[test]
    fn the_first_wave_runs_together_without_a_timing_delay() {
        let fake = Box::leak(Box::new(FakeEmbedder::default()));
        fake.release_build.store(true, Ordering::SeqCst);
        let host = EmbedderHost::new(threaded(fake)).expect("complete host");
        let context = built_context();
        let tickets: Vec<_> = (0..8).map(|_| {
            host.begin(&Slow::Realise(&context)).expect("queued ticket")
        }).collect();
        let before_collect = fake.build_calls.load(Ordering::SeqCst);
        for ticket in tickets {
            assert!(matches!(host.collect(ticket, true), Some(SlowAnswer::Realise(Ok(_)))));
        }
        assert_eq!(before_collect, 0, "all runnable roots must queue before the first build");
        assert_eq!(*fake.batch_sizes.lock().unwrap_or_else(|e| e.into_inner()), vec![8]);
    }

    #[test]
    fn pending_roots_share_bounded_batches_and_one_build_callback() {
        let fake = Box::leak(Box::new(FakeEmbedder::default()));
        let host = EmbedderHost::new(threaded(fake)).expect("complete host");
        let context = built_context();
        let first = host.begin(&Slow::Realise(&context)).expect("first ticket");
        assert!(host.collect(first, false).is_none());
        while fake.build_calls.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        // The first callback is held open, so every later request must queue.
        let tickets: Vec<_> = (0..512).map(|_| {
            host.begin(&Slow::Realise(&context)).expect("queued ticket")
        }).collect();
        let calls_before_release = fake.build_calls.load(Ordering::SeqCst);
        fake.release_build.store(true, Ordering::SeqCst);
        for ticket in std::iter::once(first).chain(tickets) {
            assert!(matches!(host.collect(ticket, true), Some(SlowAnswer::Realise(Ok(_)))));
        }
        assert_eq!(calls_before_release, 1, "pending roots must not create more Workers");
        assert_eq!(fake.max_active_builds.load(Ordering::SeqCst), 1);
        assert_eq!(*fake.batch_sizes.lock().unwrap_or_else(|e| e.into_inner()), vec![1, 256, 256]);
        assert_eq!(fake.checks.load(Ordering::SeqCst), 513, "each root must be checked");
    }

    #[test]
    fn a_failed_request_does_not_fail_its_batch_peer() {
        let fake = Box::leak(Box::new(FakeEmbedder::default()));
        let host = EmbedderHost::new(threaded(fake)).expect("complete host");
        let context = built_context();
        let first = host.begin(&Slow::Realise(&context)).expect("first ticket");
        assert!(host.collect(first, false).is_none());
        while fake.build_calls.load(Ordering::SeqCst) == 0 {
            std::thread::yield_now();
        }
        let failed_context = vec![ContextElem::Built {
            drv: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-fail.drv".into(),
            output: "out".into(),
        }];
        let failed = host.begin(&Slow::Realise(&failed_context)).expect("failed ticket");
        let successful = host.begin(&Slow::Realise(&context)).expect("successful ticket");
        fake.release_build.store(true, Ordering::SeqCst);
        assert!(matches!(host.collect(first, true), Some(SlowAnswer::Realise(Ok(_)))));
        assert!(matches!(host.collect(failed, true), Some(SlowAnswer::Realise(Err(
            crate::host::StoreError::Failed(message)
        ))) if message == "one build failed"));
        assert!(matches!(host.collect(successful, true), Some(SlowAnswer::Realise(Ok(_)))));
        assert_eq!(*fake.batch_sizes.lock().unwrap_or_else(|e| e.into_inner()), vec![1, 2]);
        assert_eq!(fake.allowed.lock().unwrap_or_else(|e| e.into_inner()).len(),
            2 * b"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-out\0".len());
    }

    #[test]
    fn dropping_the_host_joins_active_build_and_discards_queued_requests() {
        let fake: &'static FakeEmbedder = Box::leak(Box::new(FakeEmbedder::default()));
        let (dropping_tx, dropping_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = std::sync::mpsc::channel();
        let owner = std::thread::spawn(move || {
            let host = EmbedderHost::new(threaded(fake)).expect("complete host");
            let context = built_context();
            let first = host.begin(&Slow::Realise(&context)).expect("first ticket");
            assert!(host.collect(first, false).is_none());
            while fake.build_calls.load(Ordering::SeqCst) == 0 {
                std::thread::yield_now();
            }
            for _ in 0..16 {
                let _ = host.begin(&Slow::Realise(&context));
            }
            dropping_tx.send(()).expect("drop notification");
            drop(host);
            finished_tx.send(()).expect("completion notification");
        });
        dropping_rx.recv().expect("owner reached drop");
        let before_release = finished_rx.recv_timeout(std::time::Duration::from_millis(30));
        fake.release_build.store(true, Ordering::SeqCst);
        owner.join().expect("owner joined");
        assert!(matches!(before_release, Err(std::sync::mpsc::RecvTimeoutError::Timeout)),
            "session destruction must wait until the callback stops borrowing ctx");
        assert_eq!(fake.active_builds.load(Ordering::SeqCst), 0);
        assert_eq!(fake.build_calls.load(Ordering::SeqCst), 1, "abandoned queue must not start builds");
    }

    /// Exercise destruction through the exported session owner, including an
    /// unwind that abandons outstanding work. Keep a separate safety reference
    /// until worker exit so a broken no-join implementation fails without UAF.
    #[test]
    fn session_free_joins_borrowed_context_before_return_and_unwind() {
        use std::sync::{Arc, Condvar, mpsc};
        use std::time::{Duration, Instant};

        #[derive(Default)]
        struct GateState { entered: bool, released: bool }
        #[derive(Default)]
        struct Gate { state: Mutex<GateState>, changed: Condvar }
        impl Gate {
            fn release(&self) {
                self.state.lock().unwrap_or_else(|e| e.into_inner()).released = true;
                self.changed.notify_all();
            }
        }
        struct ReleaseOnDrop(Arc<Gate>);
        impl Drop for ReleaseOnDrop {
            fn drop(&mut self) { self.0.release(); }
        }
        #[derive(Debug, PartialEq, Eq)]
        struct FreeObservation { active: u32, final_reads: u32 }
        #[derive(Default)]
        struct Evidence {
            checks: AtomicU32,
            calls: AtomicU32,
            active: AtomicU32,
            final_reads: AtomicU32,
            allowed: AtomicU32,
            freed: Mutex<Option<FreeObservation>>,
            context_dropped: Mutex<Option<u32>>,
        }
        struct Context {
            gate: Arc<Gate>,
            evidence: Arc<Evidence>,
            answer: Vec<u8>,
        }
        impl Drop for Context {
            fn drop(&mut self) {
                *self.evidence.context_dropped.lock().unwrap_or_else(|e| e.into_inner()) =
                    Some(self.evidence.active.load(Ordering::SeqCst));
            }
        }
        unsafe extern "C" fn check(
            ctx: *mut c_void, _data: *const u8, _len: usize,
            class: *mut i32, out: *mut *const u8, out_len: *mut usize,
        ) -> i32 {
            // SAFETY: the test retains Context until the dispatcher's final exit.
            let context = unsafe { &*ctx.cast::<Context>() };
            context.evidence.checks.fetch_add(1, Ordering::SeqCst);
            // SAFETY: the caller supplies writable slots; the answer is static.
            unsafe { *class = 0; *out = b"".as_ptr(); *out_len = 0; }
            IxeRealiseCheck::Build as i32
        }
        unsafe extern "C" fn build(
            ctx: *mut c_void, _requests: *const super::IxeRealiseRequest,
            count: usize, results: *mut super::IxeRealiseResult,
        ) -> i32 {
            // SAFETY: Context and its owned answer survive worker exit, even
            // under the no-join mutant. No callback assertion unwinds over C.
            let context = unsafe { &*ctx.cast::<Context>() };
            context.evidence.calls.fetch_add(1, Ordering::SeqCst);
            context.evidence.active.fetch_add(1, Ordering::SeqCst);
            let mut state = context.gate.state.lock().unwrap_or_else(|e| e.into_inner());
            state.entered = true;
            context.gate.changed.notify_all();
            let state = context.gate.changed.wait_while(state, |state| !state.released)
                .unwrap_or_else(|e| e.into_inner());
            drop(state);
            // The final context read and the returned bytes are deliberately
            // after the gate. The dispatcher still has to copy these bytes.
            // SAFETY: the dispatcher provides exactly count writable slots.
            for result in unsafe { std::slice::from_raw_parts_mut(results, count) } {
                *result = super::IxeRealiseResult {
                    status: 0, data: context.answer.as_ptr(), len: context.answer.len(),
                };
            }
            context.evidence.final_reads.fetch_add(1, Ordering::SeqCst);
            context.evidence.active.fetch_sub(1, Ordering::SeqCst);
            0
        }
        unsafe extern "C" fn allow(
            ctx: *mut c_void, _data: *const u8, _len: usize,
            out: *mut *const u8, out_len: *mut usize,
        ) -> i32 {
            // SAFETY: Context remains live as for check/build above.
            let context = unsafe { &*ctx.cast::<Context>() };
            context.evidence.allowed.fetch_add(1, Ordering::SeqCst);
            // SAFETY: writable output slots and static answer.
            unsafe { *out = b"".as_ptr(); *out_len = 0; }
            0
        }
        struct SessionOwner { raw: *mut super::IxeSession, evidence: Arc<Evidence> }
        impl Drop for SessionOwner {
            fn drop(&mut self) {
                // SAFETY: created on this thread and freed exactly once here.
                unsafe { super::ixe_session_free(self.raw) };
                *self.evidence.freed.lock().unwrap_or_else(|e| e.into_inner()) = Some(FreeObservation {
                    active: self.evidence.active.load(Ordering::SeqCst),
                    final_reads: self.evidence.final_reads.load(Ordering::SeqCst),
                });
            }
        }

        for unwind in [false, true] {
            let gate = Arc::new(Gate::default());
            let release = ReleaseOnDrop(gate.clone());
            let evidence = Arc::new(Evidence::default());
            let context = Arc::new(Context {
                gate: gate.clone(), evidence: evidence.clone(),
                answer: b"from\0to\0\0/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-out\0".to_vec(),
            });
            let owner_context = context.clone();
            let (queue_tx, queue_rx) = mpsc::channel();
            let (finished_tx, finished_rx) = mpsc::channel();
            let owner = std::thread::spawn(move || {
                let _globals = crate::eval::globals_shared();
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let vtable = IxeHostVtable {
                        ctx: std::ptr::from_ref(owner_context.as_ref()).cast_mut().cast(),
                        realise_check: Some(check), realise_build: Some(build), realise_allow: Some(allow),
                        ..IxeHostVtable::empty()
                    };
                    // SAFETY: new copies the vtable; owner_context remains live.
                    let raw = unsafe { super::ixe_session_new(&raw const vtable) };
                    assert!(!raw.is_null(), "complete vtable must create a session");
                    let session = SessionOwner { raw, evidence: owner_context.evidence.clone() };
                    // SAFETY: borrow only on the creating thread, ending before free.
                    let host = unsafe { &(*session.raw).host };
                    let built = built_context();
                    let Some(ticket) = host.begin(&Slow::Realise(&built)) else { panic!("first ticket absent") };
                    assert!(host.collect(ticket, false).is_none());
                    let state = owner_context.gate.state.lock().unwrap_or_else(|e| e.into_inner());
                    let (state, timeout) = owner_context.gate.changed.wait_timeout_while(
                        state, Duration::from_secs(5), |state| !state.entered,
                    ).unwrap_or_else(|e| e.into_inner());
                    assert!(state.entered && !timeout.timed_out(), "callback never entered");
                    drop(state);
                    for _ in 0..16 { assert!(host.begin(&Slow::Realise(&built)).is_some()); }
                    let observer = {
                        let dispatcher = host.ifd.dispatcher.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(dispatcher) = dispatcher.as_ref() else { panic!("dispatcher absent") };
                        dispatcher.queue.clone()
                    };
                    assert!(queue_tx.send(observer).is_ok());
                    if unwind { std::panic::resume_unwind(Box::new("abandoned evaluation")); }
                    drop(session);
                }));
                assert_eq!(outcome.is_err(), unwind, "unexpected owner unwind");
                assert!(finished_tx.send(()).is_ok());
            });
            let observer = queue_rx.recv_timeout(Duration::from_secs(5));
            let Ok(observer) = observer else {
                gate.release();
                // There is no worker-exit witness on this failure path. Keep
                // the context alive even if the implementation detached work.
                std::mem::forget(context);
                assert!(owner.join().is_ok(), "owner failed before teardown");
                panic!("owner did not expose the dispatcher");
            };
            // This is an actual Drop-entry witness, not a sleep guessing when
            // destruction started. The observer does not retain a host/session.
            let deadline = Instant::now() + Duration::from_secs(5);
            while !observer.0.lock().unwrap_or_else(|e| e.into_inner()).closed {
                if Instant::now() >= deadline { break; }
                std::thread::yield_now();
            }
            let closed = observer.0.lock().unwrap_or_else(|e| e.into_inner()).closed;
            let before_release = finished_rx.recv_timeout(Duration::from_secs(1));
            gate.release();
            let joined = owner.join();
            // A no-join mutant detaches the worker. Wait until its final queue
            // reference is gone before dropping the owned callback buffer.
            let deadline = Instant::now() + Duration::from_secs(5);
            while Arc::strong_count(&observer) != 1 && Instant::now() < deadline {
                std::thread::yield_now();
            }
            if Arc::strong_count(&observer) != 1 {
                // A broken worker must fail safely, not turn this test into UAF.
                std::mem::forget(context);
                panic!("dispatcher did not finish after callback release");
            }
            drop(context);
            drop(release);
            assert!(joined.is_ok(), "session owner did not finish");
            assert!(closed, "session_free never entered dispatcher teardown");
            assert!(matches!(before_release, Err(mpsc::RecvTimeoutError::Timeout)),
                "session_free returned while the callback still borrowed its context");
            assert_eq!(*evidence.freed.lock().unwrap_or_else(|e| e.into_inner()),
                Some(FreeObservation { active: 0, final_reads: 1 }),
                "free must return only after the callback's final context access");
            assert_eq!(*evidence.context_dropped.lock().unwrap_or_else(|e| e.into_inner()), Some(0));
            assert_eq!(evidence.checks.load(Ordering::SeqCst), 17);
            assert_eq!(evidence.calls.load(Ordering::SeqCst), 1, "queued callbacks must be discarded");
            assert_eq!(evidence.allowed.load(Ordering::SeqCst), 0, "abandoned outputs must not be allowed");
        }
    }

    /// The whole protocol in one pass: the check runs once on the calling
    /// thread, the build runs on a thread of its own, a non-blocking collect
    /// before the build finishes says "not ready" and leaves the ticket
    /// collectable, and the blocking collect delivers the rewrites after
    /// running the allow phase -- on the collecting thread, with the
    /// outputs section of the build's answer, verbatim. That last assertion
    /// is the thread-safety fix in miniature: the allow-list mutation
    /// happens where the evaluation lives, never on the worker.
    #[test]
    fn a_begun_build_is_answered_and_allowed_at_collect() {
        let fake = Box::leak(Box::new(FakeEmbedder::default()));
        let Ok(host) = EmbedderHost::new(threaded(fake)) else {
            unreachable!("a complete async realise group has to be accepted")
        };
        let context = built_context();
        let Some(ticket) = host.begin(&Slow::Realise(&context)) else {
            unreachable!("a buildable context through a threaded vtable has to begin")
        };
        assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
        assert_eq!(
            host.collect(ticket, false).map(|_| ()),
            None,
            "the build has not been released, so a non-blocking collect \
             must say not-ready rather than wait"
        );
        fake.release_build.store(true, Ordering::SeqCst);
        let Some(SlowAnswer::Realise(Ok(rewrites))) = host.collect(ticket, true) else {
            unreachable!("a blocking collect of a released build has to answer")
        };
        assert_eq!(
            rewrites.get("from").map(String::as_str),
            Some("to"),
            "the rewrites section of the build answer is the realise result"
        );
        let build = fake
            .build_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("the build hook ran");
        let allow = fake
            .allow_thread
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .expect("the allow hook ran");
        assert_ne!(
            build,
            std::thread::current().id(),
            "the build must run off the evaluation thread or nothing overlaps"
        );
        assert_eq!(
            allow,
            std::thread::current().id(),
            "the allow phase must run on the collecting thread; on a worker \
             it would race the evaluation thread's every file access"
        );
        assert_eq!(
            fake.allowed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_slice(),
            b"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-out\0",
            "the allow phase receives the outputs section verbatim"
        );
        // Collected means gone: the ticket was consumed.
        assert!(host.collect(ticket, false).is_none());
    }

    /// A refused check is begun and delivered: the refusal is filed ready
    /// under the ticket with its class intact, no worker is started, and the
    /// check has run exactly once -- there is no second route to re-run it.
    #[test]
    fn a_refused_check_is_delivered_with_its_class() {
        let fake = Box::leak(Box::new(FakeEmbedder::default()));
        fake.check_status
            .store(IxeRealiseCheck::Failed as u32, Ordering::SeqCst);
        let Ok(host) = EmbedderHost::new(threaded(fake)) else {
            unreachable!("a complete async realise group has to be accepted")
        };
        let context = built_context();
        let Some(ticket) = host.begin(&Slow::Realise(&context)) else {
            unreachable!("a refused check is still a begun question")
        };
        let Some(SlowAnswer::Realise(Err(why))) = host.collect(ticket, true) else {
            unreachable!("the refusal is what gets delivered")
        };
        assert!(
            matches!(&why, crate::host::StoreError::ImportFromDerivation(m) if m == "declined"),
            "{why:?}"
        );
        assert_eq!(fake.checks.load(Ordering::SeqCst), 1, "the check ran once");
        assert!(
            fake.build_thread
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none(),
            "no build was started"
        );
    }

    /// A check that finds every element already valid answers the empty
    /// rewrite map through the same ticket, with no worker and no allow
    /// phase, since nothing was built.
    #[test]
    fn nothing_left_to_build_is_delivered_without_a_worker() {
        let fake = Box::leak(Box::new(FakeEmbedder::default()));
        fake.check_status
            .store(IxeRealiseCheck::Nothing as u32, Ordering::SeqCst);
        let Ok(host) = EmbedderHost::new(threaded(fake)) else {
            unreachable!("a complete async realise group has to be accepted")
        };
        let context = built_context();
        let Some(ticket) = host.begin(&Slow::Realise(&context)) else {
            unreachable!("nothing to build is still a begun question")
        };
        let Some(SlowAnswer::Realise(Ok(rewrites))) = host.collect(ticket, true) else {
            unreachable!("the empty map is what gets delivered")
        };
        assert!(rewrites.is_empty());
        assert!(
            fake.build_thread
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
        );
        assert!(
            fake.allow_thread
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
        );
    }

    /// The blocking route uses the same dispatcher, so it cannot start a
    /// second store Worker while asynchronous IFD requests are in flight.
    #[test]
    fn the_blocking_route_runs_the_three_phases_in_turn() {
        let fake = Box::leak(Box::new(FakeEmbedder::default()));
        fake.release_build.store(true, Ordering::SeqCst);
        let Ok(host) = EmbedderHost::new(threaded(fake)) else {
            unreachable!("a complete async realise group has to be accepted")
        };
        let Ok(rewrites) = host.realise(&built_context()) else {
            unreachable!("a released build answers")
        };
        assert_eq!(rewrites.get("from").map(String::as_str), Some("to"));
        assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
        let here = std::thread::current().id();
        assert_ne!(
            *fake.build_thread.lock().unwrap_or_else(|e| e.into_inner()),
            Some(here)
        );
        assert_eq!(
            *fake.allow_thread.lock().unwrap_or_else(|e| e.into_inner()),
            Some(here)
        );
        assert_eq!(
            fake.allowed
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .as_slice(),
            b"/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-out\0"
        );
    }

    /// A refusal through the blocking route keeps its class the same way.
    #[test]
    fn the_blocking_route_keeps_the_refusal_class() {
        let fake = Box::leak(Box::new(FakeEmbedder::default()));
        fake.check_status
            .store(IxeRealiseCheck::Failed as u32, Ordering::SeqCst);
        let Ok(host) = EmbedderHost::new(threaded(fake)) else {
            unreachable!("a complete async realise group has to be accepted")
        };
        let Err(why) = host.realise(&built_context()) else {
            unreachable!("a refused check refuses")
        };
        assert!(
            matches!(&why, crate::host::StoreError::ImportFromDerivation(m) if m == "declined"),
            "{why:?}"
        );
        assert!(
            fake.build_thread
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_none()
        );
    }

    /// A context with nothing to build is not begun: the blocking route
    /// answers it from the check phase alone, so a thread would cost more
    /// than it hides. The check hook is not consulted by `begin`; it runs
    /// once, on the blocking route.
    #[test]
    fn nothing_to_build_is_not_begun() {
        let fake = Box::leak(Box::new(FakeEmbedder::default()));
        let Ok(host) = EmbedderHost::new(threaded(fake)) else {
            unreachable!("a complete async realise group has to be accepted")
        };
        let context = vec![ContextElem::Opaque(
            "/nix/store/cccccccccccccccccccccccccccccccc-src".into(),
        )];
        assert!(host.begin(&Slow::Realise(&context)).is_none());
        assert_eq!(fake.checks.load(Ordering::SeqCst), 0);
    }

    /// The other three slow questions stay synchronous through this host:
    /// the fetchers, the tarball cache and the flake registry behind them
    /// have not been audited for a second thread, and `begin` answering
    /// `None` is what keeps them off one.
    #[test]
    fn only_the_realise_question_is_begun() {
        let fake = Box::leak(Box::new(FakeEmbedder::default()));
        let Ok(host) = EmbedderHost::new(threaded(fake)) else {
            unreachable!("a complete async realise group has to be accepted")
        };
        assert!(host.begin(&Slow::Flake("nixpkgs")).is_none());
        let fetch = crate::task::FetchRequest {
            url: "https://example.invalid/x".to_owned(),
            name: "x".to_owned(),
            kind: crate::task::FetchKind::File,
            expected_sha256: None,
        };
        assert!(host.begin(&Slow::Fetch(&fetch)).is_none());
    }

    /// The group is a protocol: some of it is worse than none of it.
    #[test]
    fn the_async_realise_group_is_all_or_nothing() {
        let fake = Box::leak(Box::new(FakeEmbedder::default()));
        let partial = IxeHostVtable {
            realise_build: Some(fake_build as RealiseBuildFn),
            ..IxeHostVtable::empty()
        };
        assert!(
            EmbedderHost::new(partial).is_err(),
            "a partial async realise group has to be refused"
        );
        assert!(EmbedderHost::new(threaded(fake)).is_ok());
    }
}

#[cfg(test)]
mod handle_tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The C ABI is what is under test, so the tests call it the way C does:
    /// raw pointers, out parameters, explicit frees. Exercising the Rust
    /// functions behind it would pass while the ABI was wrong.
    ///
    /// Deliberately holds no lock on the process globals, though it reads
    /// them. Making it hold `globals_shared()` for its life is the obvious fix
    /// and it deadlocks: `a_handle_from_another_session_is_rejected` and
    /// `a_typed_accessor_refuses_every_kind_but_its_own` each hold two
    /// sessions at once, so the second read request queues behind any waiting
    /// writer while the first guard is still held, and `RwLock` will not let
    /// both through. The exclusion these tests need instead comes from every
    /// test that *moves* a global taking the write lock, which is the rule
    /// the rest of the crate already follows.
    pub(super) struct Sess(*mut IxeSession);

    impl Sess {
        pub(super) fn new() -> Self {
            Sess(session_without_embedder())
        }

        /// A session whose host answers exactly one question: the store copy
        /// below. Built here rather than installed later, because a host is
        /// now a session's constructor argument and there is no window in
        /// which a live session's answers can change.
        pub(super) fn with_fake_store() -> Self {
            let vtable = IxeHostVtable {
                copy_to_store: Some(fake_copy_to_store),
                ..IxeHostVtable::empty()
            };
            // SAFETY: points at a live local for the duration of the call,
            // which is all `ixe_session_new` needs -- it copies the struct.
            Sess(unsafe { ixe_session_new(&raw const vtable) })
        }

        /// The pointer, for the calls the helpers below do not wrap.
        pub(super) fn raw(&self) -> *mut IxeSession {
            self.0
        }

        pub(super) fn eval(&self, source: &str) -> Result<u64, i32> {
            let mut handle = 0u64;
            let status = unsafe {
                ixe_session_eval(
                    self.0,
                    source.as_ptr(),
                    source.len(),
                    std::ptr::null(),
                    0,
                    // No file: these helpers evaluate source text, which is
                    // the `--expr` shape.
                    std::ptr::null(),
                    0,
                    &raw mut handle,
                )
            };
            if status == IXE_OK {
                Ok(handle)
            } else {
                Err(status)
            }
        }

        pub(super) fn select(&self, handle: u64, name: &str) -> Result<u64, i32> {
            let mut out = 0u64;
            let status = unsafe {
                ixe_attrs_select(self.0, handle, name.as_ptr(), name.len(), &raw mut out)
            };
            if status == IXE_OK {
                Ok(out)
            } else {
                Err(status)
            }
        }

        pub(super) fn render(&self, handle: u64, mode: i32) -> Result<String, i32> {
            let mut out: *mut c_char = std::ptr::null_mut();
            let status = unsafe { ixe_render(self.0, handle, mode, &raw mut out) };
            let text = take_c_string(out);
            if status == IXE_OK {
                Ok(text.unwrap_or_default())
            } else {
                Err(status)
            }
        }

        /// The bulk name enumerator, unpacked. Splits on the NUL after each
        /// name rather than trusting `ixe_attrs_len`, so a buffer that
        /// disagrees with the count shows up here instead of being papered
        /// over by a loop the count drives.
        pub(super) fn names(&self, handle: u64) -> Result<Vec<String>, i32> {
            let mut out: *mut c_char = std::ptr::null_mut();
            let mut len = 0usize;
            let status = unsafe { ixe_attrs_names(self.0, handle, &raw mut out, &raw mut len) };
            if status != IXE_OK {
                return Err(status);
            }
            if out.is_null() {
                assert_eq!(len, 0, "a null buffer must carry a zero length");
                return Ok(Vec::new());
            }
            // SAFETY: the ABI just handed back `len` readable bytes at `out`.
            let bytes = unsafe { slice::from_raw_parts(out.cast::<u8>(), len) };
            let mut names: Vec<String> = bytes
                .split(|b| *b == 0)
                .map(|n| String::from_utf8_lossy(n).into_owned())
                .collect();
            // Every name is NUL-terminated including the last, so the split
            // leaves one empty tail. Its absence would mean an unterminated
            // final name, which is exactly the bug this shape can have.
            assert_eq!(
                names.pop().as_deref(),
                Some(""),
                "the buffer did not end with a NUL"
            );
            unsafe { ixe_names_free(out, len) };
            Ok(names)
        }

        pub(super) fn error(&self) -> Option<String> {
            take_c_string(unsafe { ixe_session_take_error(self.0, std::ptr::null_mut()) })
        }

        /// Borrowed, not taken: the token is static and reading it must not
        /// clear it, so this copies rather than transferring ownership.
        pub(super) fn refusal_token(&self) -> Option<String> {
            let ptr = unsafe { ixe_session_refusal_token(self.0) };
            if ptr.is_null() {
                return None;
            }
            Some(
                unsafe { CStr::from_ptr(ptr) }
                    .to_string_lossy()
                    .into_owned(),
            )
        }

        pub(super) fn ty(&self, handle: u64) -> i32 {
            unsafe { ixe_value_type(self.0, handle) }
        }

        pub(super) fn live_dynamic_attr_origins(&self) -> usize {
            // SAFETY: `Sess` owns this session pointer until its `Drop`.
            unsafe { &*self.0 }.vm.live_dynamic_attr_origins()
        }

        pub(super) fn dynamic_attr_origin_slots(&self) -> usize {
            // SAFETY: `Sess` owns this session pointer until its `Drop`.
            unsafe { &*self.0 }.vm.dynamic_attr_origin_slots()
        }
    }

    impl Drop for Sess {
        fn drop(&mut self) {
            unsafe { ixe_session_free(self.0) };
        }
    }

    pub(super) fn take_c_string(p: *mut c_char) -> Option<String> {
        if p.is_null() {
            return None;
        }
        // SAFETY: p came from one of the ABI's string-producing calls.
        let text = unsafe { std::ffi::CStr::from_ptr(p) }
            .to_string_lossy()
            .into_owned();
        unsafe { ixe_string_free(p) };
        Some(text)
    }

    /// The embedder's store-copy hook, faked. A path interpolated into a
    /// string is the only way to get a context-bearing string in this crate,
    /// and it needs somebody to answer with a store path; the real answer
    /// comes from cppnix, which a unit test does not have.
    unsafe extern "C" fn fake_copy_to_store(
        _ctx: *mut c_void,
        _root: *const u8,
        _root_len: usize,
        _path: *const u8,
        _path_len: usize,
        out: *mut *const u8,
        out_len: *mut usize,
    ) -> i32 {
        const ANSWER: &str = "/nix/store/0000000000000000000000000000000-probe";
        // SAFETY: the ABI's contract; the caller passes writable pointers and
        // copies the bytes before returning, so a 'static buffer is enough.
        unsafe {
            *out = ANSWER.as_ptr();
            *out_len = ANSWER.len();
        }
        0
    }

    #[test]
    fn a_scalar_crosses_and_reads_back() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let h = s.eval("1 + 2").unwrap_or(0);
        assert_eq!(s.ty(h), IXE_TYPE_INT);
        let mut n = 0i64;
        assert_eq!(unsafe { ixe_get_int(s.0, h, &raw mut n) }, IXE_OK);
        assert_eq!(n, 3);
    }

    #[test]
    fn every_crossing_type_reports_itself() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        for (source, want) in [
            ("1", IXE_TYPE_INT),
            ("1.5", IXE_TYPE_FLOAT),
            ("true", IXE_TYPE_BOOL),
            ("null", IXE_TYPE_NULL),
            ("\"s\"", IXE_TYPE_STRING),
            ("/tmp/x", IXE_TYPE_PATH),
            ("[ 1 ]", IXE_TYPE_LIST),
            ("{ a = 1; }", IXE_TYPE_ATTRS),
            ("x: x", IXE_TYPE_FUNCTION),
            ("builtins.add", IXE_TYPE_FUNCTION),
        ] {
            let h = s.eval(source).unwrap_or(0);
            assert_eq!(s.ty(h), want, "type of {source}");
        }
    }

    /// The property the whole handle table exists for. If selection forced
    /// siblings this would throw, and the corpus would never notice: no
    /// lang-diff case selects an attribute.
    #[test]
    fn selecting_one_attribute_does_not_force_its_siblings() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let root = s
            .eval("{ ok = 1; boom = throw \"a sibling was forced\"; }")
            .unwrap_or(0);
        let ok = s.select(root, "ok").unwrap_or(0);
        assert_eq!(s.render(ok, IXE_RENDER_PLAIN), Ok("1".to_owned()));
        assert_eq!(s.error(), None, "something forced the throwing sibling");
    }

    /// `IXE_RENDER_XML` is `builtins.toXML`'s document, verbatim: measured
    /// against `nix-instantiate --eval --strict --xml --no-location` on this
    /// repo's cppnix, byte for byte including the single trailing newline
    /// (`od -c` showed `< / e x p r > \n` and nothing after it, because
    /// cppnix's okXML branch appends no endl of its own).
    #[test]
    fn xml_render_is_to_xmls_document() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let h = s
            .eval("{ a = [ 1 \"x\" ]; f = x: x; p = /some/path; }")
            .unwrap_or(0);
        assert_eq!(
            s.render(h, IXE_RENDER_XML),
            Ok("<?xml version='1.0' encoding='utf-8'?>\n\
                <expr>\n  \
                <attrs>\n    \
                <attr name=\"a\">\n      \
                <list>\n        \
                <int value=\"1\" />\n        \
                <string value=\"x\" />\n      \
                </list>\n    \
                </attr>\n    \
                <attr name=\"f\">\n      \
                <function>\n        \
                <varpat name=\"x\" />\n      \
                </function>\n    \
                </attr>\n    \
                <attr name=\"p\">\n      \
                <path value=\"/some/path\" />\n    \
                </attr>\n  \
                </attrs>\n\
                </expr>\n"
                .to_owned())
        );
    }

    /// Enumerating names must not force either: a set's names are known
    /// before any of its values are.
    #[test]
    fn counting_and_naming_attributes_does_not_force_them() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let root = s
            .eval("{ ok = 1; boom = throw \"forced by enumeration\"; }")
            .unwrap_or(0);
        let mut len = 0usize;
        assert_eq!(unsafe { ixe_attrs_len(s.0, root, &raw mut len) }, IXE_OK);
        assert_eq!(len, 2);
        // cppnix's lexicographicOrder: by name, not by interning order, and
        // "boom" was interned second.
        assert_eq!(s.names(root), Ok(vec!["boom".to_owned(), "ok".to_owned()]));
        assert_eq!(s.error(), None);
    }

    /// An empty set is the edge the buffer shape can get wrong: there is
    /// nothing to point at, and handing back a dangling zero-length
    /// allocation would leave a caller that skips the length check reading
    /// whatever the allocator last held.
    #[test]
    fn an_empty_set_enumerates_to_a_null_buffer() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let root = s.eval("{ }").unwrap_or(0);
        let mut out: *mut c_char = std::ptr::null_mut();
        let mut len = 1usize;
        assert_eq!(
            unsafe { ixe_attrs_names(s.0, root, &raw mut out, &raw mut len) },
            IXE_OK
        );
        assert!(out.is_null(), "an empty set produced a buffer");
        assert_eq!(len, 0);
        // Freeing null is a no-op rather than a fault, so a caller with one
        // unconditional free path is correct.
        unsafe { ixe_names_free(out, len) };
        assert_eq!(s.names(root), Ok(Vec::new()));
    }

    /// Names are bytes the interner holds, and the enumerator must carry
    /// them across unchanged: quoted names can hold anything a Nix string
    /// can, including the separator-adjacent cases (spaces, newlines) and
    /// multi-byte UTF-8.
    #[test]
    fn enumeration_carries_awkward_names_verbatim() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let root = s
            .eval("{ \"a b\" = 1; \"c\\nd\" = 2; \"\u{e9}\" = 3; \"\" = 4; }")
            .unwrap_or(0);
        assert_eq!(
            s.names(root),
            Ok(vec![
                String::new(),
                "a b".to_owned(),
                "c\nd".to_owned(),
                "\u{e9}".to_owned(),
            ])
        );
        assert_eq!(s.error(), None);
    }

    /// Enumerating something that is not a set names what it found, the same
    /// refusal `ixe_attrs_len` gives.
    #[test]
    fn enumerating_a_non_set_refuses() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let root = s.eval("42").unwrap_or(0);
        assert_eq!(s.names(root), Err(IXE_ERR_BADCALL));
        assert_eq!(
            s.error().as_deref(),
            Some("expected a set but found an integer")
        );
    }

    /// ENG-12913, the regression guard. Enumerating a set must cost one pass
    /// over it, not one pass per name.
    ///
    /// Nothing about the OUTPUT changes when this breaks, which is why the
    /// bug survived to reach the fleet: an attribute-not-found on nixpkgs'
    /// 25,442-name top level printed the right message and took 42 seconds
    /// doing it, against cppnix's 2. So the assertion has to be on the cost.
    ///
    /// Measured on an M-series Mac at this size: the one-pass version takes
    /// 1.5ms built with --release and 11.7ms unoptimised, and restoring the
    /// per-index rebuild takes 9.1s. The budget sits between them at 2s, so
    /// it is 170x above a correct unoptimised run and still catches the
    /// quadratic shape on a machine four times faster than the one measured.
    /// Both ends were run, not estimated; the numbers are in the ENG-12913
    /// PR. The elapsed time is printed as well as bounded, because a guard
    /// whose margin nobody can see cannot show a change that made
    /// enumeration ten times slower while staying inside the budget.
    #[test]
    fn enumerating_a_large_set_is_one_pass_not_one_per_name() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        const N: usize = 20_000;
        const BUDGET: Duration = Duration::from_secs(2);

        let s = Sess::new();
        let source = format!(
            "builtins.listToAttrs (builtins.genList (i: {{ name = \"attr\" + toString i; value = i; }}) {N})"
        );
        let root = s
            .eval(&source)
            .unwrap_or_else(|e| unreachable!("the large set did not evaluate: {e:?}"));

        let start = Instant::now();
        let names = s
            .names(root)
            .unwrap_or_else(|e| unreachable!("enumeration refused: {e:?}"));
        let elapsed = start.elapsed();

        assert_eq!(names.len(), N, "enumeration lost names");
        assert!(
            elapsed < BUDGET,
            "enumerating {N} names took {elapsed:?}, over the {BUDGET:?} budget: \
             this is quadratic again (ENG-12913)"
        );
        // Printed, not only bounded: a guard whose margin nobody can see is
        // one nobody can tell has been eaten by a change that made things
        // twice as slow while staying inside the budget.
        println!("enumerated {N} names in {elapsed:?} (budget {BUDGET:?})");
    }

    /// A selected handle is a cell, not a value: it says so until forced,
    /// rather than forcing to answer.
    #[test]
    fn a_selected_handle_is_unforced_until_it_is_forced() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let root = s.eval("{ a = 1 + 1; }").unwrap_or(0);
        let a = s.select(root, "a").unwrap_or(0);
        assert_eq!(s.ty(a), IXE_TYPE_UNFORCED);
        assert_eq!(unsafe { ixe_force(s.0, a) }, IXE_OK);
        assert_eq!(s.ty(a), IXE_TYPE_INT);
    }

    #[test]
    fn a_missing_attribute_is_its_own_status_and_names_itself() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let root = s.eval("{ a = 1; }").unwrap_or(0);
        assert_eq!(s.select(root, "nope"), Err(IXE_ERR_MISSING));
        assert_eq!(s.error().as_deref(), Some("nope"));
    }

    #[test]
    fn a_list_indexes_and_refuses_past_its_end() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let root = s.eval("[ 10 20 ]").unwrap_or(0);
        let mut len = 0usize;
        assert_eq!(unsafe { ixe_list_len(s.0, root, &raw mut len) }, IXE_OK);
        assert_eq!(len, 2);
        let mut item = 0u64;
        assert_eq!(unsafe { ixe_list_at(s.0, root, 1, &raw mut item) }, IXE_OK);
        assert_eq!(s.render(item, IXE_RENDER_PLAIN), Ok("20".to_owned()));
        assert_eq!(
            unsafe { ixe_list_at(s.0, root, 2, &raw mut item) },
            IXE_ERR_MISSING
        );
    }

    #[test]
    fn the_three_render_modes_produce_their_three_shapes() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let h = s.eval("{ a = 1; b = \"x\"; }").unwrap_or(0);
        assert_eq!(
            s.render(h, IXE_RENDER_PLAIN),
            Ok("{ a = 1; b = \"x\"; }".to_owned())
        );
        assert_eq!(
            s.render(h, IXE_RENDER_JSON),
            Ok("{\"a\":1,\"b\":\"x\"}".to_owned())
        );
        let text = s.eval("\"hi\"").unwrap_or(0);
        assert_eq!(s.render(text, IXE_RENDER_RAW), Ok("hi".to_owned()));
    }

    /// `--raw` is coerceToString with coerceMore = false, so an integer is an
    /// error even though `toString` would take it. Getting this wrong would
    /// print "1" where cppnix fails.
    #[test]
    fn raw_refuses_what_cppnix_refuses() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        for source in ["1", "true", "null", "[ 1 ]"] {
            let h = s.eval(source).unwrap_or(0);
            assert_eq!(
                s.render(h, IXE_RENDER_RAW),
                Err(IXE_ERR_EVAL),
                "--raw of {source} should have failed"
            );
            assert!(
                s.error().unwrap_or_default().starts_with("cannot coerce"),
                "wrong message for --raw of {source}"
            );
        }
    }

    /// The invariant behind the `--raw` refusals, stated as the thing that
    /// must never happen rather than as the thing that does: whatever this
    /// returns for a path, it is never the source path. An approximation
    /// there is a plausible wrong string and nothing downstream can tell it
    /// from a real one; a refusal is loud. When ENG-12493 routes `Raw`
    /// through the machine this test changes to assert the store path, and
    /// the one below it goes away.
    #[test]
    fn raw_of_a_path_refuses_by_name() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let h = s.eval("/tmp/some/source/path").unwrap_or(0);
        let got = s.render(h, IXE_RENDER_RAW);
        assert_ne!(
            got,
            Ok("/tmp/some/source/path".to_owned()),
            "--raw handed back the source path, which cppnix would have copied to \
             the store first; a caller cannot tell this from a real answer"
        );
        assert_eq!(got, Err(IXE_ERR_UNIMPLEMENTED));
        assert!(s.error().unwrap_or_default().contains("store"));
    }

    /// The two `--raw` cases cppnix accepts and this backend cannot: both end
    /// in the store. Named, so a user learns which one they hit.
    #[test]
    fn raw_names_the_two_cases_it_cannot_serve() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        for (source, marker) in [
            ("/tmp/x", "path"),
            ("{ outPath = \"x\"; }", "attribute set"),
        ] {
            let h = s.eval(source).unwrap_or(0);
            assert_eq!(s.render(h, IXE_RENDER_RAW), Err(IXE_ERR_UNIMPLEMENTED));
            let message = s.error().unwrap_or_default();
            assert!(message.contains(marker), "unhelpful refusal: {message}");
        }
    }

    #[test]
    fn a_function_cannot_be_rendered_as_json() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let h = s.eval("x: x").unwrap_or(0);
        assert_eq!(s.render(h, IXE_RENDER_JSON), Err(IXE_ERR_EVAL));
        assert_eq!(
            s.error().as_deref(),
            Some("cannot convert a function to JSON")
        );
    }

    /// Two sessions start their indices at 1, so without the serial in the
    /// high bits this would read the other session's first value.
    #[test]
    fn a_handle_from_another_session_is_rejected() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let a = Sess::new();
        let b = Sess::new();
        let ha = a.eval("111").unwrap_or(0);
        let _hb = b.eval("222").unwrap_or(0);
        assert_eq!(b.ty(ha), IXE_TYPE_UNKNOWN_HANDLE);
        assert_eq!(b.render(ha, IXE_RENDER_PLAIN), Err(IXE_ERR_BADCALL));
    }

    /// The enumeration is the vocabulary the C++ side builds its histogram
    /// keys from, so it has to agree with `ALL` exactly. If it returned fewer
    /// than it has, the missing kinds would have no row -- and a kind with no
    /// row reads as "never happened" rather than as "not counted", which is
    /// the failure this whole census exists to avoid.
    #[test]
    fn the_abi_enumerates_exactly_the_token_vocabulary() {
        let n = ixe_refusal_token_count();
        assert_eq!(n, crate::refusal::RefusalToken::ALL.len());

        let mut seen = Vec::with_capacity(n);
        for i in 0..n {
            let ptr = ixe_refusal_token_at(i);
            assert!(!ptr.is_null(), "index {i} within count returned null");
            seen.push(
                unsafe { CStr::from_ptr(ptr) }
                    .to_string_lossy()
                    .into_owned(),
            );
            assert!(
                (0..=3).contains(&ixe_refusal_token_raised_by(i)),
                "index {i} has no layer"
            );
        }
        let expected: Vec<String> = crate::refusal::RefusalToken::ALL
            .iter()
            .map(|t| t.as_str().to_owned())
            .collect();
        assert_eq!(seen, expected);

        // Out of range is a refusal to answer, not a wrap-around into the
        // first token, which a caller looping one step too far would then
        // double-count.
        assert!(ixe_refusal_token_at(n).is_null());
        assert_eq!(ixe_refusal_token_raised_by(n), -1);
    }

    /// The token is what a census groups by, so it has to be set when a
    /// refusal happens and absent when one does not. Both directions: a
    /// caller that reads a stale token from an unrelated failure files that
    /// failure under a refusal category it never belonged to.
    #[test]
    fn a_refusal_carries_its_token_and_nothing_else_does() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        // See `REFUSED_EXPRESSION` for what this is and why it moves.
        let refused = s.eval(REFUSED_EXPRESSION);
        assert_eq!(refused, Err(IXE_ERR_UNIMPLEMENTED));
        assert_eq!(s.refusal_token(), Some(REFUSED_EXPRESSION_TOKEN.to_owned()));

        // An ordinary evaluation error is not a refusal, so it must leave no
        // token behind for the next reader to pick up.
        let thrown = s.eval("builtins.throw \"boom\"");
        assert_eq!(thrown, Err(IXE_ERR_THROWN));
        assert_eq!(s.refusal_token(), None);
    }

    /// Holds the claim `out_string` relies on: a Nix string cannot contain a
    /// NUL, so the U+2400 substitution there is unreachable. That is a fact
    /// about the evaluator, not about the C ABI, which is exactly the kind of
    /// cross-module claim that goes stale in a comment without anything
    /// noticing -- a sibling merge falsified one of those here already.
    #[test]
    fn the_evaluator_refuses_nul_bearing_strings() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        // No string literal syntax produces a NUL, so it has to arrive
        // through a builtin that parses one.
        let source = "builtins.fromJSON \"\\\"\\\\u0000\\\"\"";
        assert_eq!(
            s.eval(source),
            Err(IXE_ERR_EVAL),
            "a NUL-bearing string now evaluates, so out_string's substitution \
             is live code deciding what a user sees rather than dead defence"
        );
        assert!(
            s.error().unwrap_or_default().contains("null bytes"),
            "the refusal should still be the evaluator's own"
        );
    }

    #[test]
    fn a_freed_handle_is_gone_and_freeing_twice_is_quiet() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let h = s.eval("1").unwrap_or(0);
        unsafe { ixe_handle_free(s.0, h) };
        assert_eq!(s.ty(h), IXE_TYPE_UNKNOWN_HANDLE);
        unsafe { ixe_handle_free(s.0, h) };
    }

    /// Dynamic attribute origins follow set ownership. A failed request has
    /// no handle to retain its set, and freeing the only successful handle
    /// must release its origin without waiting for session destruction.
    #[test]
    fn dynamic_attribute_origins_follow_live_handles() {
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let source = r#"let k = "x"; in builtins.seq ({ ${k} = 1; }) (throw "x")"#;
        for _ in 0..128 {
            assert_eq!(s.eval(source), Err(IXE_ERR_THROWN));
        }
        // No growth across the failures: at most the one slot the set used,
        // and none at all when the module was compiled per call and went
        // with the evaluation (a session with no `eval-cache-dir` compiles
        // through a per-call memo, so nothing of it outlives the failure).
        assert!(
            s.dynamic_attr_origin_slots() <= 1,
            "dynamic attribute origin slots grew across failed evaluations: {}",
            s.dynamic_attr_origin_slots()
        );
        assert_eq!(s.live_dynamic_attr_origins(), 0);

        let reset = s.eval("null").expect("the reset evaluation succeeds");
        assert_eq!(s.live_dynamic_attr_origins(), 0);
        unsafe { ixe_handle_free(s.0, reset) };

        let dynamic = s
            .eval(r#"let k = "x"; in { ${k} = 1; }"#)
            .expect("the dynamic set evaluates");
        assert_eq!(s.live_dynamic_attr_origins(), 1);
        unsafe { ixe_handle_free(s.0, dynamic) };
        assert_eq!(s.live_dynamic_attr_origins(), 0);
    }

    /// A message read once must not be read again and blamed on a later
    /// call that succeeded.
    #[test]
    fn taking_an_error_clears_it() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        assert_eq!(s.eval("1 +"), Err(IXE_ERR_PARSE));
        assert!(s.error().is_some());
        assert_eq!(s.error(), None);
    }

    /// Each failure class keeps its own status through the handle path, the
    /// same contract `ixe_eval_expr` has, so an embedder raises the same
    /// exception whichever entry point it used.
    #[test]
    fn failure_classes_survive_the_handle_path() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        for (source, want) in [
            ("throw \"x\"", IXE_ERR_THROWN),
            ("assert false; 1", IXE_ERR_ASSERT),
            ("let a = a; in a", IXE_ERR_EVAL),
            ("1 +", IXE_ERR_PARSE),
        ] {
            assert_eq!(s.eval(source), Err(want), "class of {source}");
            let _ = s.error();
        }
    }

    /// A throw inside a value that was not selected stays unthrown, but one
    /// inside a value that IS rendered has to happen: rendering is strict in
    /// both backends.
    #[test]
    fn rendering_a_selected_subtree_forces_it() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let root = s
            .eval("{ keep = { deep = throw \"reached\"; }; }")
            .unwrap_or(0);
        let keep = s.select(root, "keep").unwrap_or(0);
        assert_eq!(s.render(keep, IXE_RENDER_PLAIN), Err(IXE_ERR_THROWN));
        assert_eq!(s.error().as_deref(), Some("reached"));
    }

    #[test]
    fn asking_a_list_question_of_a_set_is_a_bad_call_that_says_so() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let s = Sess::new();
        let h = s.eval("{ a = 1; }").unwrap_or(0);
        let mut len = 0usize;
        assert_eq!(
            unsafe { ixe_list_len(s.0, h, &raw mut len) },
            IXE_ERR_BADCALL
        );
        assert_eq!(
            s.error().as_deref(),
            Some("expected a list but found a set")
        );
    }

    /// Applying a function does not force what the application did not need.
    ///
    /// The point of the lazy handle: `nix eval <flake>#lib.version` applies
    /// `call-flake.nix` to three arguments and then enters two attributes,
    /// and a `throw` in a sibling must stay unexploded. An eager
    /// `ixe_apply` would pass every other test here and fail this one.
    #[test]
    fn apply_is_lazy_in_the_result() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        let f = s
            .eval("x: { a = x; b = throw \"the sibling was forced\"; }")
            .unwrap_or_else(|e| unreachable!("function: {e:?}"));
        let arg = s
            .eval("41")
            .unwrap_or_else(|e| unreachable!("argument: {e:?}"));
        let mut applied = 0u64;
        assert_eq!(
            unsafe { ixe_apply(s.raw(), f, arg, &raw mut applied) },
            IXE_OK
        );
        // Not forced yet: the handle names a cell, not a value.
        assert_eq!(s.ty(applied), IXE_TYPE_UNFORCED);
        let a = s
            .select(applied, "a")
            .unwrap_or_else(|e| unreachable!("a: {e:?}"));
        assert_eq!(s.render(a, IXE_RENDER_PLAIN).as_deref(), Ok("41"));
    }

    /// A curried call is two applications, as the IR does it.
    #[test]
    fn apply_curries() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        let f = s
            .eval("a: b: a - b")
            .unwrap_or_else(|e| unreachable!("function: {e:?}"));
        let ten = s
            .eval("10")
            .unwrap_or_else(|e| unreachable!("argument: {e:?}"));
        let four = s
            .eval("4")
            .unwrap_or_else(|e| unreachable!("argument: {e:?}"));
        let mut once = 0u64;
        assert_eq!(unsafe { ixe_apply(s.raw(), f, ten, &raw mut once) }, IXE_OK);
        let mut twice = 0u64;
        assert_eq!(
            unsafe { ixe_apply(s.raw(), once, four, &raw mut twice) },
            IXE_OK
        );
        assert_eq!(s.render(twice, IXE_RENDER_PLAIN).as_deref(), Ok("6"));
    }

    /// cppnix's `forceFunction` passes a `__functor` set, so this does too.
    /// Everything built on `lib.makeOverridable` is one.
    #[test]
    fn apply_accepts_a_functor_set() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        let f = s
            .eval("{ __functor = self: x: x + 1; }")
            .unwrap_or_else(|e| unreachable!("functor: {e:?}"));
        let arg = s
            .eval("1")
            .unwrap_or_else(|e| unreachable!("argument: {e:?}"));
        let mut applied = 0u64;
        assert_eq!(
            unsafe { ixe_apply(s.raw(), f, arg, &raw mut applied) },
            IXE_OK
        );
        assert_eq!(s.render(applied, IXE_RENDER_PLAIN).as_deref(), Ok("2"));
    }

    /// Applying a non-function fails at the call, not silently at a force
    /// that may never happen.
    #[test]
    fn apply_refuses_a_non_function_immediately() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        let f = s
            .eval("1")
            .unwrap_or_else(|e| unreachable!("an integer: {e:?}"));
        let arg = s
            .eval("2")
            .unwrap_or_else(|e| unreachable!("argument: {e:?}"));
        let mut applied = 0u64;
        assert_eq!(
            unsafe { ixe_apply(s.raw(), f, arg, &raw mut applied) },
            IXE_ERR_BADCALL
        );
        assert_eq!(applied, 0, "no handle should have been written");
        let message = s.error().unwrap_or_default();
        assert!(
            message.contains("not a function"),
            "expected cppnix's wording, got {message:?}"
        );
    }

    /// An unknown argument handle is reported as that, rather than after an
    /// evaluation of the function that may fail for its own reasons.
    #[test]
    fn apply_reports_an_unknown_argument_handle() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        let f = s
            .eval("x: x")
            .unwrap_or_else(|e| unreachable!("function: {e:?}"));
        let mut applied = 0u64;
        assert_eq!(
            unsafe { ixe_apply(s.raw(), f, 999_999, &raw mut applied) },
            IXE_ERR_BADCALL
        );
    }

    fn alloc_json(s: &Sess, doc: &str) -> Result<u64, i32> {
        let mut out = 0u64;
        let status = unsafe { ixe_alloc_json(s.raw(), doc.as_ptr(), doc.len(), &raw mut out) };
        if status == IXE_OK {
            Ok(out)
        } else {
            Err(status)
        }
    }

    /// A JSON document becomes the value `builtins.fromJSON` would produce.
    #[test]
    fn alloc_json_builds_the_same_value_from_json_does() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        let doc = r#"{"a":[1,2.5,"s",true,null],"b":{"c":-3}}"#;
        let built = alloc_json(&s, doc).unwrap_or_else(|e| unreachable!("built: {e:?}"));
        let parsed = s
            .eval(&format!("builtins.fromJSON ''{doc}''"))
            .unwrap_or_else(|e| unreachable!("fromJSON: {e:?}"));
        assert_eq!(
            s.render(built, IXE_RENDER_JSON),
            s.render(parsed, IXE_RENDER_JSON)
        );
    }

    /// The escape produces a string that carries the path as its context, so
    /// a derivation built from it depends on the tree.
    ///
    /// Checked through `builtins.getContext`, which is the only way a Nix
    /// program can see a context -- and checked against the *absence* of one
    /// on the plain spelling, because a test that only looked at the escaped
    /// case would pass against an implementation that gave every string a
    /// context.
    #[test]
    fn the_store_path_escape_carries_context_and_the_plain_string_does_not() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        let path = "/nix/store/00000000000000000000000000000000-src";
        let get_context = s
            .eval("builtins.getContext")
            .unwrap_or_else(|e| unreachable!("getContext: {e:?}"));

        let escaped = alloc_json(&s, &format!("{{\"__storePath\":\"{path}\"}}"))
            .unwrap_or_else(|e| unreachable!("escaped: {e:?}"));
        let mut applied = 0u64;
        assert_eq!(
            unsafe { ixe_apply(s.raw(), get_context, escaped, &raw mut applied) },
            IXE_OK
        );
        assert_eq!(
            s.render(applied, IXE_RENDER_JSON).as_deref(),
            Ok(format!("{{\"{path}\":{{\"path\":true}}}}").as_str())
        );

        let plain =
            alloc_json(&s, &format!("\"{path}\"")).unwrap_or_else(|e| unreachable!("plain: {e:?}"));
        let mut applied = 0u64;
        assert_eq!(
            unsafe { ixe_apply(s.raw(), get_context, plain, &raw mut applied) },
            IXE_OK
        );
        assert_eq!(s.render(applied, IXE_RENDER_JSON).as_deref(), Ok("{}"));
    }

    /// The escape is recognised wherever it appears, not only at the root:
    /// the overrides set the flake entry hands over nests it two deep.
    #[test]
    fn the_store_path_escape_is_recognised_when_nested() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        let path = "/nix/store/00000000000000000000000000000000-src";
        let doc = format!(
            "{{\"root\":{{\"sourceInfo\":{{\"outPath\":{{\"__storePath\":\"{path}\"}}}}}}}}"
        );
        let built = alloc_json(&s, &doc).unwrap_or_else(|e| unreachable!("built: {e:?}"));
        let get_context = s
            .eval("builtins.getContext")
            .unwrap_or_else(|e| unreachable!("getContext: {e:?}"));
        let deep = s
            .select(built, "root")
            .and_then(|h| s.select(h, "sourceInfo"))
            .and_then(|h| s.select(h, "outPath"))
            .unwrap_or_else(|e| unreachable!("outPath: {e:?}"));
        let mut applied = 0u64;
        assert_eq!(
            unsafe { ixe_apply(s.raw(), get_context, deep, &raw mut applied) },
            IXE_OK
        );
        assert_eq!(
            s.render(applied, IXE_RENDER_JSON).as_deref(),
            Ok(format!("{{\"{path}\":{{\"path\":true}}}}").as_str())
        );
    }

    /// The escape is the bridge's, and a Nix program cannot reach it.
    ///
    /// `builtins.fromJSON` on the same document must produce an ordinary
    /// attribute set with a `__storePath` *attribute* and no context
    /// anywhere. If it produced the bridge's string instead, any expression
    /// could name any store path as a dependency of a derivation without the
    /// evaluator having produced it -- a forged input, which is a store
    /// integrity hole and not a convenience.
    ///
    /// Checked by asking `builtins.getContext` about the result rather than
    /// by reading which function calls which: the two decoders share their
    /// scalar rule today and could come to share more.
    #[test]
    fn the_store_path_escape_is_not_reachable_from_user_json() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        let path = "/nix/store/00000000000000000000000000000000-src";
        let doc = format!("{{\"__storePath\":\"{path}\"}}");

        // What a Nix program gets: a set with the key as an attribute.
        let user = s
            .eval(&format!("builtins.fromJSON ''{doc}''"))
            .unwrap_or_else(|e| unreachable!("fromJSON: {e:?}"));
        assert_eq!(
            s.render(user, IXE_RENDER_JSON).as_deref(),
            Ok(doc.as_str()),
            "builtins.fromJSON honoured the bridge's escape, so a program can forge string context"
        );

        // And nothing under it carries context.
        let get_context = s
            .eval("builtins.getContext")
            .unwrap_or_else(|e| unreachable!("getContext: {e:?}"));
        let inner = s
            .select(user, crate::primops_pure::STORE_PATH_ESCAPE)
            .unwrap_or_else(|e| unreachable!("the attribute: {e:?}"));
        let mut applied = 0u64;
        assert_eq!(
            unsafe { ixe_apply(s.raw(), get_context, inner, &raw mut applied) },
            IXE_OK
        );
        assert_eq!(
            s.render(applied, IXE_RENDER_JSON).as_deref(),
            Ok("{}"),
            "a string decoded by builtins.fromJSON carries context"
        );

        // The bridge's own path still honours it, so this test cannot pass by
        // the escape having been deleted.
        let bridge = alloc_json(&s, &doc).unwrap_or_else(|e| unreachable!("bridge: {e:?}"));
        let mut applied = 0u64;
        assert_eq!(
            unsafe { ixe_apply(s.raw(), get_context, bridge, &raw mut applied) },
            IXE_OK
        );
        assert_eq!(
            s.render(applied, IXE_RENDER_JSON).as_deref(),
            Ok(format!("{{\"{path}\":{{\"path\":true}}}}").as_str())
        );
    }

    /// A mistyped escape fails rather than decoding as an ordinary set. An
    /// escape with a quiet literal fallback loses a store path's context on
    /// the day somebody adds a key beside it.
    #[test]
    fn a_malformed_store_path_escape_is_an_error() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        assert!(alloc_json(&s, r#"{"__storePath":"/nix/store/x","dir":""}"#).is_err());
        assert!(alloc_json(&s, r#"{"__storePath":7}"#).is_err());
        assert!(alloc_json(&s, "not json").is_err());
    }

    fn internal_primop(s: &Sess, name: &str) -> Result<u64, i32> {
        let mut out = 0u64;
        let status =
            unsafe { ixe_internal_primop(s.raw(), name.as_ptr(), name.len(), &raw mut out) };
        if status == IXE_OK {
            Ok(out)
        } else {
            Err(status)
        }
    }

    /// `fetchFinalTree` is reachable through the internal-primop lookup and
    /// through nothing else.
    ///
    /// The second half is the one worth having. cppnix files an
    /// `.internal = true` primop in `internalPrimOps` and in neither the set
    /// nor the scope, so a backend that merely added it to its table would
    /// answer `true` to `builtins ? fetchFinalTree` where cppnix answers
    /// `false` -- a divergence visible to any program that looks.
    #[test]
    fn the_internal_primop_is_reachable_only_through_this_call() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        let f = internal_primop(&s, "fetchFinalTree")
            .unwrap_or_else(|e| unreachable!("fetchFinalTree: {e:?}"));
        assert_eq!(s.ty(f), IXE_TYPE_FUNCTION);

        let member = s
            .eval("builtins ? fetchFinalTree")
            .unwrap_or_else(|e| unreachable!("membership: {e:?}"));
        assert_eq!(s.render(member, IXE_RENDER_PLAIN).as_deref(), Ok("false"));

        // Not in the global scope either, which is the other half of
        // `addPrimOp`'s behaviour for a skipped primop.
        assert!(
            s.eval("fetchFinalTree")
                .and_then(|h| s.render(h, IXE_RENDER_PLAIN))
                .is_err(),
            "the bare global resolved, so the name reached the scope"
        );
    }

    /// An ordinarily registered primop is refused: it already has a spelling
    /// a program can write, and a second one here would differ in whether
    /// the gate applies.
    #[test]
    fn the_internal_primop_lookup_refuses_an_ordinary_name() {
        let _held = crate::eval::globals_shared();
        let s = Sess::new();
        assert_eq!(internal_primop(&s, "fetchTree"), Err(IXE_ERR_BADCALL));
        assert_eq!(internal_primop(&s, "map"), Err(IXE_ERR_BADCALL));
        assert_eq!(internal_primop(&s, "nope"), Err(IXE_ERR_BADCALL));
    }
}

#[cfg(test)]
mod one_call_token_tests {
    //! The refusal token has to survive the *session-less* call too.
    //!
    //! It did not, and the way it failed is the reason these tests exist as a
    //! pair rather than one. `ixe_session_eval` carried tokens correctly, so
    //! every check through the handle API passed and the census looked
    //! healthy from `nix eval`. Meanwhile `nix-instantiate --eval` of a whole
    //! expression takes `ixe_eval_expr`, which had nowhere to put a token, so
    //! the commonest path in the fleet reported `unrecorded` for everything
    //! (ENG-12819). One arm green and one arm blind reads exactly like two
    //! arms green.

    use super::*;
    use std::ffi::CStr;

    /// Evaluate `source` through the one-call ABI and report `(status, token)`.
    fn eval_once(source: &str) -> (i32, Option<String>) {
        let mut out: *mut c_char = std::ptr::null_mut();
        let mut token: *const c_char = std::ptr::null();
        let status = unsafe {
            ixe_eval_expr(
                std::ptr::null(),
                source.as_ptr(),
                source.len(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                &raw mut out,
                &raw mut token,
                std::ptr::null_mut(),
            )
        };
        // SAFETY: `out` is whatever the call above wrote, which is either
        // null or a string this crate allocated.
        unsafe { ixe_string_free(out) };
        let name = if token.is_null() {
            None
        } else {
            // SAFETY: static storage owned by the crate, NUL-terminated.
            Some(
                unsafe { CStr::from_ptr(token) }
                    .to_string_lossy()
                    .into_owned(),
            )
        };
        (status, name)
    }

    /// A refusal names its kind, and the name is the one the crate's own
    /// `Refusal` carried rather than a sentinel standing in for it.
    #[test]
    fn a_refusal_reports_its_token() {
        let _moving = crate::eval::globals_moving();
        let (status, token) = eval_once(REFUSED_EXPRESSION);
        assert_eq!(status, IXE_ERR_UNIMPLEMENTED, "expected a refusal");
        assert_eq!(token.as_deref(), Some(REFUSED_EXPRESSION_TOKEN));
    }

    /// Two different refusals report two different tokens. One row alone
    /// would pass against an implementation that hard-coded any single name.
    ///
    /// The second row invokes an implemented builtin without its required host
    /// capability. It must carry the store refusal through the same one-call ABI.
    #[test]
    fn two_refusal_kinds_report_two_tokens() {
        let _moving = crate::eval::globals_moving();
        let (comparison_status, comparison) = eval_once(REFUSED_EXPRESSION);
        let (store_status, store) = eval_once("builtins.toFile \"token-test\" \"contents\"");
        assert_eq!(comparison_status, IXE_ERR_UNIMPLEMENTED);
        assert_eq!(store_status, IXE_ERR_UNIMPLEMENTED);
        assert_eq!(comparison.as_deref(), Some(REFUSED_EXPRESSION_TOKEN));
        assert_eq!(store.as_deref(), Some("store-unavailable"));
        assert_ne!(comparison, store);
    }

    /// A failure that is *not* a refusal reports no token at all. Without
    /// this, an implementation that always wrote the last refusal it saw
    /// would satisfy the two tests above and mislabel every ordinary error
    /// as a refusal of whatever kind came before it.
    #[test]
    fn an_ordinary_failure_reports_no_token() {
        let _moving = crate::eval::globals_moving();
        let (status, token) = eval_once("throw \"boom\"");
        assert_eq!(status, IXE_ERR_THROWN);
        assert_eq!(token, None);
    }

    /// And a success reports none either, so a caller that reads the token
    /// unconditionally cannot be handed a stale one.
    #[test]
    fn a_success_reports_no_token() {
        let _moving = crate::eval::globals_moving();
        let (status, token) = eval_once("1 + 1");
        assert_eq!(status, IXE_OK);
        assert_eq!(token, None);
    }

    /// A caller that does not want the token passes null and is not a crash.
    /// The handle path has callers that do exactly this.
    #[test]
    fn a_null_token_slot_is_accepted() {
        let _moving = crate::eval::globals_moving();
        let source = REFUSED_EXPRESSION;
        let mut out: *mut c_char = std::ptr::null_mut();
        let status = unsafe {
            ixe_eval_expr(
                std::ptr::null(),
                source.as_ptr(),
                source.len(),
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                &raw mut out,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        // SAFETY: as above.
        unsafe { ixe_string_free(out) };
        assert_eq!(status, IXE_ERR_UNIMPLEMENTED);
    }
}

#[cfg(test)]
mod roundtrip_tests {
    //! One table, one row per value kind that crosses the C ABI, asking the
    //! same question of each: does anything the language distinguishes get
    //! lost on the way out?
    //!
    //! This exists because of how the string API's exception-class bug went.
    //! A `throw` kept its class through evaluation and lost it through
    //! memoisation, so the wrong exception appeared on the second run only,
    //! and every first-run test passed. The lesson generalises past errors:
    //! any property a value carries has to be checked at the boundary
    //! specifically, because the boundary is a separate piece of code from
    //! the evaluator and nothing else exercises it.
    //!
    //! Each row asks three things and requires them to agree: the type tag,
    //! the typed accessor, and the renderer. A kind whose accessor and
    //! renderer disagree is the failure this table is for -- one of them is
    //! being read by somebody.

    use super::handle_tests::*;
    use super::*;

    /// What a kind must be able to say about itself across the ABI.
    struct Row {
        /// Expression producing a value of this kind.
        source: &'static str,
        /// The tag `ixe_value_type` must report.
        tag: i32,
        /// What the typed accessor must give back, rendered as text for
        /// comparison, or `None` when this kind has no typed accessor at all
        /// -- which is itself a finding, not a pass.
        accessor: Option<&'static str>,
        /// What `IXE_RENDER_PLAIN` must produce.
        rendered: &'static str,
    }

    /// Read a kind's value through whichever typed accessor it has. `None`
    /// means the ABI offers no way to read this kind except by rendering it.
    fn through_accessor(session: &Sess, handle: u64, tag: i32) -> Option<String> {
        match tag {
            IXE_TYPE_INT => {
                let mut n = 0i64;
                (unsafe { ixe_get_int(session.raw(), handle, &raw mut n) } == IXE_OK)
                    .then(|| n.to_string())
            }
            IXE_TYPE_BOOL => {
                let mut b = 0i32;
                (unsafe { ixe_get_bool(session.raw(), handle, &raw mut b) } == IXE_OK)
                    .then(|| if b == 0 { "false" } else { "true" }.to_owned())
            }
            IXE_TYPE_STRING | IXE_TYPE_PATH => {
                let mut out: *mut c_char = std::ptr::null_mut();
                let status = unsafe { ixe_get_string(session.raw(), handle, &raw mut out) };
                let text = take_c_string(out);
                (status == IXE_OK).then_some(text).flatten()
            }
            IXE_TYPE_LIST => {
                let mut len = 0usize;
                (unsafe { ixe_list_len(session.raw(), handle, &raw mut len) } == IXE_OK)
                    .then(|| len.to_string())
            }
            IXE_TYPE_ATTRS => {
                let mut len = 0usize;
                (unsafe { ixe_attrs_len(session.raw(), handle, &raw mut len) } == IXE_OK)
                    .then(|| len.to_string())
            }
            IXE_TYPE_FLOAT => {
                let mut x = 0f64;
                (unsafe { ixe_get_float(session.raw(), handle, &raw mut x) } == IXE_OK)
                    .then(|| crate::value2::format_g6(x))
            }
            // `null` and a function are the only kinds with nothing to read:
            // null *is* its tag, and a function is deliberately opaque. Any
            // other tag arriving here is a kind whose accessor was forgotten.
            _ => None,
        }
    }

    #[test]
    fn every_kind_reads_back_the_same_through_the_accessor_and_the_renderer() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let table = [
            Row {
                source: "0",
                tag: IXE_TYPE_INT,
                accessor: Some("0"),
                rendered: "0",
            },
            Row {
                source: "42",
                tag: IXE_TYPE_INT,
                accessor: Some("42"),
                rendered: "42",
            },
            Row {
                source: "(-1)",
                tag: IXE_TYPE_INT,
                accessor: Some("-1"),
                rendered: "-1",
            },
            // The extremes, because an accessor that truncates or wraps is a
            // plausible wrong answer and nothing in the corpus reaches here.
            Row {
                source: "9223372036854775807",
                tag: IXE_TYPE_INT,
                accessor: Some("9223372036854775807"),
                rendered: "9223372036854775807",
            },
            Row {
                source: "(-9223372036854775807 - 1)",
                tag: IXE_TYPE_INT,
                accessor: Some("-9223372036854775808"),
                rendered: "-9223372036854775808",
            },
            Row {
                source: "true",
                tag: IXE_TYPE_BOOL,
                accessor: Some("true"),
                rendered: "true",
            },
            Row {
                source: "false",
                tag: IXE_TYPE_BOOL,
                accessor: Some("false"),
                rendered: "false",
            },
            Row {
                source: "null",
                tag: IXE_TYPE_NULL,
                accessor: None,
                rendered: "null",
            },
            Row {
                source: "\"\"",
                tag: IXE_TYPE_STRING,
                accessor: Some(""),
                rendered: "\"\"",
            },
            Row {
                source: "\"hi\"",
                tag: IXE_TYPE_STRING,
                accessor: Some("hi"),
                rendered: "\"hi\"",
            },
            // The accessor gives bytes, the renderer gives the escaped
            // literal. Both are right and they must not be confused for each
            // other, which is why the row carries them separately.
            Row {
                source: "\"a\\nb\\t\\\"q\\\"\"",
                tag: IXE_TYPE_STRING,
                accessor: Some("a\nb\t\"q\""),
                rendered: "\"a\\nb\\t\\\"q\\\"\"",
            },
            Row {
                source: "\"unicode é ☃\"",
                tag: IXE_TYPE_STRING,
                accessor: Some("unicode é ☃"),
                rendered: "\"unicode é ☃\"",
            },
            Row {
                source: "/tmp/a/b",
                tag: IXE_TYPE_PATH,
                accessor: Some("/tmp/a/b"),
                rendered: "/tmp/a/b",
            },
            Row {
                source: "[ ]",
                tag: IXE_TYPE_LIST,
                accessor: Some("0"),
                rendered: "[ ]",
            },
            Row {
                source: "[ 1 2 3 ]",
                tag: IXE_TYPE_LIST,
                accessor: Some("3"),
                rendered: "[ 1 2 3 ]",
            },
            Row {
                source: "{ }",
                tag: IXE_TYPE_ATTRS,
                accessor: Some("0"),
                rendered: "{ }",
            },
            Row {
                source: "{ a = 1; b = 2; }",
                tag: IXE_TYPE_ATTRS,
                accessor: Some("2"),
                rendered: "{ a = 1; b = 2; }",
            },
            Row {
                source: "x: x",
                tag: IXE_TYPE_FUNCTION,
                accessor: None,
                rendered: "<LAMBDA>",
            },
            Row {
                source: "builtins.add",
                tag: IXE_TYPE_FUNCTION,
                accessor: None,
                rendered: "<PRIMOP>",
            },
            // The accessor's text is the printer's rendering of what came
            // back, so these rows compare the number against the bytes rather
            // than restating the bytes twice.
            Row {
                source: "1.5",
                tag: IXE_TYPE_FLOAT,
                accessor: Some("1.5"),
                rendered: "1.5",
            },
            Row {
                source: "0.0",
                tag: IXE_TYPE_FLOAT,
                accessor: Some("0"),
                rendered: "0",
            },
            Row {
                source: "(0.0 - 2.25)",
                tag: IXE_TYPE_FLOAT,
                accessor: Some("-2.25"),
                rendered: "-2.25",
            },
            Row {
                source: "1.0e10",
                tag: IXE_TYPE_FLOAT,
                accessor: Some("1e+10"),
                rendered: "1e+10",
            },
        ];

        let mut checked = 0;
        for row in &table {
            let session = Sess::new();
            let evaluated = session.eval(row.source);
            assert!(
                evaluated.is_ok(),
                "{} did not evaluate: {evaluated:?}, {:?}",
                row.source,
                session.error()
            );
            let handle = evaluated.unwrap_or(0);
            assert_eq!(session.ty(handle), row.tag, "type tag of {}", row.source);
            assert_eq!(
                through_accessor(&session, handle, row.tag).as_deref(),
                row.accessor,
                "accessor for {}",
                row.source
            );
            assert_eq!(
                session.render(handle, IXE_RENDER_PLAIN),
                Ok(row.rendered.to_owned()),
                "rendering of {}",
                row.source
            );
            assert_eq!(
                session.error(),
                None,
                "{} left a message behind",
                row.source
            );
            checked += 1;
        }
        // A table that silently shrank would pass every assertion above.
        assert_eq!(checked, 23, "the round-trip table lost rows");
    }

    /// The other half of a typed accessor: it has to refuse every kind that
    /// is not its own.
    ///
    /// Reading back correctly is only half the contract. An accessor that
    /// also answers for a neighbouring kind is a silent coercion, and the
    /// Nix-level distinction it erases is real -- `1` and `1.0` are different
    /// values, and a caller asking `ixe_get_float` is asking *which one it
    /// had*, not for a number by any route. This cross-product is what
    /// catches that; the round-trip table above cannot, because it only ever
    /// calls the right accessor for the kind.
    #[test]
    fn a_typed_accessor_refuses_every_kind_but_its_own() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        // Each accessor, the one expression it must accept, and the ones it
        // must not. Neighbours first: those are the coercions somebody would
        // actually write by accident.
        struct Accessor {
            name: &'static str,
            call: fn(&Sess, u64) -> i32,
            accepts: &'static str,
            rejects: &'static [&'static str],
        }

        let cases = [
            Accessor {
                name: "ixe_get_int",
                call: |s, h| {
                    let mut n = 0i64;
                    unsafe { ixe_get_int(s.raw(), h, &raw mut n) }
                },
                accepts: "1",
                rejects: &[
                    "1.5", "true", "null", "\"1\"", "/tmp/x", "[ ]", "{ }", "x: x",
                ],
            },
            Accessor {
                name: "ixe_get_float",
                call: |s, h| {
                    let mut x = 0f64;
                    unsafe { ixe_get_float(s.raw(), h, &raw mut x) }
                },
                accepts: "1.5",
                rejects: &[
                    "1", "true", "null", "\"1.5\"", "/tmp/x", "[ ]", "{ }", "x: x",
                ],
            },
            Accessor {
                name: "ixe_get_bool",
                call: |s, h| {
                    let mut b = 0i32;
                    unsafe { ixe_get_bool(s.raw(), h, &raw mut b) }
                },
                accepts: "true",
                rejects: &["1", "0", "1.5", "null", "\"true\"", "[ ]", "{ }", "x: x"],
            },
            Accessor {
                name: "ixe_get_string",
                call: |s, h| {
                    let mut out: *mut c_char = std::ptr::null_mut();
                    let status = unsafe { ixe_get_string(s.raw(), h, &raw mut out) };
                    let _ = take_c_string(out);
                    status
                },
                accepts: "\"s\"",
                // A path is deliberately absent: ixe_get_string serves both,
                // which is stated in ixe.h rather than being an accident.
                rejects: &["1", "1.5", "true", "null", "[ ]", "{ }", "x: x"],
            },
        ];

        let mut refusals = 0;
        for Accessor {
            name,
            call,
            accepts,
            rejects,
        } in cases
        {
            let session = Sess::new();
            let handle = session.eval(accepts).unwrap_or(0);
            assert_eq!(call(&session, handle), IXE_OK, "{name} refused {accepts}");
            let _ = session.error();
            for source in rejects {
                let session = Sess::new();
                let handle = session.eval(source).unwrap_or(0);
                assert_eq!(
                    call(&session, handle),
                    IXE_ERR_BADCALL,
                    "{name} accepted {source}, which is a different kind"
                );
                let message = session.error().unwrap_or_default();
                assert!(
                    message.starts_with("expected "),
                    "{name} on {source} refused without saying what it found: {message}"
                );
                refusals += 1;
            }
        }
        assert_eq!(refusals, 31, "the cross-product lost cases");
    }

    /// Every tag `ixe_value_type` can report has to be reachable from an
    /// expression and appear in the table above. Without this, adding a tag
    /// and forgetting to test it is invisible.
    #[test]
    fn the_table_covers_every_tag_the_abi_can_report() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let reachable = [
            IXE_TYPE_INT,
            IXE_TYPE_FLOAT,
            IXE_TYPE_BOOL,
            IXE_TYPE_NULL,
            IXE_TYPE_STRING,
            IXE_TYPE_PATH,
            IXE_TYPE_LIST,
            IXE_TYPE_ATTRS,
            IXE_TYPE_FUNCTION,
        ];
        let session = Sess::new();
        for (source, tag) in [
            ("1", IXE_TYPE_INT),
            ("1.5", IXE_TYPE_FLOAT),
            ("true", IXE_TYPE_BOOL),
            ("null", IXE_TYPE_NULL),
            ("\"s\"", IXE_TYPE_STRING),
            ("/tmp/x", IXE_TYPE_PATH),
            ("[ ]", IXE_TYPE_LIST),
            ("{ }", IXE_TYPE_ATTRS),
            ("x: x", IXE_TYPE_FUNCTION),
        ] {
            assert!(reachable.contains(&tag));
            let handle = session.eval(source).unwrap_or(0);
            assert_eq!(session.ty(handle), tag, "{source}");
        }
    }

    /// Every kind that carries a value a caller could want has a typed
    /// accessor for it. `null` and a function do not: null *is* its tag, and
    /// a function is deliberately opaque. A float is neither, so it needs one.
    #[test]
    fn every_kind_with_a_value_has_a_way_to_read_it() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let session = Sess::new();
        let handle = session.eval("1.5").unwrap_or(0);
        assert_eq!(session.ty(handle), IXE_TYPE_FLOAT);
        let mut x = 0f64;
        assert_eq!(
            unsafe { ixe_get_float(session.raw(), handle, &raw mut x) },
            IXE_OK,
            "a float crosses and reports its tag, so it must be readable as a number \
             rather than only as rendered text"
        );
        assert!((x - 1.5).abs() < f64::EPSILON, "float read back as {x}");
    }

    /// A string's context is part of the string. Dropping it silently at the
    /// boundary is the same shape as the exception class that survived
    /// evaluation and not memoisation: the value looks complete and is not.
    #[test]
    fn a_string_carrying_context_cannot_be_read_as_a_bare_string() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let session = Sess::with_fake_store();
        let evaluated = session.eval("\"${/tmp/ctx-probe}\"");
        assert!(
            evaluated.is_ok(),
            "no context-bearing string to test with: {evaluated:?} {:?}",
            session.error()
        );
        let handle = evaluated.unwrap_or(0);
        assert_eq!(session.ty(handle), IXE_TYPE_STRING);
        let mut out: *mut c_char = std::ptr::null_mut();
        let status = unsafe { ixe_get_string(session.raw(), handle, &raw mut out) };
        let got = take_c_string(out);
        assert_ne!(
            status, IXE_OK,
            "ixe_get_string handed back {got:?} with the context dropped and no way for \
             the caller to know one existed"
        );
        let message = session.error().unwrap_or_default();
        assert!(
            message.contains("context"),
            "the refusal has to name what was lost, got: {message}"
        );
    }

    /// The other half: a string with no context reads back normally, so the
    /// refusal above is scoped to the thing it is about.
    #[test]
    fn a_string_without_context_still_reads_back() {
        // This test's subject is the process-global configuration the C ABI
        // sets, so it reads it deliberately and holds it still while it does.
        let _globals = crate::eval::globals_shared();
        let session = Sess::new();
        let handle = session.eval("\"plain\"").unwrap_or(0);
        let mut out: *mut c_char = std::ptr::null_mut();
        assert_eq!(
            unsafe { ixe_get_string(session.raw(), handle, &raw mut out) },
            IXE_OK
        );
        assert_eq!(take_c_string(out).as_deref(), Some("plain"));
    }
}

/// ENG-12830 end to end at the C ABI: the handle path serves, and the read
/// set it files covers the walk.
///
/// The second of those is the soundness half and the reason `machine_and_host`
/// exists. Between the two calls of the question protocol the embedder is
/// forcing values, and every one of those forces reaches the world. If they
/// went through `RealFs` instead of the session's recorder the row would be
/// filed under a read set that names only what the first evaluation asked,
/// and an edit to a file the *walk* read would not invalidate it. That is a
/// stale answer served for ever, visible only on the second run, and it is
/// the failure this whole change could most plausibly have introduced.
#[cfg(test)]
mod warm_starts {
    use super::handle_tests::take_c_string;
    use super::*;

    fn scratch(label: &str) -> std::path::PathBuf {
        crate::eval::scratch_dir("ixe-warm", label)
    }

    /// Exclusive use of the process settings for a test, and a check that they
    /// did not move, run however the test ends.
    ///
    /// Every test here records a row in one session and looks it up in
    /// another, and the key is built from `Settings::current()`, so a setting
    /// that changes in between makes the row unaddressable and the test
    /// reports a cache that has stopped serving. `store_dir` is the one that
    /// does it: the setter is a `OnceLock`, so it transitions exactly once from
    /// unset to set. Making that transition happen here, before anything is
    /// recorded, removes it from the window; the value is the same one every
    /// caller uses, so this perturbs nobody.
    ///
    /// # Why this is a guard and not a returned hash
    ///
    /// It used to return the fingerprint for the test to compare at the end,
    /// and that had two failure modes, both of which happened.
    ///
    /// Two tests dropped the returned hash and never compared it (ENG-13012),
    /// so their check could not fail. A guard cannot be forgotten: the
    /// comparison is the destructor.
    ///
    /// Worse, the comparison was written *after* the assertions it exists to
    /// disambiguate. Its whole message is "any failure above is about a moved
    /// setting rather than about the cache" -- and on the run where something
    /// above did fail, the panic unwound past it and it never ran. It could
    /// only report on runs where there was nothing to report. ENG-13024 is the
    /// open ~1% failure that this blindness has so far made un-triageable:
    /// each occurrence ends in "cannot tell whether a setting moved". A
    /// destructor runs during the unwind, so it reports on exactly the run
    /// that needs it.
    ///
    /// Holding the write guard rather than asking the test to take one is the
    /// same reasoning as `CacheDir`: the only way to reach the pin is through
    /// the thing that takes the lock, so a test cannot pin without
    /// serialising (ENG-12904).
    pub(super) struct SettingsPin {
        at_entry: ix_kernel::hash::Hash,
        /// Dropped after `Drop::drop` runs, which is what lets the body read
        /// `Settings::current()` while the globals are still held. Field order
        /// is not load-bearing for that -- `drop` runs before any field is
        /// dropped -- but the guard must be owned here rather than by the
        /// caller, or the caller could release it first.
        #[allow(dead_code)]
        globals: crate::eval::GlobalsGuard,
    }

    impl SettingsPin {
        pub(super) fn exclusive() -> Self {
            let globals = crate::eval::globals_moving();
            assert!(crate::eval::set_store_dir("/nix/store").is_ok());
            assert!(crate::eval::set_host_build_identity("nix-eval-rs-test-host").is_ok());
            SettingsPin {
                at_entry: crate::eval::Settings::current().fingerprint(),
                globals,
            }
        }
    }

    impl Drop for SettingsPin {
        fn drop(&mut self) {
            let now = crate::eval::Settings::current().fingerprint();
            if now == self.at_entry {
                return;
            }
            // Already unwinding: a panic here would abort the process and take
            // the report of the original failure with it. Say the thing and
            // let that failure stand -- which is the whole point, since the
            // reader needs to know the two are related.
            if std::thread::panicking() {
                eprintln!(
                    "\nENG-13024: a process setting ALSO moved during this test, so the \
                     memo key moved with it. The failure above is very likely about that \
                     and not about the cache.\n"
                );
                return;
            }
            assert_eq!(
                self.at_entry, now,
                "a process setting moved during this test, so the memo key moved with it"
            );
        }
    }

    /// One counted string, for a call that passes a single attribute path.
    ///
    /// The borrow is the caller's: `IxeBytes` holds a raw pointer, so the
    /// `&str` it is built from has to outlive the call it is passed to.
    fn one_path(path: &str) -> IxeBytes {
        IxeBytes {
            text: path.as_ptr(),
            len: path.len(),
        }
    }

    /// Points the evaluator at `dir` and unpoints it however the test ends.
    ///
    /// A guard rather than a line at the end of each test: a panic skips the
    /// line, and the next test would then run against this one's cache.
    ///
    /// It does not delete the directory, which is not laziness. `eval-cache-dir`
    /// is a process global and the tests that only *read* the globals take no
    /// lock (`Sess` explains why they cannot), so while this is set any
    /// concurrent test may be evaluating against the same store. Sharing the
    /// store is harmless -- rows key on the source and the settings, so a
    /// neighbour gets its own answers and merely gets them faster -- but
    /// deleting it under them is not: the first draft of these tests removed
    /// the directory and
    /// `every_kind_reads_back_the_same_through_the_accessor_and_the_renderer`
    /// began failing intermittently with "publishing .../objects/ebde92b1...:
    /// No such file or directory", an error naming a scratch directory that
    /// test has never heard of. A few kilobytes left in the temp directory is
    /// the cheaper half of that trade.
    ///
    /// Sharing the *setting* is not harmless, which is what the lock below is
    /// for.
    /// The guard is held for its `Drop`, never read, which `dead_code` sees
    /// as an unused field.
    pub(super) struct CacheDir(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    /// One holder of `eval-cache-dir` at a time.
    ///
    /// The setting is a process global and these tests each point it at their
    /// own scratch store, so two of them running at once means the second
    /// one's directory is what the first one's second session opens -- and
    /// the first then reports a cold miss where it recorded a row. That is
    /// the ENG-12939 race, and it is not a flake in the "occasionally fails"
    /// sense: `cargo test --lib warm_starts` failed 4 of 10 reproducibly
    /// before this lock and passes with `--test-threads=1`. The comment this
    /// replaces said sharing the directory was harmless, which is true of the
    /// *contents* -- rows key on the source -- and false of the pointer.
    static CACHE_DIR_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    impl CacheDir {
        pub(super) fn set(dir: &std::path::Path) -> Self {
            // The lock is taken by the guard rather than by each test, so a
            // test that sets the directory cannot forget to serialise: there
            // is no way to reach the setter except through this.
            let held = CACHE_DIR_LOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let text = dir.to_string_lossy().into_owned();
            // SAFETY: the pointer is to a live local for the length of the call.
            unsafe { ixe_set_eval_cache_dir(text.as_ptr(), text.len()) };
            CacheDir(held)
        }
    }

    impl Drop for CacheDir {
        fn drop(&mut self) {
            // SAFETY: a null path is the documented "off" spelling.
            unsafe { ixe_set_eval_cache_dir(std::ptr::null(), 0) };
        }
    }

    /// One whole question, driven the way `rust-eval-session.cc` drives it:
    /// ask, walk if told to, render, report. A new session every time, which
    /// is what a new process gets.
    ///
    /// Returns the answer and the `IXE_SERVE_*` mode, because a test that
    /// only compared answers could not tell a hit from a re-evaluation that
    /// happened to agree -- which is every correct cache and every cache that
    /// has silently stopped serving.
    fn ask(source: &str, attr_path: &str) -> (String, i32) {
        ask_applied(source, attr_path, &[])
    }

    /// The same, for a source that is applied to arguments first: the shape a
    /// flake evaluand has.
    ///
    /// `arguments` is `(kind, text)` pairs, and they cross on the question
    /// call rather than through `ixe_apply` -- which is the fix this exists to
    /// exercise, and which `ixe_apply` now refuses to let anybody undo.
    fn ask_applied(source: &str, attr_path: &str, arguments: &[(i32, &str)]) -> (String, i32) {
        let args: Vec<IxeArgument> = arguments
            .iter()
            .map(|(kind, text)| IxeArgument {
                kind: *kind,
                text: IxeBytes {
                    text: text.as_ptr(),
                    len: text.len(),
                },
            })
            .collect();
        // SAFETY: every pointer is to a live local, and the session is freed
        // on every path out.
        unsafe {
            let session = session_without_embedder();
            assert!(!session.is_null(), "no session");
            let base = ".";
            let mut mode = -1;
            let mut root = 0u64;
            let mut answer: *mut c_char = std::ptr::null_mut();
            let rc = ixe_session_eval_question(
                session,
                source.as_ptr(),
                source.len(),
                base.as_ptr(),
                base.len(),
                std::ptr::null(),
                0,
                if args.is_empty() {
                    std::ptr::null()
                } else {
                    args.as_ptr()
                },
                args.len(),
                IXE_QUESTION_SELECT,
                &one_path(attr_path),
                1,
                1,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                0,
                IXE_RENDER_RAW,
                &mut mode,
                &mut root,
                &mut answer,
            );
            assert_eq!(rc, IXE_OK, "the question failed with status {rc}");
            let served = if answer.is_null() {
                None
            } else {
                let text = CStr::from_ptr(answer).to_string_lossy().into_owned();
                ixe_string_free(answer);
                Some(text)
            };
            if mode == IXE_SERVE_ANSWER {
                ixe_session_free(session);
                return (served.unwrap_or_default(), mode);
            }
            let mut current = root;
            for component in attr_path.split('.').filter(|c| !c.is_empty()) {
                let mut next = 0u64;
                let rc = ixe_attrs_select(
                    session,
                    current,
                    component.as_ptr(),
                    component.len(),
                    &mut next,
                );
                assert_eq!(rc, IXE_OK, "selecting '{component}' failed with {rc}");
                current = next;
            }
            assert_eq!(ixe_force(session, current), IXE_OK, "forcing the selection");
            let mut out: *mut c_char = std::ptr::null_mut();
            let rc = ixe_render(session, current, IXE_RENDER_RAW, &mut out);
            assert_eq!(rc, IXE_OK, "rendering failed with {rc}");
            let fresh = CStr::from_ptr(out).to_string_lossy().into_owned();
            ixe_string_free(out);
            ixe_session_question_answer(session, IXE_OK, fresh.as_ptr(), fresh.len());
            ixe_session_free(session);
            let answer = if mode == IXE_SERVE_VERIFY {
                served.unwrap_or_default()
            } else {
                fresh
            };
            (answer, mode)
        }
    }

    /// The terminal condition of ENG-12830: a second session over the same
    /// cache directory is served rather than re-evaluated.
    ///
    /// The mode is what is asserted, not the timing. A wall clock says a hit
    /// happened on a fixture expensive enough to tell apart from process
    /// startup and says nothing on a cheap one, which is how a touched index
    /// row once scored a re-evaluation as a match. The mode is the fact.
    #[test]
    fn a_second_session_is_served_the_first_one_s_answer() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("served");
        let _cache = CacheDir::set(&dir);
        let source = r#"{ a = "the answer"; b = throw "b must not be entered"; }"#;

        let (cold, cold_mode) = ask(source, "a");
        let (warm, warm_mode) = ask(source, "a");

        assert_eq!(
            cold_mode, IXE_SERVE_EVALUATE,
            "the first session was not cold"
        );
        assert_eq!(
            warm_mode, IXE_SERVE_ANSWER,
            "the second session evaluated again: eval-cache-dir is writing and not serving, \
             which is ENG-12830 back"
        );
        assert_eq!((cold.as_str(), warm.as_str()), ("the answer", "the answer"));
    }

    /// Two questions about one module are two rows.
    ///
    /// The module object is shared and the settings are identical, so the
    /// question is the only thing telling these apart. If it were not in the
    /// key the second would be served the first's answer, and `nix eval -f
    /// x.nix a` would print what `nix eval -f x.nix b` printed.
    #[test]
    fn two_questions_about_one_module_do_not_share_a_row() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("two-questions");
        let _cache = CacheDir::set(&dir);
        let source = r#"{ a = "first"; b = "second"; }"#;

        let (first, _) = ask(source, "a");
        let (second, second_mode) = ask(source, "b");
        let (first_again, first_again_mode) = ask(source, "a");

        assert_eq!(first, "first");
        assert_eq!(
            (second.as_str(), second_mode),
            ("second", IXE_SERVE_EVALUATE),
            "a different attribute path was served the first one's row"
        );
        assert_eq!(
            (first_again.as_str(), first_again_mode),
            ("first", IXE_SERVE_ANSWER)
        );
    }

    /// One flake-document question, driven the way the flake reader drives
    /// it: ask, read the document off the root when told to, report. The
    /// document (or the session's error text) and the `IXE_SERVE_*` mode.
    fn ask_flake_document(source: &str) -> (Result<String, String>, i32) {
        ask_flake_document_at(source, "", "/flake", "/flake/flake.nix")
    }

    fn ask_flake_document_at(
        source: &str,
        mount: &str,
        base: &str,
        file: &str,
    ) -> (Result<String, String>, i32) {
        // SAFETY: every pointer is to a live local, and the session is freed
        // on every path out.
        unsafe {
            let session = session_without_embedder();
            assert!(!session.is_null(), "no session");
            assert_eq!(
                ixe_session_set_source_root(session, mount.as_ptr(), mount.len()),
                IXE_OK
            );
            let mut mode = -1;
            let mut root = 0u64;
            let mut answer: *mut c_char = std::ptr::null_mut();
            let rc = ixe_session_eval_question(
                session,
                source.as_ptr(),
                source.len(),
                base.as_ptr(),
                base.len(),
                file.as_ptr(),
                file.len(),
                std::ptr::null(),
                0,
                IXE_QUESTION_FLAKE_DOCUMENT,
                &one_path(""),
                1,
                0,
                std::ptr::null(),
                0,
                std::ptr::null(),
                0,
                0,
                IXE_RENDER_RAW,
                &mut mode,
                &mut root,
                &mut answer,
            );
            if rc != IXE_OK {
                let text = take_c_string(ixe_session_take_error(session, std::ptr::null_mut()))
                    .unwrap_or_else(|| format!("status {rc} with no message"));
                ixe_session_free(session);
                return (Err(text), mode);
            }
            let served = take_c_string(answer);
            if mode == IXE_SERVE_ANSWER {
                ixe_session_free(session);
                return (Ok(served.unwrap_or_default()), mode);
            }
            let mut out: *mut c_char = std::ptr::null_mut();
            let rc = ixe_flake_document(session, root, &mut out);
            if rc != IXE_OK {
                let text = take_c_string(ixe_session_take_error(session, std::ptr::null_mut()))
                    .unwrap_or_else(|| format!("status {rc} with no message"));
                ixe_session_free(session);
                return (Err(text), mode);
            }
            let fresh = take_c_string(out).unwrap_or_default();
            ixe_session_question_answer(session, IXE_OK, fresh.as_ptr(), fresh.len());
            ixe_session_free(session);
            let answer = if mode == IXE_SERVE_VERIFY {
                served.unwrap_or_default()
            } else {
                fresh
            };
            (Ok(answer), mode)
        }
    }

    fn flake_document_of(source: &str) -> serde_json::Value {
        let (document, _) = ask_flake_document(source);
        let text = match document {
            Ok(text) => text,
            Err(error) => panic!("the flake document failed: {error}"),
        };
        match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => panic!("the flake document is not JSON ({error}): {text}"),
        }
    }

    fn flake_document_error(source: &str) -> String {
        match ask_flake_document(source) {
            (Err(error), _) => error,
            (Ok(text), _) => panic!("the flake document was produced: {text}"),
        }
    }

    /// Rust validates declarations and returns normalized host operations,
    /// without invoking the outputs function.
    #[test]
    fn a_flake_document_normalizes_declarations() {
        // Sessions read the process globals (`store-dir`), so the test holds
        // the pin every other session-building test holds.
        let _pin = SettingsPin::exclusive();
        let document = flake_document_of(
            r#"{
              description = "d";
              inputs = {
                nixpkgs = { url = "github:a/b"; flake = false; inputs.x.follows = "y"; };
                local.url = ./sub;
                signed.type = "git";
                signed.publicKeys = [ { type = "ssh-ed25519"; key = "k"; } ];
              };
              outputs = { self, nixpkgs, ... }: throw "outputs is not called";
              nixConfig = {
                extra-substituters = [ "https://c" ];
                allow-import-from-derivation = true;
                max-jobs = 4;
                flake-registry = ./registry.json;
              };
            }"#,
        );
        assert_eq!(
            document,
            serde_json::json!({
                "description": "d",
                "inputs": {
                    "nixpkgs": {
                        "reference": { "kind": "url", "value": "github:a/b" },
                        "is_flake": false,
                        "follows": null,
                        "overrides": { "x": {
                            "reference": null, "is_flake": true,
                            "follows": ["y"], "overrides": {},
                        } },
                    },
                    "local": {
                        "reference": { "kind": "url", "value": "path:sub" },
                        "is_flake": true, "follows": null, "overrides": {},
                    },
                    "signed": {
                        "reference": { "kind": "attrs", "value": {
                            "type": { "kind": "string", "value": "git" },
                            "publicKeys": { "kind": "string", "value": r#"[{"key":"k","type":"ssh-ed25519"}]"# },
                        } },
                        "is_flake": true, "follows": null, "overrides": {},
                    },
                },
                "self_attrs": {},
                "config": {
                    "extra-substituters": { "kind": "strings", "value": ["https://c"] },
                    "allow-import-from-derivation": { "kind": "bool", "value": true },
                    "max-jobs": { "kind": "int", "value": 4 },
                    "flake-registry": { "kind": "path", "value": "registry.json" },
                },
            })
        );
    }

    #[test]
    fn a_document_keeps_its_mount_and_original_directory() {
        let _pin = SettingsPin::exclusive();
        let mount = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-source";
        let base = format!("{mount}/config");
        let file = format!("{mount}/flake.nix");
        let (document, _) = ask_flake_document_at(
            "{ inputs.dep.url = ./dep; outputs = args: {}; }",
            mount,
            &base,
            &file,
        );
        let document: serde_json::Value =
            serde_json::from_str(&document.expect("document")).expect("JSON");
        assert_eq!(
            document["inputs"]["dep"]["reference"]["value"],
            "path:config/dep"
        );
        let (absolute, _) = ask_flake_document_at(
            "{ inputs.dep.url = /dep; outputs = args: {}; }",
            mount,
            &base,
            &file,
        );
        assert!(
            absolute.is_err(),
            "an ambient path crossed the flake's root"
        );
        for (base, file) in [
            ("/outside", file.as_str()),
            (base.as_str(), "/outside/flake.nix"),
        ] {
            let (outside, _) = ask_flake_document_at("{}", mount, base, file);
            assert!(outside.is_err(), "a source outside the mount was accepted");
        }
    }

    #[test]
    fn a_flake_document_evaluates_computed_metadata_but_not_outputs() {
        let _pin = SettingsPin::exclusive();
        let document = flake_document_of(
            r#"
            let suffix = "b"; in {
                description = "a" + suffix;
                inputs.a.flake = !true;
                nixConfig.extra-substituters = [ ("https://" + suffix) ];
                outputs = { self }: throw "outputs called";
            }
        "#,
        );
        assert_eq!(document["description"], "ab");
        assert_eq!(document["inputs"]["a"]["is_flake"], false);
        assert_eq!(
            document["config"]["extra-substituters"]["value"],
            serde_json::json!(["https://b"])
        );
        assert!(document["inputs"].get("self").is_none());
        assert!(
            flake_document_error(r#"{ description = throw "metadata entered"; }"#)
                .contains("metadata entered")
        );
        assert!(flake_document_error("42").contains("expected a set"));
    }

    #[test]
    fn computed_input_graphs_reject_cycles_and_excessive_depth() {
        let _pin = SettingsPin::exclusive();
        let cycle = flake_document_error(
            "let x = { a.inputs = x; }; in { inputs = x; outputs = args: {}; }",
        );
        assert!(cycle.contains("cyclic flake inputs"), "{cycle}");
        let deep = flake_document_error(
            "let mk = n: if n == 0 then {} else { a.inputs = mk (n - 1); }; in { inputs = mk 140; outputs = args: {}; }",
        );
        assert!(deep.contains("nesting exceeds 128"), "{deep}");
        let shared = flake_document_of(
            "let x = {}; in { inputs = { a.inputs = x; b.inputs = x; }; outputs = args: {}; }",
        );
        assert_eq!(shared["inputs"]["a"]["overrides"], serde_json::json!({}));
        assert_eq!(shared["inputs"]["b"]["overrides"], serde_json::json!({}));
    }

    #[test]
    fn computed_document_reads_invalidate_and_reuse_previous_answers() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("computed-flake-document");
        let _cache = CacheDir::set(&dir);
        let file = scratch("computed-flake-description");
        let source = format!(
            "{{ description = builtins.readFile {}; outputs = args: throw \"outputs called\"; }}",
            serde_json_string(&file.to_string_lossy())
        );
        for (contents, expected_mode) in [
            ("before", IXE_SERVE_EVALUATE),
            ("before", IXE_SERVE_ANSWER),
            ("after", IXE_SERVE_EVALUATE),
            ("after", IXE_SERVE_ANSWER),
            ("before", IXE_SERVE_ANSWER),
        ] {
            std::fs::write(&file, contents).expect("write metadata");
            let (answer, mode) = ask_flake_document(&source);
            let document: serde_json::Value =
                serde_json::from_str(&answer.expect("document")).expect("JSON");
            assert_eq!(document["description"], contents);
            assert_eq!(mode, expected_mode, "metadata {contents}");
        }
        std::fs::remove_file(file).expect("remove metadata");
    }

    #[test]
    fn a_flake_document_rejects_invalid_declarations() {
        // Sessions read the process globals (`store-dir`), so the test holds
        // the pin every other session-building test holds.
        let _pin = SettingsPin::exclusive();
        let error = flake_document_error(r#"{ foo = 1; }"#);
        assert!(error.contains("unsupported attribute 'foo'"), "{error}");
        let error = flake_document_error(r#"{ outputs = 1; }"#);
        assert!(
            error.contains("expected a function but got an integer"),
            "{error}"
        );
        let error = flake_document_error(r#"{ inputs.a.url = 1.5; }"#);
        assert!(
            error.contains("expected a string but got a float at inputs.a.url"),
            "{error}"
        );
        let error = flake_document_error(r#"{ nixConfig.x = [ 1 ]; }"#);
        assert!(error.contains("expected a string"), "{error}");
    }

    /// The document is a memoised answer like any other: the second session
    /// is served it and reads nothing.
    #[test]
    fn a_flake_document_is_served_on_the_second_ask() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("flake-document");
        let _cache = CacheDir::set(&dir);
        let source = r#"{ description = "memoised"; outputs = { self }: self; }"#;

        let (cold, cold_mode) = ask_flake_document(source);
        let (warm, warm_mode) = ask_flake_document(source);

        assert_eq!(
            cold_mode, IXE_SERVE_EVALUATE,
            "the first session was not cold"
        );
        assert_eq!(
            warm_mode, IXE_SERVE_ANSWER,
            "the second session evaluated instead of being served"
        );
        assert_eq!(
            cold, warm,
            "the served document differs from the evaluated one"
        );
        assert!(
            cold.as_deref().is_ok_and(|text| text.contains("memoised")),
            "{cold:?}"
        );
    }

    /// An edit to a file only the *walk* reads invalidates the row.
    ///
    /// The attribute is a thunk, so the first evaluation stops at the
    /// attribute set and never reads anything; the `readFile` happens while
    /// the embedder is selecting and rendering, between the two halves of the
    /// question protocol. A read set that did not cover that window would
    /// still hit here after the edit, and serve the old contents.
    #[test]
    fn an_edit_the_walk_reads_invalidates_the_row() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("walk-reads");
        let _cache = CacheDir::set(&dir);
        let file = scratch("walk-dep");
        assert!(
            std::fs::write(&file, "before").is_ok(),
            "write the dependency"
        );
        let source = format!(
            "{{ a = builtins.readFile {}; }}",
            serde_json_string(&file.to_string_lossy())
        );

        let (cold, cold_mode) = ask(&source, "a");
        let (warm, warm_mode) = ask(&source, "a");
        assert!(
            std::fs::write(&file, "after").is_ok(),
            "edit the dependency"
        );
        let (edited, edited_mode) = ask(&source, "a");
        drop(std::fs::remove_file(&file));

        assert_eq!((cold.as_str(), cold_mode), ("before", IXE_SERVE_EVALUATE));
        assert_eq!(
            (warm.as_str(), warm_mode),
            ("before", IXE_SERVE_ANSWER),
            "the row was not there to be invalidated, so this test proves nothing \
             about invalidation"
        );
        assert_eq!(
            (edited.as_str(), edited_mode),
            ("after", IXE_SERVE_EVALUATE),
            "an edit to a file the walk read did not invalidate the row: the read set \
             does not cover the window between the two halves of the question"
        );
    }

    /// A question that fails is not filed, and fails the same way next time.
    ///
    /// Stated as a test rather than left implicit because it is a deliberate
    /// gap: a failure on this path can be raised by the C++ bridge, carrying
    /// suggestions or a refusal token, and none of that round-trips through
    /// the `(status, text)` pair a row holds. ENG-12857. The thing that must
    /// not happen is a half-recorded failure, which would serve a wrong
    /// exception class on the second run.
    #[test]
    fn a_failing_question_is_not_memoised() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("failing");
        let _cache = CacheDir::set(&dir);
        let source = r#"{ a = throw "no"; }"#;

        for round in 0..2 {
            // SAFETY: pointers to live locals; the session is freed below.
            unsafe {
                let session = session_without_embedder();
                let base = ".";
                let attr = "a";
                let mut mode = -1;
                let mut root = 0u64;
                let mut answer: *mut c_char = std::ptr::null_mut();
                let rc = ixe_session_eval_question(
                    session,
                    source.as_ptr(),
                    source.len(),
                    base.as_ptr(),
                    base.len(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    IXE_QUESTION_SELECT,
                    &one_path(attr),
                    1,
                    1,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    0,
                    IXE_RENDER_RAW,
                    &mut mode,
                    &mut root,
                    &mut answer,
                );
                assert_eq!(rc, IXE_OK, "round {round}: the root itself must evaluate");
                assert_eq!(
                    mode, IXE_SERVE_EVALUATE,
                    "round {round}: a failed question was filed and served"
                );
                let mut next = 0u64;
                assert_eq!(
                    ixe_attrs_select(session, root, attr.as_ptr(), attr.len(), &mut next),
                    IXE_OK
                );
                assert_eq!(
                    ixe_force(session, next),
                    IXE_ERR_THROWN,
                    "round {round}: the throw did not arrive as a throw"
                );
                // What the bridge does when it is about to raise: report the
                // status, file nothing.
                ixe_session_question_answer(session, IXE_ERR_THROWN, std::ptr::null(), 0);
                ixe_session_free(session);
            }
        }
    }

    /// Sets the sampling rate and puts it back however the test ends.
    struct VerifyRate(u32);

    impl VerifyRate {
        fn set(rate: u32) -> Self {
            let previous = verify_rate();
            ixe_set_cache_verify_rate(rate);
            VerifyRate(previous)
        }
    }

    impl Drop for VerifyRate {
        fn drop(&mut self) {
            ixe_set_cache_verify_rate(self.0);
        }
    }

    /// The sampled-verification branch exists on this path too, and something
    /// exercises it.
    ///
    /// It is off by default, which means the whole `IXE_SERVE_VERIFY` arm --
    /// in the C ABI, in the bridge's `askQuestion`, and in the quiet recorder
    /// -- is dead code in every ordinary run and in every gate. A branch
    /// nobody enters is a branch nobody has watched work, and the one thing
    /// it is for is catching a cache that lies.
    #[test]
    fn a_sampled_hit_is_checked_rather_than_trusted() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("verify");
        let _cache = CacheDir::set(&dir);
        let _rate = VerifyRate::set(1);
        let source = r#"{ a = "checked"; }"#;

        let (cold, cold_mode) = ask(source, "a");
        let (warm, warm_mode) = ask(source, "a");

        assert_eq!((cold.as_str(), cold_mode), ("checked", IXE_SERVE_EVALUATE));
        assert_eq!(
            (warm.as_str(), warm_mode),
            ("checked", IXE_SERVE_VERIFY),
            "with the rate at 1 every hit must be checked; anything else means the \
             sampler never fires on this path and the verify arm is dead code"
        );
    }

    /// A row that does not say what evaluating says is complained about, at
    /// error priority, on this path.
    ///
    /// The complaint is the whole product of the verifier. A check that
    /// noticed and said nothing would be indistinguishable from a check that
    /// did not run, which is the shape this repo has been caught by twice.
    #[test]
    fn a_poisoned_row_is_shouted_about_on_the_handle_path() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("poisoned");
        let _cache = CacheDir::set(&dir);
        let _rate = VerifyRate::set(1);
        let source = r#"{ a = "honest"; }"#;

        // Poison first, because the memo table is `Keyed`: a second record
        // under a key that already has a row reuses the row rather than
        // replacing it, which is correct and means an honest run first would
        // leave nothing to catch. Filed through the public protocol rather
        // than written into the store by hand, so the key the poison lands on
        // is by construction the one the honest walk computes -- reaching in
        // and deriving the key a second way here would be testing the test.
        // `{ a = "honest"; }` asks the host nothing, so both runs record an
        // empty read set and address the same row.
        poison(source, "a", "a lie");

        // SAFETY: pointers to live locals; the session is freed below.
        let complaints = unsafe {
            let session = session_without_embedder();
            let base = ".";
            let attr = "a";
            let mut mode = -1;
            let mut root = 0u64;
            let mut answer: *mut c_char = std::ptr::null_mut();
            assert_eq!(
                ixe_session_eval_question(
                    session,
                    source.as_ptr(),
                    source.len(),
                    base.as_ptr(),
                    base.len(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    IXE_QUESTION_SELECT,
                    &one_path(attr),
                    1,
                    1,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    0,
                    IXE_RENDER_RAW,
                    &mut mode,
                    &mut root,
                    &mut answer,
                ),
                IXE_OK
            );
            assert_eq!(mode, IXE_SERVE_VERIFY, "the poison was not served");
            let served = CStr::from_ptr(answer).to_string_lossy().into_owned();
            ixe_string_free(answer);
            assert_eq!(served, "a lie", "the poison did not take");
            let mut next = 0u64;
            assert_eq!(
                ixe_attrs_select(session, root, attr.as_ptr(), attr.len(), &mut next),
                IXE_OK
            );
            assert_eq!(ixe_force(session, next), IXE_OK);
            let mut out: *mut c_char = std::ptr::null_mut();
            assert_eq!(ixe_render(session, next, IXE_RENDER_RAW, &mut out), IXE_OK);
            let fresh = CStr::from_ptr(out).to_string_lossy().into_owned();
            ixe_string_free(out);
            assert_eq!(fresh, "honest");
            ixe_session_question_answer(session, IXE_OK, fresh.as_ptr(), fresh.len());
            let mut said = Vec::new();
            loop {
                let warning = ixe_session_take_warning(session);
                if warning.is_null() {
                    break;
                }
                said.push(CStr::from_ptr(warning).to_string_lossy().into_owned());
                ixe_string_free(warning);
            }
            ixe_session_free(session);
            said
        };

        let shouted: Vec<&String> = complaints
            .iter()
            .filter(|c| c.starts_with("error: "))
            .collect();
        assert_eq!(
            shouted.len(),
            1,
            "expected one error-priority complaint about the poisoned row, got {complaints:?}"
        );
        let Some(message) = shouted.first() else {
            unreachable!("checked non-empty above");
        };
        // Both answers, so a reader can tell which side is wrong.
        assert!(message.contains("a lie"), "{message}");
        assert!(message.contains("honest"), "{message}");
    }

    /// File `answer` under the row `attr_path` of `source` addresses.
    ///
    /// Runs the real protocol and lies at the last step, which is the only
    /// way to be sure the poison lands on the key the honest path computes.
    fn poison(source: &str, attr_path: &str, answer: &str) {
        // SAFETY: pointers to live locals; the session is freed below.
        unsafe {
            let session = session_without_embedder();
            let base = ".";
            let mut mode = -1;
            let mut root = 0u64;
            let mut served: *mut c_char = std::ptr::null_mut();
            assert_eq!(
                ixe_session_eval_question(
                    session,
                    source.as_ptr(),
                    source.len(),
                    base.as_ptr(),
                    base.len(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    IXE_QUESTION_SELECT,
                    &one_path(attr_path),
                    1,
                    1,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    0,
                    IXE_RENDER_RAW,
                    &mut mode,
                    &mut root,
                    &mut served,
                ),
                IXE_OK
            );
            if !served.is_null() {
                ixe_string_free(served);
            }
            let mut current = root;
            for component in attr_path.split('.').filter(|c| !c.is_empty()) {
                let mut next = 0u64;
                assert_eq!(
                    ixe_attrs_select(
                        session,
                        current,
                        component.as_ptr(),
                        component.len(),
                        &mut next
                    ),
                    IXE_OK
                );
                current = next;
            }
            assert_eq!(ixe_force(session, current), IXE_OK);
            ixe_session_question_answer(session, IXE_OK, answer.as_ptr(), answer.len());
            ixe_session_free(session);
        }
    }

    /// A Nix string literal for a path, so a scratch directory with a
    /// character the language would read as syntax cannot make a test about
    /// caching fail as a parse error.
    fn serde_json_string(text: &str) -> String {
        let mut out = String::with_capacity(text.len() + 2);
        out.push('"');
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '$' => out.push_str("\\$"),
                other => out.push(other),
            }
        }
        out.push('"');
        out
    }

    /// The shape a flake evaluand has: one source applied to three values the
    /// embedder built, with the answer depending on all of them.
    ///
    /// Not `call-flake.nix` itself, deliberately. What is under test is the
    /// memo key, and the property is that two evaluands differing only in
    /// their arguments are two rows -- which is true of `call-flake.nix` for
    /// the same reason it is true of this, and provable here in milliseconds
    /// without a store, a lock file or a fetcher. The `nix eval <flake>#attr`
    /// end of it is `maintainers/ix/drv-parity.sh`'s "two flakes, one cache"
    /// arm.
    const FLAKE_SHAPED: &str = "lockFile: overrides: fetchFinalTree: \
         { packages.default.drvPath = \
             \"/nix/store/\" + (builtins.fromJSON lockFile).hash + \"-\" + overrides.name; }";

    const DRV_PATH: &str = "packages.default.drvPath";

    /// The three arguments `rustEvaluandOf` builds, with the two documents
    /// spelled the way the bridge spells them: the lock file is a JSON
    /// *string* (cppnix hands over `lockFile.to_string()`, not its parse),
    /// the overrides are a JSON object.
    fn flake_arguments<'a>(lock_hash: &'a str, name: &'a str) -> [(i32, String); 3] {
        [
            (
                IXE_ARG_JSON,
                format!("\"{{\\\"hash\\\":\\\"{lock_hash}\\\"}}\""),
            ),
            (IXE_ARG_JSON, format!("{{\"name\":\"{name}\"}}")),
            (IXE_ARG_INTERNAL_PRIMOP, "fetchFinalTree".to_owned()),
        ]
    }

    fn ask_flake(lock_hash: &str, name: &str) -> (String, i32) {
        let owned = flake_arguments(lock_hash, name);
        let borrowed: Vec<(i32, &str)> = owned
            .iter()
            .map(|(kind, text)| (*kind, text.as_str()))
            .collect();
        ask_applied(FLAKE_SHAPED, DRV_PATH, &borrowed)
    }

    /// ENG-12915, the warm half: a second run of the same flake is served.
    ///
    /// This is the saving the whole ticket is about. Before it, `mayBeMemoised`
    /// refused to ask the question at all when the evaluand had arguments, so
    /// every `nix eval <flake>#attr` and every `nix build <flake>#attr`
    /// evaluated cold however warm `eval-cache-dir` was.
    #[test]
    fn a_second_run_of_one_flake_is_served() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("flake-warm");
        let _cache = CacheDir::set(&dir);

        let (cold, cold_mode) = ask_flake("aaaa", "one");
        assert_eq!(cold, "/nix/store/aaaa-one");
        assert_eq!(
            cold_mode, IXE_SERVE_EVALUATE,
            "the first run should be cold"
        );

        let (warm, warm_mode) = ask_flake("aaaa", "one");
        assert_eq!(
            warm_mode, IXE_SERVE_ANSWER,
            "the second run of one flake was not served: the argument axis \
             made every flake evaluand unaddressable rather than making each \
             one its own row"
        );
        assert_eq!(
            warm, cold,
            "the served answer is not the bytes the cold run produced"
        );
    }

    /// ENG-12915, the sound half: two flakes over one cache are two rows.
    ///
    /// **This is the assertion the ticket was opened for, and the reason the
    /// conservative rule was kept.** The two evaluands here share a module
    /// digest (one `call-flake.nix`), a base directory, a settings
    /// fingerprint and a question. Everything that distinguishes them is in
    /// the arguments.
    ///
    /// The read-set replay is not a second line of defence against this, and
    /// that was the open question. A witness is filed under the identity
    /// alone (`DirWitness::path`), so two evaluands with one identity have one
    /// witness: the second flake replays the first flake's questions, those
    /// questions still give the answers they gave, and the composite key
    /// matches the first flake's row. Nothing in the replay knows a different
    /// value was applied. What separated the two fixtures in the measurement
    /// on the ticket was that they read different store paths and so recorded
    /// different questions -- true of that pair, and not a property of the
    /// mechanism. Here neither evaluand reads anything at all, both read sets
    /// are empty, and the replay separates nothing.
    #[test]
    fn two_flakes_over_one_cache_are_two_rows() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("flake-two");
        let _cache = CacheDir::set(&dir);

        let (first, _) = ask_flake("aaaa", "one");
        assert_eq!(first, "/nix/store/aaaa-one");

        // A different overrides document: the same lock, a different source
        // for the flake being called.
        let (second, second_mode) = ask_flake("aaaa", "two");
        assert_eq!(
            second_mode, IXE_SERVE_EVALUATE,
            "the second flake was served from the first flake's row"
        );
        assert_eq!(
            second, "/nix/store/aaaa-two",
            "one flake was served another's answer -- a wrong store path out \
             of a cache, which is the failure this key exists to prevent"
        );

        // A different lock file: the same overrides, a different closure of
        // inputs.
        let (third, third_mode) = ask_flake("bbbb", "one");
        assert_eq!(third_mode, IXE_SERVE_EVALUATE);
        assert_eq!(third, "/nix/store/bbbb-one");

        // And the first one is still there and still itself, so the misses
        // above are the key discriminating rather than the cache being inert.
        let (again, again_mode) = ask_flake("aaaa", "one");
        assert_eq!(
            again_mode, IXE_SERVE_ANSWER,
            "the first flake stopped being served once two others were \
             recorded, so this test proves nothing about the key"
        );
        assert_eq!(again, first);
    }

    /// An evaluand with arguments and one without are not one row.
    ///
    /// The empty list has to be a distinguishable value rather than an absent
    /// field, or a source that is a function would address the same row
    /// whether or not anything was applied to it.
    #[test]
    fn applying_nothing_is_not_applying_something() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("flake-none");
        let _cache = CacheDir::set(&dir);

        // A source that answers with or without an argument, so the two runs
        // differ in the argument list and in nothing else.
        let source = "{ a = { b = \"unapplied\"; }; }";
        let (bare, _) = ask(source, "a.b");
        assert_eq!(bare, "unapplied");

        let identity_bare = crate::readset::EvalId::of(
            &ix_kernel::hash::tagged("m", &[source.as_bytes()]),
            &crate::eval::Settings::current(),
            &crate::session::Arguments::none(),
            &crate::session::Question::Select {
                selection: crate::session::Selection::one("a.b"),
                render: RenderMode::Raw,
            },
        );
        let identity_applied = crate::readset::EvalId::of(
            &ix_kernel::hash::tagged("m", &[source.as_bytes()]),
            &crate::eval::Settings::current(),
            &crate::session::Arguments::new(vec![crate::session::Argument::Json("1".to_owned())]),
            &crate::session::Question::Select {
                selection: crate::session::Selection::one("a.b"),
                render: RenderMode::Raw,
            },
        );
        assert_ne!(identity_bare, identity_applied);
    }

    /// The embedder cannot inject a value into a question in flight.
    ///
    /// The three value-building calls refuse while `session.memo` is set,
    /// which is what makes "everything applied is in the key" an enforced
    /// property rather than a convention living in the bridge. Break this and
    /// the flake path is unsound again in exactly the way it was: a value
    /// nothing keys on, forced through a recorder that files its reads under
    /// a row that does not name it.
    #[test]
    fn a_value_cannot_be_injected_while_a_question_is_in_flight() {
        let _pin = SettingsPin::exclusive();
        let dir = scratch("flake-inject");
        let _cache = CacheDir::set(&dir);

        let source = "{ a = 1; }";
        // SAFETY: pointers to live locals; the session is freed below.
        unsafe {
            let session = session_without_embedder();
            let base = ".";
            let attr = "a";
            let mut mode = -1;
            let mut root = 0u64;
            let mut answer: *mut c_char = std::ptr::null_mut();
            assert_eq!(
                ixe_session_eval_question(
                    session,
                    source.as_ptr(),
                    source.len(),
                    base.as_ptr(),
                    base.len(),
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    IXE_QUESTION_SELECT,
                    &one_path(attr),
                    1,
                    1,
                    std::ptr::null(),
                    0,
                    std::ptr::null(),
                    0,
                    0,
                    IXE_RENDER_RAW,
                    &mut mode,
                    &mut root,
                    &mut answer,
                ),
                IXE_OK
            );
            assert_eq!(mode, IXE_SERVE_EVALUATE, "nothing should be cached yet");

            let json = "1";
            let mut out = 0u64;
            assert_eq!(
                ixe_alloc_json(session, json.as_ptr(), json.len(), &mut out),
                IXE_ERR_BADCALL,
                "a JSON value was built into a question in flight"
            );
            let name = "fetchFinalTree";
            assert_eq!(
                ixe_internal_primop(session, name.as_ptr(), name.len(), &mut out),
                IXE_ERR_BADCALL,
                "an internal primop was built into a question in flight"
            );
            assert_eq!(
                ixe_apply(session, root, root, &mut out),
                IXE_ERR_BADCALL,
                "an application was performed inside a question in flight"
            );
            let message = ixe_session_take_error(session, std::ptr::null_mut());
            assert!(!message.is_null());
            let text = CStr::from_ptr(message).to_string_lossy().into_owned();
            ixe_string_free(message);
            assert!(
                text.contains("ENG-12915"),
                "the refusal does not say why, so the next embedder will \
                 route around it: {text}"
            );

            // Abandon the question rather than filing it, and free.
            ixe_session_question_answer(session, IXE_ERR_EVAL, std::ptr::null(), 0);
            ixe_session_free(session);
        }
        // The same three calls work with no question in flight, so the
        // refusal above is the scope and not the calls being broken.
        // SAFETY: pointers to live locals; the session is freed below.
        unsafe {
            let session = session_without_embedder();
            let json = "1";
            let mut out = 0u64;
            assert_eq!(
                ixe_alloc_json(session, json.as_ptr(), json.len(), &mut out),
                IXE_OK
            );
            ixe_session_free(session);
        }
    }
}

#[cfg(test)]
mod memo_reach_tests {
    /// Every entry point that reaches the memo table takes a *source*, and
    /// none of them takes a value the embedder built.
    ///
    /// This is the structural half of the three `CannotChangeAnAnswer` rows
    /// `ixe_alloc_json`, `ixe_internal_primop` and `ixe_apply` carry. Those
    /// rows claim an injected value cannot be keyed on, and the reason is
    /// that no call which consults the cache accepts one: the three below key
    /// on the text they are handed, and a handle reaches the VM only through
    /// `force_handle`, which drives against `RealFs` with nothing recording.
    ///
    /// The list grew from two to three when the question protocol landed
    /// (ENG-12830), and this test is how that was noticed rather than
    /// assumed. A prose claim would have gone stale silently. It shrank again
    /// when the compile cache moved onto the machine: `ixe_session_eval` had
    /// only ever reached that cache, which is keyed on the source text by
    /// construction and is not a memo table, and the scan had been counting
    /// the directory read rather than the consult.
    ///
    /// **There is a second half this cannot see.** `ixe_session_eval_question`
    /// keys on the source, the base directory, the origin, the attribute
    /// path, the question kind and the render mode -- not on arguments
    /// applied afterwards. So the *bridge* must not ask it about an evaluand
    /// that has arguments, and `mayBeMemoised` in `rust-eval-session.cc` is
    /// that rule. Nothing here can check C++; `drv-parity.sh`'s
    /// two-flakes-one-cache arm is what does.
    ///
    /// Read out of the source because there is nothing to ask at run time: a
    /// function that does not consult the cache leaves no trace of not
    /// having done so.
    #[test]
    fn only_source_keyed_entry_points_reach_the_memo_table() {
        const WHOLE: &str = include_str!("capi.rs");
        // What consulting the memo table looks like in source. The two calls
        // are the only readers of it; the handle is how a new reader would
        // get at the store without either.
        const MEMO_REACHES: &[&str] = &[
            "crate::session::evaluate_once(",
            "crate::session::QuestionCache::open(",
            "modules().store()",
        ];
        // Everything before the first test module. Three reasons, each one
        // arriving after the cut before it had already gone wrong. Without a
        // cut the scanner finds its own matcher line and reports this test as
        // an entry point that consults the cache, which it did on the first
        // run. A cut keyed on *this* module's name assumed it was the last
        // thing in the file; `warm_starts` landed after it, and everything in
        // that module would have been scanned. And a cut keyed on the first
        // bare `#[cfg(test)]` assumed no test-only item sat among the ABI;
        // `session_without_embedder` does, so the cut moved 1700 lines up the
        // file and hid two of the three entry points. The boundary is the
        // first test *module*, and the two guards below are what turn a cut
        // that swallows the file into a failure rather than an empty list.
        let src = WHOLE
            .split_once("\n#[cfg(test)]\nmod ")
            .map_or(WHOLE, |(before, _)| before);
        // The invariant the cut rests on, stated rather than assumed: every
        // exported entry point is above it. A prefix missing one is a cut in
        // the wrong place, and the failure it causes -- a short `reaching`
        // list -- reads exactly like an entry point that stopped using the
        // cache.
        for spelling in ["pub extern \"C\" fn ", "pub unsafe extern \"C\" fn "] {
            assert_eq!(
                src.matches(spelling).count(),
                WHOLE.matches(spelling).count(),
                "the cut dropped a `{spelling}` declaration, so the scan below \
                 covers only part of the ABI"
            );
        }
        let mut current: Option<&str> = None;
        let mut reaching: Vec<&str> = Vec::new();
        let mut seen_declarations = 0usize;
        for line in src.lines() {
            let trimmed = line.trim_start();
            if let Some(rest) = trimmed
                .strip_prefix("pub extern \"C\" fn ")
                .or_else(|| trimmed.strip_prefix("pub unsafe extern \"C\" fn "))
                .or_else(|| trimmed.strip_prefix("fn "))
                && let Some(name) = rest.split('(').next()
            {
                current = Some(name);
                seen_declarations += 1;
            }
            // The literal consult, not a mention: the doc comments above talk
            // about the cache and must not count as reaching it. The store
            // handle lives on the machine (`Vm::modules().store()`), so the
            // marker is a call that reads the memo through it, or the handle
            // itself where a caller takes it directly.
            if MEMO_REACHES.iter().any(|reach| trimmed.contains(reach))
                && !trimmed.starts_with("//")
                && let Some(name) = current
                && !reaching.contains(&name)
            {
                reaching.push(name);
            }
        }
        assert!(
            seen_declarations > 40,
            "the scanner found only {seen_declarations} function declarations, \
             so it has stopped matching this file and would report an empty \
             list however many entry points reached the cache"
        );
        reaching.sort_unstable();
        assert_eq!(
            reaching,
            vec![
                "ixe_eval_expr",
                "ixe_session_eval_question",
                // Takes no value and no source: it forgets the row the last
                // question served, keyed on what that question computed, so
                // nothing an embedder injects can reach a key through it.
                "ixe_session_question_reject",
                // Reads only whether persistence exists to enforce the external
                // host identity precondition; never reads or writes memo rows.
                "require_session_cache_identity"
            ],
            "the set of entry points that consult the eval cache has changed. \
             Each of these must take a source and key on it. If one that takes \
             a value handle from the embedder is now among them, the \
             CannotChangeAnAnswer rows for ixe_alloc_json, ixe_apply and \
             ixe_internal_primop are no longer true and an injected value can \
             be served from a key that does not carry it. If a new source-keyed \
             one arrived, check what its key covers and whether \
             `mayBeMemoised` in the bridge needs to know about it, then add it \
             here."
        );
    }
}

#[cfg(test)]
mod export_tests {
    /// With the counters compiled out, the snapshot is null rather than a
    /// line of zeros, so the stats block is absent rather than claiming the
    /// evaluator did nothing.
    ///
    /// Both directions, because a snapshot that always returned null would
    /// satisfy half of this and silently remove the feature.
    #[test]
    fn a_countless_build_reports_nothing_rather_than_zeros() {
        let p = super::ixe_perf_snapshot();
        if cfg!(feature = "perf") {
            assert!(
                !p.is_null(),
                "with counters compiled in there must be a line"
            );
            // SAFETY: non-null, allocated by this crate just above.
            unsafe { super::ixe_string_free(p) };
        } else {
            assert!(
                p.is_null(),
                "with counters compiled out the snapshot must be null, or the \
                 stats block becomes a page of zeros that reads as real"
            );
        }
    }

    /// Every `extern "C" fn ixe_*` carries `#[unsafe(no_mangle)]`.
    ///
    /// Without the attribute the function still compiles, still passes every
    /// test in this crate, and simply is not in the static library under the
    /// name C expects. Nothing in Rust depends on the symbol name, so the
    /// suite stays green and the failure surfaces as an undefined symbol at
    /// the C++ link, a six-minute round trip from the edit that caused it.
    ///
    /// Not hypothetical. Adding `ixe_perf_snapshot` immediately above
    /// `ixe_string_free` put the new item *between* `ixe_string_free`'s doc
    /// comment and its attribute, so the attribute landed on the new function
    /// and `ixe_string_free` quietly stopped being exported. 402 tests passed;
    /// the linker did not.
    ///
    /// Read out of the source text, the way `table_entry_modules` in
    /// `builtins.rs` is, because an attribute leaves no runtime trace to ask
    /// about.
    #[test]
    fn every_c_entry_point_is_exported() {
        const SRC: &str = include_str!("capi.rs");
        let lines: Vec<&str> = SRC.lines().collect();
        let mut missing: Vec<&str> = Vec::new();
        let mut found = 0usize;
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim_start();
            if !trimmed.starts_with("pub extern \"C\" fn ixe_")
                && !trimmed.starts_with("pub unsafe extern \"C\" fn ixe_")
            {
                continue;
            }
            found += 1;
            // Walk the contiguous run of attributes above the item, not just
            // the line immediately above: `ixe_session_eval_question` carries
            // `#[allow(clippy::too_many_arguments)]` between its
            // `#[unsafe(no_mangle)]` and its signature, and the first version
            // of this test reported it as unexported. It is exported, and the
            // C++ has linked against it for weeks -- the parser was wrong, not
            // the code. A run stops at the first line that is not an
            // attribute, which is what makes an item *inserted* into the run
            // still fail.
            let mut exported = false;
            let mut j = i;
            while let Some(prev) = j.checked_sub(1).and_then(|k| lines.get(k)) {
                let prev = prev.trim();
                if prev == "#[unsafe(no_mangle)]" {
                    exported = true;
                    break;
                }
                if !prev.starts_with("#[") {
                    break;
                }
                j -= 1;
            }
            if !exported {
                missing.push(trimmed);
            }
        }
        // A floor like `found > 20` is too weak: this loop has two match arms
        // and breaking one of them halves the coverage while a floor of 20
        // still passes. Checked against an independent count of the attribute
        // itself instead, so the two numbers can only agree when the parser is
        // seeing every entry point.
        let attributes = lines
            .iter()
            .filter(|l| l.trim() == "#[unsafe(no_mangle)]")
            .count();
        assert_eq!(
            found, attributes,
            "{found} C entry points matched but the file carries {attributes} \
             #[unsafe(no_mangle)] attributes. Either the parser stopped seeing \
             some entry points, in which case this test is asserting less than \
             it looks like, or something other than an ixe_ entry point is \
             wearing the attribute."
        );
        assert!(
            missing.is_empty(),
            "these C entry points have no #[unsafe(no_mangle)] on the line \
             above, so they are absent from the static library and the C++ \
             link fails with an undefined symbol: {missing:#?}"
        );
    }
}

#[cfg(test)]
mod realise_wire_tests {
    use crate::value2::ContextElem;

    /// The bytes on the wire, pinned. This is the encoding
    /// `NixStringContextElem::parse` accepts, and the failure mode of getting
    /// it wrong is a `BadStorePath` thrown inside the embedder for every IFD
    /// -- loud, but only reachable through a full cpp build, which is a much
    /// slower place to learn it than here.
    ///
    /// Measured against nix 2.34.7's `NixStringContextElem::to_string`
    /// (`value/context.cc:64`): a bare `<hash>-<name>` for an opaque path,
    /// `=` before one for a deep dependency, `!<output>!` before one for a
    /// single output. No store directory in any of the three.
    #[test]
    fn a_context_element_goes_over_the_wire_as_cppnix_writes_it() {
        let hash = "00000000000000000000000000000000";
        assert_eq!(
            ContextElem::Opaque(format!("/nix/store/{hash}-a").into()).display_base_name(),
            format!("{hash}-a")
        );
        assert_eq!(
            ContextElem::DrvDeep(format!("/nix/store/{hash}-a.drv").into()).display_base_name(),
            format!("={hash}-a.drv")
        );
        assert_eq!(
            ContextElem::Built {
                drv: format!("/nix/store/{hash}-a.drv").into(),
                output: "dev".into(),
            }
            .display_base_name(),
            format!("!dev!{hash}-a.drv")
        );
    }
}

#[cfg(test)]
mod derivation_batch_tests {
    use super::{EmbedderHost, IxeHostVtable, PendingDrv};
    use std::sync::atomic::{AtomicUsize, Ordering};

    static WRITES: AtomicUsize = AtomicUsize::new(0);

    unsafe extern "C" fn refuse_batch(
        _ctx: *mut core::ffi::c_void,
        _batch: *const u8,
        _batch_len: usize,
        _out: *mut *const u8,
        _out_len: *mut usize,
    ) -> i32 {
        // The caller initializes an empty answer; no pointer access needed.
        WRITES.fetch_add(1, Ordering::SeqCst);
        1
    }

    #[test]
    fn a_failed_import_poison_is_shared_and_never_retried() -> Result<(), String> {
        let host = EmbedderHost::new(IxeHostVtable {
            write_derivations: Some(refuse_batch),
            ..IxeHostVtable::empty()
        })?;
        let drv = PendingDrv::new(
            "/nix/store",
            "batch-failure",
            r#"Derive([],[],[],"x86_64-linux","/bin/sh",[],[])"#,
        )?;
        {
            let mut writes = host
                .drv_writes
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            writes.paths.insert(drv.expected.clone());
            writes.pending.push(drv);
        }
        let before = WRITES.load(Ordering::SeqCst);
        let failed = host.flush_derivations();
        assert!(failed.is_err());
        assert_eq!(WRITES.load(Ordering::SeqCst), before + 1);
        let clone = host.clone();
        assert_eq!(clone.flush_derivations(), failed);
        assert_eq!(clone.settle_writes_if(|_| false), failed);
        assert_eq!(WRITES.load(Ordering::SeqCst), before + 1);
        let writes = host
            .drv_writes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(writes.pending.is_empty());
        assert!(writes.paths.is_empty());
        assert!(writes.failed.is_some());
        Ok(())
    }
}

#[cfg(test)]
mod eval_cache_tests;
