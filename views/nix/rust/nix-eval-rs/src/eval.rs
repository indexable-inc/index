//! Top-level entry: source text -> compiled module -> VM -> printed value.
//! The M1 CST-walking evaluator is gone; the compiler + VM are the one
//! implementation (one concept, one implementation).

use crate::compile::{self, CompileError};
use crate::host::{Host, LookupError, RealFs, StoreError};
use crate::refusal::{Refusal, RefusalToken};
use crate::task::NeedPath;
use crate::value2::{Attrs, Slot, Value};
use crate::vm::{ErrKind, Step, Vm, VmError};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::atomic::{AtomicU32, Ordering};

/// The call-depth ceiling the next evaluation runs under, mirroring cppnix's
/// `max-call-depth`. Process-global because the nix setting it shadows is,
/// and because the C ABI evaluates one expression per call with no handle to
/// hang configuration off; the bridge sets it before evaluating.
static MAX_CALL_DEPTH: AtomicU32 = AtomicU32::new(crate::vm::DEFAULT_MAX_CALL_DEPTH);

/// The `nix` version string `builtins.nixVersion` reports. Supplied by the
/// bridge from cppnix's `nixVersion` global rather than written down here,
/// because a second copy of a version number is a second thing to forget to
/// bump, and the value would then differ between the two arms of a
/// comparison whose whole point is that they agree. Unset means the caller
/// never told us, and `builtins.nixVersion` stays unimplemented rather than
/// inventing an answer.
static NIX_VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Set a once-only evaluator setting, refusing a *conflicting* second call.
///
/// `OnceLock::set` returns the value back when the slot is taken, and every
/// one of these setters used to drop it on the floor with `let _ =`. That is
/// right for the expected case -- the bridge sets the same value once per
/// evaluation -- and silently wrong for the other one: a persistent evaluator
/// asked to serve a second store kept the first store's directory and
/// computed every path under it, with no error anywhere and an `outPath` that
/// looks exactly like a right one (ENG-12541).
///
/// Repeating the same value stays silent, because that is the case that
/// happens constantly and means nothing. Only a change is refused, and it is
/// refused rather than applied because the alternative is worse: honouring it
/// would leave results already memoised under the old value addressable under
/// the new one.
fn set_once(
    slot: &'static std::sync::OnceLock<String>,
    what: &'static str,
    value: &str,
) -> Result<(), SettingConflict> {
    match slot.set(value.to_owned()) {
        Ok(()) => Ok(()),
        // Taken already. Same value is the expected case and not a conflict.
        Err(rejected) => match slot.get() {
            Some(existing) if *existing == rejected => Ok(()),
            Some(existing) => Err(SettingConflict {
                setting: what,
                existing: existing.clone(),
                attempted: rejected,
            }),
            // Unreachable: `set` only fails when the slot is populated.
            None => Ok(()),
        },
    }
}

/// A once-only setting given two different values in one process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SettingConflict {
    pub setting: &'static str,
    pub existing: String,
    pub attempted: String,
}

impl std::fmt::Display for SettingConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} was already set to '{}' and cannot be changed to '{}': it is \
             fixed for the lifetime of the process, and results already \
             memoised under the first value would be served under the second",
            self.setting, self.existing, self.attempted
        )
    }
}

impl core::error::Error for SettingConflict {}

/// Tell the evaluator what version to report. Repeat calls with the same
/// value are the expected case (one per evaluation) and are ignored.
///
/// # Errors
/// When the process has already been told a *different* version. Repeating
/// the same one is the expected case and is not an error.
pub fn set_nix_version(v: &str) -> Result<(), SettingConflict> {
    #[cfg(test)]
    assert_globals_exclusive("nix-version");
    set_once(&NIX_VERSION, "builtins.nixVersion", v)
}

/// The version to report, if the embedder supplied one.
pub fn nix_version() -> Option<&'static str> {
    #[cfg(test)]
    assert_globals_guarded("nix-version");
    NIX_VERSION.get().map(String::as_str)
}

/// The immutable artifact implementing callbacks and command-side answer policy.
/// Kept separate from the user-visible language version: two host builds can
/// expose the same version while computing different answers for the same question.
static HOST_BUILD_IDENTITY: std::sync::OnceLock<String> = std::sync::OnceLock::new();

pub fn set_host_build_identity(identity: &str) -> Result<(), SettingConflict> {
    #[cfg(test)]
    assert_globals_exclusive("host-build-identity");
    set_once(&HOST_BUILD_IDENTITY, "host build identity", identity)
}

pub fn host_build_identity() -> Option<&'static str> {
    #[cfg(test)]
    assert_globals_guarded("host-build-identity");
    HOST_BUILD_IDENTITY.get().map(String::as_str)
}

/// The platform string `builtins.currentSystem` reports.
///
/// Handed over rather than detected, for the same reason the version string
/// is: cppnix takes it from `settings.thisSystem`, which `--system` and
/// `nix.conf` can both move, so a value this crate worked out from its own
/// build target would disagree with the arm it is being compared against.
///
/// **It is configuration that changes what an expression means**, unlike the
/// store directory, and it does not pass through `Host`, so a read set cannot
/// see it and a memoised result does not miss when `--system` changes. That
/// is the same hole `nixVersion` and `storeDir` have and it is ENG-12541's,
/// not a new one; it is written here because this is the first of the three
/// that a real package set reads on its first line.
static CURRENT_SYSTEM: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Tell the evaluator which platform to report.
///
/// # Errors
/// When the process has already been told a *different* platform.
pub fn set_current_system(system: &str) -> Result<(), SettingConflict> {
    #[cfg(test)]
    assert_globals_exclusive("current-system");
    set_once(&CURRENT_SYSTEM, "builtins.currentSystem", system)
}

/// The platform, if the embedder supplied one. `None` keeps
/// `builtins.currentSystem` unimplemented rather than guessing.
pub fn current_system() -> Option<&'static str> {
    #[cfg(test)]
    assert_globals_guarded("current-system");
    CURRENT_SYSTEM.get().map(String::as_str)
}

/// What `~/...` expands to.
///
/// Handed over rather than read out of the environment, for the same reason
/// the platform string is. cppnix's `getHome()`
/// (`src/libutil/unix/users.cc:31`) is not `getenv("HOME")`: it `stat`s the
/// directory, falls back to the `passwd` entry when `$HOME` is unset or
/// names a directory this euid does not own, and warns when it does. A
/// second implementation of that rule would be a second answer to one
/// question, and on the day the two disagreed the difference would be a
/// path -- silently the wrong file, not a missing one.
static HOME_DIR: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Tell the evaluator what `~/` means, from the embedder's `getHome()`.
///
/// # Errors
/// When the process has already been told a *different* home directory.
pub fn set_home_dir(dir: &str) -> Result<(), SettingConflict> {
    #[cfg(test)]
    assert_globals_exclusive("home-dir");
    set_once(&HOME_DIR, "the home directory", dir)
}

/// The home directory the embedder supplied, if it supplied one.
#[must_use]
pub fn home_dir() -> Option<&'static str> {
    #[cfg(test)]
    assert_globals_guarded("home-dir");
    HOME_DIR.get().map(String::as_str)
}

/// The store directory every path `builtins.derivationStrict` computes is
/// under.
///
/// It is an input to the hash and not only a prefix -- `makeStorePath` puts it
/// inside the fingerprint (`drvpath::make_store_path`) -- so a guessed
/// `/nix/store` against a store rooted elsewhere produces a path that is
/// wrong in all 32 characters and looks exactly like a right one. Supplied by
/// the embedder from `state.store->storeDir`, for the same reason the version
/// string is: this crate has no store and inventing one of the two things that
/// decide a store path is not a default it can pick.
static STORE_DIR: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// Tell the evaluator which store its derivations land in. Repeat calls with
/// the same value are the expected case (one per evaluation) and are ignored.
///
/// # Errors
/// When the process has already been told a *different* store directory. That
/// is the case worth failing on: the directory is hashed into every path
/// `builtins.derivationStrict` computes, so carrying on with the first one
/// produces paths for the wrong store that nothing downstream can tell from
/// right ones.
pub fn set_store_dir(dir: &str) -> Result<(), SettingConflict> {
    #[cfg(test)]
    assert_globals_exclusive("store-dir");
    set_once(&STORE_DIR, "the store directory", dir)
}

/// The store directory, if the embedder supplied one. `None` makes
/// `builtins.derivationStrict` refuse by name rather than guess.
pub fn store_dir() -> Option<&'static str> {
    #[cfg(test)]
    assert_globals_guarded("store-dir");
    STORE_DIR.get().map(String::as_str)
}

/// Set the ceiling for subsequent evaluations. Called by the bridge with
/// `EvalState::settings.maxCallDepth` so a corpus case passing
/// `--max-call-depth` exercises the same limit on both arms.
pub fn set_max_call_depth(depth: u32) {
    #[cfg(test)]
    assert_globals_exclusive("max-call-depth");
    MAX_CALL_DEPTH.store(depth, Ordering::Relaxed);
}

/// The ceiling the next evaluation runs under.
///
/// Every VM a user's expression runs on has to be given this, and the one
/// that was not is why [`Settings`] exists: `session::evaluate_once` built its
/// VM with `Vm::with_settings(crate::eval::Settings::default())`, so `--max-call-depth 50` bounded the run without an
/// `eval-cache-dir` and did not bound it with one (ENG-12540).
#[must_use]
pub fn max_call_depth() -> u32 {
    #[cfg(test)]
    assert_globals_guarded("max-call-depth");
    MAX_CALL_DEPTH.load(Ordering::Relaxed)
}

#[derive(Debug)]
pub enum EvalError {
    /// A construct the rust evaluator does not implement yet; the payload
    /// names it. Counted as `unimplemented` by the harnesses, never
    /// `mismatch`.
    Unimplemented(crate::refusal::Refusal),
    /// Real evaluation failure with cppnix-equivalent behavior, tagged with
    /// the cppnix exception class the bridge should raise, and where in the
    /// source it happened when the VM knew.
    ///
    /// The position is `Option` and not a sentinel because "no position" is a
    /// real answer that cppnix also gives: an error raised inside a builtin
    /// with nothing on the frame stack above it has no source to point at,
    /// and printing `at :0:0` would be worse than printing nothing.
    Eval(ErrKind, String, Option<crate::vm::SrcPos>),
    Parse(String),
}

impl EvalError {
    /// An evaluation failure with no position, which is every one raised by
    /// this crate outside the VM's unwind path.
    #[must_use]
    pub fn eval(kind: ErrKind, message: impl Into<String>) -> EvalError {
        EvalError::Eval(kind, message.into(), None)
    }

    /// Where this failure happened, when it has a place.
    #[must_use]
    pub fn pos(&self) -> Option<&crate::vm::SrcPos> {
        match self {
            EvalError::Eval(_, _, pos) => pos.as_ref(),
            EvalError::Unimplemented(_) | EvalError::Parse(_) => None,
        }
    }
}

/// Evaluate a string with no file behind it, which is what `--expr` is.
/// `__curPos` is `null` here, as it is under cppnix.
pub fn eval_str(src: &str) -> Result<String, EvalError> {
    eval_str_at(src, ".", compile::Origin::String)
}

/// Compile `src`, evaluate it against `host`, and render the answer the way a
/// test wants to assert on it: a string as itself, any other value as its
/// debug form, and any failure as its own text.
///
/// One body for the six that the test modules of `primops_host` and
/// `drvstrict` each kept privately, which differed only in the host they
/// named. Sharing it is not only less to read: every copy rendered a compile
/// failure as the bare string `"compile failed"`, so a typo in a test
/// expression surfaced as a mismatch against a string that named no cause.
/// Here the cause is in the message.
///
/// Every failure is rendered by its `Debug`, including a refusal: the
/// refusal-by-name tests read the `Unimplemented(...)` wrapper and the token
/// inside it out of this string, so a friendlier spelling would hide what they
/// assert on. `primops_host::context_tests::run_on` had a `"unimplemented: "`
/// arm with no assertion behind it and lost it here.
///
/// `&dyn Host` and not a type parameter, because there is nothing to
/// monomorphise for: the body is the same code whatever the host, and this is
/// how [`crate::readset::RecordingHost`] and the rest of the crate already
/// pass hosts around.
///
/// `settings` is a parameter and not [`Settings::current`] for ENG-12939's
/// reason: a helper reading the process configuration makes every test that
/// uses it depend on what the rest of the binary is doing. Callers that need
/// a store directory pass [`settings_with_store`]; the rest pass
/// [`Settings::default`].
/// An error's debug form with the source position left out.
///
/// The renderers below feed assertions about an error's CLASS and its
/// MESSAGE. Positions are compared against cppnix in `tests/positions.rs`,
/// which pins exact columns; including them here as well would tie every
/// message assertion in the crate to the byte offsets of its own fixture, so
/// rewording one test's source would fail an unrelated test with a diff about
/// a column number.
#[cfg(test)]
fn debug_without_pos(error: &EvalError) -> String {
    match error {
        EvalError::Eval(kind, message, _) => format!("Eval({kind:?}, {message:?})"),
        other => format!("{other:?}"),
    }
}

/// The same for a [`VmError`], whose position sits in a NAMED field of
/// [`crate::vm::Catchable`] rather than in a tuple slot.
///
/// Scrubbed out of the derived form rather than rebuilt from the fields,
/// because rebuilding pins the field ORDER of a struct these tests do not
/// own: a field added to `Catchable` tomorrow would then silently stop being
/// asserted on, which is the failure this whole helper exists to avoid. The
/// one thing it cannot survive is a message that literally contains
/// `", pos: "`; no fixture has one, and a test that grew one would fail
/// loudly rather than quietly.
#[cfg(test)]
fn vm_debug_without_pos(error: &VmError) -> String {
    let mut text = format!("{error:?}");
    const KEY: &str = ", pos: ";
    while let Some(start) = text.find(KEY) {
        let after = start.saturating_add(KEY.len());
        let Some(rest) = text.get(after..) else { break };
        let len = if rest.starts_with("None") {
            "None".len()
        } else if let Some(close) = rest.find(')') {
            close.saturating_add(1)
        } else {
            break;
        };
        text.replace_range(start..after.saturating_add(len), "");
    }
    text
}

#[cfg(test)]
pub(crate) fn render_with(settings: &Settings, host: &dyn Host, src: &str) -> String {
    let module = match compile::compile_source(src, "/m", compile::Origin::String, settings) {
        Ok(module) => module,
        Err(error) => return format!("compile failed: {error:?}"),
    };
    let mut vm = Vm::with_settings(settings.clone());
    vm.start_module(&Rc::new(module));
    let value = match drive(&mut vm, host) {
        Ok(value) => value,
        Err(error) => return vm_debug_without_pos(&error),
    };
    vm.start_print(value);
    match drive(&mut vm, host) {
        Ok(Value::Str(s)) => s.expect_text(),
        Ok(other) => format!("{other:?}"),
        Err(error) => vm_debug_without_pos(&error),
    }
}

/// [`eval_str_with`] rendered the same way: the answer as itself, a failure as
/// its debug form. Assertions never panic by hand here, since the workspace
/// denies `panic` in tests too.
///
/// `eval_str_with` and not `eval_str`, so the answer does not depend on what
/// any other test in the binary has configured (ENG-12939).
#[cfg(test)]
pub(crate) fn render_str_with(settings: &Settings, src: &str) -> String {
    match eval_str_with(src, ".", compile::Origin::String, settings) {
        Ok(value) => value,
        Err(error) => debug_without_pos(&error),
    }
}

/// [`render_str_with`] under the default configuration, which is what a test
/// wants unless it says otherwise.
#[cfg(test)]
pub(crate) fn render_str(src: &str) -> String {
    render_str_with(&Settings::default(), src)
}

/// A temp directory for a test, under a name no later process can land on.
///
/// # Why the pid and a counter were not enough
///
/// Three test modules each had their own copy of
/// `temp_dir()/{prefix}-{label}-{process::id()}-{TEMP.fetch_add()}`, and
/// neither component is unique across processes: the OS recycles pids, and
/// `TEMP` is a `static` reset to 0 by every new process, so it is not a nonce
/// but a small per-process ordinal (0 when a test runs alone, 10 when it runs
/// among its neighbours -- it counts the calls that happened to precede it).
///
/// `capi::warm_starts` then never removes what it made, on purpose:
/// `CacheDir` points the process-global `eval-cache-dir` at the directory,
/// and tests that merely *evaluate* read that global without taking
/// `CACHE_DIR_LOCK` -- the lock serialises setters, not readers -- so a
/// concurrent evaluation can be using the directory when the guard drops.
/// Deleting it there once broke a neighbouring test with `publishing
/// .../objects/ebde92b1...: No such file or directory`. **That reason
/// survives `CACHE_DIR_LOCK` and delete-on-drop is still wrong**; it was
/// re-checked rather than assumed when this was written.
///
/// A re-derivable name plus a directory that outlives the process is a
/// later run opening a store a dead one left warm, and being served rows it
/// never wrote. Measured: 33,400 leftover `ixe-warm-*` directories, and
/// `cargo test -p nix-eval-rs --lib` over 200 runs scored 5 failures against
/// the accumulated temp directory and 0 with a fresh `TMPDIR` per run
/// (ENG-13024).
///
/// [`SCRATCH_NONCE`] is what breaks it: one value per process, from a source
/// a later process cannot reproduce.
#[cfg(test)]
pub(crate) fn scratch_dir(prefix: &str, label: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static ORDINAL: AtomicU64 = AtomicU64::new(0);

    std::env::temp_dir().join(format!(
        "{prefix}-{label}-{}-{:016x}-{}",
        std::process::id(),
        scratch_nonce(),
        ORDINAL.fetch_add(1, Ordering::Relaxed)
    ))
}

/// One unpredictable value per process, for [`scratch_dir`].
///
/// `RandomState` seeds itself from the OS and is built to be unguessable
/// across processes, which is exactly the property the pid and the ordinal
/// lack. The wall clock would also mostly work and is worse: NTP can step it
/// backwards, so two processes can read the same instant, and the failure
/// this guards against needs only one repeat.
#[cfg(test)]
fn scratch_nonce() -> u64 {
    use std::hash::{BuildHasher as _, Hasher as _};
    static NONCE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *NONCE.get_or_init(|| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u32(std::process::id());
        hasher.finish()
    })
}

pub fn eval_str_at(
    src: &str,
    base_dir: &str,
    origin: compile::Origin<'_>,
) -> Result<String, EvalError> {
    eval_str_with(src, base_dir, origin, &Settings::current())
}

/// Evaluate under a stated configuration, reading no process state.
///
/// The entry point a test should reach for. [`eval_str`] takes the process
/// settings, so its answer depends on what every other test in the binary is
/// doing (ENG-12939).
pub fn eval_str_with(
    src: &str,
    base_dir: &str,
    origin: compile::Origin<'_>,
    settings: &Settings,
) -> Result<String, EvalError> {
    eval_str_at_with_depth(src, base_dir, origin, settings, settings.max_call_depth)
}

/// Evaluate with an explicit call-depth ceiling. Separate from the global so
/// a caller that has a limit in hand does not have to mutate process state to
/// use it, which is also what lets two tests want different ceilings.
pub fn eval_str_at_with_depth(
    src: &str,
    base_dir: &str,
    origin: compile::Origin<'_>,
    base: &Settings,
    max_call_depth: u32,
) -> Result<String, EvalError> {
    let mut settings = base.clone();
    settings.max_call_depth = max_call_depth;
    let mut vm = Vm::with_settings(settings);
    eval_str_on(src, base_dir, origin, &mut vm, &RealFs)
}

/// Evaluate on a machine the caller built, against a host the caller chose.
///
/// The bottom of the family: every other `eval_str*` here is this with a
/// fresh `Vm` and [`RealFs`]. Separate because who answers a file read, a
/// store copy or a warning is the embedder's to decide and used to be a
/// process global -- so an entry point that always evaluated against `RealFs`
/// silently meant "against whatever hooks the last caller installed".
/// The one mapping from a compile failure to an [`EvalError`].
///
/// `From` rather than a helper function so a second copy cannot quietly
/// appear: any caller holding a `CompileError` reaches this by `?` or
/// `.into()`. The arms are deliberately not a `Debug` catch-all --
/// `UndefinedVariable` has to read exactly as cppnix's
/// "undefined variable 'x'", or a gate comparing stderr sees a divergence
/// where the two evaluators agree. No arm attaches a position, and that is
/// not an omission: a `CompileError` carries no offset, so there is nothing
/// to attach (ENG-13128).
impl From<CompileError> for EvalError {
    fn from(e: CompileError) -> Self {
        match e {
            CompileError::Unimplemented(w) => EvalError::Unimplemented(w),
            CompileError::UndefinedVariable(n) => {
                EvalError::eval(ErrKind::Eval, format!("undefined variable '{n}'"))
            }
            CompileError::Parse(m) => EvalError::Parse(m),
            CompileError::Eval(m) => EvalError::Eval(ErrKind::Eval, m, None),
        }
    }
}

/// A compile through the machine's cache ([`Vm::compile`]) fails either
/// because the source is bad, which keeps the class the source-level mapping
/// above gives it, or because the cache's store is (an I/O refusal, a row
/// naming an object that is gone): not the expression's fault, and reported
/// as an evaluation error naming the cache rather than blamed on the syntax.
/// `session::compile_failure` is this same split for the embedder's status
/// strings.
impl From<crate::modcache::CacheError> for EvalError {
    fn from(e: crate::modcache::CacheError) -> Self {
        match e {
            crate::modcache::CacheError::Compile(compile) => EvalError::from(compile),
            other => EvalError::eval(ErrKind::Eval, format!("compile cache: {other}")),
        }
    }
}

pub fn eval_str_on(
    src: &str,
    base_dir: &str,
    origin: compile::Origin<'_>,
    vm: &mut Vm,
    host: &dyn Host,
) -> Result<String, EvalError> {
    let module = vm
        .compile(src, base_dir, origin)
        .map_err(EvalError::from)?
        .module;
    vm.start_module(&module);
    let value = drive(vm, host).map_err(map_vm_error)?;
    vm.start_print(value);
    match drive(vm, host).map_err(map_vm_error)? {
        Value::Str(s) => Ok(crate::primops_pure::text_of(&s)
            .map_err(map_vm_error)?
            .to_owned()),
        _ => Err(EvalError::eval(
            ErrKind::Eval,
            "internal: printer produced a non-string",
        )),
    }
}

/// A slow question a host is working on while the scheduler runs something
/// else.
struct InFlight {
    /// The suspension this answer will wake.
    resume: crate::vm::ResumeToken,
    /// What the host called it.
    ticket: crate::host::Ticket,
    /// Which of the three slow shapes came back, so the collect knows how to
    /// turn it into a value.
    shape: SlowShape,
    /// The builtin's name, for the `NoStore` message. Captured when the
    /// question was begun, because the request is gone by the time the
    /// answer arrives.
    who: String,
    /// The perf bucket this question counts against.
    kind: usize,
    /// When it was begun. A question costs the wall time from asking to
    /// having the answer, which for an asynchronous one is not the time the
    /// collect took.
    began: std::time::Instant,
    /// The context a `Realise` question was about, kept so the answer can
    /// be remembered in [`JobMemo::realised`] when it arrives; `None` for
    /// every other shape.
    realised: Option<Vec<crate::value2::ContextElem>>,
}

/// Which value-building half a collected [`crate::host::SlowAnswer`] needs.
///
/// The one owner of "which questions can go to a worker thread": `begin_slow`
/// classifies by producing one of these, and `perf` reports under
/// [`SlowShape::ALL`]'s names, so the scheduler and the census cannot list
/// different sets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlowShape {
    Fetch,
    FetchTree,
    Flake,
    Realise,
}

impl SlowShape {
    /// Every shape, in reporting order.
    pub const ALL: [SlowShape; 4] = [
        SlowShape::Fetch,
        SlowShape::FetchTree,
        SlowShape::Flake,
        SlowShape::Realise,
    ];

    /// The question kind's name, as `purity::question_kind` spells it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            SlowShape::Fetch => "Fetch",
            SlowShape::FetchTree => "FetchTree",
            SlowShape::Flake => "Flake",
            SlowShape::Realise => "Realise",
        }
    }
}

/// One evaluation the scheduler is running.
struct Job<'a> {
    vm: &'a mut Vm,
    host: &'a dyn Host,
    /// Lent by whoever owns the recording this job answers into, for exactly
    /// as long as that recording: [`drive`] makes one per call, and the
    /// handle API keeps one per question (`capi::MemoScope`), so every force
    /// a question performs shares it. Never shared across two recordings:
    /// that would let the job that missed record the `ReadDir` and the job
    /// that hit record nothing, so the second one's read set would omit a
    /// directory its value depends on -- a wrong answer from a memo, not a
    /// slow one. See the type's own doc.
    memo: &'a mut JobMemo,
    /// The slow questions this job has in flight, oldest resume token first.
    ///
    /// A map and not a slot, since ENG-13150: one job can have several
    /// strands parked at once, each on its own question. Ordered by token --
    /// mint order, which is ask order -- because that is the order the
    /// answers must be *delivered* in, whatever order they arrive in; see
    /// [`drive_concurrent`].
    inflight: std::collections::BTreeMap<crate::vm::ResumeToken, InFlight>,
    /// Set once, when the job stops having anything more to do.
    outcome: Option<Result<Value, VmError>>,
    /// Whether [`settle_job`] has run for this job; it runs once, on the
    /// scheduler turn after `outcome` is set.
    settled: bool,
}

/// The scheduler side of the poll loop: the only place in the crate that
/// touches a filesystem. The VM asks, this answers, and the frame chain is
/// untouched across the gap -- which is the property that lets the effects
/// kernel later record what was read, or replay it, without the evaluator
/// knowing.
///
/// One evaluation, so nothing overlaps: a question this machine asks is the
/// only question outstanding, and the scheduler waits for it. See
/// [`drive_concurrent`] for the case that has something else to be getting
/// on with.
pub fn drive(vm: &mut Vm, host: &dyn Host) -> Result<Value, VmError> {
    let mut memo = JobMemo::default();
    drive_with(vm, host, &mut memo)
}

/// [`drive`] with the evaluation's memo lent by the caller.
///
/// The caller owns the memo's lifetime, and with it the soundness argument
/// on [`Job::memo`]: the memo must live exactly as long as the recording it
/// answers into. Two callers lend one across several drives, and both hold
/// one recording across them -- the handle API for every force of one
/// question (`capi::MemoScope`), and `session::run` for the evaluation and
/// the render that prints it. Measured before this existed: one `nix eval`
/// of a host's `toplevel.drvPath` asked the host for 126,094 directory
/// listings over 8,084 directories, 15.6 per directory, one per force the
/// embedder made (`goals/rust-eval.md`, 2026-09-04).
pub(crate) fn drive_with(
    vm: &mut Vm,
    host: &dyn Host,
    memo: &mut JobMemo,
) -> Result<Value, VmError> {
    let mut outcomes = drive_concurrent_with(vec![(vm, host, memo)]);
    outcomes.pop().unwrap_or_else(|| {
        Err(VmError::eval(
            "internal: the scheduler dropped its only job",
        ))
    })
}

/// Run several independent evaluations, overlapping the time they spend
/// waiting on the world.
///
/// # What overlaps
///
/// A job that asks a slow question ([`crate::host::Slow`]) and gets a ticket
/// back is parked, and the scheduler moves to the next job rather than
/// waiting. So two evaluations that each stall on a fetch stall at the same
/// time, and the wall clock is the longer of the two rather than their sum.
/// The scheduler blocks in exactly one place -- when every job is parked --
/// and by then every question it could be waiting for is already in flight.
///
/// # What overlaps inside one evaluation (ENG-13150)
///
/// A job whose strand parks on a begun question is not necessarily stuck:
/// the forcing walks (`crate::deepwalk`, `crate::drvstrict`) publish the
/// child they would force next, and the scheduler seeds it as a sibling
/// strand ([`crate::vm::Vm::spawn_root`]) at the moment a question goes into
/// flight. The offer is consumed by that one begin and republished only when
/// the walk's own fiber next reaches a child force, so within one walk the
/// steady state is a pipeline -- the question in flight plus the sibling
/// running toward the next one -- rather than an N-wide burst; N-wide is
/// what several jobs get. What keeps any of it within [`crate::vm::Fiber`]'s
/// determinism rules is that nothing about it depends on *arrival*: strands
/// are seeded at points the program (and the host's `begin`) determine, and
/// answers are delivered strictly in resume-token order -- the oldest
/// question in flight is collected first, blocking on it even while a
/// younger answer sits ready. Token order is mint order is ask order, so the
/// delivery schedule is a property of the programs rather than of the
/// network, within a job exactly as across jobs.
///
/// The question order a `RecordingHost` sees, and the `Sym` intern order,
/// do change relative to a host that begins nothing -- deterministically,
/// the same way on every run against the same host. That moves the read-set
/// memo key, which is a miss and not a wrong answer (see `crate::readset`'s
/// module doc).
///
/// The jobs must be independent. Two `Vm`s cannot exchange a `Value` -- `Sym`
/// is an index into one interner -- so this is a property of the type rather
/// than a rule a caller has to keep.
///
/// # A host that begins nothing
///
/// [`crate::host::Host::begin`] defaults to `None`, and the fallback is the
/// same synchronous [`answer_path`] call in the same place. A host that has
/// not opted in sees the previous behaviour, question for question, whether
/// it is driven through here or through [`drive`].
pub fn drive_concurrent<'a>(
    jobs: impl IntoIterator<Item = (&'a mut Vm, &'a dyn Host)>,
) -> Vec<Result<Value, VmError>> {
    // One memo per job, living for this call: the recording each job answers
    // into is its host's, and nothing outlives the call.
    let jobs: Vec<(&'a mut Vm, &'a dyn Host)> = jobs.into_iter().collect();
    let mut memos: Vec<JobMemo> = jobs.iter().map(|_| JobMemo::default()).collect();
    drive_concurrent_with(
        jobs.into_iter()
            .zip(memos.iter_mut())
            .map(|((vm, host), memo)| (vm, host, memo)),
    )
}

/// [`drive_concurrent`] with each job's memo lent by the caller; see
/// [`drive_with`] for who lends one and why.
fn drive_concurrent_with<'a>(
    jobs: impl IntoIterator<Item = (&'a mut Vm, &'a dyn Host, &'a mut JobMemo)>,
) -> Vec<Result<Value, VmError>> {
    let mut jobs: Vec<Job<'a>> = jobs
        .into_iter()
        .map(|(vm, host, memo)| Job {
            vm,
            host,
            memo,
            inflight: std::collections::BTreeMap::new(),
            outcome: None,
            settled: false,
        })
        .collect();
    loop {
        // Advance everything that can be advanced without waiting.
        // `step_job` always leaves a job either finished or parked on
        // something in flight, so one pass of this ends with every job in one
        // of those two states -- which is what makes the loop terminate: each
        // turn either finishes a job or consumes one answer, and the number
        // of answers is bounded by the number of questions the programs ask.
        for job in &mut jobs {
            if job.outcome.is_none() {
                step_job(job);
            }
            // A job can finish with questions still in flight: a strand was
            // seeded for a child the program then never needed (`tryEval`
            // caught past the walk). Those answers are abandoned -- dropping
            // the ticket is dropping our half of the rendezvous, and the
            // host's worker finishes into a channel nobody reads. They must
            // not be *collected*: the fiber a collect would resume was
            // cleared with the machine, and blocking on an answer nobody
            // wants is the stall this scheduler exists to remove.
            if job.outcome.is_some() && !job.settled {
                for question in job.inflight.values() {
                    crate::perf::note_question_abandoned(question.kind);
                }
                job.inflight.clear();
                job.settled = true;
                settle_job(job);
            }
        }
        // Nothing can move until an answer arrives. Wait for the oldest
        // question in flight rather than polling for whichever finishes
        // first: every one of them is already being worked on, so waiting for
        // the one that has been waiting longest is waiting for the set, and
        // it keeps the order answers are delivered in a property of the
        // programs rather than of the network -- across jobs and, since
        // ENG-13150, within one, where it is the determinism invariant
        // rather than a nicety.
        let waiting = jobs
            .iter_mut()
            .filter(|j| !j.inflight.is_empty())
            .min_by_key(|j| j.inflight.keys().next().copied());
        let Some(job) = waiting else {
            // Nothing is waiting, so by `step_job`'s postcondition every job
            // has an outcome. `unwrap_or_else` rather than dropping the job:
            // a caller reads these back by position, and a short vector would
            // silently hand it another job's answer.
            return jobs
                .into_iter()
                .map(|j| {
                    j.outcome.unwrap_or_else(|| {
                        Err(VmError::eval(
                            "internal: the scheduler returned a job it never finished",
                        ))
                    })
                })
                .collect();
        };
        collect_one(job);
    }
}

/// A finished job settles: the host gets its one chance to make every effect
/// the evaluation deferred visible ([`Host::settle`]) before the value leaves
/// the scheduler. This is the only place it happens, so every route into an
/// evaluation -- a forced handle, a render, a memo verification, a
/// derivation-set walk -- is covered by construction rather than by a flush
/// at each entry point (a flush at `force_slot` and friends missed
/// `ixe_render`, whose deep force is where `nix-instantiate --eval --strict`
/// builds its derivations; the functional test rust-eval-host-questions.sh
/// counted `flushes: 0`). A settled failure turns a finished value into an
/// error; a failed evaluation keeps its own error, the settle still running so
/// nothing stays deferred across the boundary.
fn settle_job(job: &mut Job<'_>) {
    match (job.host.settle(), &job.outcome) {
        (Ok(()), _) => {}
        (Err(error), Some(Ok(_))) => {
            job.outcome = Some(Err(VmError::eval(format!(
                "settling the evaluation's deferred store effects: {error}"
            ))));
        }
        // The evaluation's own error stays the error. The store's refusal is
        // still said: a queued derivation the store would not take is a fact
        // about the store, and the next evaluation meets the same poison.
        (Err(error), _) => job.host.warn(&format!(
            "settling the failed evaluation's deferred store effects: {error}"
        )),
    }
}

/// Advance one job until it finishes or every strand it has is parked on a
/// slow question.
///
/// # Postcondition
///
/// On return, `job.outcome` is set or `job.inflight` is non-empty.
/// `drive_concurrent`'s termination rests on it: a job that came back having
/// neither finished nor parked would be stepped again in exactly the same
/// state, for ever.
fn step_job(job: &mut Job<'_>) {
    loop {
        match job.vm.poll() {
            Err(error) => {
                job.outcome = Some(Err(error));
                return;
            }
            Ok(Step::Done(value)) => {
                job.outcome = Some(Ok(value));
                return;
            }
            Ok(Step::Perform { domain, .. }) => {
                job.outcome = Some(Err(VmError::Unimplemented(Refusal::new(
                    RefusalToken::EffectDomain,
                    format!("effect domain '{domain}'"),
                ))));
                return;
            }
            // Every strand is parked. With questions in flight that is the
            // working state -- the scheduler collects the oldest and comes
            // back. With none it is a defect in this loop rather than
            // anything the program did (every path below either resumes the
            // machine or records a ticket), and the job ends here instead of
            // being stepped again for ever.
            Ok(Step::Idle { outstanding }) => {
                if job.inflight.is_empty() {
                    job.outcome = Some(Err(VmError::eval(format!(
                        "internal: the evaluation is parked on {outstanding} suspension(s) \
                         with nothing in flight to answer them"
                    ))));
                }
                return;
            }
            Ok(Step::NeedPath { need, resume }) => {
                // Counted here and nowhere else: this is the one place a
                // question crosses out of the VM, which is what makes the
                // count complete in the same way the read set is.
                // The dense index comes from the same `question_kinds!` list
                // as the name, so it cannot miss the way a name lookup with
                // a fallback once did (ENG-13065: `Flake` was in the match
                // and not the list, and every `getFlake` landed in a bucket
                // the per-kind counters dropped).
                let kind = crate::purity::question_kind_index(&need);
                crate::perf::note_question_asked(kind);
                job.vm.note_question_argument(&need);
                match begin_slow(job.vm, job.host, &need, resume, kind, &*job.memo) {
                    Err(error) => {
                        job.outcome = Some(Err(error));
                        return;
                    }
                    Ok(Some(begun)) => {
                        job.inflight.insert(resume, begun);
                        // The question is being worked on off-thread, so the
                        // machine is free. Seed the walk's offered sibling if
                        // there is one (ENG-13150) -- this is the one moment
                        // overlap buys anything -- and keep polling: the next
                        // runnable strand advances instead of this loop
                        // blocking on the answer.
                        if let Some(slot) = job.vm.take_fanout_offer() {
                            job.vm.spawn_root(slot);
                        }
                        continue;
                    }
                    Ok(None) => {}
                }
                // A failed answer resumes the machine with the failure rather
                // than returning it from here, so it unwinds through the
                // frames and `tryEval` can catch it. See `Vm::resume_error`.
                let (answered, nanos) =
                    crate::perf::timed(|| answer_path(job.vm, job.host, &need, &mut *job.memo));
                crate::perf::note_question_answered(kind, nanos);
                let resumed = match answered {
                    Ok(answer) => job.vm.resume(resume, answer),
                    Err(error) => job.vm.resume_error(resume, error),
                };
                if let Err(error) = resumed {
                    job.outcome = Some(Err(error));
                    return;
                }
            }
        }
    }
}

/// Ask the host to begin this question in the background, if it is one of the
/// slow ones and the host has an asynchronous path for it.
///
/// `Ok(None)` means the caller should answer it synchronously, which is both
/// the default and every case that is not a fetch.
fn begin_slow(
    vm: &Vm,
    host: &dyn Host,
    need: &NeedPath,
    resume: crate::vm::ResumeToken,
    kind: usize,
    memo: &JobMemo,
) -> Result<Option<InFlight>, VmError> {
    let mut realised = None;
    let (question, shape, who) = match need {
        NeedPath::Fetch(request) => (
            crate::host::Slow::Fetch(request),
            SlowShape::Fetch,
            request.kind.who().to_owned(),
        ),
        NeedPath::FetchTree(request) => (
            crate::host::Slow::FetchTree(request),
            SlowShape::FetchTree,
            request.fetcher.as_str().to_owned(),
        ),
        NeedPath::Flake(flake_ref) => (
            crate::host::Slow::Flake(flake_ref),
            SlowShape::Flake,
            "getFlake".to_owned(),
        ),
        // A context this evaluation has already had built is answered by
        // `answer_path` from the memo; beginning it would spend a validity
        // check, a thread and a `buildPaths` on an answer already in hand.
        NeedPath::Realise(context) if memo.realised.contains_key(context) => return Ok(None),
        NeedPath::Realise(context) => {
            realised = Some(context.clone());
            (
                crate::host::Slow::Realise(context),
                SlowShape::Realise,
                realise_who(context),
            )
        }
        _ => return Ok(None),
    };
    // The same gate `answer_path` puts in front of the blocking call, and for
    // the same reason. A question the access check would refuse must not
    // reach a host by this route either; letting it would be ENG-12543 with a
    // thread in the middle. When the check has something to say, this
    // declines to begin and the synchronous path says it.
    if access_check(vm, need)?.is_some() {
        return Ok(None);
    }
    let began = std::time::Instant::now();
    Ok(host.begin(&question).map(|ticket| {
        crate::perf::note_slow_question_started(kind);
        InFlight {
            resume,
            ticket,
            shape,
            who,
            kind,
            began,
            realised,
        }
    }))
}

/// Wait for this job's oldest in-flight answer and put it back into the
/// machine. Oldest and only oldest: delivering a younger answer first would
/// make the resume order -- and through it the intern order and the recorded
/// read set -- depend on which fetch won a race.
fn collect_one(job: &mut Job<'_>) {
    let Some((_, done)) = job.inflight.pop_first() else {
        job.outcome = Some(Err(VmError::eval(
            "internal: collected a job with nothing in flight",
        )));
        return;
    };
    let (answer, collect_call_ns) = crate::perf::timed(|| job.host.collect(done.ticket, true));
    crate::perf::note_collect_call(collect_call_ns);
    let latency_ns = u64::try_from(done.began.elapsed().as_nanos()).unwrap_or(u64::MAX);
    let Some(answer) = answer else {
        crate::perf::note_question_abandoned(done.kind);
        job.outcome = Some(Err(VmError::eval(format!(
            "internal: the host abandoned the '{}' question it agreed to answer",
            done.who
        ))));
        return;
    };
    crate::perf::note_question_answered(done.kind, latency_ns);
    crate::perf::note_slow_question_answered(done.kind, latency_ns);
    // The value is built here, on the thread that owns the VM. Only plain
    // owned data crossed from the worker: nothing `Rc` and nothing interned
    // was ever touched off this thread.
    let value = match (done.shape, answer) {
        (SlowShape::Fetch, crate::host::SlowAnswer::Store(answer)) => {
            fetch_answer(answer, &done.who)
        }
        (SlowShape::FetchTree, crate::host::SlowAnswer::Store(answer)) => {
            tree_answer(job.vm, answer, &done.who)
        }
        (SlowShape::Flake, crate::host::SlowAnswer::Flake(answer)) => flake_answer(job.vm, answer),
        (SlowShape::Realise, crate::host::SlowAnswer::Realise(answer)) => {
            if let Some(context) = done.realised {
                remember_realised(&mut *job.memo, context, &answer);
            }
            realise_answer(answer, &done.who)
        }
        // The host answered a different shape from the one it was asked. Not
        // recoverable and not the program's fault: a value invented here
        // would be this evaluator making up a store path.
        (_, _) => Err(VmError::eval(format!(
            "internal: the host answered the '{}' question with the wrong shape",
            done.who
        ))),
    };
    let resumed = match value {
        Ok(value) => job.vm.resume(done.resume, value),
        Err(error) => job.vm.resume_error(done.resume, error),
    };
    if let Err(error) = resumed {
        job.outcome = Some(Err(error));
    }
}

/// Directory listings already answered during this evaluation.
///
/// # Why this is worth having
///
/// A minimal NixOS toplevel asks 13,485 `Entries` questions covering **767
/// distinct directories** -- it reads the average directory 17.6 times. The
/// repetition is `lib.cleanSourceWith` and its callers: 33 filtered copies
/// walk overlapping trees, and each walk re-read every directory from
/// scratch. ENG-12862 guessed the cost was the two traversals this evaluator
/// makes where cppnix makes one, which would be a factor of two; the
/// measurement says the traversals were not the problem, the repeats were.
///
/// # Why it lives here and not on the `Vm`
///
/// It is keyed by path, and a path-keyed entry is valid only while the file
/// behind the path has not changed -- true within one evaluation, false for
/// anything that outlives one. `Vm::modules` is content-keyed for exactly
/// that reason, and the `Vm` does outlive an evaluation on the warm-start
/// path. The cache lives in a [`JobMemo`] owned by whoever owns the recording
/// it answers into ([`drive`] for one call, `capi::MemoScope` for one
/// question), which makes the lifetime structural: it is created when a
/// recording starts and dropped when the recording is filed, so no clearing
/// discipline exists to get wrong.
///
/// # What it does to the read set
///
/// A recording `Host` now sees one `ReadDir` per directory instead of 17.6,
/// which is the dependency stated once rather than repeatedly -- still
/// complete, since the evaluation's result does depend on exactly those
/// directories. Witnesses recorded before this change replay a longer
/// question list and so compute a key nothing was stored under, which is a
/// miss and not a wrong answer (see `readset`'s module doc).
///
/// Sharing the answer is sound because these attrsets are immutable and
/// already forced: every slot is a `Slot::value`, never a thunk that a
/// second consumer could force differently.
type DirCache = std::collections::HashMap<Rc<crate::value2::PathValue>, Rc<Attrs>>;

/// What one evaluation remembers of the host's answers, beside the read set:
/// directory listings, imported sources, and the store copy of each path.
///
/// One per recording, never shared across two, for the reason `Job::memo`
/// gives: a memo outliving its recording would let the job that missed
/// record the question and the job that hit record nothing. And one per
/// machine: the listings hold `Sym`s of the VM that interned them, and a
/// `Sym` is an index into that VM's interner alone.
///
/// Every root is memoised, the ambient filesystem included. One evaluation
/// is one snapshot of the world: the read set records a question once, with
/// one answer, and a world that changed under a running evaluation would make
/// its key unreproducible (a miss, never a wrong answer) whether or not the
/// second ask reached the host. cppnix makes the same assumption with its
/// parse cache and its per-evaluation `srcToStore`. What repeating the ask
/// cost was measured, not supposed: one home-manager evaluation coerced
/// 170k paths, 780 of them distinct, and every repeat crossed the ABI and
/// reached the store (`maintainers/ix/host-questions.md`).
#[derive(Default)]
pub(crate) struct JobMemo {
    dirs: DirCache,
    /// The store path each coerced path copied to.
    copies: std::collections::HashMap<Rc<crate::value2::PathValue>, String>,
    /// The `import` answer for each path: resolved path, root and text, as
    /// the attrset the VM compiles from. cppnix's `fileParseCache`.
    imports: std::collections::HashMap<Rc<crate::value2::PathValue>, Rc<Attrs>>,
    /// The rewrites each realised context answered with. A `Realise` that
    /// has already succeeded in this evaluation is not asked again: every
    /// ask, built or not, costs the embedder a validity check per element, a
    /// worker thread and a `buildPaths` round trip (about 2 ms each; one
    /// home-manager evaluation asked 2,269 times for 48 distinct contexts,
    /// 4.4 s). Sound for the reason `copies` is: within one evaluation the
    /// store either still has the outputs or the evaluation's key is
    /// unreproducible anyway. A `BTreeMap` because [`ContextElem`] orders
    /// but does not hash. Only answered contexts are here: two strands that
    /// begin the same context before either collects both build it, as they
    /// did before the memo, and the store answers both alike; joining the
    /// second onto the first's ticket would be a scheduler feature, not a
    /// memo.
    realised: std::collections::BTreeMap<
        Vec<crate::value2::ContextElem>,
        std::collections::BTreeMap<String, String>,
    >,
}

/// Remember a successful `Realise` answer for `context` in this evaluation's
/// memo, whichever route (synchronous or begun) it came by. A failure is not
/// remembered: `realise_answer` makes it uncatchable, so no later asker
/// exists in this evaluation and a remembered failure would be dead state.
fn remember_realised(
    memo: &mut JobMemo,
    context: Vec<crate::value2::ContextElem>,
    answer: &Result<std::collections::BTreeMap<String, String>, StoreError>,
) {
    if let Ok(rewrites) = answer {
        memo.realised.insert(context, rewrites.clone());
    }
}

/// `pure-eval`, as the embedder set it.
///
/// Two globals rather than the one `FILESYSTEM_ACCESS` flag that used to
/// stand for `restrictEval || pureEval`, because the two settings forbid
/// different things and conflating them refused every host question under
/// either. `crate::purity` is where the difference is written down and
/// cited; these two are only the storage.
static PURE_EVAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// `restrict-eval`, as the embedder set it. See [`PURE_EVAL`].
static RESTRICT_EVAL: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tell the evaluator whether `pure-eval` is on.
pub fn set_pure_eval(on: bool) {
    #[cfg(test)]
    assert_globals_exclusive("pure-eval");
    PURE_EVAL.store(on, Ordering::Relaxed);
}

/// Whether `pure-eval` is on.
#[must_use]
pub fn pure_eval() -> bool {
    #[cfg(test)]
    assert_globals_guarded("pure-eval");
    PURE_EVAL.load(Ordering::Relaxed)
}

/// Tell the evaluator whether `restrict-eval` is on.
pub fn set_restrict_eval(on: bool) {
    #[cfg(test)]
    assert_globals_exclusive("restrict-eval");
    RESTRICT_EVAL.store(on, Ordering::Relaxed);
}

/// Whether `restrict-eval` is on.
#[must_use]
pub fn restrict_eval() -> bool {
    #[cfg(test)]
    assert_globals_guarded("restrict-eval");
    RESTRICT_EVAL.load(Ordering::Relaxed)
}

/// cppnix's `trace-verbose`, which decides what `builtins.traceVerbose` *is*.
///
/// Not a presentation flag. cppnix picks the implementation when it builds
/// the base environment -- `settings.traceVerbose ? prim_trace : prim_second`
/// (`primops.cc:5560`) -- and the two differ in whether the first argument is
/// forced at all, so `builtins.traceVerbose (throw "x") 1` is `1` with the
/// setting off and a dead evaluation with it on. That is a different value
/// from the same text, which is why this is a keyed setting and not a hook.
static TRACE_VERBOSE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tell the evaluator whether `builtins.traceVerbose` traces.
pub fn set_trace_verbose(on: bool) {
    #[cfg(test)]
    assert_globals_exclusive("trace-verbose");
    TRACE_VERBOSE.store(on, Ordering::Relaxed);
}

/// Whether `builtins.traceVerbose` traces.
#[must_use]
pub fn trace_verbose() -> bool {
    #[cfg(test)]
    assert_globals_guarded("trace-verbose");
    TRACE_VERBOSE.load(Ordering::Relaxed)
}

/// cppnix's `abort-on-warn`, which turns `builtins.warn` into a failure.
///
/// Also not presentation: with it on, cppnix warns and then throws
/// (`primops.cc:1369`), so an expression that evaluates to a value with the
/// setting off has no value at all with it on. A backend that ignored it
/// would answer where cppnix dies, which is the worst direction for a
/// divergence to run.
static ABORT_ON_WARN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tell the evaluator whether `builtins.warn` aborts after warning.
pub fn set_abort_on_warn(on: bool) {
    #[cfg(test)]
    assert_globals_exclusive("abort-on-warn");
    ABORT_ON_WARN.store(on, Ordering::Relaxed);
}

/// Whether `builtins.warn` aborts after warning.
#[must_use]
pub fn abort_on_warn() -> bool {
    #[cfg(test)]
    assert_globals_guarded("abort-on-warn");
    ABORT_ON_WARN.load(Ordering::Relaxed)
}

/// cppnix's `ca-derivations` experimental feature.
///
/// A value-deciding setting like the two above: with it off,
/// `__contentAddressed = true` is the feature-is-disabled error, and with it
/// on the same derivation is a floating-CA `.drv`. A backend that ignored it
/// would compute paths cppnix refuses to.
static CA_DERIVATIONS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tell the evaluator whether cppnix's `ca-derivations` feature is enabled.
pub fn set_ca_derivations(on: bool) {
    #[cfg(test)]
    assert_globals_exclusive("ca-derivations");
    CA_DERIVATIONS.store(on, Ordering::Relaxed);
}

/// Whether the `ca-derivations` feature is enabled.
#[must_use]
pub fn ca_derivations() -> bool {
    #[cfg(test)]
    assert_globals_guarded("ca-derivations");
    CA_DERIVATIONS.load(Ordering::Relaxed)
}

/// Policy the memo must not serve across. None of the three changes what an
/// expression evaluates to; each changes whether the host lets it: a
/// realisation served by validity never reaches `realiseContextCheck`
/// (`allow-import-from-derivation`), a fetch served by validity never reaches
/// `checkURI` (`allowed-uris`), and under `--repair` a served derivation
/// write skips the rewrite repair exists for. So the first two are in the
/// key and the third turns the memo off.
static ALLOW_IMPORT_FROM_DERIVATION: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(true);
static ALLOWED_URIS: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
static REPAIR: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tell the evaluator whether `allow-import-from-derivation` is on.
pub fn set_allow_import_from_derivation(on: bool) {
    #[cfg(test)]
    assert_globals_exclusive("allow-import-from-derivation");
    ALLOW_IMPORT_FROM_DERIVATION.store(on, Ordering::Relaxed);
}

/// Whether `allow-import-from-derivation` is on.
#[must_use]
pub fn allow_import_from_derivation() -> bool {
    #[cfg(test)]
    assert_globals_guarded("allow-import-from-derivation");
    ALLOW_IMPORT_FROM_DERIVATION.load(Ordering::Relaxed)
}

/// Tell the evaluator the `allowed-uris` list, newline-terminated entries.
pub fn set_allowed_uris(uris: String) {
    #[cfg(test)]
    assert_globals_exclusive("allowed-uris");
    *ALLOWED_URIS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(uris);
}

/// The `allowed-uris` list as the embedder sent it, if it did.
#[must_use]
pub fn allowed_uris() -> Option<String> {
    #[cfg(test)]
    assert_globals_guarded("allowed-uris");
    ALLOWED_URIS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Tell the evaluator whether the evaluation runs under `--repair`.
pub fn set_repair(on: bool) {
    #[cfg(test)]
    assert_globals_exclusive("repair");
    REPAIR.store(on, Ordering::Relaxed);
}

/// Whether the evaluation runs under `--repair`.
#[must_use]
pub fn repair() -> bool {
    #[cfg(test)]
    assert_globals_guarded("repair");
    REPAIR.load(Ordering::Relaxed)
}

/// cppnix's `blake3-hashes` experimental feature.
///
/// Value-deciding wherever a hash algorithm is parsed: with it off cppnix
/// raises `MissingExperimentalFeature`, while with it on the same expression
/// computes a 32-byte Blake3 digest (`libutil/hash.cc:25-29,468-473`).
static BLAKE3_HASHES: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tell the evaluator whether cppnix's `blake3-hashes` feature is enabled.
pub fn set_blake3_hashes(on: bool) {
    #[cfg(test)]
    assert_globals_exclusive("blake3-hashes");
    BLAKE3_HASHES.store(on, Ordering::Relaxed);
}

/// Whether the `blake3-hashes` feature is enabled.
#[must_use]
pub fn blake3_hashes() -> bool {
    #[cfg(test)]
    assert_globals_guarded("blake3-hashes");
    BLAKE3_HASHES.load(Ordering::Relaxed)
}

/// cppnix's `parse-toml-timestamps` experimental feature.
///
/// Value-deciding like `ca-derivations` above: with it off a TOML date or
/// time is `error: while parsing TOML: Dates and times are not supported`,
/// and with it on the same document evaluates to
/// `{ _type = "timestamp"; value = "..."; }` sets (`primops.cc`,
/// `prim_fromTOML`'s `visit` on `toml::value_t`).
static PARSE_TOML_TIMESTAMPS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// Tell the evaluator whether cppnix's `parse-toml-timestamps` feature is
/// enabled.
pub fn set_parse_toml_timestamps(on: bool) {
    #[cfg(test)]
    assert_globals_exclusive("parse-toml-timestamps");
    PARSE_TOML_TIMESTAMPS.store(on, Ordering::Relaxed);
}

/// Whether the `parse-toml-timestamps` feature is enabled.
#[must_use]
pub fn parse_toml_timestamps() -> bool {
    #[cfg(test)]
    assert_globals_guarded("parse-toml-timestamps");
    PARSE_TOML_TIMESTAMPS.load(Ordering::Relaxed)
}

/// A parser lint's level: cppnix's `Diagnose` (`diagnose.hh:16-29`).
///
/// Only `Fatal` decides a value. At `fatal` the lint makes the program
/// illegal -- cppnix's parser throws -- so the compiler here must throw for
/// the same text. At `warn` the only difference from `ignore` is a stderr
/// line cppnix prints and this backend does not; that is warning text,
/// tier 2 ("Parity bar"), the line measured and drawn when the bridge
/// refused `fatal` and waved `warn` through (ENG-12569). `Warn` is still a
/// distinct value here so the bridge reports what the setting *is*, not
/// what this backend happens to do about it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Diagnose {
    Ignore,
    Warn,
    Fatal,
}

impl Diagnose {
    /// Whether this level makes the linted construct a compile error.
    #[must_use]
    pub fn is_fatal(self) -> bool {
        matches!(self, Diagnose::Fatal)
    }

    /// The level as the C ABI carries it: 0 ignore, 1 warn, 2 fatal.
    ///
    /// Out-of-range values clamp to `Ignore` deliberately: the only caller
    /// is the bridge, which maps a three-value `enum class` and cannot send
    /// anything else, and a backend that *refused to evaluate* on a garbage
    /// level would turn an ABI slip into a fleet-wide refusal.
    #[must_use]
    pub fn from_c(level: i32) -> Self {
        match level {
            2 => Diagnose::Fatal,
            1 => Diagnose::Warn,
            _ => Diagnose::Ignore,
        }
    }

    fn as_u8(self) -> u8 {
        match self {
            Diagnose::Ignore => 0,
            Diagnose::Warn => 1,
            Diagnose::Fatal => 2,
        }
    }

    fn from_u8(v: u8) -> Self {
        Self::from_c(i32::from(v))
    }
}

/// cppnix's three parser lints (`eval-settings.hh:566-651`), which its
/// parser fires on URL, short-path and absolute/home-path literals
/// (`parser.y:372-466`). The compiler mirrors those sites (`compile.rs`),
/// which is what lets the bridge forward the levels instead of refusing
/// every evaluation under a `fatal` one (ENG-12597).
static LINT_URL_LITERALS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// See [`LINT_URL_LITERALS`].
static LINT_SHORT_PATH_LITERALS: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// See [`LINT_URL_LITERALS`]. Covers home (`~/x`) literals too, as cppnix's
/// `HPATH` rule does (`parser.y:461-466`).
static LINT_ABSOLUTE_PATH_LITERALS: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

/// Tell the evaluator cppnix's `lint-url-literals` level.
pub fn set_lint_url_literals(level: Diagnose) {
    #[cfg(test)]
    assert_globals_exclusive("lint-url-literals");
    LINT_URL_LITERALS.store(level.as_u8(), Ordering::Relaxed);
}

/// The `lint-url-literals` level.
#[must_use]
pub fn lint_url_literals() -> Diagnose {
    #[cfg(test)]
    assert_globals_guarded("lint-url-literals");
    Diagnose::from_u8(LINT_URL_LITERALS.load(Ordering::Relaxed))
}

/// Tell the evaluator cppnix's `lint-short-path-literals` level.
pub fn set_lint_short_path_literals(level: Diagnose) {
    #[cfg(test)]
    assert_globals_exclusive("lint-short-path-literals");
    LINT_SHORT_PATH_LITERALS.store(level.as_u8(), Ordering::Relaxed);
}

/// The `lint-short-path-literals` level.
#[must_use]
pub fn lint_short_path_literals() -> Diagnose {
    #[cfg(test)]
    assert_globals_guarded("lint-short-path-literals");
    Diagnose::from_u8(LINT_SHORT_PATH_LITERALS.load(Ordering::Relaxed))
}

/// Tell the evaluator cppnix's `lint-absolute-path-literals` level.
pub fn set_lint_absolute_path_literals(level: Diagnose) {
    #[cfg(test)]
    assert_globals_exclusive("lint-absolute-path-literals");
    LINT_ABSOLUTE_PATH_LITERALS.store(level.as_u8(), Ordering::Relaxed);
}

/// The `lint-absolute-path-literals` level.
#[must_use]
pub fn lint_absolute_path_literals() -> Diagnose {
    #[cfg(test)]
    assert_globals_guarded("lint-absolute-path-literals");
    Diagnose::from_u8(LINT_ABSOLUTE_PATH_LITERALS.load(Ordering::Relaxed))
}

/// cppnix's `pipe-operators` experimental feature.
///
/// Value-deciding at parse time: with it off, `a |> f` is cppnix's
/// feature-is-disabled `ParseError` (`lexer.l:89-96`), and with it on the
/// same text is `f a` (`parser.y:287-295`). A backend that ignored it would
/// evaluate programs cppnix rejects.
static PIPE_OPERATORS: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tell the evaluator whether cppnix's `pipe-operators` feature is enabled.
pub fn set_pipe_operators(on: bool) {
    #[cfg(test)]
    assert_globals_exclusive("pipe-operators");
    PIPE_OPERATORS.store(on, Ordering::Relaxed);
}

/// Whether the `pipe-operators` feature is enabled.
#[must_use]
pub fn pipe_operators() -> bool {
    #[cfg(test)]
    assert_globals_guarded("pipe-operators");
    PIPE_OPERATORS.load(Ordering::Relaxed)
}

pub use crate::builtin_catalogue::BuiltinFeatures;

// Standalone evaluation enables implemented features; embedding hosts set their explicit policy.
static BUILTIN_FEATURES: AtomicU32 = AtomicU32::new(BuiltinFeatures::ALL.bits());

pub fn set_builtin_features(features: BuiltinFeatures) {
    #[cfg(test)]
    assert_globals_exclusive("builtin-features");
    BUILTIN_FEATURES.store(features.bits(), Ordering::Relaxed);
}

pub fn builtin_features() -> BuiltinFeatures {
    #[cfg(test)]
    assert_globals_guarded("builtin-features");
    let bits = BUILTIN_FEATURES.load(Ordering::Relaxed);
    BuiltinFeatures {
        flakes: bits & 1 != 0,
        fetch_tree: bits & 2 != 0,
        wasm: bits & 4 != 0,
    }
}

/// Everything outside the source text and the read set that decides what an
/// evaluation produces.
///
/// # Why this type exists rather than four `load()` calls
///
/// Each field below is process state the embedder sets before evaluating, and
/// each one changes answers: the store directory is hashed into every path
/// `builtins.derivationStrict` computes, the version string is what
/// `builtins.nixVersion` returns, the ceiling decides whether a recursion is
/// an error, and the access flag decides whether a read is answered or
/// refused. None of them was in the memo key, so one `eval-cache-dir` shared
/// between two stores served the first store's `outPath` to the second
/// (ENG-12541) -- a wrong 32-character hash that looks exactly like a right
/// one.
///
/// Gathering them into one struct is what makes the omission a compile error
/// instead of an oversight: [`Settings::fingerprint`] destructures every
/// field by name, so a new setting does not build until somebody has decided
/// whether it belongs in the key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    /// `ixe_set_store_dir`. `None` when the embedder never said, which makes
    /// `builtins.derivationStrict` refuse; that refusal is itself an answer
    /// worth keying on, so `None` and `Some` digest differently.
    pub store_dir: Option<String>,
    /// `ixe_set_nix_version`.
    pub nix_version: Option<String>,
    /// Immutable host artifact identity, independent of `nix_version`.
    pub host_build_identity: Option<String>,
    /// `ixe_set_current_system`, what `builtins.currentSystem` reports. The
    /// comment on `CURRENT_SYSTEM` calls this hole ENG-12541's, which it is,
    /// and this is where it closes.
    pub current_system: Option<String>,
    /// `ixe_set_max_call_depth`.
    pub max_call_depth: u32,
    /// `ixe_set_pure_eval`. Which host questions it forbids is
    /// [`crate::purity::verdict`]'s business; what matters here is that a
    /// result computed under one purity regime must never be served to
    /// another, so it is in the key.
    pub pure_eval: bool,
    /// `ixe_set_restrict_eval`. A second field rather than an `||` with
    /// `pure_eval`, because the two forbid different questions and two
    /// configurations that differ only in which one is on are two different
    /// evaluations.
    pub restrict_eval: bool,
    /// `ixe_set_builtin_features`. It decides which gated names `builtins`
    /// has and which bare globals resolve, so the same text under two of
    /// these is two different evaluations.
    pub builtin_features: BuiltinFeatures,
    /// Whether `readFile`, `pathExists`, `readDir`, `readFileType` and
    /// `import` reach the world through the embedder's accessor or through
    /// this crate's `std::fs`.
    ///
    /// Not a process setting and not settable on its own: it is read off the
    /// host vtable the session was created with (`ixe_session_new`), so two
    /// sessions in one process can answer differently and neither can move
    /// the other's.
    ///
    /// A capability rather than a setting, and in the key anyway, because
    /// under `pure-eval` or `restrict-eval` it changes the answer:
    /// [`crate::purity::verdict`] serves those five through an embedder and
    /// refuses them without one. A refusal is a memoisable result like any
    /// other, so leaving this out would let a witness recorded by a
    /// standalone embedding -- empty read set, result "unimplemented" -- be
    /// served to the `nix` binary, which would have read the file. ENG-12792.
    pub path_reads: crate::purity::PathReads,
    /// `ixe_set_trace_verbose`. Decides whether `builtins.traceVerbose`
    /// forces its first argument, so it decides whether a `throw` there is
    /// reached.
    pub trace_verbose: bool,
    /// `ixe_set_abort_on_warn`. Decides whether `builtins.warn` returns its
    /// second argument or kills the evaluation.
    pub abort_on_warn: bool,
    /// What `~/...` expands to. `None` when neither the embedder nor `$HOME`
    /// says, which makes a home path literal fail the way cppnix's
    /// `getHomeOf` fails rather than resolve to a guess.
    ///
    /// A path literal is resolved by the *compiler*, so this decides what a
    /// module compiles to and not merely what it evaluates to -- which is
    /// why it is in the fingerprint below and not read at the point of use.
    pub home_dir: Option<String>,
    /// `ixe_set_ca_derivations`: whether cppnix's `ca-derivations`
    /// experimental feature is enabled. Read where `derivationStrict` reads
    /// `__contentAddressed`, and in the key because the same derivation
    /// evaluates to a floating-CA `.drv` under one setting and to the
    /// feature-is-disabled error under the other.
    pub ca_derivations: bool,
    /// `ixe_set_allow_import_from_derivation`. In the key because a
    /// realisation the verifier serves by validity never reaches the host's
    /// `realiseContextCheck`, which is where the refusal lives.
    pub allow_import_from_derivation: bool,
    /// `ixe_set_allowed_uris`, newline-terminated entries, `None` when the
    /// embedder never said. In the key because a fetch served by validity
    /// never reaches `checkURI`.
    pub allowed_uris: Option<String>,
    /// `ixe_set_repair`: whether the evaluation runs under `--repair`. Not in
    /// the key: it turns the memo off (`ReadSet`'s lookup serves nothing),
    /// since a served derivation write would skip the rewrite repair is for.
    pub repair: bool,
    /// `ixe_set_blake3_hashes`: whether cppnix's `blake3-hashes`
    /// experimental feature is enabled. In the key because the same hash or
    /// derivation refuses under one setting and computes a value under the
    /// other.
    pub blake3_hashes: bool,
    /// `ixe_set_lint_url_literals`. Compile-time like `home_dir`: at `fatal`
    /// a URL literal is a compile error, so the level decides what a module
    /// compiles to. Only fatal-ness enters the fingerprint -- `warn` and
    /// `ignore` compile identically (the warning cppnix prints at `warn` is
    /// tier 2 text this backend does not print).
    pub lint_url_literals: Diagnose,
    /// `ixe_set_lint_short_path_literals`. See [`Self::lint_url_literals`].
    pub lint_short_path_literals: Diagnose,
    /// `ixe_set_lint_absolute_path_literals`, which also covers `~/x`
    /// literals, as cppnix's `HPATH` rule does. See
    /// [`Self::lint_url_literals`].
    pub lint_absolute_path_literals: Diagnose,
    /// `ixe_set_pipe_operators`: whether cppnix's `pipe-operators`
    /// experimental feature is enabled. In the key because `a |> f` is a
    /// compile error under one setting and `f a` under the other.
    pub pipe_operators: bool,
    /// `ixe_set_parse_toml_timestamps`: whether cppnix's
    /// `parse-toml-timestamps` experimental feature is enabled. In the key
    /// because the same `builtins.fromTOML` call evaluates to timestamp sets
    /// under one setting and to a parse error under the other.
    pub parse_toml_timestamps: bool,
}

/// The configuration a process has before any embedder has said anything:
/// every static in this module at its initial value.
///
/// This is what a `Vm` gets when nobody hands it settings, and it is why a
/// test needs neither a guard nor luck. Before ENG-12939 the evaluator read
/// the statics as it went, so a test's answer depended on whether some other
/// test happened to be holding `pure-eval` on at that moment; taking a value
/// here instead means the only way to evaluate under a non-default setting is
/// to say so.
///
/// `the_default_settings_are_the_statics_initial_values` holds the two in
/// step, so a static whose initialiser moves cannot leave this behind.
impl Default for Settings {
    fn default() -> Self {
        Self {
            store_dir: None,
            nix_version: None,
            host_build_identity: None,
            current_system: None,
            max_call_depth: crate::vm::DEFAULT_MAX_CALL_DEPTH,
            pure_eval: false,
            restrict_eval: false,
            builtin_features: BuiltinFeatures::default(),
            path_reads: crate::purity::PathReads::Direct,
            trace_verbose: false,
            abort_on_warn: false,
            home_dir: None,
            ca_derivations: false,
            allow_import_from_derivation: true,
            allowed_uris: None,
            repair: false,
            blake3_hashes: false,
            lint_url_literals: Diagnose::Ignore,
            lint_short_path_literals: Diagnose::Ignore,
            lint_absolute_path_literals: Diagnose::Ignore,
            pipe_operators: false,
            parse_toml_timestamps: false,
        }
    }
}

/// The configuration a test wants when the expression it evaluates needs a
/// store directory.
///
/// `/nix/store` because that is what cppnix used on the machines the golden
/// paths in `drvstrict` and `primops_host` were recorded on, so a path
/// recorded there reproduces here.
///
/// A value handed to a `Vm`, not a call to [`set_store_dir`]. That setter is
/// a `OnceLock`, so one test using it configured the whole process for the
/// rest of the binary and every test that ran before it saw `None` instead --
/// a difference in answers decided by scheduling order (ENG-12939).
#[cfg(test)]
#[must_use]
pub(crate) fn settings_with_store() -> Settings {
    Settings {
        store_dir: Some("/nix/store".to_owned()),
        ..Settings::default()
    }
}

/// Domain separation for the settings fingerprint.
const SETTINGS_TAG: &str = "ixe-eval-settings-v2";

impl Settings {
    /// What the process is configured to do right now.
    #[must_use]
    pub fn current() -> Self {
        Self {
            store_dir: store_dir().map(str::to_owned),
            nix_version: nix_version().map(str::to_owned),
            host_build_identity: host_build_identity().map(str::to_owned),
            current_system: current_system().map(str::to_owned),
            max_call_depth: max_call_depth(),
            pure_eval: pure_eval(),
            restrict_eval: restrict_eval(),
            builtin_features: builtin_features(),
            // Direct, always: whether reads go through an embedder is a
            // property of the host a session was handed, not of the process,
            // so the session overwrites this field from its own vtable. A
            // caller that reaches here without a session genuinely has no
            // embedder to read through.
            path_reads: crate::purity::PathReads::Direct,
            trace_verbose: trace_verbose(),
            abort_on_warn: abort_on_warn(),
            ca_derivations: ca_derivations(),
            allow_import_from_derivation: allow_import_from_derivation(),
            allowed_uris: allowed_uris(),
            repair: repair(),
            blake3_hashes: blake3_hashes(),
            lint_url_literals: lint_url_literals(),
            lint_short_path_literals: lint_short_path_literals(),
            lint_absolute_path_literals: lint_absolute_path_literals(),
            pipe_operators: pipe_operators(),
            parse_toml_timestamps: parse_toml_timestamps(),
            // `$HOME` is the fallback and not the answer: it is what
            // `getHome()` starts from, minus the ownership check and the
            // `passwd` fallback, and it exists only for an embedding that
            // never calls `ixe_set_home_dir` -- the probe, the examples, the
            // tests. The `nix` binary always says, so the divergence cannot
            // reach a real evaluation. Read here rather than in the compiler
            // so that whatever decided the path is in the memo key below.
            home_dir: home_dir()
                .map(str::to_owned)
                .or_else(|| std::env::var("HOME").ok().filter(|h| !h.is_empty())),
        }
    }

    /// The two purity settings as [`crate::purity`] wants them.
    ///
    /// A view of two fields rather than a third copy of them, so
    /// `Purity::current()` and this cannot come to disagree.
    #[must_use]
    pub fn purity(&self) -> crate::purity::Purity {
        crate::purity::Purity {
            pure_eval: self.pure_eval,
            restrict_eval: self.restrict_eval,
        }
    }

    /// A digest of every setting, for folding into a memo key.
    ///
    /// The destructure is the point: adding a field to [`Settings`] stops
    /// this compiling, so the next setting cannot reach the evaluator without
    /// somebody deciding where it goes.
    #[must_use]
    pub fn fingerprint(&self) -> ix_kernel::hash::Hash {
        let Self {
            store_dir,
            nix_version,
            host_build_identity,
            current_system,
            max_call_depth,
            pure_eval,
            restrict_eval,
            builtin_features,
            path_reads,
            trace_verbose,
            abort_on_warn,
            home_dir,
            ca_derivations,
            allow_import_from_derivation,
            allowed_uris,
            // Not a key part on purpose: repair disables the memo instead
            // (see the field). Two evaluations differing only in it share
            // their witness, and neither is served while it is on.
            repair: _,
            blake3_hashes,
            lint_url_literals,
            lint_short_path_literals,
            lint_absolute_path_literals,
            pipe_operators,
            parse_toml_timestamps,
        } = self;
        // Each optional field contributes a presence byte before its value,
        // so an unset setting and one set to the empty string are different
        // keys. They are different facts: `derivationStrict` refuses under
        // the first and computes a path under the second.
        let depth = max_call_depth.to_be_bytes();
        let mut parts: Vec<&[u8]> = Vec::new();
        let (store_tag, store_bytes) = tagged_option(store_dir);
        let (version_tag, version_bytes) = tagged_option(nix_version);
        let (host_tag, host_bytes) = tagged_option(host_build_identity);
        let (system_tag, system_bytes) = tagged_option(current_system);
        let feature_bytes = builtin_features.bits().to_be_bytes();
        let (home_tag, home_bytes) = tagged_option(home_dir);
        parts.push(store_tag);
        parts.push(store_bytes);
        parts.push(version_tag);
        parts.push(version_bytes);
        parts.push(host_tag);
        parts.push(host_bytes);
        parts.push(system_tag);
        parts.push(system_bytes);
        parts.push(&feature_bytes);
        parts.push(home_tag);
        parts.push(home_bytes);
        parts.push(&depth);
        // Two parts, not one: `pure-eval` alone and `restrict-eval` alone
        // forbid different questions, so they must address different rows.
        parts.push(if *pure_eval { b"pure-yes" } else { b"pure-no" });
        parts.push(if *restrict_eval {
            b"restrict-yes"
        } else {
            b"restrict-no"
        });
        // Only meaningful together with the two above -- with neither setting
        // on, both configurations serve every question -- but unconditional
        // here, because a fingerprint that folded a field in only sometimes
        // would be two key schemes sharing one tag.
        parts.push(match path_reads {
            crate::purity::PathReads::ThroughEmbedder => b"reads-bridged",
            crate::purity::PathReads::Direct => b"reads-direct",
        });
        // Both of these change what an expression evaluates *to*, not how it
        // is shown, so they go in the key rather than beside it. See the
        // statics for the one-line reason each.
        parts.push(if *trace_verbose { b"tv-yes" } else { b"tv-no" });
        parts.push(if *abort_on_warn {
            b"aow-yes"
        } else {
            b"aow-no"
        });
        // In the key for the reason on the field: the feature decides what
        // `__contentAddressed = true` evaluates to.
        parts.push(if *ca_derivations { b"ca-yes" } else { b"ca-no" });
        // Host policy the verifier would otherwise serve across (see the
        // fields): the IFD gate and the URI allow list.
        parts.push(if *allow_import_from_derivation {
            b"ifd-yes"
        } else {
            b"ifd-no"
        });
        let (uris_tag, uris_bytes) = tagged_option(allowed_uris);
        parts.push(uris_tag);
        parts.push(uris_bytes);
        // The feature decides whether a recognised hash algorithm is usable.
        parts.push(if *blake3_hashes {
            b"blake3-yes"
        } else {
            b"blake3-no"
        });
        // Fatal-ness only, on purpose: `warn` and `ignore` compile the same
        // module (the lint's warning is a stderr line, not a value), so
        // folding the full level would split cache rows between two
        // configurations that provably answer alike. The field comments say
        // the same.
        parts.push(if lint_url_literals.is_fatal() {
            b"lint-url-fatal"
        } else {
            b"lint-url-lax"
        });
        parts.push(if lint_short_path_literals.is_fatal() {
            b"lint-short-fatal"
        } else {
            b"lint-short-lax"
        });
        parts.push(if lint_absolute_path_literals.is_fatal() {
            b"lint-abs-fatal"
        } else {
            b"lint-abs-lax"
        });
        // The feature decides whether `|>` compiles at all.
        parts.push(if *pipe_operators {
            b"pipes-yes"
        } else {
            b"pipes-no"
        });
        // In the key for the reason on the field: the feature decides what
        // a TOML date evaluates to.
        parts.push(if *parse_toml_timestamps {
            b"toml-ts-yes"
        } else {
            b"toml-ts-no"
        });
        ix_kernel::hash::tagged(SETTINGS_TAG, &parts)
    }
}

/// `(presence marker, bytes)` for an optional setting, kept apart so `None`
/// cannot digest the same way as `Some("")`.
fn tagged_option(value: &Option<String>) -> (&'static [u8], &[u8]) {
    match value {
        Some(text) => (b"set", text.as_bytes()),
        None => (b"unset", b""),
    }
}

/// Serialises tests that move process-global evaluator state against tests
/// that need it to hold still.
///
/// Two kinds of state need this and they arrived for different reasons.
/// Folding the settings into the memo key (ENG-12541) made them part of what
/// a cached answer is filed under, which is the point -- and it means a test
/// moving `max-call-depth` while another sits between its two `evaluate`
/// calls turns that test's expected hit into a miss. Separately, the
/// interrupt hook is one slot for the whole process, so a test that clears it
/// on the way out disarms another test mid-run; that is how
/// `an_armed_interrupt_stops_a_long_pure_evaluation` started failing the day
/// a second interrupt test appeared.
///
/// Both are the machinery working correctly and the tests colliding, so the
/// lock belongs here rather than a `--test-threads=1` that would hide the
/// next one.
///
/// A read-write lock and not a mutex: readers do not exclude each other, so
/// only the handful of tests that move something give up their parallelism.
#[cfg(test)]
static SETTINGS_LOCK: std::sync::RwLock<()> = std::sync::RwLock::new(());

// How many guards this thread is holding.
//
// A thread-local and not a count on the lock, because the question
// `assert_globals_guarded` asks is "did *this* test take a guard", and an
// `RwLock` cannot be asked that: a read guard taken by some other test's
// thread makes the lock look held to everybody. The crate spawns no threads,
// and `libtest` gives each test its own, so thread identity is test identity
// here.
//
// A depth rather than a flag because the guards nest: a test holding
// `globals_shared` across a helper that takes its own must not be reported as
// unguarded when the inner one drops.
#[cfg(test)]
thread_local! {
    static GUARD_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

// Whether this thread holds the write guard. Separate from `GUARD_DEPTH`
// because the question a setter asks is not "is anything held" but "is this
// exclusive" -- and a read guard answers the first and not the second.
#[cfg(test)]
thread_local! {
    static EXCLUSIVE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn guard_depth_inc() {
    GUARD_DEPTH.with(|d| d.set(d.get().saturating_add(1)));
}

#[cfg(test)]
fn guard_depth_dec() {
    GUARD_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
}

/// Refuse a read of process-global evaluator state from a test that has not
/// taken a guard.
///
/// This is the whole of ENG-12939's fix, and the reason it is an assertion
/// rather than a review rule: a test that reads a global without a guard is
/// correct in isolation and wrong only under a particular interleaving, so it
/// passes on the machine that wrote it and fails about once in thirty runs
/// somewhere else. Guarding the tests that had been *seen* to fail did not
/// converge -- each change to the set of tests reshuffles the schedule and the
/// failure relocates to a pair nobody had guarded yet.
///
/// Turning the read itself into the failure converges, because it does not
/// depend on the schedule: the offending test fails every run, by name, on
/// the line that reached the global.
///
/// The intended answer is almost never to add a guard. It is to stop reading
/// the global: build a [`Settings`] and hand it to [`crate::vm::Vm::with_settings`],
/// so the test's behaviour does not depend on what another test is doing.
/// A guard is for the tests whose subject *is* the process-global surface --
/// the C ABI setters, and the hooks that have nowhere else to live.
/// Refuse a read of process-global evaluator state from a test that has not
/// taken a guard.
///
/// This is what makes ENG-12939 stay fixed, and the reason it is an assertion
/// rather than a review rule: a test that reads a global without a guard is
/// correct in isolation and wrong only under a particular interleaving, so it
/// passes on the machine that wrote it and fails about once in thirty runs
/// somewhere else. Guarding the tests that had been *seen* to fail did not
/// converge -- each change to the set of tests reshuffles the schedule and the
/// failure relocates to a pair nobody had guarded yet. Turning the read itself
/// into the failure does converge, because it does not depend on the schedule:
/// the offending test fails every run, by name, on the line that reached the
/// global.
///
/// **The intended answer is almost never to add a guard.** It is to stop
/// reading the global: build a [`Settings`] and hand it to
/// [`crate::vm::Vm::with_settings`], or evaluate through [`eval_str_with`], so
/// the test's behaviour does not depend on what another test is doing. A guard
/// is for the tests whose subject *is* the process-global surface, which today
/// means the C ABI setters in `capi`.
///
/// # Reached through `extern "C"`, this aborts rather than fails
///
/// Rust cannot unwind across the C ABI, so a `capi` test that forgets the
/// guard ends the whole binary with `fatal runtime error: failed to initiate
/// panic` instead of a tidy one-test failure. The explanation is written to
/// stderr before the assertion for exactly that case: the abort keeps the text
/// and the name of the test that was running, which is enough to act on.
#[cfg(test)]
pub(crate) fn assert_globals_guarded(what: &str) {
    if GUARD_DEPTH.with(std::cell::Cell::get) == 0 {
        eprintln!(
            "\nENG-12939: this test read the process global `{what}` without holding a \
             guard, so another test moving it mid-run would change this one's answer.\n\
             \n\
             Prefer removing the read: build a `crate::eval::Settings` and pass it to \
             `Vm::with_settings`, or evaluate with `eval::eval_str_with`. Neither \
             touches process state, so neither can race.\n\
             \n\
             If the global really is the subject of the test -- which for now means \
             the C ABI setters -- open it with \
             `let _globals = crate::eval::globals_shared();`, or `globals_moving()` if \
             the test moves one.\n"
        );
        assert!(
            GUARD_DEPTH.with(std::cell::Cell::get) > 0,
            "read of process global `{what}` without a guard (ENG-12939)"
        );
    }
}

/// Refuse a *write* to process-global evaluator state from a test that is
/// not holding the globals exclusively.
///
/// The other half of [`assert_globals_guarded`], and the half whose absence
/// this crate has already paid for twice. A reader without a guard was
/// ENG-12939. A writer holding only a *read* guard is ENG-12904, and it is
/// worse in one way: the read guard makes the test look careful, and readers
/// do not exclude readers, so the setter still lands in the middle of a
/// neighbour. Four `capi::warm_starts` tests were in exactly that state and
/// nothing said so, because the reader assertion is satisfied by a read guard.
///
/// `set_store_dir` and friends are `OnceLock`s, so the transition happens once
/// per process and the window is small -- which is why it surfaces as a
/// one-in-many failure reading "the cache did not serve" rather than as
/// anything that names a setting. ENG-12830 lost an hour to that shape.
#[cfg(test)]
pub(crate) fn assert_globals_exclusive(what: &str) {
    if !EXCLUSIVE.with(std::cell::Cell::get) {
        eprintln!(
            "\nENG-12904: this test wrote the process global `{what}` without holding \
             the globals exclusively.\n\
             \n\
             A read guard is not enough: `globals_shared()` does not exclude other \
             readers, so the write still lands in the middle of a neighbour's run. \
             Use `let _globals = crate::eval::globals_moving();`.\n\
             \n\
             Better, where the setting is not the subject of the test: do not write \
             it at all. Build a `crate::eval::Settings` and pass it to \
             `Vm::with_settings`, which needs no process state.\n"
        );
        assert!(
            EXCLUSIVE.with(std::cell::Cell::get),
            "write to process global `{what}` without exclusive access (ENG-12904)"
        );
    }
}

/// Share the process globals with other readers for the length of a test:
/// no test may *move* one while this is held, and any number may read.
///
/// # The name says shared because it is, and the old one did not
///
/// This was `globals_held`, which reads as "I hold the globals" and grants
/// nothing of the sort -- it is a read guard, so every test taking it holds
/// it simultaneously. Two tests that each registered into and cleared one
/// process-global map therefore interleaved under it, and the second one
/// written passed alone and failed beside the first, with an error that
/// looked exactly like the bug it had been written to catch (ENG-13094).
///
/// A guard whose name overstates what it grants is worse than no guard: it
/// makes the careless call site look careful, so nobody re-reads it. If you
/// need to exclude other readers, that is [`globals_moving`]. If the state
/// you are protecting is not the settings statics at all, it needs a guard of
/// its own, because this one has never covered anything but
/// [`SETTINGS_LOCK`]. (The virtual-file registry once needed one; it is a
/// per-host value now and needs none.)
#[cfg(test)]
pub(crate) fn globals_shared() -> GlobalsGuard {
    let inner = SETTINGS_LOCK
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard_depth_inc();
    GlobalsGuard::Read(inner)
}

/// Take exclusive use of the process globals, for a test that moves one.
#[cfg(test)]
pub(crate) fn globals_moving() -> GlobalsGuard {
    let inner = SETTINGS_LOCK
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    guard_depth_inc();
    EXCLUSIVE.with(|e| e.set(true));
    GlobalsGuard::Write(inner)
}

/// A held [`SETTINGS_LOCK`], and this thread's record that it is held.
///
/// One type for both modes so the two constructors can share the depth
/// bookkeeping; the distinction that matters to a caller is which constructor
/// it called, not what it gets back.
#[cfg(test)]
pub(crate) enum GlobalsGuard {
    // Held for the `Drop`, never read: what a caller wants from these is the
    // exclusion and the depth bookkeeping, not the guard object.
    Read(#[allow(dead_code)] std::sync::RwLockReadGuard<'static, ()>),
    Write(#[allow(dead_code)] std::sync::RwLockWriteGuard<'static, ()>),
}

#[cfg(test)]
impl Drop for GlobalsGuard {
    fn drop(&mut self) {
        if matches!(self, GlobalsGuard::Write(_)) {
            EXCLUSIVE.with(|e| e.set(false));
        }
        guard_depth_dec();
    }
}

/// A store path as the string a Nix program sees: the path, carrying itself
/// as its own `Opaque` context element.
///
/// cppnix's `allowAndSetStorePathString`, and the shape every store question's
/// answer takes -- the store path *is* the dependency the string carries
/// (`eval.cc:2660`). One function because three answers need it and three
/// copies is how one of them ends up with no context at all.
pub(crate) fn store_path_string(store_path: String) -> Value {
    let path: std::rc::Rc<str> = store_path.into();
    let mut context = std::collections::BTreeSet::new();
    context.insert(crate::value2::ContextElem::Opaque(std::rc::Rc::clone(
        &path,
    )));
    Value::Str(crate::value2::NixStr::with_context(path, context))
}

fn store_path_reference_string(answer: crate::host::StorePathResult) -> Value {
    let mut context = std::collections::BTreeSet::new();
    context.insert(crate::value2::ContextElem::Opaque(answer.store_path.into()));
    Value::Str(crate::value2::NixStr::with_context(
        answer.path.into_bytes(),
        context,
    ))
}

/// The `FetchTree` answer: cppnix's `emitTreeAttrs` set, decoded.
///
/// JSON carries every attribute except the one that matters most. `outPath`
/// is a store path and cppnix gives it its own path as string context
/// (`mkStorePathString`), which is what makes a derivation that reads
/// `(fetchTree ...).outPath` depend on the tree. A plain JSON string would
/// lose that silently -- the value prints identically and the derivation ends
/// up with one fewer input -- so it is rebuilt here rather than trusted from
/// the wire.
/// Turn a store answer into a value, mapping every [`StoreError`] class
/// the way every store-backed question maps them.
///
/// One function rather than the arm each question used to spell out, because
/// there are now two callers per question: [`answer_path`], which asked the
/// host and blocked, and the scheduler's collect, which asked the host to
/// begin and came back later. Two copies of a `NoStore` message that has to
/// name the right builtin is exactly the kind of thing that drifts.
fn store_answer<T>(
    answer: Result<T, StoreError>,
    no_store: &str,
    ok: impl FnOnce(T) -> Result<Value, VmError>,
) -> Result<Value, VmError> {
    match answer {
        Ok(payload) => ok(payload),
        Err(StoreError::Failed(message)) => Err(VmError::eval(message)),
        Err(StoreError::ImportFromDerivation(message)) => {
            Err(VmError::import_from_derivation(message))
        }
        // A gap in this backend, not a fault in the program, so it is
        // unimplemented rather than an evaluation error -- reported as a Nix
        // error it would score a mismatch against a cpp arm that answers
        // fine.
        Err(StoreError::Unsupported(message)) => Err(VmError::Unimplemented(Refusal::new(
            RefusalToken::UnimplementedBuiltin,
            message,
        ))),
        Err(StoreError::NoStore) => Err(VmError::Unimplemented(Refusal::new(
            RefusalToken::StoreUnavailable,
            no_store.to_owned(),
        ))),
    }
}

/// The refusal a store-less evaluator gives `builtins.<who>`. The questions
/// that are not a builtin, or that miss something other than a store, spell
/// their own sentence at the call.
fn no_store(who: &str) -> String {
    format!("builtins.{who} (no store behind this evaluator)")
}

/// [`NeedPath::Flake`]'s answer as a value, whichever route asked.
///
/// Standalone: no embedder, so no `lockFlake`. Locking is not something this
/// crate can stand in for -- it would be inventing lock-file data, exactly as
/// it would for a fetch -- so the refusal names the locker, not a store.
fn flake_answer(
    vm: &mut Vm,
    answer: Result<crate::host::FlakeCall, StoreError>,
) -> Result<Value, VmError> {
    store_answer(
        answer,
        "builtins.getFlake (no flake locking behind this evaluator)",
        |call| Ok(flake_attrs(vm, call)),
    )
}

/// [`NeedPath::Fetch`]'s answer as a value.
fn fetch_answer(answer: Result<String, StoreError>, who: &str) -> Result<Value, VmError> {
    store_answer(answer, &no_store(who), |store_path| {
        Ok(store_path_string(store_path))
    })
}

/// [`NeedPath::FetchTree`]'s answer as a value.
fn tree_answer(
    vm: &mut Vm,
    answer: Result<String, StoreError>,
    who: &str,
) -> Result<Value, VmError> {
    match answer {
        Ok(json) => tree_attrs(vm, &json),
        other => store_answer(other, &no_store(who), |_| {
            Err(VmError::eval("internal: unreachable store answer"))
        }),
    }
}

/// [`NeedPath::Flake`]'s answer as a value: the three documents
/// `call-flake.nix` needs, in an attribute set.
fn flake_attrs(vm: &mut Vm, call: crate::host::FlakeCall) -> Value {
    let mut m = BTreeMap::new();
    let k = vm.intern("source");
    m.insert(k, Slot::value(Value::Str(call.source.into())));
    let k = vm.intern("lockFile");
    m.insert(k, Slot::value(Value::Str(call.lock_file.into())));
    let k = vm.intern("overrides");
    m.insert(k, Slot::value(Value::Str(call.overrides.into())));
    Value::Attrs(Rc::new(Attrs::new(m)))
}

/// [`NeedPath::ParseFlakeRef`]'s answer as a value: the flat JSON object the
/// embedder built with `fetchers::attrsToJSON`, as the attribute set
/// `prim_parseFlakeRef` builds. Only the three `fetchers::Attr` shapes can
/// appear; anything else is a malformed answer, not an attribute to guess
/// at. An unsigned integer becomes `Value::Int` through the same implicit
/// narrowing cppnix's `mkInt(uint64_t)` performs.
fn flake_ref_attrs(vm: &mut Vm, json: &str) -> Result<Value, VmError> {
    let doc: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| VmError::eval(format!("the embedder's flake ref answer is not JSON: {e}")))?;
    let Some(obj) = doc.as_object() else {
        return Err(VmError::eval(
            "the embedder's flake ref answer is not a JSON object",
        ));
    };
    let mut m = BTreeMap::new();
    for (name, field) in obj {
        let value = match field {
            serde_json::Value::String(s) => Value::Str(s.as_str().into()),
            serde_json::Value::Bool(b) => Value::Bool(*b),
            #[allow(clippy::cast_possible_wrap)]
            serde_json::Value::Number(n) => {
                match n.as_i64().or_else(|| n.as_u64().map(|u| u as i64)) {
                    Some(i) => Value::Int(i),
                    None => {
                        return Err(VmError::eval(format!(
                            "the embedder's flake ref answer has a non-integer number in '{name}'"
                        )));
                    }
                }
            }
            _ => {
                return Err(VmError::eval(format!(
                    "the embedder's flake ref answer has a non-scalar field '{name}'"
                )));
            }
        };
        let sym = vm.intern(name);
        m.insert(sym, Slot::value(value));
    }
    Ok(Value::Attrs(Rc::new(Attrs::new(m))))
}

/// How a `Realise` question names itself in an error, and the one place that
/// spelling lives.
///
/// Captured before the question is begun, because by the time an
/// asynchronous answer comes back the context slice is gone.
fn realise_who(context: &[crate::value2::ContextElem]) -> String {
    context
        .iter()
        .map(crate::value2::ContextElem::display)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Turn what [`Host::realise`] answered into a value, whichever route asked.
///
/// Shared by `answer_path` and `collect_one` for the reason `store_answer` is:
/// two copies of "what a failed build means" would be two things to keep in
/// step, and the difference between them would be a Tier 1 difference --
/// `Failed` is an uncatchable `VmError::eval` here rather than a `throw`,
/// which is cppnix's behaviour and not a default (see the `NeedPath::Realise`
/// arm's own note about `prim_tryEval`).
fn realise_answer(
    answer: Result<std::collections::BTreeMap<String, String>, StoreError>,
    who: &str,
) -> Result<Value, VmError> {
    // Nothing to build with, so the path the read is about was never going to
    // exist. Refusing by name is the honest answer; reading anyway would
    // report "no such file" for a program cppnix runs.
    let no_store =
        format!("import from derivation ({who}: no store behind this evaluator to build it with)");
    store_answer(answer, &no_store, |rewrites| {
        let mut items: Vec<Slot> = Vec::with_capacity(rewrites.len() * 2);
        for (from, to) in rewrites {
            items.push(Slot::value(Value::Str(from.as_str().into())));
            items.push(Slot::value(Value::Str(to.as_str().into())));
        }
        Ok(Value::list(items))
    })
}

fn tree_attrs(vm: &mut Vm, json: &str) -> Result<Value, VmError> {
    let doc: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| VmError::eval(format!("while decoding a fetched tree: {e}")))?;
    let Value::Attrs(attrs) = crate::primops_pure::json_to_value(vm, &doc)? else {
        return Err(VmError::eval(
            "internal: a fetched tree did not answer with an attribute set",
        ));
    };
    let key = vm.intern("outPath");
    let mut out = (*attrs).clone();
    let Some(slot) = out.get(&key) else {
        return Err(VmError::eval(
            "internal: a fetched tree answered without an outPath",
        ));
    };
    let path = match slot.peek() {
        Some(Value::Str(s)) => crate::primops_pure::text_of(&s)?.to_owned(),
        _ => {
            return Err(VmError::eval(
                "internal: a fetched tree's outPath is not a string",
            ));
        }
    };
    out.insert(key, Slot::value(store_path_string(path)));
    // `out` is a clone of the decoded set, so it keeps that set's origin --
    // which is `None`: it came off the wire, not out of anyone's source.
    Ok(Value::Attrs(Rc::new(out)))
}

/// The access check every question passes before it can reach a host.
///
/// `Ok(None)` means ask; `Ok(Some(value))` means the check answered it
/// without asking; `Err` means the check refused it.
///
/// Factored out of [`answer_path`] because the scheduler has a second way to
/// reach a host -- [`Host::begin`], for the slow questions -- and that path
/// must pass the same check. It is a pure function of the settings and the
/// question, so running it on both paths costs nothing and cannot drift.
/// Skipping it on one of them is ENG-12543 exactly: a read that
/// `restrict-eval` exists to prevent, performed because a new route to the
/// host did not go past the gate.
fn access_check(vm: &Vm, need: &NeedPath) -> Result<Option<Value>, VmError> {
    // The one place this crate asks the outside world anything, which is what
    // makes one check here complete rather than merely thorough. What each
    // purity setting says about each question, and the cppnix line every row
    // was read off, is `crate::purity`; this only carries out the verdict.
    match crate::purity::verdict(need, vm.settings().purity(), vm.settings().path_reads) {
        crate::purity::Verdict::Ask => {}
        crate::purity::Verdict::EmptyString => {
            return Ok(Some(Value::Str(String::new().into())));
        }
        // cppnix raises this too, for the same program, so it unwinds as an
        // ordinary evaluation error rather than a refusal. Uncatchable by
        // `builtins.tryEval`, matching cppnix: it raises an `EvalError`, and
        // `prim_tryEval` catches `AssertionError` only (`primops.cc:1219`).
        crate::purity::Verdict::Error(message) => return Err(VmError::eval(message)),
        crate::purity::Verdict::Refuse => {
            let purity = vm.settings().purity();
            let what = match need {
                NeedPath::Import(p) => format!("importing '{p}'"),
                NeedPath::Flake(r) => format!("locking the flake '{r}'"),
                NeedPath::Contents(p) => format!("reading '{p}'"),
                NeedPath::HashFile { path, .. } => format!("hashing '{path}'"),
                NeedPath::Exists(p) | NeedPath::DirExists(p) => {
                    format!("testing whether '{p}' exists")
                }
                NeedPath::Entries(p) => format!("listing '{p}'"),
                NeedPath::Kind(p) | NeedPath::MaybeKind(p) => format!("asking what '{p}' is"),
                // Unreachable: `purity::verdict` refuses only the questions
                // named above, and only when nothing routes them through an
                // embedder. A new one cannot be added without failing its
                // policy test -- which is not hypothetical: `MaybeKind` was
                // added by ENG-13123 and the policy test failed until it got
                // a row.
                // Named rather than a wildcard so that if one ever is, this
                // arm has to be written rather than silently producing the
                // wrong noun.
                other => crate::purity::question_kind(other).to_owned(),
            };
            return Err(VmError::Unimplemented(Refusal::new(
                RefusalToken::AccessControl,
                format!(
                    "{what} under {} (this evaluator has no embedder to read through, \
                     so it would read the filesystem with std::fs rather than through \
                     cppnix's rootFS and could not tell an allowed path from a \
                     forbidden one; ENG-12792)",
                    purity.names()
                ),
            )));
        }
    }
    Ok(None)
}

fn answer_path(
    vm: &mut Vm,
    host: &dyn Host,
    need: &NeedPath,
    memo: &mut JobMemo,
) -> Result<Value, VmError> {
    if let Some(answered) = access_check(vm, need)? {
        return Ok(answered);
    }
    // Outputs rather than questions, and answered here rather than by a
    // host-shaped question so the two cannot drift: `purity::verdict` already
    // said `Ask` for both under every setting, because refusing a warning or
    // a trace would let a purity setting change what a program prints.
    if let NeedPath::Warn(message) = need {
        host.warn(message);
        return Ok(Value::Null);
    }
    if let NeedPath::Trace(message) = need {
        host.trace(message);
        return Ok(Value::Null);
    }
    match need {
        NeedPath::Import(p) => {
            if let Some(hit) = memo.imports.get(p) {
                crate::perf::note_import_hit();
                return Ok(Value::Attrs(Rc::clone(hit)));
            }
            let imported = host.import_source(p).map_err(VmError::eval)?;
            let (resolved, text) = match imported {
                crate::host::ImportedSource::Nix { path, text } => (path, text),
                crate::host::ImportedSource::Derivation(drv) => {
                    let argument = crate::imported_drv::argument(vm, &drv);
                    let key = vm.intern("derivation");
                    let attrs = Rc::new(Attrs::new(BTreeMap::from([(key, Slot::value(argument))])));
                    memo.imports.insert(Rc::clone(p), Rc::clone(&attrs));
                    return Ok(Value::Attrs(attrs));
                }
            };
            // Both halves in one answer: the VM needs the resolved path to
            // give the imported file its own base directory for relative
            // paths, and asking twice would let the two disagree.
            let mut m = BTreeMap::new();
            let k = vm.intern("path");
            m.insert(k, Slot::value(Value::Str(resolved.to_string().into())));
            let k = vm.intern("root");
            m.insert(k, Slot::value(Value::Str(resolved.root.wire_name().into())));
            let k = vm.intern("text");
            m.insert(k, Slot::value(Value::Str(text.into())));
            let attrs = Rc::new(Attrs::new(m));
            memo.imports.insert(Rc::clone(p), Rc::clone(&attrs));
            Ok(Value::Attrs(attrs))
        }
        // Raw bytes, as cppnix's `readFile` answers: a Nix string is a byte
        // string, and `builtins.wasm` reads its module through this question.
        // `Host::read_file` (text) is now the import's reader alone.
        NeedPath::Contents(p) => Ok(Value::Str(crate::value2::NixStr::from(
            host.read_file_bytes(p).map_err(VmError::eval)?.as_slice(),
        ))),
        // The digest is computed here, from the raw bytes, so no string ever
        // carries the contents; see the variant's own comment (ENG-13146).
        NeedPath::HashFile { path, algo } => {
            let bytes = host.read_file_bytes(path).map_err(VmError::eval)?;
            Ok(Value::Str(
                crate::primops_pure::hash_hex(*algo, &bytes).into(),
            ))
        }
        // Three strings in one answer, for the reason `Import` hands back two:
        // the machine needs all of them together and asking three times would
        // let a re-lock in between hand it a lock file and an overrides
        // document from two different locks.
        NeedPath::Flake(flake_ref) => flake_answer(vm, host.lock_flake(flake_ref)),
        // The exploded form comes back as JSON -- string, integer and
        // Boolean fields, the three shapes `fetchers::Attr` holds -- and
        // becomes the attribute set `prim_parseFlakeRef` builds from
        // `toAttrs`. Standalone hosts refuse for the reason `Flake`'s do:
        // the grammar is the embedder's, and this crate parsing flake
        // references itself would be a second parser to drift.
        NeedPath::ParseFlakeRef(flake_ref) => store_answer(
            host.parse_flake_ref(flake_ref),
            "builtins.parseFlakeRef (no flake-ref grammar behind this evaluator)",
            |json| flake_ref_attrs(vm, &json),
        ),
        // The answer is the reference string, carrying nothing: a flake ref
        // names a source, it does not depend on one, so no context.
        NeedPath::FlakeRefToString(attrs) => store_answer(
            host.flake_ref_to_string(attrs),
            "builtins.flakeRefToString (no flake-ref grammar behind this evaluator)",
            |text| Ok(Value::Str(text.as_str().into())),
        ),
        // cppnix renders an unset variable as the empty string rather than
        // failing, and the corpus compares the rendering.
        NeedPath::Env(name) => Ok(Value::Str(
            host.get_env(name).unwrap_or_default().as_str().into(),
        )),
        // A path inside a string is the store path cppnix would copy it to,
        // not the source path (ENG-12447). A host with no store behind it
        // says so rather than answering with something wrong.
        // The store path IS the dependency, so the string carries it: cppnix's
        // copyPathToStore inserts an Opaque element for exactly the path it
        // returns (eval.cc:2660).
        NeedPath::StorePath(p) => {
            if let Some(copied) = memo.copies.get(p) {
                crate::perf::note_copy_hit();
                return Ok(store_path_string(copied.clone()));
            }
            let answer = host.copy_to_store(p);
            // A failure is deliberately not memoised, as a directory read's
            // is not: the next asker should get the error afresh.
            if let Ok(store_path) = &answer {
                memo.copies.insert(Rc::clone(p), store_path.clone());
            }
            store_answer(
                answer,
                "interpolating a path into a string (no store behind this evaluator)",
                |store_path| Ok(store_path_string(store_path)),
            )
        }
        NeedPath::UseStorePath(p) => store_answer(
            host.store_path(p),
            "builtins.storePath (no store behind this evaluator)",
            |answer| Ok(store_path_reference_string(answer)),
        ),
        // `builtins.toFile`. The result carries the new path and *only* that:
        // cppnix says so in its own comment ("we don't need to add `context`
        // to the context of the result, since `storePath` itself has
        // references to the paths used in args[1]"), so propagating the
        // argument's context here would give the string more dependencies
        // than cpp gives it.
        NeedPath::StoreText {
            name,
            contents,
            references,
        } => store_answer(
            host.store_text(name, contents, references),
            "builtins.toFile (no store behind this evaluator)",
            |store_path| Ok(store_path_string(store_path)),
        ),
        // `builtins.derivationStrict`. The answer is checked against the path
        // the evaluator computed and then discarded, so it carries no
        // context: the `drvPath` the expression sees is built by the task out
        // of its own computation, and taking it from here instead would make
        // the value depend on whether anybody wrote the file.
        NeedPath::WriteDrv {
            name,
            aterm,
            expected,
        } => match host.write_derivation(name, aterm) {
            Ok(written) => Ok(Value::Str(crate::value2::NixStr::from(written.as_str()))),
            Err(StoreError::Failed(message)) => Err(VmError::eval(message)),
            Err(StoreError::ImportFromDerivation(message)) => {
                Err(VmError::import_from_derivation(message))
            }
            Err(StoreError::Unsupported(message)) => Err(VmError::Unimplemented(Refusal::new(
                RefusalToken::UnimplementedBuiltin,
                message,
            ))),
            // Not a refusal, unlike every other store question: the path was
            // computed from these bytes before the question was asked, so
            // nothing about the value depends on the answer. `Null` says
            // "nobody wrote it", which is cppnix under `readOnlyMode`, and
            // the task carries on with the path it already has.
            // `NeedPath::WriteDrv` is where the difference is argued.
            Err(StoreError::NoStore) => {
                let _ = expected;
                Ok(Value::Null)
            }
        },
        // `builtins.path`. The context is the copied path and only that, as
        // for `StorePath`: the result is a fresh store object, and whatever
        // the source path's own coercion carried is not a dependency of it.
        NeedPath::StoreFiltered(request) => store_answer(
            host.store_filtered(request),
            // Named for the question and not for a builtin: `builtins.path`
            // and `builtins.filterSource` both reach this one, and a message
            // naming either sends half the callers to the wrong line. The
            // alternative -- carrying the caller's name in `FilteredCopy` --
            // would put it in the read-set key, so the same copy would key
            // differently depending on which spelling asked for it.
            "a filtered copy into the store (no store behind this evaluator)",
            |store_path| Ok(store_path_string(store_path)),
        ),
        // The fixed-output fetchers. The context is the fetched path and only
        // that: cppnix's `allowAndSetStorePathString` puts exactly the path it
        // returns in, and a fetch has no other dependency -- the URL is not
        // one, which is why a fetched tarball can be substituted from a cache
        // that never saw it.
        // The tree fetchers. The answer is JSON because the attribute set
        // cppnix builds has no fixed shape -- which attributes appear depends
        // on the input type and on what the fetcher found.
        NeedPath::FetchTree(request) => {
            tree_answer(vm, host.fetch_tree(request), request.fetcher.as_str())
        }
        NeedPath::Fetch(request) => fetch_answer(host.fetch(request), request.kind.who()),
        // Nothing to hand back: the builtin wanted the path present, not a
        // value. `Null` rather than a bool, so a caller cannot read a
        // meaningful answer out of something that has none.
        NeedPath::EnsurePath(p) => store_answer(
            host.ensure_path(p),
            "builtins.appendContext (no store behind this evaluator)",
            |()| Ok(Value::Null),
        ),
        // Import from derivation. The answer is the rewrite map
        // `realiseContext` returns, flattened to `[from, to, from, to, ...]`.
        // A list of strings and not an attribute set, so that a placeholder --
        // an arbitrary 32-character hash -- never becomes an interned symbol
        // that outlives the evaluation asking about it. Flat and not nested
        // because the one reader, `primops_pure::apply_rewrites`, walks it in
        // `chunks_exact(2)`; the shape is internal to this pair and never
        // reaches a Nix program.
        //
        // A build that fails is `VmError::eval`, which is uncatchable, and
        // that is cppnix's behaviour rather than a default. Every way
        // `realiseContext` fails raises something `prim_tryEval` does not
        // catch: it catches `AssertionError` alone (`primops.cc:1219`), while
        // an invalid path is an `InvalidPathError` (an `EvalError`), the
        // disabled-IFD refusal is an `IFDError`, and a failed build is a
        // `BuildError` from `buildPaths`. Measured on nix
        // 2.34.7+ix.h24085346: `(builtins.tryEval (import failing.drv)).success`
        // does not return false, it aborts.
        NeedPath::Realise(context) => {
            if let Some(rewrites) = memo.realised.get(context) {
                crate::perf::note_realise_hit();
                return realise_answer(Ok(rewrites.clone()), &realise_who(context));
            }
            let answer = host.realise(context);
            remember_realised(memo, context.clone(), &answer);
            realise_answer(answer, &realise_who(context))
        }
        // Answered above, before the access check.
        NeedPath::Warn(_) | NeedPath::Trace(_) => Ok(Value::Null),
        // cppnix's `prim_findFile` returns a path value (`primops.cc:2293`),
        // not a string, and the difference is visible: `builtins.typeOf
        // <nixpkgs>` is "path", and interpolating one copies it to the store.
        NeedPath::FindFile { entries, name } => match host.find_file(entries, name) {
            Ok(path) => Ok(Value::Path(Rc::new(path))),
            Err(LookupError::NotFound(message)) => Err(VmError::thrown(message)),
            Err(LookupError::Failed(message)) => Err(VmError::eval(message)),
            Err(LookupError::Unsupported(message)) => Err(VmError::Unimplemented(Refusal::new(
                RefusalToken::SearchPath,
                message,
            ))),
            Err(LookupError::NoResolver) => Err(VmError::Unimplemented(Refusal::new(
                RefusalToken::SearchPath,
                format!("looking up '<{name}>' (no search path behind this evaluator)"),
            ))),
        },
        NeedPath::NixPath => match host.nix_path() {
            Ok(entries) => {
                let path_key = vm.intern("path");
                let prefix_key = vm.intern("prefix");
                let items: Vec<Slot> = entries
                    .into_iter()
                    .map(|e| {
                        // Both attributes always present and both strings,
                        // as cppnix builds them (`primops.cc:5565`); an entry
                        // with no prefix carries an empty one rather than
                        // dropping the attribute, which `attrNames` can see.
                        let mut m = BTreeMap::new();
                        m.insert(path_key, Slot::value(Value::Str(e.path.into())));
                        m.insert(prefix_key, Slot::value(Value::Str(e.prefix.into())));
                        Slot::value(Value::Attrs(Rc::new(Attrs::new(m))))
                    })
                    .collect();
                Ok(Value::list(items))
            }
            Err(LookupError::NotFound(message) | LookupError::Failed(message)) => {
                Err(VmError::eval(message))
            }
            Err(LookupError::Unsupported(message)) => Err(VmError::Unimplemented(Refusal::new(
                RefusalToken::SearchPath,
                message,
            ))),
            Err(LookupError::NoResolver) => Err(VmError::Unimplemented(Refusal::new(
                RefusalToken::SearchPath,
                "builtins.nixPath (no search path behind this evaluator)",
            ))),
        },
        NeedPath::Exists(p) => Ok(Value::Bool(
            host.path_exists_checked(p).map_err(VmError::eval)?,
        )),
        // The trailing-slash half of `pathExists`: full resolution, then a
        // directory test. Its dedicated existence question keeps a vanished
        // mount or I/O failure as an error while cppnix's narrow
        // `RestrictedPathError` catch remains `false`.
        NeedPath::DirExists(p) => Ok(Value::Bool(
            host.dir_exists_checked(p).map_err(VmError::eval)?,
        )),
        // cppnix's `SourceAccessor::lstat`, spelled out: `maybeLstat` and
        // then `throw FileNotFound("path '%s' does not exist")` on nullopt
        // (`source-accessor.cc:73`). The throw is here rather than in the
        // embedder so that the other caller of the same read can decline it;
        // see [`NeedPath::MaybeKind`].
        NeedPath::Kind(p) => match host.file_type(p).map_err(VmError::eval)? {
            Some(kind) => Ok(Value::Str(kind.as_str().into())),
            None => Err(VmError::eval(format!("path '{p}' does not exist"))),
        },
        NeedPath::MaybeKind(p) => Ok(match host.file_type(p).map_err(VmError::eval)? {
            Some(kind) => Value::Str(kind.as_str().into()),
            None => Value::Null,
        }),
        NeedPath::Entries(p) => {
            if let Some(hit) = memo.dirs.get(p) {
                crate::perf::note_dir_hit();
                return Ok(Value::Attrs(Rc::clone(hit)));
            }
            let entries = host.read_dir(p).map_err(VmError::eval)?;
            let mut m = BTreeMap::new();
            for (name, t) in entries {
                let k = vm.intern(&name);
                m.insert(k, Slot::value(Value::Str(t.as_str().into())));
            }
            // A failure is deliberately not cached: the next asker should get
            // the error the filesystem gives it then, not the one it gave now.
            let attrs = Rc::new(Attrs::new(m));
            memo.dirs.insert(p.clone(), Rc::clone(&attrs));
            Ok(Value::Attrs(attrs))
        }
    }
}

/// The evaluator's errors as the embedder's classes. Public because
/// `session` maps the same way and two copies would drift.
pub fn map_vm_error(e: VmError) -> EvalError {
    match e {
        VmError::Unimplemented(w) => EvalError::Unimplemented(w),
        VmError::Throw(c) => EvalError::Eval(c.kind, c.message, c.pos.map(|p| *p)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Import from derivation, end to end on this arm.
    ///
    /// Every case here drives a real expression through `drive` against a
    /// host that answers `realise`, rather than calling `Host::realise`
    /// directly, because what is being tested is that the *builtins* reach it
    /// -- a `Realise` question nothing yields is a question that does not
    /// exist.
    mod realise {
        use super::*;
        use crate::host::{FileType, StoreError};
        use std::cell::RefCell;
        use std::collections::BTreeMap;

        /// A store that says yes. `realise` records what it was asked, so a
        /// test can assert the context reached it and in what shape.
        struct Built {
            asked: RefCell<Vec<String>>,
            /// What `realise` answers with; `None` means the build failed.
            rewrites: Option<BTreeMap<String, String>>,
        }

        /// The default is a build that succeeded and rewrote nothing, which
        /// is what an input-addressed derivation looks like -- the common
        /// case, and the one a test that says nothing about rewrites wants.
        /// `#[derive(Default)]` would give the opposite (`None`, a failed
        /// build) and every such test would be measuring the error path.
        impl Default for Built {
            fn default() -> Self {
                Built {
                    asked: RefCell::new(Vec::new()),
                    rewrites: Some(BTreeMap::new()),
                }
            }
        }

        impl Host for Built {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                warn,
                find_file,
                nix_path,
                trace
            );
            /// `appendContext` validates every key it is handed against the
            /// store before attaching it, so the fixture that builds a
            /// context has to answer this to build one at all.
            fn ensure_path(&self, _p: &str) -> std::result::Result<(), StoreError> {
                Ok(())
            }

            fn realise(
                &self,
                context: &[crate::value2::ContextElem],
            ) -> std::result::Result<BTreeMap<String, String>, StoreError> {
                self.asked.borrow_mut().push(
                    context
                        .iter()
                        .map(crate::value2::ContextElem::display)
                        .collect::<Vec<_>>()
                        .join(" "),
                );
                match &self.rewrites {
                    Some(map) => Ok(map.clone()),
                    None => Err(StoreError::Failed(
                        "builder for '/nix/store/d-x.drv' failed with exit code 1".to_owned(),
                    )),
                }
            }
            fn read_file(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                if path.path.as_ref() == "/abc" {
                    return Ok("abc".to_owned());
                }
                if path.ends_with("-out") || path.ends_with("-rewritten") {
                    return Ok("42".to_owned());
                }
                Err(format!("path '{path}' does not exist"))
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok({
                    path.path.as_ref() == "/abc"
                        || path.ends_with("-out")
                        || path.ends_with("-rewritten")
                })
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
        }

        /// A string whose context names a derivation output, which is what
        /// interpolating `drv.out` produces in a real evaluation. Built with
        /// `appendContext` so the fixture needs no builder and no store: the
        /// context is the whole point and this is the one builtin that
        /// attaches one directly.
        const WITH_CONTEXT: &str = r#"builtins.appendContext "/nix/store/00000000000000000000000000000000-out" { "/nix/store/11111111111111111111111111111111-x.drv" = { outputs = [ "out" ]; }; }"#;

        fn run(host: &Built, src: &str) -> String {
            render_with(&crate::eval::settings_with_store(), host, src)
        }

        /// `hashFile` reaches the file the way `readFile` does -- realise
        /// first, then one contents question -- and hashes what came back:
        /// the fixture file reads "42", and the result is cpp's
        /// `builtins.hashString "sha256" "42"`.
        #[test]
        fn hash_file_realises_then_hashes_the_answer() {
            let host = Built::default();
            assert_eq!(
                run(
                    &host,
                    &format!("builtins.hashFile \"sha256\" ({WITH_CONTEXT})")
                ),
                "\"73475cb40a568e8da8a045ced110137e159f890ac4da883b6b17dc651b3a8049\""
            );
            assert_eq!(
                host.asked.borrow().as_slice(),
                ["!out!/nix/store/11111111111111111111111111111111-x.drv"],
                "hashFile should realise its path argument before reading it"
            );
        }

        /// The enabled half of `hashFile` reaches the raw-byte host question
        /// and hashes its answer, rather than merely accepting the algorithm
        /// name. The disabled half is pinned beside the other builtin gates in
        /// `primops_host`.
        #[test]
        fn blake3_hash_file_matches_cppnixs_known_answer_when_enabled() {
            let host = Built::default();
            let settings = crate::eval::Settings {
                blake3_hashes: true,
                ..crate::eval::settings_with_store()
            };
            assert_eq!(
                render_with(&settings, &host, r#"builtins.hashFile "blake3" /abc"#),
                "\"6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85\""
            );
        }

        /// The digest is of the file's raw bytes. Before ENG-13146 the
        /// contents travelled back as a string, invalid UTF-8 was repaired
        /// to U+FFFD on the way, and a binary hashed to a digest no other
        /// tool computes. The fixture's `read_file` answers the repaired
        /// text, so the digest below can only have come from
        /// `read_file_bytes`.
        #[test]
        fn hash_file_digests_raw_bytes_not_repaired_text() {
            const RAW: &[u8] = &[0xFF, 0xFE, 0x00, b'h', b'e', b'l', b'l', b'o'];
            struct Binary;
            impl Host for Binary {
                crate::host::host_stubs!(settle);
                crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
                crate::host::host_stubs!(
                    realise,
                    store_text,
                    write_derivation,
                    store_filtered,
                    fetch,
                    lock_flake,
                    fetch_tree,
                    not_async,
                );
                crate::host::host_stubs!(
                    file_type_resolved,
                    get_env,
                    copy_to_store,
                    ensure_path,
                    warn,
                    find_file,
                    nix_path,
                    trace
                );
                fn read_file(
                    &self,
                    _p: &crate::value2::PathValue,
                ) -> std::result::Result<String, String> {
                    Ok(String::from_utf8_lossy(RAW).into_owned())
                }
                fn read_file_bytes(
                    &self,
                    _p: &crate::value2::PathValue,
                ) -> std::result::Result<Vec<u8>, String> {
                    Ok(RAW.to_vec())
                }
                fn read_dir(
                    &self,
                    _p: &crate::value2::PathValue,
                ) -> std::result::Result<Vec<(String, FileType)>, String> {
                    Ok(Vec::new())
                }
                fn path_exists_checked(
                    &self,
                    _p: &crate::value2::PathValue,
                ) -> std::result::Result<bool, String> {
                    Ok(true)
                }
                fn dir_exists_checked(
                    &self,
                    path: &crate::value2::PathValue,
                ) -> std::result::Result<bool, String> {
                    self.file_type_resolved(path)
                        .map(|kind| kind == crate::host::FileType::Directory)
                }
                fn file_type(
                    &self,
                    _p: &crate::value2::PathValue,
                ) -> std::result::Result<Option<FileType>, String> {
                    Ok(Some(FileType::Regular))
                }
            }
            // What `sha256sum` prints for these eight bytes. The repaired
            // text digests to 5222f94d2ea3f738... instead, so equality here is the
            // whole regression.
            assert_eq!(
                render_with(
                    &crate::eval::settings_with_store(),
                    &Binary,
                    "builtins.hashFile \"sha256\" /binary.bin"
                ),
                "\"9a0b8baf0d5c7049ecd6f313fe3a93216bea411fe27fbd565267a472d389abc8\""
            );
        }

        /// A read through a derivation output evaluates, where before it
        /// refused and dropped the whole evaluation to the cpp arm.
        #[test]
        fn a_read_through_a_derivation_output_evaluates() {
            let host = Built::default();
            assert_eq!(
                run(&host, &format!("builtins.readFile ({WITH_CONTEXT})")),
                "\"42\""
            );
            assert_eq!(
                host.asked.borrow().as_slice(),
                ["!out!/nix/store/11111111111111111111111111111111-x.drv"],
                "the built element should have reached the store, rendered \
                 cppnix's way"
            );
        }

        /// The realise comes first and the read second, which is
        /// `realisePath`'s order (`primops.cc:167`). A read that happened
        /// before the build would be reading a path nothing had produced.
        #[test]
        fn every_read_shaped_builtin_realises_first() {
            for src in [
                "builtins.readFile",
                "builtins.pathExists",
                "builtins.readDir",
                "builtins.readFileType",
                // The one the phrase "import from derivation" is named
                // after. Its own state machine, so it is its own case: the
                // fixture's file reads "42", which parses.
                "import",
            ] {
                let host = Built::default();
                let _ = run(&host, &format!("{src} ({WITH_CONTEXT})"));
                assert_eq!(
                    host.asked.borrow().len(),
                    1,
                    "{src} did not realise its argument's context"
                );
            }
        }

        /// Under `ca-derivations` the path the evaluator holds is a
        /// downstream placeholder, and reading it unrewritten is reading a
        /// path that never exists. The rewrite map is not decoration.
        #[test]
        fn the_rewrite_map_is_applied_to_the_path_before_the_read() {
            let host = Built {
                rewrites: Some(
                    [(
                        "/nix/store/00000000000000000000000000000000-out".to_owned(),
                        "/nix/store/22222222222222222222222222222222-rewritten".to_owned(),
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Built::default()
            };
            // The fixture's `read_file` answers only for `-out` and
            // `-rewritten`, so this passing means one of the two was read.
            // `readFileType` pins which: it is answered from `file_type`,
            // whereas `pathExists` below distinguishes them by name.
            assert_eq!(
                run(&host, &format!("builtins.pathExists ({WITH_CONTEXT})")),
                "true"
            );
            let host = Built {
                rewrites: Some(
                    [(
                        "/nix/store/00000000000000000000000000000000-out".to_owned(),
                        "/nix/store/33333333333333333333333333333333-elsewhere".to_owned(),
                    )]
                    .into_iter()
                    .collect(),
                ),
                ..Built::default()
            };
            assert_eq!(
                run(&host, &format!("builtins.pathExists ({WITH_CONTEXT})")),
                "false",
                "the rewrite was dropped: the unrewritten path still ends -out \
                 and would have answered true"
            );
        }

        /// A build failure is an evaluation error and `tryEval` does not
        /// catch it. That is cppnix's behaviour and not a default:
        /// `prim_tryEval` catches `AssertionError` alone
        /// (`primops.cc:1219`), while `realiseContext` raises
        /// `InvalidPathError` for an invalid element, `IFDError` when
        /// `allow-import-from-derivation` is off, and a `BuildError` out of
        /// `buildPaths`. None of the three is an `AssertionError`.
        #[test]
        fn a_failed_build_is_not_catchable() {
            let host = Built {
                rewrites: None,
                ..Built::default()
            };
            let rendered = run(&host, &format!("builtins.readFile ({WITH_CONTEXT})"));
            assert!(
                rendered.contains("failed with exit code 1"),
                "the embedder's message should reach the user: {rendered}"
            );
            let caught = run(
                &host,
                &format!("(builtins.tryEval (builtins.readFile ({WITH_CONTEXT}))).success"),
            );
            // The rendered *value*, not a substring: the debug form of an
            // uncaught error contains the word "false" in `catchable: false`,
            // so a `contains` check here would pass on exactly the failure it
            // is meant to catch.
            assert!(
                caught != "false" && caught != "true",
                "tryEval must not have caught it, but it answered {caught}"
            );
            assert!(
                caught.contains("failed with exit code 1") && caught.contains("catchable: false"),
                "and the error should still be the build's, uncatchable: {caught}"
            );
        }

        /// One evaluation asks for a build once. Every later ask for the
        /// same context is answered from `JobMemo::realised`: at the
        /// embedder each ask costs a validity check per element, a thread
        /// and a `buildPaths` round trip whether or not the outputs exist
        /// (one home-manager evaluation: 2,269 asks, 48 distinct contexts,
        /// 4.4 s). Two reads of the same built path, one build; the second
        /// read still gets the built contents, so the memo answered with
        /// the same rewrites the build did.
        #[test]
        fn a_context_is_realised_once_per_evaluation() {
            let once = Built::default();
            let single = run(&once, &format!("builtins.readFile ({WITH_CONTEXT})"));
            assert_eq!(once.asked.borrow().len(), 1);
            let inner = single.trim_matches('"');
            assert!(!inner.is_empty(), "the fixture read nothing: {single}");

            let twice = Built::default();
            let doubled = run(
                &twice,
                &format!("let p = {WITH_CONTEXT}; in builtins.readFile p + builtins.readFile p"),
            );
            assert_eq!(
                doubled,
                format!("\"{inner}{inner}\""),
                "both reads see the built file"
            );
            assert_eq!(
                twice.asked.borrow().len(),
                1,
                "the second read of the same context was answered from the memo: {:?}",
                twice.asked.borrow()
            );

            // The key is the whole context: another derivation's output is
            // another build, however alike the two look.
            let other = WITH_CONTEXT.replace(
                "11111111111111111111111111111111-x.drv",
                "22222222222222222222222222222222-y.drv",
            );
            let distinct = Built::default();
            let _ = run(
                &distinct,
                &format!(
                    "let p = {WITH_CONTEXT}; q = {other}; in builtins.readFile p + builtins.readFile q"
                ),
            );
            let asked = distinct.asked.borrow();
            assert_eq!(asked.len(), 2, "two contexts, two builds: {asked:?}");
            assert_ne!(asked[0], asked[1], "and they were different builds");
        }

        /// The question reaches the read set, so a warm start that replays
        /// this evaluation re-asks the build rather than assuming the output
        /// is still there. A store can be garbage collected between two runs;
        /// an unrecorded realise would replay as a read of a path that has
        /// gone.
        #[test]
        fn the_realise_is_visible_to_a_read_set() {
            let inner = Built::default();
            let host = crate::readset::RecordingHost::new(&inner);
            let _ = render_with(
                &crate::eval::settings_with_store(),
                &host,
                &format!("builtins.readFile ({WITH_CONTEXT})"),
            );
            let asked = format!("{:?}", host.take().questions());
            assert!(
                asked.contains("Realise("),
                "the build is missing from {asked}"
            );
            // `readFile` asks for the bytes, as cppnix's does (`Contents`
            // answers `read_file_bytes`); the text reader is `import`'s.
            assert!(
                asked.contains("ReadFileBytes("),
                "the read is missing from {asked}"
            );
        }

        /// With no store there is nothing to build with, so the read refuses
        /// by name rather than reporting "no such file" for a path cppnix
        /// would have produced. A census counts this as a gap in the
        /// embedder, which is what it is.
        #[test]
        fn without_a_store_it_refuses_by_name() {
            struct NoStore;
            impl Host for NoStore {
                crate::host::host_stubs!(settle);
                crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
                fn read_file_bytes(
                    &self,
                    path: &crate::value2::PathValue,
                ) -> Result<Vec<u8>, String> {
                    self.read_file(path).map(String::into_bytes)
                }
                crate::host::host_stubs!(
                    realise,
                    store_text,
                    write_derivation,
                    store_filtered,
                    fetch,
                    lock_flake,
                    fetch_tree,
                    not_async,
                );
                crate::host::host_stubs!(
                    file_type_resolved,
                    get_env,
                    copy_to_store,
                    warn,
                    find_file,
                    nix_path,
                    trace
                );
                /// A store that can validate a key but cannot build:
                /// `appendContext` succeeds and the realise behind the read
                /// is the only thing missing, which is the gap under test.
                fn ensure_path(&self, _p: &str) -> std::result::Result<(), StoreError> {
                    Ok(())
                }

                fn read_file(
                    &self,
                    _p: &crate::value2::PathValue,
                ) -> std::result::Result<String, String> {
                    Ok("unreachable".to_owned())
                }
                fn read_dir(
                    &self,
                    _p: &crate::value2::PathValue,
                ) -> std::result::Result<Vec<(String, FileType)>, String> {
                    Ok(Vec::new())
                }
                fn path_exists_checked(
                    &self,
                    _p: &crate::value2::PathValue,
                ) -> std::result::Result<bool, String> {
                    Ok(true)
                }
                fn dir_exists_checked(
                    &self,
                    path: &crate::value2::PathValue,
                ) -> std::result::Result<bool, String> {
                    self.file_type_resolved(path)
                        .map(|kind| kind == crate::host::FileType::Directory)
                }
                fn file_type(
                    &self,
                    _p: &crate::value2::PathValue,
                ) -> std::result::Result<Option<FileType>, String> {
                    Ok(Some(FileType::Regular))
                }
            }
            let rendered = render_with(
                &crate::eval::settings_with_store(),
                &NoStore,
                &format!("builtins.readFile ({WITH_CONTEXT})"),
            );
            assert!(
                rendered.contains("StoreUnavailable")
                    && rendered.contains("import from derivation"),
                "{rendered}"
            );
        }
    }

    /// `+` is cppnix's `ExprConcatStrings`, so a set operand coerces through
    /// `__toString` or `outPath` rather than being an arithmetic error
    /// (ENG-12593). All four bytes below are what `eval-backend = cpp`
    /// printed for the same expressions on dev-compute-4.
    ///
    /// Both orders matter and they are not the same rule: in cppnix the
    /// *first* element decides the branch, so a set on the left makes the
    /// whole expression a string concatenation. A fix that only handled a set
    /// on the right passes the first two of these and still fails nixpkgs,
    /// which is where this came from -- `hello.src.outPath` raised "expected
    /// an integer or float but found a set".
    #[test]
    fn a_set_operand_of_plus_coerces_like_cppnix() {
        assert_eq!(render(r#""a" + { outPath = "/x"; }"#), r#""a/x""#);
        assert_eq!(render(r#""a" + { __toString = self: "t"; }"#), r#""at""#);
        assert_eq!(render(r#"{ outPath = "/x"; } + "a""#), r#""/xa""#);
        // `outPath` wins over `__toString`? No: cppnix tries `__toString`
        // first (`coerceToString`, eval.cc), so this is "t" and not "/x".
        assert_eq!(
            render(r#""a" + { __toString = self: "t"; outPath = "/x"; }"#),
            r#""at""#
        );
    }

    /// cppnix reads `copyToStore` off the *first* part of a concatenation
    /// (`eval.cc`, `ExprConcatStrings::eval`), not off the part being
    /// coerced, so the same path on the right of `+` is treated two ways:
    ///
    ///     $ nix-instantiate --eval -E '"/pre" + ./relfile'
    ///     error: path '/private/tmp/relfile' does not exist   # copied
    ///     $ nix-instantiate --eval -E '{ outPath = "/pre"; } + ./relfile'
    ///     "/pre/private/tmp/relfile"                          # not copied
    ///
    /// Driven against a host with no store, because that is what makes the
    /// two cases tell each other apart without any IO: the copying one has to
    /// ask for a store and report the gap, the other must never ask.
    ///
    /// The distinction is easy to lose. Coercing the left-hand set replaces it
    /// with a string, and any pass that then re-reads the left operand sees a
    /// string and copies -- which is what this backend did when a set operand
    /// first learned to coerce.
    #[test]
    fn a_set_on_the_left_of_plus_does_not_copy_the_path_on_the_right() {
        use crate::host::{FileType, Host};

        struct NoStore;
        impl Host for NoStore {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Err(format!("path \'{path}\' does not exist"))
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
        }

        // Rendered rather than asserted branch by branch, as the sibling
        // no-store test is: the workspace denies `panic`, tests included.
        fn run(src: &str) -> String {
            match compile::compile_source(
                src,
                "/m",
                crate::compile::Origin::String,
                &crate::eval::Settings::default(),
            ) {
                Err(e) => format!("compile failed: {e:?}"),
                Ok(module) => {
                    let mut vm = Vm::with_settings(crate::eval::Settings::default());
                    vm.start_module(&Rc::new(module));
                    match drive(&mut vm, &NoStore) {
                        Ok(Value::Str(s)) => s.expect_text(),
                        Ok(v) => format!("{v:?}"),
                        Err(VmError::Unimplemented(what)) => format!("unimplemented: {what}"),
                        Err(e) => format!("error: {e:?}"),
                    }
                }
            }
        }

        // A set first: no copy, so it answers with no store present at all.
        assert_eq!(run(r#"{ outPath = "/pre"; } + /m/f"#), "/pre/m/f");
        // A string first: the copy is cppnix's, so the gap is reported and
        // no path is invented. Without this half the assertion above would
        // also pass on a backend that had simply stopped copying.
        assert_eq!(
            run(r#""/pre" + /m/f"#),
            "unimplemented: interpolating a path into a string \
             (no store behind this evaluator)"
        );
    }

    /// `coerceMore` is off for `+` exactly as it is for interpolation, both
    /// being one `ExprConcatStrings` in cppnix, so a number on the right of a
    /// string is an error and not a rendered digit. And only a number on the
    /// *left* is arithmetic at all, so a Boolean, a null and a list are
    /// coercion failures rather than "expected an integer or float".
    ///
    /// Every expectation is what nix 2.34.7's cpp arm answered for the same
    /// expression; the three that used to return a value are marked.
    #[test]
    fn plus_refuses_what_cppnix_refuses() {
        for (src, want) in [
            // Answered "a1" before.
            (r#""a" + 1"#, "cannot coerce an integer to a string"),
            // Answered "a1.5" before.
            (r#""a" + 1.5"#, "cannot coerce a float to a string"),
            // Answered "/x1" before.
            (
                r#"{ outPath = "/x"; } + 1"#,
                "cannot coerce an integer to a string",
            ),
            (r#""a" + true"#, "cannot coerce a Boolean to a string"),
            (r#"[ 1 ] + "x""#, "cannot coerce a list to a string"),
            (r#"true + "x""#, "cannot coerce a Boolean to a string"),
            (r#"null + "x""#, "cannot coerce null to a string"),
        ] {
            let got = render(src);
            assert!(
                got.contains(want),
                "`{src}` should fail with {want:?}; got: {got}"
            );
        }
        // Arithmetic and ordinary concatenation still work, so the refusals
        // above are not the operator having been switched off wholesale.
        assert_eq!(render("1 + 2"), "3");
        assert_eq!(render("1.5 + 2"), "3.5");
        assert_eq!(render(r#""a" + "b""#), r#""ab""#);
        assert_eq!(render(r#""a" + { outPath = "/x"; }"#), r#""a/x""#);
    }

    /// A pure computation long enough to cross several interrupt strides and
    /// short enough that the un-armed half of the test is not a wait.
    const LONG: &str = "builtins.foldl' (a: b: a + b) 0 (builtins.genList (x: x) 200000)";

    /// ENG-12533. Both halves matter: the armed one shows the check fires,
    /// and the un-armed one shows it does not fire on its own -- a check that
    /// interrupted every evaluation would pass the first assertion alone.
    ///
    /// Arming is a call on one `Vm` and reaches nothing else, so this needs
    /// no lock and no thread-local. It used to need both: the hook was a
    /// process-global slot, so two tests arming it at once disarmed each
    /// other and an "always interrupted" flag would have interrupted whatever
    /// else the binary happened to be evaluating.
    #[test]
    fn an_armed_interrupt_stops_a_long_pure_evaluation() {
        // Stated, not read from the process: nothing here is about a global,
        // and reading one would make this test's answer depend on what else
        // the binary is doing (ENG-12939).
        let settings = Settings::default();

        let mut quiet_vm = Vm::with_settings(settings.clone());
        let quiet = eval_str_on(LONG, ".", compile::Origin::String, &mut quiet_vm, &RealFs);
        assert_eq!(quiet.ok().as_deref(), Some("19999900000"));

        let mut armed_vm = Vm::with_settings(settings);
        armed_vm.set_interrupt(Box::new(|| true));
        let interrupted = eval_str_on(LONG, ".", compile::Origin::String, &mut armed_vm, &RealFs);

        // cppnix's own wording, and an ordinary evaluation error rather than
        // a catchable throw: `tryEval` must not turn an operator's SIGTERM
        // into a value. Asserted rather than matched with a `panic!` arm
        // because the workspace denies `panic`, tests included.
        assert!(
            matches!(
                &interrupted,
                Err(EvalError::Eval(ErrKind::Eval, message, _))
                    if message == "interrupted by the user"
            ),
            "expected an interrupt, got {interrupted:?}"
        );
    }

    /// A once-only setting refuses a conflicting change and shrugs at a
    /// repeat.
    ///
    /// Held on a scratch slot rather than the real `STORE_DIR`, because the
    /// real one is a process global that other tests in this binary have
    /// already populated, and a test that depended on being first would pass
    /// or fail on scheduling order.
    #[test]
    fn a_once_only_setting_refuses_a_conflicting_change() {
        static SLOT: std::sync::OnceLock<String> = std::sync::OnceLock::new();

        assert_eq!(set_once(&SLOT, "the scratch setting", "/nix/store"), Ok(()));
        // The expected case: the bridge sets the same value once per
        // evaluation, and that must stay silent.
        assert_eq!(set_once(&SLOT, "the scratch setting", "/nix/store"), Ok(()));

        let conflict = set_once(&SLOT, "the scratch setting", "/tmp/other/store");
        let Err(conflict) = conflict else {
            unreachable!("a conflicting re-set was accepted");
        };
        assert_eq!(conflict.existing, "/nix/store");
        assert_eq!(conflict.attempted, "/tmp/other/store");
        // The message has to name both values, or the reader cannot tell
        // which of their two configurations won.
        let message = conflict.to_string();
        assert!(message.contains("/nix/store"), "{message}");
        assert!(message.contains("/tmp/other/store"), "{message}");

        // And the refusal did not change the setting.
        assert_eq!(SLOT.get().map(String::as_str), Some("/nix/store"));
    }

    /// Every field of [`Settings`] moves the fingerprint.
    ///
    /// The destructure is the guard, not the assertions: a field added to
    /// `Settings` stops this compiling, so nobody can add a setting and leave
    /// it out of the memo key without noticing. The assertions below then say
    /// the field is not merely present but load-bearing -- a `fingerprint`
    /// that hashed a field's *name* and not its value would satisfy the
    /// destructure and fail here.
    #[test]
    fn every_setting_is_in_the_memo_key() {
        let base = Settings {
            store_dir: Some("/nix/store".to_owned()),
            nix_version: Some("2.34.7".to_owned()),
            host_build_identity: Some("/test/host-a".to_owned()),
            current_system: Some("x86_64-linux".to_owned()),
            max_call_depth: 10_000,
            pure_eval: false,
            restrict_eval: false,
            builtin_features: BuiltinFeatures::ALL,
            path_reads: crate::purity::PathReads::Direct,
            trace_verbose: false,
            abort_on_warn: false,
            home_dir: Some("/home/nixer".to_owned()),
            ca_derivations: false,
            allow_import_from_derivation: true,
            allowed_uris: None,
            repair: false,
            blake3_hashes: false,
            lint_url_literals: Diagnose::Ignore,
            lint_short_path_literals: Diagnose::Ignore,
            lint_absolute_path_literals: Diagnose::Ignore,
            pipe_operators: false,
            parse_toml_timestamps: false,
        };
        // Naming every field. Adding one to the struct breaks this line until
        // it is given a perturbation below.
        let Settings {
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
            allow_import_from_derivation: _,
            allowed_uris: _,
            repair: _,
            blake3_hashes: _,
            lint_url_literals: _,
            lint_short_path_literals: _,
            lint_absolute_path_literals: _,
            pipe_operators: _,
            parse_toml_timestamps: _,
        } = base.clone();

        let perturbed = [
            (
                "host_build_identity",
                Settings {
                    host_build_identity: Some("/test/host-b".to_owned()),
                    ..base.clone()
                },
            ),
            (
                "host_build_identity unset",
                Settings {
                    host_build_identity: None,
                    ..base.clone()
                },
            ),
            (
                "store_dir",
                Settings {
                    store_dir: Some("/tmp/other/store".to_owned()),
                    ..base.clone()
                },
            ),
            (
                "store_dir unset",
                Settings {
                    store_dir: None,
                    ..base.clone()
                },
            ),
            (
                "nix_version",
                Settings {
                    nix_version: Some("2.35.0".to_owned()),
                    ..base.clone()
                },
            ),
            (
                "nix_version unset",
                Settings {
                    nix_version: None,
                    ..base.clone()
                },
            ),
            (
                "current_system",
                Settings {
                    current_system: Some("aarch64-darwin".to_owned()),
                    ..base.clone()
                },
            ),
            (
                "current_system unset",
                Settings {
                    current_system: None,
                    ..base.clone()
                },
            ),
            (
                "max_call_depth",
                Settings {
                    max_call_depth: 50,
                    ..base.clone()
                },
            ),
            (
                "pure_eval",
                Settings {
                    pure_eval: true,
                    ..base.clone()
                },
            ),
            (
                "restrict_eval",
                Settings {
                    restrict_eval: true,
                    ..base.clone()
                },
            ),
            (
                "path_reads",
                Settings {
                    path_reads: crate::purity::PathReads::ThroughEmbedder,
                    ..base.clone()
                },
            ),
            (
                "builtin_features",
                Settings {
                    builtin_features: BuiltinFeatures {
                        flakes: false,
                        ..BuiltinFeatures::ALL
                    },
                    ..base.clone()
                },
            ),
            (
                "builtin_features unset",
                Settings {
                    builtin_features: BuiltinFeatures::NONE,
                    ..base.clone()
                },
            ),
            (
                "trace_verbose",
                Settings {
                    trace_verbose: true,
                    ..base.clone()
                },
            ),
            (
                "abort_on_warn",
                Settings {
                    abort_on_warn: true,
                    ..base.clone()
                },
            ),
            (
                "ca_derivations",
                Settings {
                    ca_derivations: true,
                    ..base.clone()
                },
            ),
            (
                "blake3_hashes",
                Settings {
                    blake3_hashes: true,
                    ..base.clone()
                },
            ),
            (
                "allow_import_from_derivation",
                Settings {
                    allow_import_from_derivation: false,
                    ..base.clone()
                },
            ),
            (
                "allowed_uris",
                Settings {
                    allowed_uris: Some("https://example.org/\n".to_owned()),
                    ..base.clone()
                },
            ),
            (
                "home_dir",
                Settings {
                    home_dir: Some("/home/other".to_owned()),
                    ..base.clone()
                },
            ),
            (
                "home_dir unset",
                Settings {
                    home_dir: None,
                    ..base.clone()
                },
            ),
            // `Ignore` -> `Fatal` is the perturbation that changes what a
            // module compiles to; `Ignore` -> `Warn` deliberately does not
            // (see `fingerprint`), so it is not asserted distinct here.
            (
                "lint_url_literals",
                Settings {
                    lint_url_literals: Diagnose::Fatal,
                    ..base.clone()
                },
            ),
            (
                "lint_short_path_literals",
                Settings {
                    lint_short_path_literals: Diagnose::Fatal,
                    ..base.clone()
                },
            ),
            (
                "lint_absolute_path_literals",
                Settings {
                    lint_absolute_path_literals: Diagnose::Fatal,
                    ..base.clone()
                },
            ),
            (
                "pipe_operators",
                Settings {
                    pipe_operators: true,
                    ..base.clone()
                },
            ),
            (
                "parse_toml_timestamps",
                Settings {
                    parse_toml_timestamps: true,
                    ..base.clone()
                },
            ),
        ];
        for (what, other) in perturbed {
            assert_ne!(
                base.fingerprint(),
                other.fingerprint(),
                "{what} does not reach the memo key, so a cache shared across two \
                 values of it serves the wrong answer"
            );
        }
        assert_eq!(
            base.fingerprint(),
            base.clone().fingerprint(),
            "the fingerprint is not a function of the settings"
        );
    }

    /// The two purity settings are not one flag with two names.
    ///
    /// This is the regression the split exists to prevent. Before it, the
    /// embedder passed `restrictEval || pureEval` as a single
    /// `filesystem_access` bit, so `pure-eval` alone and `restrict-eval`
    /// alone were the same key -- and they forbid different questions, so a
    /// result computed under one would have been served to the other. Three
    /// of the four configurations have to be distinct keys; the fourth,
    /// neither setting, is the base.
    #[test]
    fn each_purity_configuration_is_its_own_memo_key() {
        let base = Settings {
            store_dir: Some("/nix/store".to_owned()),
            nix_version: Some("2.34.7".to_owned()),
            host_build_identity: Some("/test/host-a".to_owned()),
            current_system: Some("x86_64-linux".to_owned()),
            max_call_depth: 10_000,
            pure_eval: false,
            restrict_eval: false,
            builtin_features: BuiltinFeatures::ALL,
            path_reads: crate::purity::PathReads::Direct,
            trace_verbose: false,
            abort_on_warn: false,
            home_dir: Some("/home/nixer".to_owned()),
            ca_derivations: false,
            allow_import_from_derivation: true,
            allowed_uris: None,
            repair: false,
            blake3_hashes: false,
            lint_url_literals: Diagnose::Ignore,
            lint_short_path_literals: Diagnose::Ignore,
            lint_absolute_path_literals: Diagnose::Ignore,
            pipe_operators: false,
            parse_toml_timestamps: false,
        };
        let configurations = [
            ("neither", false, false),
            ("pure only", true, false),
            ("restrict only", false, true),
            ("both", true, true),
        ];
        let mut seen: Vec<(&str, ix_kernel::hash::Hash)> = Vec::new();
        for (label, pure_eval, restrict_eval) in configurations {
            let fingerprint = Settings {
                pure_eval,
                restrict_eval,
                ..base.clone()
            }
            .fingerprint();
            for (other, previous) in &seen {
                assert_ne!(
                    &fingerprint, previous,
                    "{label} and {other} share a memo key, so a result computed \
                     under one purity regime can be served under the other"
                );
            }
            seen.push((label, fingerprint));
        }
        assert_eq!(seen.len(), 4);
    }

    /// What the split changed for a running program, at the two ends of the
    /// table.
    ///
    /// Before it, `pure-eval` refused every host question, so both of these
    /// were the same wholesale refusal. cppnix answers the first (`""`,
    /// `primops.cc:1261`, measured on nix 2.34.7+ix.h24085346) and refuses
    /// the second, so only one of them should still be a refusal here -- and
    /// its detail has to name `pure-eval` rather than "restrict-eval or
    /// pure-eval", which named a setting that was not on.
    /// Under `pure-eval` the two impure constants are in neither `builtins`
    /// nor the global scope, as cppnix's `addConstant` leaves them
    /// (`eval.cc:541`, `Constant{.impureOnly = true}`).
    ///
    /// This was measured wrong before it was written: a flake fixture whose
    /// `flake.nix` said `packages.${builtins.currentSystem}` *built* on this
    /// backend under `nix build` and raised `attribute 'currentSystem'
    /// missing` on cpp. A value where cppnix has none is the worst outcome
    /// the parity bar admits -- worse than a refusal, because nothing says
    /// anything went wrong.
    ///
    /// Both directions, because a check that only looked at the pure case
    /// would pass against a backend that had simply deleted the constants.
    #[test]
    fn the_impure_constants_are_absent_under_pure_eval_and_present_otherwise() {
        let pure = Settings {
            pure_eval: true,
            ..Settings::default()
        };
        let impure = Settings::default();

        let pure_member = render_under(&pure, "builtins ? currentSystem");
        let pure_time = render_under(&pure, "builtins ? currentTime");
        let pure_use = render_under(&pure, "builtins.currentSystem");
        let pure_global = render_under(&pure, "__currentSystem");

        let impure_member = render_under(&impure, "builtins ? currentSystem");
        let impure_use = render_under(&impure, "builtins.currentSystem");

        assert_eq!(
            pure_member, "false",
            "currentSystem is in builtins under pure-eval"
        );
        assert_eq!(
            pure_time, "false",
            "currentTime is in builtins under pure-eval"
        );
        assert!(
            pure_use.contains("missing"),
            "selecting currentSystem under pure-eval answered instead of failing: {pure_use}"
        );
        assert!(
            pure_global.contains("undefined variable"),
            "the bare global resolved under pure-eval: {pure_global}"
        );
        assert_eq!(
            impure_member, "true",
            "currentSystem should be back when pure-eval is off"
        );
        assert!(
            !impure_use.contains("missing"),
            "currentSystem should answer when pure-eval is off: {impure_use}"
        );
    }

    #[test]
    fn pure_eval_answers_getenv_and_still_refuses_a_read() {
        let pure = Settings {
            pure_eval: true,
            ..Settings::default()
        };
        let env = render_under(&pure, r#"builtins.getEnv "HOME""#);
        let read = render_under(&pure, r#"builtins.readFile /etc/passwd"#);

        assert_eq!(
            env, "\"\"",
            "pure-eval should answer getEnv with the empty string, as cppnix does"
        );
        assert!(
            read.contains("pure-eval") && !read.contains("restrict-eval"),
            "the refusal does not name the setting that is actually on: {read}"
        );
        assert!(
            read.contains("/etc/passwd"),
            "the refusal does not name what was refused: {read}"
        );
    }

    /// Every combination of the two trace-family settings is its own memo
    /// key, not just every field taken one at a time.
    ///
    /// `every_setting_is_in_the_memo_key` above cannot see this, and the gap
    /// is not theoretical: it perturbs one field at a time from an all-false
    /// base, so a `fingerprint` that folded `trace_verbose` and
    /// `abort_on_warn` into one combined part would satisfy every one of its
    /// assertions while giving `(true, false)` and `(false, true)` the same
    /// key. That is a real collision -- one decides whether
    /// `traceVerbose (throw "x") 1` throws, the other whether
    /// `warn "m" 1` has a value at all -- so a result computed under one
    /// would be served under the other.
    ///
    /// Same shape as `each_purity_configuration_is_its_own_memo_key`, and
    /// same reason.
    #[test]
    fn each_trace_setting_configuration_is_its_own_memo_key() {
        let base = Settings {
            store_dir: Some("/nix/store".to_owned()),
            nix_version: Some("2.34.7".to_owned()),
            host_build_identity: Some("/test/host-a".to_owned()),
            current_system: Some("x86_64-linux".to_owned()),
            max_call_depth: 10_000,
            pure_eval: false,
            restrict_eval: false,
            builtin_features: BuiltinFeatures::ALL,
            path_reads: crate::purity::PathReads::Direct,
            trace_verbose: false,
            abort_on_warn: false,
            home_dir: Some("/home/nixer".to_owned()),
            ca_derivations: false,
            allow_import_from_derivation: true,
            allowed_uris: None,
            repair: false,
            blake3_hashes: false,
            lint_url_literals: Diagnose::Ignore,
            lint_short_path_literals: Diagnose::Ignore,
            lint_absolute_path_literals: Diagnose::Ignore,
            pipe_operators: false,
            parse_toml_timestamps: false,
        };
        let mut seen: Vec<(&str, ix_kernel::hash::Hash)> = Vec::new();
        for (label, trace_verbose, abort_on_warn) in [
            ("neither", false, false),
            ("trace-verbose only", true, false),
            ("abort-on-warn only", false, true),
            ("both", true, true),
        ] {
            let key = Settings {
                trace_verbose,
                abort_on_warn,
                ..base.clone()
            }
            .fingerprint();
            for (other, previous) in &seen {
                assert_ne!(
                    &key, previous,
                    "the same module under {label} and under {other} is one cache entry"
                );
            }
            seen.push((label, key));
        }
    }

    /// An unset setting and one set to the empty string are different facts:
    /// `derivationStrict` refuses under the first and computes a path under
    /// the second.
    #[test]
    fn an_unset_setting_does_not_digest_as_an_empty_one() {
        let unset = Settings {
            store_dir: None,
            nix_version: None,
            host_build_identity: None,
            current_system: None,
            max_call_depth: 10_000,
            pure_eval: false,
            restrict_eval: false,
            builtin_features: BuiltinFeatures::NONE,
            path_reads: crate::purity::PathReads::Direct,
            trace_verbose: false,
            abort_on_warn: false,
            home_dir: Some("/home/nixer".to_owned()),
            ca_derivations: false,
            allow_import_from_derivation: true,
            allowed_uris: None,
            repair: false,
            blake3_hashes: false,
            lint_url_literals: Diagnose::Ignore,
            lint_short_path_literals: Diagnose::Ignore,
            lint_absolute_path_literals: Diagnose::Ignore,
            pipe_operators: false,
            parse_toml_timestamps: false,
        };
        let empty = Settings {
            store_dir: Some(String::new()),
            nix_version: Some(String::new()),
            host_build_identity: Some(String::new()),
            current_system: Some(String::new()),
            builtin_features: BuiltinFeatures::ALL,
            ..unset.clone()
        };
        assert_ne!(unset.fingerprint(), empty.fingerprint());
    }

    /// Two settings cannot be swapped past each other in the digest. Without
    /// per-field framing, `store_dir = "ab", nix_version = ""` and
    /// `store_dir = "a", nix_version = "b"` would hash the same.
    #[test]
    fn a_setting_cannot_borrow_its_neighbour_s_bytes() {
        let left = Settings {
            store_dir: Some("ab".to_owned()),
            nix_version: Some(String::new()),
            host_build_identity: Some(String::new()),
            current_system: Some(String::new()),
            max_call_depth: 1,
            pure_eval: false,
            restrict_eval: false,
            builtin_features: BuiltinFeatures::ALL,
            path_reads: crate::purity::PathReads::Direct,
            trace_verbose: false,
            abort_on_warn: false,
            home_dir: Some("/home/nixer".to_owned()),
            ca_derivations: false,
            allow_import_from_derivation: true,
            allowed_uris: None,
            repair: false,
            blake3_hashes: false,
            lint_url_literals: Diagnose::Ignore,
            lint_short_path_literals: Diagnose::Ignore,
            lint_absolute_path_literals: Diagnose::Ignore,
            pipe_operators: false,
            parse_toml_timestamps: false,
        };
        let right = Settings {
            store_dir: Some("a".to_owned()),
            nix_version: Some("b".to_owned()),
            ..left.clone()
        };
        assert_ne!(left.fingerprint(), right.fingerprint());
    }

    /// Value renders as-is, errors as their debug form; assertions never
    /// panic by hand (the workspace denies `panic`, tests included).
    /// A VM evaluates under the settings it was handed, not the ones the
    /// process happens to be holding.
    ///
    /// The property ENG-12939 turns on, stated as an assertion rather than
    /// left implicit in 398 tests that would each fail rarely if it broke.
    /// This test is the one place in the suite that deliberately moves a
    /// process global while evaluating, and it moves it to the *wrong* value:
    /// if any read on the evaluation path went back to `Settings::current()`,
    /// the first assertion below would see `false` where it wants `true`.
    ///
    /// `globals_moving()` because it really does move one, and the write lock
    /// is what keeps the `capi` tests -- the only readers of the process
    /// configuration left -- from seeing it.
    #[test]
    fn a_vm_evaluates_under_the_settings_it_was_given_and_not_the_process_ones() {
        let _moving = globals_moving();
        let before = pure_eval();

        // The process says "pure". Neither VM below is told that.
        set_pure_eval(true);
        let under_default = render_under(&Settings::default(), "builtins ? currentSystem");
        let under_pure = render_under(
            &Settings {
                pure_eval: true,
                ..Settings::default()
            },
            "builtins ? currentSystem",
        );
        set_pure_eval(before);

        assert_eq!(
            under_default, "true",
            "a VM built from `Settings::default()` saw the process' `pure-eval`, so some \
             read on the evaluation path is still going to the static (ENG-12939)"
        );
        assert_eq!(
            under_pure, "false",
            "a VM told `pure_eval: true` must honour it -- otherwise the assertion above \
             passes for the wrong reason, because nothing reads the setting at all"
        );
    }

    /// Evaluate under a configuration the caller states.
    fn render_under(settings: &Settings, src: &str) -> String {
        super::render_str_with(settings, src)
    }

    /// A later process cannot land on a scratch directory this one made.
    ///
    /// ENG-13024, and it is a soundness test wearing test-infrastructure
    /// clothes: `capi::warm_starts` deliberately leaves its directories
    /// behind (see `scratch_dir`), so a name a future process can re-derive
    /// means that process opens a warm store and is served rows it never
    /// wrote. What surfaced was `two_questions_about_one_module_do_not_share_a_row`
    /// reporting `("second", IXE_SERVE_ANSWER)` where it demands
    /// `IXE_SERVE_EVALUATE` -- the right value, served instead of computed.
    ///
    /// The three components a later process *can* reproduce are the prefix,
    /// the label and the pid; the ordinal it can reproduce too, since the
    /// counter restarts at zero every process. So the check is that the name
    /// is not those and nothing else, and the demonstration is that a
    /// directory pre-seeded under exactly the old scheme's name is not the
    /// one this call hands back.
    #[test]
    fn a_scratch_directory_cannot_be_re_derived_by_a_later_process() {
        let path = super::scratch_dir("ixe-collision-probe", "regression");
        let Some(name) = path.file_name().and_then(std::ffi::OsStr::to_str) else {
            unreachable!("{} has no file name", path.display())
        };
        // Split the ordinal back off, so the rest of the check does not have
        // to guess which one this call drew under a parallel run.
        let Some((head, ordinal)) = name.rsplit_once('-') else {
            unreachable!("{name} has no ordinal")
        };

        let re_derivable = format!("ixe-collision-probe-regression-{}", std::process::id());
        assert_ne!(
            head, re_derivable,
            "the scratch name is the prefix, the label, the pid and an ordinal, all of \
             which a later process reproduces exactly; it needs a component that one \
             cannot (ENG-13024)"
        );

        // The behavioural half: seed the directory the old scheme would have
        // named and require this call to have missed it. On the old code the
        // two are equal and this fails.
        let collided = std::env::temp_dir().join(format!("{re_derivable}-{ordinal}"));
        assert_ne!(
            path, collided,
            "this call handed back the very name a dead process would have used"
        );
        let Ok(()) = std::fs::create_dir_all(&collided) else {
            unreachable!("cannot create {}", collided.display())
        };
        let marker = collided.join("left-by-a-dead-process");
        let Ok(()) = std::fs::write(&marker, b"warm") else {
            unreachable!("cannot write {}", marker.display())
        };
        assert!(
            !path.exists(),
            "seeding {} created {}, so the two names are the same directory",
            collided.display(),
            path.display()
        );
        drop(std::fs::remove_dir_all(&collided));
    }

    /// Evaluate under the default settings. Both spellings live in
    /// `super`, so the three test modules that want them share one body.
    use super::render_str as render;

    /// cppnix's `eqValues` compares two derivation-shaped sets by `outPath`
    /// alone, before it looks at their sizes or their other attributes. The
    /// four cases here are the four branches of that rule, and the bytes are
    /// what nix 2.34.7 answered on the cpp backend, dev-compute-4:
    ///
    ///   1. both derivation-typed, same `outPath`, different attributes: equal
    ///      anyway, which is the whole point and is what a structural walk
    ///      would get wrong;
    ///   2. both derivation-typed, different `outPath`: unequal;
    ///   3. `type` present but not `"derivation"`: the rule does not apply and
    ///      the sets compare structurally;
    ///   4. derivation-typed with no `outPath`: cppnix falls through to the
    ///      structural comparison rather than calling them unequal.
    ///
    /// Written with hand-made sets rather than real derivations so it pins the
    /// comparison rule and not the wrapper.
    #[test]
    fn derivation_shaped_sets_compare_by_out_path() {
        assert_eq!(
            render(
                r#"[ ({type="derivation"; outPath="x";} == {type="derivation"; outPath="x"; extra=1;})
                     ({type="derivation"; outPath="x";} == {type="derivation"; outPath="y";})
                     ({type="car"; outPath="x";} == {type="car"; outPath="x"; extra=1;})
                     ({type="derivation";} == {type="derivation"; extra=1;}) ]"#
            ),
            "[ true false false false ]"
        );
    }

    /// What `builtins.<name>` answers, now that most spellings of it resolve
    /// at compile time and the rest come out of a set the `Vm` keeps. Every
    /// expected byte here is what nix 2.34.7 (`2.34.7+ix.g69e4d9e9db39`)
    /// printed for the same expression on hydra, so the fold is pinned to
    /// cppnix rather than to this evaluator's previous answer. ENG-12539.
    #[test]
    fn builtins_references_answer_what_cppnix_answers() {
        assert_eq!(render(r#"builtins.stringLength "abc""#), "3");
        // Shadowing: a binding wins, a `with` does not.
        assert_eq!(
            render(r#"let builtins = { stringLength = _: 99; }; in builtins.stringLength "abc""#),
            "99"
        );
        assert_eq!(
            render(
                r#"with { builtins = { stringLength = _: 99; }; }; builtins.stringLength "abc""#
            ),
            "3"
        );
        assert_eq!(
            render(
                r#"({ builtins }: builtins.stringLength "abc") { builtins = { stringLength = _: 99; }; }"#
            ),
            "99"
        );
        assert_eq!(
            render(
                r#"(rec { builtins = { stringLength = _: 99; }; x = builtins.stringLength "abc"; }).x"#
            ),
            "99"
        );
        // Members that are not plain primops, and non-members.
        assert_eq!(render("builtins.langVersion"), "6");
        assert_eq!(render("builtins ? stringLength"), "true");
        assert_eq!(render("builtins.nope or 7"), "7");
        assert_eq!(
            render(r#"builtins.typeOf builtins.derivation"#),
            r#""lambda""#
        );
        // cppnix compares any two functions as unequal, whether or not they
        // are the same primop, so sharing one value for the set cannot make
        // this true.
        assert_eq!(
            render("builtins.stringLength == builtins.stringLength"),
            "false"
        );
    }

    /// The two failures a `builtins.<name>` that does not resolve has to keep
    /// producing. Separate from the answers above because these are errors,
    /// and an error that changes shape is a corpus mismatch.
    #[test]
    fn unresolvable_builtins_references_still_fail_the_same_way() {
        assert!(
            render("builtins.nope").contains("nope"),
            "{}",
            render("builtins.nope")
        );
        // An unsupported builtin is absent, so capability checks can choose another path.
        assert!(
            render("builtins.fetchMercurial").contains("missing"),
            "{}",
            render("builtins.fetchMercurial")
        );
        // Selecting through a resolved builtin is a type error, not a miss.
        assert!(
            render("builtins.stringLength.x").contains("expected a set"),
            "{}",
            render("builtins.stringLength.x")
        );
    }

    /// The `GetLocal` fast path returns an already-forced slot without a force
    /// frame, and the states that are not values still take the long way.
    /// Blackholing lives on that long way, so a fast path that fired too
    /// widely would turn a cycle into a hang or a wrong answer, and a memoised
    /// failure into a second evaluation. ENG-12539.
    #[test]
    fn locals_still_blackhole_and_memoise_failures() {
        assert!(
            render("let x = x; in x").contains("infinite recursion"),
            "{}",
            render("let x = x; in x")
        );
        assert!(
            render("let f = n: f n; in f 1").contains("stack overflow"),
            "{}",
            render("let f = n: f n; in f 1")
        );
        // Forced twice: the second read is the fast path over a slot the first
        // read left holding a value.
        assert_eq!(render("let x = 1 + 1; in x + x"), "4");
        // A thrown thunk read twice rethrows rather than re-running.
        assert_eq!(
            render(r#"let x = throw "boom"; in [ (builtins.tryEval x) (builtins.tryEval x) ]"#),
            "[ { success = false; value = false; } { success = false; value = false; } ]"
        );
    }

    #[test]
    fn arithmetic() {
        assert_eq!(render("1 + 2"), "3");
        assert_eq!(render("2 * 3 + 4"), "10");
        assert_eq!(render("(1 + 2) * -3"), "-9");
        assert_eq!(render("7 / 2"), "3");
        assert_eq!(render("7.0 / 2"), "3.5");
        assert_eq!(render("1.0 + 2"), "3");
    }

    #[test]
    fn bindings_and_functions() {
        assert_eq!(render("let a = 1; b = a + 1; in a + b"), "3");
        assert_eq!(render("let f = x: x * 2; in f 21"), "42");
        assert_eq!(render("(x: y: x + y) 1 2"), "3");
        assert_eq!(render("({ a, b ? 10 }: a + b) { a = 1; }"), "11");
        assert_eq!(
            render("({ a, ... } @ all: a + all.b) { a = 1; b = 2; }"),
            "3"
        );
        assert_eq!(
            render("let f = n: if n == 0 then 1 else n * f (n - 1); in f 5"),
            "120"
        );
    }

    #[test]
    fn data_structures() {
        assert_eq!(render("[ 1 2 3 ]"), "[ 1 2 3 ]");
        assert_eq!(render("{ b = 2; a = 1; }"), "{ a = 1; b = 2; }");
        assert_eq!(render("{ a = 1; b = 2; }.b"), "2");
        assert_eq!(render("{ a = { b = 42; }; }.a.b"), "42");
        assert_eq!(render("{ a = 1; }.b or 7"), "7");
        assert_eq!(render("{ a = 1; } ? a"), "true");
        assert_eq!(render("rec { a = 1; b = a + 1; }.b"), "2");
        assert_eq!(render("[ 1 ] ++ [ 2 ]"), "[ 1 2 ]");
        assert_eq!(
            render("{ a = 1; } // { a = 2; b = 3; }"),
            "{ a = 2; b = 3; }"
        );
    }

    #[test]
    fn scoping() {
        assert_eq!(render("with { a = 1; }; a"), "1");
        assert_eq!(render("let a = 2; in with { a = 1; }; a"), "2");
        assert_eq!(render("with { a = 1; }; with { a = 2; }; a"), "2");
    }

    #[test]
    fn strings() {
        assert_eq!(render("\"abc\""), "\"abc\"");
        assert_eq!(render("let x = \"b\"; in \"a${x}c\""), "\"abc\"");
        assert_eq!(render("\"a\" + \"b\""), "\"ab\"");
        assert_eq!(render("toString 42"), "\"42\"");
        assert_eq!(render("toString [ 1 2 ]"), "\"1 2\"");
    }

    #[test]
    fn logic_and_comparison() {
        assert_eq!(render("true && false"), "false");
        assert_eq!(render("false || true"), "true");
        assert_eq!(render("false -> false"), "true");
        assert_eq!(render("1 < 2"), "true");
        assert_eq!(render("\"a\" < \"b\""), "true");
        assert_eq!(render("[ 1 2 ] == [ 1 2 ]"), "true");
        assert_eq!(render("{ a = 1; } == { a = 1; }"), "true");
        assert_eq!(render("assert 1 == 1; 42"), "42");
    }

    #[test]
    fn laziness() {
        assert_eq!(render("let boom = throw \"x\"; in 1"), "1");
        assert_eq!(render("[ (throw \"x\") ] == [ ]"), "false");
        assert_eq!(render("{ a = throw \"x\"; } ? a"), "true");
    }

    #[test]
    fn global_spelling_mirrors_cpp_registration() {
        // Registered "__length": bare `length` is undefined, __length and
        // builtins.length work. Registered "map": bare map works.
        assert_eq!(render("__length [ 1 2 ]"), "2");
        assert_eq!(render("builtins.length [ 1 2 ]"), "2");
        assert_eq!(
            render("length [ 1 2 ]"),
            "Eval(Eval, \"undefined variable 'length'\")"
        );
        assert_eq!(render("map (x: x + 1) [ 1 2 ]"), "[ 2 3 ]");
    }

    #[test]
    fn builtins_over_lists_attrsets_and_strings() {
        assert_eq!(
            render("builtins.attrNames { b = 1; a = 2; }"),
            "[ \"a\" \"b\" ]"
        );
        assert_eq!(render("builtins.foldl' (a: b: a + b) 0 [ 1 2 3 ]"), "6");
        assert_eq!(
            render("builtins.tryEval (throw \"x\")"),
            "{ success = false; value = false; }"
        );
        assert_eq!(
            render("builtins.tryEval 42"),
            "{ success = true; value = 42; }"
        );
        assert_eq!(render("builtins.substring 1 2 \"abcd\""), "\"bc\"");
        assert_eq!(
            render("builtins.replaceStrings [ \"o\" ] [ \"0\" ] \"foobar\""),
            "\"f00bar\""
        );
        assert_eq!(render("builtins.compareVersions \"1.0\" \"1.0.1\""), "-1");
        assert_eq!(render("builtins.compareVersions \"2.3pre1\" \"2.3\""), "-1");
        assert_eq!(
            render("builtins.sort builtins.lessThan [ 3 1 2 ]"),
            "[ 1 2 3 ]"
        );
        assert_eq!(render("builtins.typeOf 1.5"), "\"float\"");
        assert_eq!(
            render("builtins.functionArgs ({ a, b ? 1 }: a)"),
            "{ a = false; b = true; }"
        );
    }

    /// cppnix's `DrvName` split rule, transcribed case by case.
    ///
    /// The first two rows are cppnix's own unit test
    /// (`src/libexpr-tests/primops.cc:858`, `INSTANTIATE_TEST_SUITE_P(parseDrvName, ...)`);
    /// the next five are `tests/functional/lang/eval-okay-versions.nix`. The
    /// rest are the edge cases the loop in `src/libstore/names.cc:23` has and
    /// the prose does not: a trailing dash is not a separator because the
    /// condition includes `i + 1 < s.size()`, a doubled dash separates at the
    /// first of the two because the test is `!isalpha` rather than "is a
    /// digit", and `isalpha` is ASCII-only so a dash before a multi-byte
    /// character separates.
    ///
    /// Every expected string on the right is what
    /// `nix-instantiate --eval --strict -E` printed on nix
    /// 2.34.7+ix.h24085346
    /// (/nix/store/hgfhl6yjvzcng2qszc1j6s7spy2lvc72-nix-aarch64-apple-darwin-2.34.7+ix.h24085346),
    /// not what the rule was reasoned to imply.
    #[test]
    fn parse_drv_name_splits_at_the_first_dash_not_followed_by_a_letter() {
        let cases: &[(&str, &str)] = &[
            (
                "nix-0.12pre12876",
                r#"{ name = "nix"; version = "0.12pre12876"; }"#,
            ),
            (
                "a-b-c-1234pre5+git",
                r#"{ name = "a-b-c"; version = "1234pre5+git"; }"#,
            ),
            ("hello-1.0.2", r#"{ name = "hello"; version = "1.0.2"; }"#),
            ("hello", r#"{ name = "hello"; version = ""; }"#),
            (
                "915resolution-0.5.2",
                r#"{ name = "915resolution"; version = "0.5.2"; }"#,
            ),
            (
                "xf86-video-i810-1.7.4",
                r#"{ name = "xf86-video-i810"; version = "1.7.4"; }"#,
            ),
            (
                "name-that-ends-with-dash--1.0",
                r#"{ name = "name-that-ends-with-dash"; version = "-1.0"; }"#,
            ),
            // Trailing dash: `i + 1 < s.size()` fails, so nothing splits.
            ("hello-", r#"{ name = "hello-"; version = ""; }"#),
            ("-", r#"{ name = "-"; version = ""; }"#),
            ("", r#"{ name = ""; version = ""; }"#),
            // Doubled dash: the FIRST one separates and the second is kept.
            ("--", r#"{ name = ""; version = "-"; }"#),
            ("a--1", r#"{ name = "a"; version = "-1"; }"#),
            // Leading dash, so an empty name.
            ("-1", r#"{ name = ""; version = "1"; }"#),
            // A letter after the dash never separates, upper or lower.
            ("foo-bar", r#"{ name = "foo-bar"; version = ""; }"#),
            ("foo-Bar", r#"{ name = "foo-Bar"; version = ""; }"#),
            // Everything else does: digits, punctuation, underscore.
            ("foo-9", r#"{ name = "foo"; version = "9"; }"#),
            ("foo-_", r#"{ name = "foo"; version = "_"; }"#),
            ("foo-.", r#"{ name = "foo"; version = "."; }"#),
            // First match wins; later dashes stay in the version.
            ("a-b-1-c", r#"{ name = "a-b"; version = "1-c"; }"#),
            // No dash at all.
            ("1234", r#"{ name = "1234"; version = ""; }"#),
            ("1.2.3", r#"{ name = "1.2.3"; version = ""; }"#),
            // `isalpha` is ASCII in the C locale, so a dash before a
            // multi-byte character separates -- and the byte-indexed split
            // still lands on a character boundary, because the cut is at the
            // ASCII dash.
            ("foo-\u{e9}", "{ name = \"foo\"; version = \"\u{e9}\"; }"),
            ("caf\u{e9}-1", "{ name = \"caf\u{e9}\"; version = \"1\"; }"),
            // The dash here is followed by `a`, which IS a letter, so the
            // multi-byte name is left whole.
            ("\u{e9}-a", "{ name = \"\u{e9}-a\"; version = \"\"; }"),
        ];
        for (input, expected) in cases {
            let escaped = input.replace('\\', "\\\\").replace('"', "\\\"");
            let rendered = render(&format!(r#"builtins.parseDrvName "{escaped}""#));
            assert_eq!(&rendered.as_str(), expected, "parseDrvName {input:?}");
        }
    }

    /// A digit run wider than `int` is not a number to cppnix, and that
    /// inverts the comparison rather than rounding it.
    ///
    /// `componentsLT` (`src/libstore/names.cc:76`) asks `string2Int<int>`,
    /// which returns `nullopt` when `boost::lexical_cast` overflows, and then
    /// takes the "numeric beats non-numeric" branch -- so `"2147483648"` sorts
    /// BELOW `"1"`. This crate parsed with `i64` and answered the opposite.
    ///
    /// Each expectation is nix 2.34.7+ix.h24085346's answer. `"007"` is here
    /// because the fix must not overshoot into a stricter parse: boost accepts
    /// leading zeros, so `"007"` and `"7"` are still the same number.
    #[test]
    fn versions_past_int_max_are_not_numbers() {
        let cases: &[(&str, &str, &str)] = &[
            ("2147483647", "1", "1"),
            ("2147483648", "1", "-1"),
            ("2147483648", "2147483647", "-1"),
            ("2147483648", "2147483648", "0"),
            ("99999999999999", "10", "-1"),
            ("1.99999999999999", "1.a", "-1"),
            ("007", "7", "0"),
            ("1.007", "1.7", "0"),
        ];
        for (a, b, expected) in cases {
            assert_eq!(
                render(&format!(r#"builtins.compareVersions "{a}" "{b}""#)),
                *expected,
                "compareVersions {a:?} {b:?}"
            );
        }
    }

    /// Nesting far past what any host stack holds. Building, deep-forcing,
    /// printing and comparing each walk every level, and each recursed once
    /// per level in the old interpreter. Runs on a default 2 MiB test thread.
    ///
    /// `N` is under `max-call-depth` on purpose, and it used to be 100,000,
    /// which is over it. The old doc comment said the results were
    /// "cross-checked against the cppnix arm at small n" and that was the
    /// hole: at small n the arms agree, and at 100,000 cppnix refuses all
    /// four of these with `stack overflow; max-call-depth exceeded` while
    /// this backend answered. So the test was pinning three divergences
    /// (ENG-12900).
    ///
    /// 9,000 levels still proves everything this test exists for. No host
    /// stack holds 9,000 frames of an evaluator, and it is the deepest value
    /// a program can legally build, so the property is unchanged and every
    /// assertion now matches cppnix.
    #[test]
    fn deep_values_never_reach_the_host_stack() {
        // The read guard every test touching a process global takes. This one
        // did not, and it reads `max-call-depth`:
        // `the_flat_walks_read_the_call_depth_setting` lowers that ceiling to
        // 50 under the write lock, and a run that interleaved with its window
        // saw the lowered value and failed with "stack overflow;
        // max-call-depth exceeded". Intermittent, and it surfaced when
        // unrelated tests were added and changed the schedule. ENG-12916.
        let _held = globals_shared();
        const N: usize = 9_000;
        let build = "let f = n: if n == 0 then [ ] else [ (f (n - 1)) ]; in ";
        assert_eq!(
            render(&format!("{build} builtins.deepSeq (f {N}) \"ok\"")),
            "\"ok\""
        );
        // "[ ]" at the bottom, four more characters per level above it.
        let printed = render(&format!("{build} f {N}"));
        assert_eq!(printed.len(), 3 + 4 * N);
        assert!(printed.starts_with("[ [ [ "));
        assert_eq!(render(&format!("{build} (f {N}) == (f {N})")), "true");
        assert_eq!(
            render(&format!("{build} (f {N}) == (f {})", N - 1)),
            "false"
        );
        assert_eq!(render(&format!("{build} (f {N}) < (f {})", N - 1)), "false");
    }

    /// A dynamic component stops the static descent, and everything after it
    /// becomes a set hanging off a name nothing knows until run time. All
    /// four assertions are the corpus's own `.exp` text, so this is the same
    /// claim `lang-diff` makes, made where a one-file change can be caught in
    /// a second instead of a rebuild.
    #[test]
    fn a_dynamic_component_can_have_more_path_after_it() {
        // eval-okay-dynamic-attrs-2: two dynamic names under one static one,
        // which is the case that has to merge into a single `a` rather than
        // defining it twice.
        assert_eq!(
            render(r#"{ a."${"b"}" = true; a."${"c"}" = false; }.a.b"#),
            "true"
        );
        // eval-okay-dynamic-attrs `binds`: dynamic, then dynamic again.
        assert_eq!(
            render(r#"let a = "a"; b = "b"; in { "${a}"."${b}c" = true; }.a.bc"#),
            "true"
        );
        // eval-okay-dynamic-attrs `recBinds`: the NAME is not in the rec
        // scope but the VALUE is still evaluated inside it.
        assert_eq!(
            render(r#"let b = "b"; in (rec { "${b}" = a; a = true; }).b"#),
            "true"
        );
    }

    /// eval-okay-merge-dynamic-attrs, all four orderings. A static and a
    /// dynamic attribute landing in one set must merge whichever is written
    /// first, and a dynamic name must never be treated as a redefinition of
    /// a static one: nothing at compile time can tell whether they collide.
    #[test]
    fn static_and_dynamic_attributes_merge_in_either_order() {
        let src = r#"{
          set1 = { a = 1; }; set1 = { "${"b" + ""}" = 2; };
          set2 = { "${"b" + ""}" = 2; }; set2 = { a = 1; };
          set3.a = 1; set3."${"b" + ""}" = 2;
          set4."${"b" + ""}" = 2; set4.a = 1;
        }"#;
        assert_eq!(
            render(src),
            "{ set1 = { a = 1; b = 2; }; set2 = { a = 1; b = 2; }; \
set3 = { a = 1; b = 2; }; set4 = { a = 1; b = 2; }; }"
        );
    }

    /// A dynamic name in a `let` is still a parse error, path or no path.
    /// cppnix rejects it because a binding nothing can name at compile time
    /// cannot be brought into scope, and widening the insertion path must not
    /// have opened a way around that.
    #[test]
    fn a_dynamic_binding_is_still_refused_in_a_let() {
        assert_eq!(
            render(r#"let "${"a"}".b = 1; in 2"#),
            r#"Parse("dynamic attributes not allowed in let")"#
        );
        assert_eq!(
            render(r#"let "${"a"}" = 1; in 2"#),
            r#"Parse("dynamic attributes not allowed in let")"#
        );
    }

    /// The other unbounded shape: a call chain rather than a value nest. The
    /// fold is 100k applications in one builtin, and `f` is 100k pending
    /// additions each waiting on the next call.
    ///
    /// The second half runs with the ceiling lifted, and the distinction is
    /// the point: 100k nested calls is past `max-call-depth`, so cppnix
    /// refuses it too (which is why the corpus ships
    /// `eval-okay-tail-call-1.exp-disabled`). What this asserts is that the
    /// only thing standing in the way is that policy number -- raise it and
    /// the chain runs, because the frames are on the heap and the host stack
    /// never enters into it. The fold needs no lifting: it is 100k calls one
    /// after another, two frames deep.
    #[test]
    fn long_call_chains_never_reach_the_host_stack() {
        assert_eq!(
            render("builtins.foldl' (a: b: a + b) 0 (builtins.genList (i: i) 100000)"),
            "4999950000"
        );
        assert_eq!(
            eval_str_at_with_depth(
                "let f = n: if n == 0 then 0 else 1 + f (n - 1); in f 100000",
                ".",
                compile::Origin::String,
                &Settings::default(),
                200_000
            )
            .map_err(|e| format!("{e:?}")),
            Ok("100000".to_owned())
        );
    }

    /// The two flat walks honour `max-call-depth`, rather than a constant
    /// that happens to equal its default.
    ///
    /// This is the "a setting is not a capability" shape: the ceiling was a
    /// hard-coded 10,000, so `nix config show max-call-depth` reported
    /// whatever you set and both walks ignored it. Measured, not guessed --
    /// `--max-call-depth 100000` changed nothing on either walk until this
    /// was fixed.
    ///
    /// Both directions, because a walk that refused everything would satisfy
    /// a lowered ceiling and tell you nothing.
    #[test]
    fn the_flat_walks_read_the_call_depth_setting() {
        let build = "let f = n: if n == 0 then [ ] else [ (f (n - 1)) ]; in ";
        // The ceiling is two values, not two moments. This test lowering the
        // process-global ceiling and putting it back is the exact pair
        // ENG-12939 opens with: `deep_values_never_reach_the_host_stack` read
        // the same global with no guard and saw the lowered 50 whenever the
        // scheduler interleaved them.
        let lowered = Settings {
            max_call_depth: 50,
            ..Settings::default()
        };
        let standard = Settings::default();

        for src in [
            format!("{build} builtins.deepSeq (f 200) \"ok\""),
            format!("{build} builtins.toJSON (f 200)"),
            format!("{build} builtins.toXML (f 200)"),
        ] {
            let got = render_under(&lowered, &src);
            assert!(
                got.contains("stack overflow; max-call-depth exceeded"),
                "a lowered ceiling must bite: {src} gave {got}"
            );
        }

        let got = render_under(
            &standard,
            &format!("{build} builtins.deepSeq (f 200) \"ok\""),
        );
        assert_eq!(got, "\"ok\"", "the same value must pass under the default");
    }

    /// Two walks that still answer where cppnix refuses, pinned so the gap is
    /// recorded rather than merely absent.
    ///
    /// Printing and `==` are flat worklists with no depth counter, so at
    /// 100,000 levels cppnix reports `stack overflow; max-call-depth
    /// exceeded` and this backend produces a value. `deepSeq` was the third
    /// and is fixed; these two are not, because they run on every evaluation
    /// and three behaviour changes behind one review is how one of them goes
    /// unnoticed. ENG-12900.
    ///
    /// **This test is meant to fail when they are fixed.** It asserts the
    /// wrong-but-current behaviour on purpose. A gap left as an absence is
    /// invisible; a gap left as a red test when it closes is a reminder to
    /// delete the test and raise the bar.
    #[test]
    fn printing_and_equality_still_have_no_call_depth_ceiling() {
        const OVER: usize = 100_000;
        let build = "let f = n: if n == 0 then [ ] else [ (f (n - 1)) ]; in ";

        let printed = render(&format!("{build} f {OVER}"));
        assert!(
            printed.starts_with("[ [ [ "),
            "cppnix refuses to print this; if the ceiling now covers the printer, \
             delete this test and fold the case into the one above. Got: {}",
            &printed[..printed.len().min(60)]
        );

        let compared = render(&format!("{build} (f {OVER}) == (f {OVER})"));
        assert_eq!(
            compared, "true",
            "cppnix refuses this comparison; if the ceiling now covers equality, \
             delete this test and fold the case into the one above"
        );

        // The one that IS fixed, at the same nesting, so this test also shows
        // the three are no longer the same story.
        let sequenced = render(&format!("{build} builtins.deepSeq (f {OVER}) \"ok\""));
        assert!(
            sequenced.contains("stack overflow; max-call-depth exceeded"),
            "deepSeq should refuse past the ceiling; got: {sequenced}"
        );
    }

    /// `deepSeq` refuses a value nested past `max-call-depth`, as cppnix's
    /// `forceValueDeep` does by opening every level with `addCallDepth`
    /// (`eval.cc:2421`).
    ///
    /// This walk is flat, so nothing stops it on its own: before ENG-12900 it
    /// answered `1` for a 20,000-deep list that cppnix refuses with
    /// `stack overflow; max-call-depth exceeded`, which is a value divergence
    /// and not a wording one.
    ///
    /// The shallow row is what makes the deep row mean something. A ceiling
    /// applied one level too eagerly would refuse ordinary values and satisfy
    /// any single-row version of this test.
    #[test]
    fn deep_seq_refuses_a_value_nested_past_the_call_depth() {
        let shallow = render(
            "let f = n: if n == 0 then [] else [ (f (n - 1)) ]; in builtins.deepSeq (f 100) 1",
        );
        assert_eq!(shallow, "1");

        let deep = render(
            "let f = n: if n == 0 then [] else [ (f (n - 1)) ]; in builtins.deepSeq (f 20000) 1",
        );
        assert!(
            deep.contains("stack overflow; max-call-depth exceeded"),
            "want cppnix's refusal; got: {deep}"
        );
    }

    /// `builtins.toJSON` and `builtins.deepSeq` are two flat walks with two
    /// depth counters, because folding `deepSeq` into the shared
    /// strict-deep-walk driver would have given it an output buffer it never
    /// writes (see `maintainers/ix/strict-deep-walk.md`). The cost of that
    /// choice is that the ceiling has two readers, and this is what keeps
    /// them from drifting: both refuse at the same nesting.
    #[test]
    fn the_two_flat_walks_share_one_ceiling() {
        let deep = "let f = n: if n == 0 then [] else [ (f (n - 1)) ]; in f 20000";
        for src in [
            format!("builtins.deepSeq ({deep}) 1"),
            format!("builtins.toJSON ({deep})"),
        ] {
            let got = render(&src);
            assert!(
                got.contains("stack overflow; max-call-depth exceeded"),
                "{src} should refuse at the shared ceiling; got: {got}"
            );
        }
    }

    /// cppnix's forceValueDeep carries a seen-set so a cyclic attrset bottoms
    /// out; without one this is an infinite descent, which is what made
    /// eval-okay-deepseq the corpus's only crash.
    #[test]
    fn deep_seq_terminates_on_a_self_referential_attrset() {
        assert_eq!(
            render("builtins.deepSeq (let as = { x = 123; y = as; }; in as) 456"),
            "456"
        );
    }

    /// deepSeq finishes the first argument before it looks at the second, so
    /// a throw buried in the deep walk beats a throw sitting in the result.
    #[test]
    fn deep_seq_reports_the_deep_failure_first() {
        assert_eq!(
            render("builtins.deepSeq [ (throw \"deep\") ] (throw \"result\")"),
            "Eval(Thrown, \"deep\")"
        );
    }

    /// The `to` side of replaceStrings stays lazy: cppnix only forces the
    /// replacements it actually uses.
    #[test]
    fn replace_strings_leaves_unused_replacements_unforced() {
        assert_eq!(
            render(
                "builtins.replaceStrings [ \"oo\" \"XX\" ] [ \"u\" (throw \"unreachable\") ] \"foobar\""
            ),
            "\"fubar\""
        );
    }

    /// cppnix's eqValues bookends: the same cell equals itself whatever it
    /// holds, and functions equal nothing. Only both together give `f == f`
    /// false and `[ f ] == [ f ]` true, and the second needs the compiler to
    /// pass a bare variable as its own slot rather than a fresh thunk.
    #[test]
    fn function_equality_follows_cell_identity() {
        assert_eq!(render("let f = x: x; in f == f"), "false");
        assert_eq!(render("let f = x: x; in [ f ] == [ f ]"), "true");
        assert_eq!(render("let f = x: x; in { a = f; } == { a = f; }"), "true");
        assert_eq!(render("(x: x) == (x: x)"), "false");
        // Distinct cells holding equal data are still equal.
        assert_eq!(render("let a = [ 1 ]; b = [ 1 ]; in a == b"), "true");
    }

    /// cppnix builds map/genList/mapAttrs results with mkApp, so the function
    /// runs only when an element is forced. eval-okay-intersectAttrs is the
    /// corpus case: it maps `throw` over a set and never looks at the values.
    #[test]
    fn mapped_functions_do_not_run_until_forced() {
        assert_eq!(
            render("builtins.attrNames (builtins.mapAttrs throw { a = 1; })"),
            "[ \"a\" ]"
        );
        assert_eq!(render("builtins.length (builtins.map throw [ 1 2 ])"), "2");
        assert_eq!(render("builtins.length (builtins.genList throw 3)"), "3");
        assert_eq!(
            render("builtins.mapAttrs (n: v: n + v) { a = \"1\"; }"),
            "{ a = \"a1\"; }"
        );
    }

    /// cppnix's parser merges attrpaths that share a prefix, and merges two
    /// bindings of one name when both values are set literals. The `rec` of
    /// the FIRST set covers whatever is merged in later (NixOS/nix#9020).
    #[test]
    fn attrpaths_and_set_literals_merge() {
        assert_eq!(
            render("{ a.b = 1; a.c = 2; }"),
            "{ a = { b = 1; c = 2; }; }"
        );
        assert_eq!(
            render("{ a = { b = 1; }; a = { c = 2; }; }"),
            "{ a = { b = 1; c = 2; }; }"
        );
        assert_eq!(render("{ a.b.c = 1; }.a.b.c"), "1");
        assert_eq!(render("(let a.b = 1; in a).b"), "1");
        assert_eq!(
            render("{ a = rec { b = c + 1; d = 2; }; a.c = d + 3; }.a.b"),
            "6"
        );
        // Two non-set values under one name is still a redefinition.
        assert_eq!(
            render("{ a = 1; a = 2; }"),
            "Parse(\"attribute 'a' already defined\")"
        );
    }

    /// `let { body = …; }`: cppnix's pre-`let ... in` syntax, defined as the
    /// `body` attribute of the equivalent rec set.
    #[test]
    fn legacy_let_is_the_body_of_a_rec_set() {
        assert_eq!(render("let { body = a; a = 1; }"), "1");
        assert_eq!(render("let { body = x + y; x = 1; y = x + 1; }"), "3");
    }

    /// `inherit x` in a rec scope takes the OUTER x; joining the frame it is
    /// being added to would make it recurse on itself.
    #[test]
    fn rec_inherit_resolves_outside_its_own_frame() {
        assert_eq!(
            render("let x = 1; in (rec { inherit x; y = x + 1; }).y"),
            "2"
        );
        assert_eq!(render("let x = 1; in { inherit x; }.x"), "1");
    }

    /// A path interpolated into a string is the STORE path, not the source
    /// path. cppnix coerces it with `copyToStore` (eval.cc:2582) and this
    /// backend used to hand back the source path instead, which is a wrong
    /// answer rather than a missing feature: for a path that exists the
    /// expression succeeded and the value was wrong, and the lang corpus
    /// could not see it because it runs with `NIX_REMOTE=dummy://` and only
    /// ever interpolates paths that do not exist (ENG-12447).
    #[test]
    fn a_path_in_a_string_is_the_store_path_it_copies_to() {
        use crate::host::{FileType, Host, StoreError};

        /// Answers a copy the way a store would: one path in, its store path
        /// out, and a refusal for anything absent.
        struct Store;
        impl Host for Store {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                match path {
                    p if p.path.as_ref() == "/m/f" => Ok("hi".to_owned()),
                    _ => Err(format!("path '{path}' does not exist")),
                }
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(self.read_file(path).is_ok())
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                match path {
                    p if self.path_exists_checked(p)? => Ok(Some(FileType::Regular)),
                    p => Err(format!("path '{p}' does not exist")),
                }
            }
            fn copy_to_store(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, StoreError> {
                match path {
                    p if p.path.as_ref() == "/m/f" => {
                        Ok("/nix/store/00000000000000000000000000000000-f".to_owned())
                    }
                    p => Err(StoreError::Failed(format!("path '{p}' does not exist"))),
                }
            }
        }

        fn run(src: &str) -> String {
            let Ok(module) = compile::compile_source(
                src,
                "/m",
                crate::compile::Origin::String,
                &crate::eval::Settings::default(),
            ) else {
                return "compile failed".to_owned();
            };
            let module = Rc::new(module);
            let mut vm = Vm::with_settings(crate::eval::Settings::default());
            vm.start_module(&module);
            let v = match drive(&mut vm, &Store) {
                Ok(v) => v,
                Err(e) => return format!("{e:?}"),
            };
            vm.start_print(v);
            match drive(&mut vm, &Store) {
                Ok(Value::Str(s)) => s.expect_text(),
                other => format!("{other:?}"),
            }
        }

        let store_f = "/nix/store/00000000000000000000000000000000-f";
        assert_eq!(run(r#""${/m/f}""#), format!("\"{store_f}\""));
        // Every part gets its own copy, and the literal text between them
        // survives: this is the shape `eval-fail-nonexist-path` has.
        assert_eq!(run(r#""${/m/f}/xyzzy""#), format!("\"{store_f}/xyzzy\""));
        assert_eq!(
            run(r#""${/m/f}${/m/f}""#),
            format!("\"{store_f}{store_f}\"")
        );
        // `+` is the same coercion when a string starts it, and NOT when a
        // path does: `path + string` stays a path and copies nothing.
        assert_eq!(run(r#""" + /m/f"#), format!("\"{store_f}\""));
        assert_eq!(run(r#"/m/f + "/xyzzy""#), "/m/f/xyzzy");
        // toString does not copy; cppnix passes copyToStore = false there.
        assert_eq!(run("builtins.toString /m/f"), "\"/m/f\"");
        // A path that cannot be copied fails with the store\'s wording, which
        // is where `eval-fail-nonexist-path` gets its error.
        assert!(
            run(r#""${/m/nope}""#).contains("path \'/m/nope\' does not exist"),
            "unexpected: {}",
            run(r#""${/m/nope}""#)
        );
    }

    /// The store path a string coerces to is also what the string depends
    /// on, and a concatenation depends on everything its parts did. Nothing
    /// in the language can read a context yet (`builtins.getContext` is
    /// unimplemented, ENG-12465), so this is asserted through the value
    /// rather than through an expression's output.
    #[test]
    fn a_coerced_path_is_carried_as_the_string_s_context() {
        use crate::host::{FileType, Host, StoreError};
        use crate::value2::ContextElem;

        struct Store;
        impl Host for Store {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Ok(String::new())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
            fn copy_to_store(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, StoreError> {
                Ok(format!("/nix/store/hash{}", path.replace('/', "-")))
            }
        }

        fn context_of(src: &str) -> Vec<String> {
            let Ok(module) = compile::compile_source(
                src,
                "/m",
                crate::compile::Origin::String,
                &crate::eval::Settings::default(),
            ) else {
                return vec!["compile failed".to_owned()];
            };
            let mut vm = Vm::with_settings(crate::eval::Settings::default());
            vm.start_module(&Rc::new(module));
            match drive(&mut vm, &Store) {
                Ok(Value::Str(s)) => s
                    .context()
                    .map(|c| {
                        c.iter()
                            .map(|e| match e {
                                ContextElem::Opaque(p) => p.to_string(),
                                ContextElem::DrvDeep(p) => format!("={p}"),
                                ContextElem::Built { drv, output } => format!("!{output}!{drv}"),
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
                other => vec![format!("unexpected: {other:?}")],
            }
        }

        assert_eq!(context_of(r#""${/m/a}""#), vec!["/nix/store/hash-m-a"]);
        // Both parts, deduplicated and ordered, which is what a derivation's
        // inputSrcs will be read off.
        assert_eq!(
            context_of(r#""${/m/a} and ${/m/b} and ${/m/a}""#),
            vec!["/nix/store/hash-m-a", "/nix/store/hash-m-b"]
        );
        // `+` unions too, and a string with no paths in it has no context.
        assert_eq!(context_of(r#""" + /m/a"#), vec!["/nix/store/hash-m-a"]);
        assert_eq!(context_of(r#""plain ${"string"}""#), Vec::<String>::new());
        // toString does not copy, so it does not create a dependency either.
        assert_eq!(context_of("builtins.toString /m/a"), Vec::<String>::new());
    }

    /// The coercion's depth bound counts depth, not values visited.
    ///
    /// cppnix takes an `addCallDepth` guard on entry to `coerceToString`
    /// (`eval-inline.hh:200`) whose destructor runs when that value's coercion
    /// returns, so two elements of one list are siblings that cost each other
    /// nothing. A counter on the walk instead counts breadth as depth, and
    /// that got both directions wrong at once: it refused a wide list cppnix
    /// accepts, and, because it only counted attribute sets, it accepted a
    /// deep list cppnix refuses.
    ///
    /// The numbers here are cppnix's, taken from
    /// `nix-instantiate (Nix) 2.34.7+ix.g69e4d9e9db39.h950203b1` on the same
    /// expressions rather than derived from the bound.
    #[test]
    fn the_coercion_depth_bound_counts_depth_and_not_values_visited() {
        // No host: none of these expressions touches the filesystem or the
        // store, which is what makes them a clean test of the bound alone.
        fn run(src: &str) -> String {
            match super::eval_str_with(src, "/m", compile::Origin::String, &Settings::default()) {
                Ok(v) => v,
                Err(e) => format!("{e:?}"),
            }
        }

        // 12,000 sets in one list, each one hop deep. Wide, not deep, so it
        // is nowhere near the bound of 10,000 and cppnix answers 23999.
        assert_eq!(
            run(
                r#"builtins.stringLength (builtins.toString (builtins.genList (i: { outPath = "x"; }) 12000))"#
            ),
            "23999",
            "a wide list must not be read as a deep one"
        );
        // Under the bound on any reading, so it pins the assertion above to
        // the width and not to some other difference between the two.
        assert_eq!(
            run(
                r#"builtins.stringLength (builtins.toString (builtins.genList (i: { outPath = "x"; }) 9000))"#
            ),
            "17999"
        );
        // 20,000 nested lists. Deep, and cppnix refuses it: a list coerces
        // element by element through a recursive call, so the nesting is
        // frames. Counting only attribute sets missed this entirely.
        assert!(
            run(
                r#"builtins.stringLength (builtins.toString (builtins.foldl' (acc: _: [ acc ]) "x" (builtins.genList (i: i) 20000)))"#
            )
            .contains("stack overflow; max-call-depth exceeded"),
            "a deep list must exhaust the budget"
        );
    }

    /// `concatStringsSep` coerces its elements, which is what cppnix's
    /// `coerceToString` does to them (`primops.cc:5127`) and what the Rust
    /// backend used to refuse: it matched a string and a path and rejected
    /// everything else, so a list of derivations -- the way nixpkgs turns a
    /// package list into shell words -- failed to evaluate. The refusal cost
    /// roughly one top-level nixpkgs attribute in nine (ENG-12628).
    #[test]
    fn concat_strings_sep_coerces_the_way_cppnix_does() {
        use crate::host::{FileType, Host, StoreError};

        struct Store;
        impl Host for Store {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Ok(String::new())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
            fn copy_to_store(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, StoreError> {
                Ok(format!("/nix/store/h{}", path.replace('/', "-")))
            }
        }

        fn run(src: &str) -> String {
            let Ok(module) = compile::compile_source(
                src,
                "/m",
                crate::compile::Origin::String,
                &crate::eval::Settings::default(),
            ) else {
                return "compile failed".to_owned();
            };
            let mut vm = Vm::with_settings(crate::eval::Settings::default());
            vm.start_module(&Rc::new(module));
            let value = match drive(&mut vm, &Store) {
                Ok(v) => v,
                Err(VmError::Unimplemented(w)) => return format!("unimplemented: {w}"),
                Err(e) => return format!("{e:?}"),
            };
            vm.start_print(value);
            match drive(&mut vm, &Store) {
                Ok(Value::Str(s)) => s.expect_text(),
                Ok(other) => format!("{other:?}"),
                Err(e) => format!("{e:?}"),
            }
        }

        // A set coerces through `outPath`, which is how a derivation does it
        // and therefore how `concatStringsSep " " buildInputs` works at all.
        assert_eq!(
            run(r#"builtins.concatStringsSep "-" [ { outPath = "/a"; } { outPath = "/b"; } ]"#),
            r#""/a-/b""#
        );
        // `__toString` first, then `outPath`: cppnix's `tryAttrsToString`
        // runs before the `outPath` lookup, so a set carrying both uses the
        // function.
        assert_eq!(
            run(r#"builtins.concatStringsSep "," [ { __toString = self: "T"; outPath = "O"; } ]"#),
            r#""T""#
        );
        // The coercion is a tail call in cppnix, so an `outPath` that is
        // itself a set keeps going rather than stopping at "a set".
        assert_eq!(
            run(r#"builtins.concatStringsSep "" [ { outPath = { outPath = "deep"; }; } ]"#),
            r#""deep""#
        );
        // A path element is copied to the store, because cppnix leaves
        // `copyToStore` at its default here. The source path would be a
        // plausible wrong answer that no downstream check could catch.
        assert_eq!(
            run(r#"builtins.concatStringsSep ":" [ /m/a /m/b ]"#),
            r#""/nix/store/h-m-a:/nix/store/h-m-b""#
        );
        assert_eq!(
            run(r#"builtins.hasContext (builtins.concatStringsSep ":" [ /m/a ])"#),
            "true"
        );
        // The separator's context is copied before any element is looked at,
        // so an empty list still depends on what the separator depended on.
        assert_eq!(
            run(r#"builtins.hasContext (builtins.concatStringsSep "${/m/a}" [ ])"#),
            "true"
        );
        assert_eq!(run(r#"builtins.concatStringsSep "${/m/a}" [ ]"#), r#""""#);

        // What still does not coerce, with cppnix's wording. `coerceMore` is
        // off here, so the list and the integer are errors even though
        // `toString` accepts both.
        assert!(run(r#"builtins.concatStringsSep "" [ 1 ]"#).contains("cannot coerce an integer"));
        assert!(
            run(r#"builtins.concatStringsSep "" [ [ "a" ] ]"#).contains("cannot coerce a list")
        );
        assert!(
            run(r#"builtins.concatStringsSep "" [ { a = 1; } ]"#).contains("cannot coerce a set")
        );
        // The separator is `forceString`, not `coerceToString`: a path there
        // is a type error rather than a store copy.
        assert!(run(r#"builtins.concatStringsSep /m/a [ "x" ]"#).contains("expected a string"));

        // Element by element, in order. cppnix coerces each element as it
        // reaches it, so the integer fails before the `throw` is forced; the
        // force-the-whole-list-first shape this replaced reported "later".
        assert!(
            run(r#"builtins.concatStringsSep "" [ 1 (throw "later") ]"#)
                .contains("cannot coerce an integer"),
            "elements must be coerced in order, not forced as a batch"
        );

        // The bound is live: one element nested deeper than `max-call-depth`
        // overflows, with cppnix's wording. Without this the next assertion
        // would pass whether the budget reset per element or was never
        // enforced at all.
        let deep = run(
            r#"builtins.concatStringsSep "" [ (builtins.foldl' (acc: _: { outPath = acc; }) "x" (builtins.genList (i: i) 10050)) ]"#,
        );
        assert!(
            deep.contains("stack overflow; max-call-depth exceeded"),
            "one deep element must exhaust the budget: {deep}"
        );

        // The elements are siblings, not a chain, so a long list is wide and
        // not deep: cppnix makes a separate `coerceToString` call per element
        // and each one's depth guard is released before the next is taken.
        // 12,000 is over the default bound of 10,000 on purpose -- a counter
        // that added elements up would refuse this.
        assert_eq!(
            run(
                r#"builtins.stringLength (builtins.concatStringsSep "" (builtins.genList (i: { outPath = "x"; }) 12000))"#
            ),
            "12000"
        );
    }

    /// What a program can now see of a context, and what it must not.
    ///
    /// The verdicts are cppnix's, taken one builtin at a time out of
    /// `primops.cc`: a builtin whose result is a string built from another
    /// one keeps the dependency, and a builtin cppnix forces with
    /// `forceStringNoCtx` refuses it rather than losing it. Both halves are
    /// asserted here because dropping a context silently is the failure mode
    /// this whole mechanism exists to prevent (ENG-12447, ENG-12465).
    #[test]
    fn string_builtins_keep_or_refuse_a_context_the_way_cppnix_does() {
        use crate::host::{FileType, Host, StoreError};

        struct Store;
        impl Host for Store {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Ok(String::new())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
            fn copy_to_store(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, StoreError> {
                Ok(format!("/nix/store/h{}", path.replace('/', "-")))
            }
        }

        fn run(src: &str) -> String {
            let Ok(module) = compile::compile_source(
                src,
                "/m",
                crate::compile::Origin::String,
                &crate::eval::Settings::default(),
            ) else {
                return "compile failed".to_owned();
            };
            let mut vm = Vm::with_settings(crate::eval::Settings::default());
            vm.start_module(&Rc::new(module));
            let value = match drive(&mut vm, &Store) {
                Ok(v) => v,
                Err(VmError::Unimplemented(w)) => return format!("unimplemented: {w}"),
                Err(e) => return format!("{e:?}"),
            };
            vm.start_print(value);
            match drive(&mut vm, &Store) {
                Ok(Value::Str(s)) => s.expect_text(),
                Ok(other) => format!("{other:?}"),
                Err(e) => format!("{e:?}"),
            }
        }

        let p = "/nix/store/h-m-f";

        // The dependency is visible, and it is the store path, not the source.
        assert_eq!(
            run(r#"builtins.getContext "${/m/f}""#),
            format!("{{ \"{p}\" = {{ path = true; }}; }}")
        );
        assert_eq!(run(r#"builtins.hasContext "${/m/f}""#), "true");
        assert_eq!(run(r#"builtins.hasContext "plain""#), "false");

        // Discarding leaves the bytes alone. This is `eval-okay-context`.
        assert_eq!(
            run(r#"builtins.unsafeDiscardStringContext "${/m/f}""#),
            format!("\"{p}\"")
        );
        assert_eq!(
            run(r#"builtins.hasContext (builtins.unsafeDiscardStringContext "${/m/f}")"#),
            "false"
        );
        // And a set, which cppnix's own doc calls out: "a value that can be
        // coerced to a string" (`context.cc:21`). Both ways in, because
        // `__toString` and `outPath` are separate arms of the coercion.
        for expr in [
            r#"builtins.unsafeDiscardStringContext { outPath = "${/m/f}"; }"#,
            r#"builtins.unsafeDiscardStringContext { __toString = self: "${/m/f}"; }"#,
        ] {
            assert_eq!(run(expr), format!("\"{p}\""), "coercion refused by: {expr}");
            assert_eq!(
                run(&format!("builtins.hasContext ({expr})")),
                "false",
                "context kept by: {expr}"
            );
        }

        // Propagating builtins. Each of these would be a lost dependency in a
        // derivation if it dropped the context instead.
        for expr in [
            r#"builtins.substring 0 3 "${/m/f}""#,
            r#"builtins.substring 0 0 "${/m/f}""#,
            r#"builtins.concatStringsSep "-" [ "a" "${/m/f}" ]"#,
            r#"builtins.replaceStrings [ "x" ] [ "y" ] "${/m/f}""#,
            r#"builtins.toString "${/m/f}""#,
            r#"builtins.toString [ "a" "${/m/f}" ]"#,
            r#"baseNameOf "${/m/f}""#,
            r#"dirOf "${/m/f}""#,
            r#"builtins.toJSON { a = "${/m/f}"; }"#,
            r#""prefix" + "${/m/f}""#,
        ] {
            assert_eq!(
                run(&format!("builtins.hasContext ({expr})")),
                "true",
                "context lost by: {expr}"
            );
        }

        // Refusing builtins, with cppnix's forceStringNoCtx wording. One
        // entry per `forceStringNoCtx` site in cppnix that this crate
        // implements, enumerated from `rg forceStringNoCtx src/libexpr`
        // rather than from the ones that came to mind: the gap this closes
        // was three builtins that had no verdict at all while `getContext`
        // could already see the consequences.
        for expr in [
            r#"builtins.hashString "sha256" "${/m/f}""#,
            // The algorithm argument is forceStringNoCtx too, not just the subject.
            r#"builtins.hashString "${/m/f}" "x""#,
            r#"builtins.fromJSON "${/m/f}""#,
            r#"builtins.fromTOML "${/m/f}""#,
            r#"builtins.splitVersion "${/m/f}""#,
            r#"builtins.compareVersions "${/m/f}" "1.0""#,
            r#"builtins.compareVersions "1.0" "${/m/f}""#,
            r#"builtins.parseDrvName "${/m/f}""#,
            r#"builtins.getAttr "${/m/f}" { }"#,
            r#"builtins.hasAttr "${/m/f}" { }"#,
            r#"builtins.removeAttrs { } [ "${/m/f}" ]"#,
            r#"builtins.listToAttrs [ { name = "${/m/f}"; value = 1; } ]"#,
            r#"builtins.catAttrs "${/m/f}" [ ]"#,
            r#"builtins.groupBy (x: "${/m/f}") [ 1 ]"#,
            r#"builtins.getEnv "${/m/f}""#,
            // The pattern of both regex builtins, never the subject.
            r#"builtins.match "${/m/f}" "x""#,
            r#"builtins.split "${/m/f}" "x""#,
            // The two language-level names, which are not builtins at all:
            // cppnix's `getName` (eval.cc:247) for a dynamic select, and
            // eval.cc:1434 for a dynamic binding. A backend that refused
            // every builtin and accepted these would still disagree.
            r#"({ a = 1; }).${"${/m/f}"}"#,
            r#"({ a = 1; }) ? ${"${/m/f}"}"#,
            r#"{ ${"${/m/f}"} = 1; }"#,
        ] {
            let got = run(expr);
            assert!(
                got.contains("is not allowed to refer to a store path"),
                "should have refused: {expr}, got {got}"
            );
        }

        // A subject with a context is fine where only the pattern is
        // restricted, and the captures carry none, as cppnix's do.
        assert_eq!(run(r#"builtins.match ".*" "${/m/f}" != null"#), "true");
        // "zzz" and not "x": the store path contains the x in "/nix/".
        assert_eq!(
            run(r#"builtins.length (builtins.split "zzz" "${/m/f}")"#),
            "1"
        );
        // A context-bearing string is still a fine attribute *value*, and a
        // static name is unaffected: the refusal is scoped to names, so a
        // whole-language rule would be too strong.
        assert_eq!(run(r#"builtins.hasContext { a = "${/m/f}"; }.a"#), "true");
        assert_eq!(run(r#"{ a = 1; }.${"a"}"#), "1");
        // stringLength coerces with context and returns an int, so it neither
        // propagates nor refuses.
        assert_eq!(
            run(r#"builtins.stringLength "${/m/f}""#),
            p.len().to_string()
        );

        // Printing shows bytes and never a context, which is why
        // `nix-instantiate --eval` output is unchanged by any of this.
        assert_eq!(run(r#""${/m/f}""#), format!("\"{p}\""));
    }

    /// Without a store behind it the coercion reports the gap instead of
    /// answering. Unimplemented, never a value: a wrong store path is
    /// indistinguishable from a right one downstream.
    #[test]
    fn a_path_in_a_string_needs_a_store_and_says_so_without_one() {
        use crate::host::{FileType, Host};

        struct NoStore;
        impl Host for NoStore {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Err(format!("path \'{path}\' does not exist"))
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Ok(Some(FileType::Regular))
            }
        }

        // Rendered rather than asserted branch by branch: the workspace
        // denies `panic`, tests included, so a test says what happened and
        // compares it.
        let outcome = match compile::compile_source(
            r#""${/m/f}""#,
            "/m",
            crate::compile::Origin::String,
            &crate::eval::Settings::default(),
        ) {
            Err(e) => format!("compile failed: {e:?}"),
            Ok(module) => {
                let mut vm = Vm::with_settings(crate::eval::Settings::default());
                vm.start_module(&Rc::new(module));
                match drive(&mut vm, &NoStore) {
                    Err(VmError::Unimplemented(refusal)) => refusal.detail,
                    Err(e) => format!("wrong error: {e:?}"),
                    Ok(v) => format!("answered {v:?} with no store to answer from"),
                }
            }
        };
        assert_eq!(
            outcome,
            "interpolating a path into a string (no store behind this evaluator)"
        );
    }

    /// The scheduler answers path questions; the VM never reads a file. A
    /// host that resolves everything from memory proves the seam holds, and
    /// is what an effects-kernel host will replace.
    #[test]
    fn the_vm_reads_files_only_through_the_host() {
        use crate::host::{FileType, Host};

        struct Fake;
        impl Host for Fake {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                match path {
                    p if p.path.as_ref() == "/m/lib.nix" => Ok("{ id = x: x; n = 7; }".to_owned()),
                    p if p.path.as_ref() == "/m/dir/default.nix" => {
                        Ok("import /m/lib.nix".to_owned())
                    }
                    _ => Err(format!("path '{path}' does not exist")),
                }
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(vec![("a".to_owned(), FileType::Regular)])
            }
            fn path_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(self.read_file(path).is_ok())
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                match path {
                    p if p.path.as_ref() == "/m/dir" => Ok(Some(FileType::Directory)),
                    p if self.path_exists_checked(p)? => Ok(Some(FileType::Regular)),
                    p => Err(format!("path '{p}' does not exist")),
                }
            }
        }

        fn run(src: &str) -> String {
            let Ok(module) = compile::compile_source(
                src,
                "/m",
                crate::compile::Origin::String,
                &crate::eval::Settings::default(),
            ) else {
                return "compile failed".to_owned();
            };
            let module = Rc::new(module);
            let mut vm = Vm::with_settings(crate::eval::Settings::default());
            vm.start_module(&module);
            let v = match drive(&mut vm, &Fake) {
                Ok(v) => v,
                Err(e) => return format!("{e:?}"),
            };
            vm.start_print(v);
            match drive(&mut vm, &Fake) {
                Ok(Value::Str(s)) => s.expect_text(),
                other => format!("{other:?}"),
            }
        }

        assert_eq!(run("(import /m/lib.nix).n"), "7");
        assert_eq!(run("(import /m/lib.nix).id 1"), "1");
        // A directory imports its default.nix, and the imported file's own
        // relative paths resolve against the RESOLVED file's directory.
        assert_eq!(run("(import /m/dir).n"), "7");
        assert_eq!(run("builtins.pathExists /m/lib.nix"), "true");
        assert_eq!(run("builtins.pathExists /m/nope.nix"), "false");
        assert_eq!(run("builtins.readFileType /m/dir"), "\"directory\"");
        assert_eq!(run("builtins.attrNames (builtins.readDir /m)"), "[ \"a\" ]");
        // One compile per file however many times it is imported.
        assert_eq!(
            run("let a = import /m/lib.nix; b = import /m/lib.nix; in a.n + b.n"),
            "14"
        );
    }

    /// foldl' is strict in the accumulator it produces, not the one it is
    /// handed: the machine's argument forcing is per-argument for exactly
    /// this, since `foldl'` forces its function and its list but not its nul.
    #[test]
    fn foldl_strict_leaves_the_initial_accumulator_alone() {
        assert_eq!(
            render("builtins.foldl' (_: x: x) (throw \"never\") [ 1 42 ]"),
            "42"
        );
        // Nothing consumed it, so an empty list hands the thunk straight back
        // and the throw surfaces where the value is finally wanted.
        assert_eq!(
            render("builtins.foldl' (_: x: x) (throw \"never\") [ ]"),
            "Eval(Thrown, \"never\")"
        );
        assert_eq!(render("builtins.foldl' (a: b: a + b) 0 [ 1 2 3 ]"), "6");
    }

    /// `path + string` stays canonical, `string + path` does not. cppnix
    /// normalizes only the path-valued side, and eval-okay-string pins both.
    #[test]
    fn path_concatenation_normalises_only_on_the_path_side() {
        assert_eq!(
            render("toString (/foo/bar + \"/../xyzzy/.\" + \"/a.txt\")"),
            "\"/foo/xyzzy/a.txt\""
        );
        assert_eq!(render("\"/../foo\" + toString /x/y"), "\"/../foo/x/y\"");
        assert_eq!(render("toString (/a/b + \"/c\")"), "\"/a/b/c\"");
    }

    /// A `Vm` and its import cache may outlive one evaluation. A path-keyed
    /// cache would serve pre-edit module text after the host changes it. The
    /// content key makes edited text miss and recompile, with no invalidation
    /// pass to maintain.
    #[test]
    fn a_reused_vm_does_not_serve_a_stale_import() {
        use crate::host::{FileType, Host};
        use std::cell::RefCell;

        struct Mutable {
            lib: RefCell<String>,
        }
        impl Host for Mutable {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                match path {
                    p if p.path.as_ref() == "/m/lib.nix" => Ok(self.lib.borrow().clone()),
                    p if p.path.as_ref() == "/m/main.nix" => Ok("(import /m/lib.nix).n".to_owned()),
                    _ => Err(format!("path '{path}' does not exist")),
                }
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(self.read_file(path).is_ok())
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                if self.path_exists_checked(path)? {
                    Ok(Some(FileType::Regular))
                } else {
                    Err(format!("path '{path}' does not exist"))
                }
            }
        }

        let host = Mutable {
            lib: RefCell::new("{ n = 1; }".to_owned()),
        };
        // One VM across both requests: this is the persistent evaluator's
        // shape, and the whole point of the test.
        let mut vm = Vm::with_settings(crate::eval::Settings::default());

        let run = |vm: &mut Vm, host: &Mutable| -> String {
            let Ok(module) = compile::compile_source(
                "(import /m/lib.nix).n",
                "/m",
                crate::compile::Origin::String,
                &crate::eval::Settings::default(),
            ) else {
                return "compile failed".to_owned();
            };
            let module = Rc::new(module);
            vm.start_module(&module);
            let value = match drive(vm, host) {
                Ok(value) => value,
                Err(e) => return format!("{e:?}"),
            };
            vm.start_print(value);
            match drive(vm, host) {
                Ok(Value::Str(s)) => s.expect_text(),
                other => format!("{other:?}"),
            }
        };

        assert_eq!(run(&mut vm, &host), "1");
        *host.lib.borrow_mut() = "{ n = 2; }".to_owned();
        assert_eq!(
            run(&mut vm, &host),
            "2",
            "the reused VM served the pre-edit import"
        );
        // And an unchanged file still shares one compile, which is the
        // sharing the cache exists for in the first place.
        assert_eq!(run(&mut vm, &host), "2");
    }

    /// A host that counts how often it is asked to list a directory: the
    /// only place a memo hit and a miss look different, because `q.Entries`
    /// counts what the VM asked, not what was read.
    struct Counting {
        reads: std::cell::Cell<u64>,
    }
    impl crate::host::Host for Counting {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            file_type_resolved,
            get_env,
            copy_to_store,
            ensure_path,
            warn,
            find_file,
            nix_path,
            trace
        );
        fn read_file(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<String, String> {
            Err(format!("path '{path}' does not exist"))
        }
        fn read_dir(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, crate::host::FileType)>, String> {
            self.reads.set(self.reads.get() + 1);
            Ok(vec![("a".to_owned(), crate::host::FileType::Regular)])
        }
        fn path_exists_checked(
            &self,
            _path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(true)
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(
            &self,
            _path: &crate::value2::PathValue,
        ) -> std::result::Result<Option<crate::host::FileType>, String> {
            Ok(Some(crate::host::FileType::Directory))
        }
    }

    /// The directory cache exists to stop this: two `builtins.readDir` calls
    /// on one path inside one evaluation must reach the filesystem once.
    ///
    /// Counted at the `Host`, which is the only place the difference shows:
    /// `q.Entries` keeps counting both questions on purpose, because it
    /// counts what the VM asked, not what was read.
    #[test]
    fn one_evaluation_reads_a_directory_once() {
        use std::cell::Cell;

        let host = Counting {
            reads: Cell::new(0),
        };
        let src = "builtins.length ((builtins.attrNames (builtins.readDir /d)) \
                   ++ (builtins.attrNames (builtins.readDir /d)))";
        let Ok(module) = compile::compile_source(
            src,
            "/",
            crate::compile::Origin::String,
            &crate::eval::Settings::default(),
        ) else {
            unreachable!("the source above must compile")
        };
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        vm.start_module(&Rc::new(module));
        let got = drive(&mut vm, &host);
        assert!(matches!(got, Ok(Value::Int(2))), "{got:?}");
        assert_eq!(
            host.reads.get(),
            1,
            "two readDir calls on one path in one evaluation reached the filesystem twice"
        );
    }

    /// Two drives lending one memo -- the shape of every force a question
    /// performs through the handle API -- read a directory once between them.
    /// The control is two drives with their own memos, which read it twice:
    /// without it, a host that never counted would pass the first assertion.
    #[test]
    fn drives_lending_one_memo_read_a_directory_once() {
        use std::cell::Cell;

        let Ok(module) = compile::compile_source(
            "builtins.length (builtins.attrNames (builtins.readDir /d))",
            "/",
            crate::compile::Origin::String,
            &crate::eval::Settings::default(),
        ) else {
            unreachable!("the source above must compile")
        };
        let module = Rc::new(module);
        let mut vm = Vm::with_settings(crate::eval::Settings::default());

        let host = Counting {
            reads: Cell::new(0),
        };
        let mut memo = JobMemo::default();
        for _ in 0..2 {
            vm.start_module(&module);
            let got = drive_with(&mut vm, &host, &mut memo);
            assert!(matches!(got, Ok(Value::Int(1))), "{got:?}");
        }
        assert_eq!(
            host.reads.get(),
            1,
            "two drives lending one memo asked the host for one directory twice"
        );

        let host = Counting {
            reads: Cell::new(0),
        };
        for _ in 0..2 {
            vm.start_module(&module);
            let got = drive(&mut vm, &host);
            assert!(matches!(got, Ok(Value::Int(1))), "{got:?}");
        }
        assert_eq!(
            host.reads.get(),
            2,
            "the control did not read twice, so the assertion above proved nothing"
        );
    }

    /// ...and it must not survive the evaluation that filled it.
    ///
    /// The cache is keyed by path, so an entry is valid only while the
    /// directory behind that path has not changed. A `Vm` outlives one
    /// evaluation on the warm-start path, so a cache living there would serve
    /// a pre-edit listing to a post-edit request -- the same defect the
    /// import cache avoids by being content-keyed. This one avoids it by
    /// living in a `JobMemo` owned by whoever holds the recording (`drive`
    /// here, one per call), and this is the test that says so.
    #[test]
    fn a_reused_vm_does_not_serve_a_stale_directory() {
        use crate::host::{FileType, Host};
        use std::cell::RefCell;

        struct Mutable {
            names: RefCell<Vec<String>>,
        }
        impl Host for Mutable {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Err(format!("path '{path}' does not exist"))
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(self
                    .names
                    .borrow()
                    .iter()
                    .map(|n| (n.clone(), FileType::Regular))
                    .collect())
            }
            fn path_exists_checked(
                &self,
                _path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(true)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _path: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Ok(Some(FileType::Directory))
            }
        }

        let host = Mutable {
            names: RefCell::new(vec!["a".to_owned()]),
        };
        // One VM across both evaluations: the persistent evaluator's shape.
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let run = |vm: &mut Vm, host: &Mutable| -> String {
            let Ok(module) = compile::compile_source(
                "builtins.concatStringsSep \",\" (builtins.attrNames (builtins.readDir /d))",
                "/",
                crate::compile::Origin::String,
                &crate::eval::Settings::default(),
            ) else {
                return "compile failed".to_owned();
            };
            vm.start_module(&Rc::new(module));
            match drive(vm, host) {
                Ok(Value::Str(s)) => s.expect_text(),
                other => format!("{other:?}"),
            }
        };

        assert_eq!(run(&mut vm, &host), "a");
        host.names.borrow_mut().push("b".to_owned());
        assert_eq!(
            run(&mut vm, &host),
            "a,b",
            "the reused VM served a directory listing from the previous evaluation"
        );
    }

    /// `builtins.getEnv` used to call `std::env::var` itself, which made the
    /// evaluator's answer depend on the environment of whatever process was
    /// hosting it and left the read set blind to it. It goes through the host
    /// now, so a host that reports no environment is genuinely environment
    /// free: this test would read the real PATH if the routing regressed.
    #[test]
    fn get_env_goes_through_the_host() {
        use crate::host::{FileType, Host};

        struct Env;
        impl Host for Env {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                copy_to_store,
                ensure_path,
                warn,
                find_file,
                nix_path,
                trace
            );
            fn get_env(&self, name: &str) -> Option<String> {
                match name {
                    "IXE_TEST_SET" => Some("from-the-host".to_owned()),
                    _ => None,
                }
            }
            fn read_file(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Err("no files".to_owned())
            }
            fn read_dir(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, FileType)>, String> {
                Ok(Vec::new())
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<Option<FileType>, String> {
                Err("no files".to_owned())
            }
        }

        fn run(src: &str) -> String {
            let Ok(module) = compile::compile_source(
                src,
                "/m",
                crate::compile::Origin::String,
                &crate::eval::Settings::default(),
            ) else {
                return "compile failed".to_owned();
            };
            let module = Rc::new(module);
            let mut vm = Vm::with_settings(crate::eval::Settings::default());
            vm.start_module(&module);
            let value = match drive(&mut vm, &Env) {
                Ok(value) => value,
                Err(e) => return format!("{e:?}"),
            };
            vm.start_print(value);
            match drive(&mut vm, &Env) {
                Ok(Value::Str(s)) => s.expect_text(),
                other => format!("{other:?}"),
            }
        }

        assert_eq!(run("builtins.getEnv \"IXE_TEST_SET\""), "\"from-the-host\"");
        // cppnix renders an unset variable as the empty string rather than
        // failing, and this host reports every other name as unset. PATH is
        // set in any real process, so a non-empty answer here would mean the
        // builtin bypassed the host.
        assert_eq!(run("builtins.getEnv \"PATH\""), "\"\"");
    }

    #[test]
    fn infinite_recursion_detected() {
        assert_eq!(
            render("let a = a; in a"),
            "Eval(Eval, \"infinite recursion encountered\")"
        );
    }

    /// A host for the coercion tests: one readable file, one directory, and
    /// a store copy that produces a *different* readable file. The two
    /// contents differ so that a coercion which wrongly copied to the store
    /// is caught by the bytes it returns rather than only by a store call
    /// nobody counted.
    struct CoerceFs;

    impl crate::host::Host for CoerceFs {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            file_type_resolved,
            get_env,
            ensure_path,
            warn,
            find_file,
            nix_path,
            trace
        );
        fn read_file(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<String, String> {
            match path {
                p if p.path.as_ref() == "/m/f" => Ok("source".to_owned()),
                p if p.path.as_ref() == "/nix/store/h-m-f" => Ok("copied".to_owned()),
                p if p.path.as_ref() == "/m/d/default.nix" => Ok("42".to_owned()),
                _ => Err(format!("path '{path}' does not exist")),
            }
        }
        fn read_dir(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, crate::host::FileType)>, String> {
            match path {
                p if p.path.as_ref() == "/m/d" => Ok(vec![(
                    "default.nix".to_owned(),
                    crate::host::FileType::Regular,
                )]),
                p => Err(format!("path '{p}' does not exist")),
            }
        }
        fn path_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(path.path.as_ref() == "/m/d" || self.read_file(path).is_ok())
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<Option<crate::host::FileType>, String> {
            match path {
                p if p.path.as_ref() == "/m/d" => Ok(Some(crate::host::FileType::Directory)),
                p if self.read_file(p).is_ok() => Ok(Some(crate::host::FileType::Regular)),
                p => Err(format!("path '{p}' does not exist")),
            }
        }
        fn copy_to_store(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<String, crate::host::StoreError> {
            Ok(format!("/nix/store/h{}", path.replace('/', "-")))
        }
    }

    fn coerce_fs_run(src: &str) -> String {
        let Ok(module) = compile::compile_source(
            src,
            "/m",
            crate::compile::Origin::String,
            &crate::eval::Settings::default(),
        ) else {
            return "compile failed".to_owned();
        };
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        vm.start_module(&Rc::new(module));
        let value = match drive(&mut vm, &CoerceFs) {
            Ok(v) => v,
            Err(VmError::Unimplemented(w)) => return format!("unimplemented: {w}"),
            Err(e) => return format!("{e:?}"),
        };
        vm.start_print(value);
        match drive(&mut vm, &CoerceFs) {
            Ok(Value::Str(s)) => s.expect_text(),
            Ok(other) => format!("{other:?}"),
            Err(e) => format!("{e:?}"),
        }
    }

    /// Every builtin that takes a path runs cppnix's `EvalState::coerceToPath`
    /// on its argument, so a set carrying `__toString` or `outPath` is a path
    /// -- which is how a derivation reaches `builtins.readFile`. The Rust
    /// backend used to match only a string and a path value and refuse the
    /// rest with "cannot coerce a set to a path", a message cppnix never
    /// prints (ENG-12669, the ENG-12628 class).
    ///
    /// The outcomes below are `nix-instantiate (Nix) 2.34.7+ix` on the same
    /// expressions, not derived from the code under test.
    #[test]
    fn the_path_family_coerces_its_argument_the_way_cppnix_does() {
        // A set coerces through `outPath` and through `__toString`, and
        // `__toString` first when both are present.
        assert_eq!(
            coerce_fs_run(r#"builtins.readFile { outPath = "/m/f"; }"#),
            r#""source""#
        );
        assert_eq!(
            coerce_fs_run(r#"builtins.readFile { __toString = self: "/m/f"; }"#),
            r#""source""#
        );
        assert_eq!(
            coerce_fs_run(
                r#"builtins.readFile { __toString = self: "/m/f"; outPath = "/nowhere"; }"#
            ),
            r#""source""#
        );
        // cppnix recurses on what `__toString` returns, so it may be another
        // set, and `outPath` is a tail call, so it may be one too.
        assert_eq!(
            coerce_fs_run(r#"builtins.readFile { __toString = self: { outPath = "/m/f"; }; }"#),
            r#""source""#
        );
        assert_eq!(
            coerce_fs_run(r#"builtins.readFile { outPath = { __toString = s: "/m/f"; }; }"#),
            r#""source""#
        );

        // `coerceToPath` passes `copyToStore = false`, so a path inside the
        // set stays a source path. This host answers "copied" for the store
        // copy of `/m/f` and "source" for `/m/f` itself, so a coercion that
        // copied would read the other file.
        assert_eq!(
            coerce_fs_run(r#"builtins.readFile { outPath = /m/f; }"#),
            r#""source""#
        );
        assert_eq!(coerce_fs_run(r#"builtins.readFile /m/f"#), r#""source""#);
        assert_eq!(
            coerce_fs_run(r#"builtins.readFile { __toString = self: /m/f; }"#),
            r#""source""#
        );

        // The whole family, not only `readFile`: every `ask()` caller and
        // `import`, which reaches its file through `realisePath` too.
        assert_eq!(
            coerce_fs_run(r#"builtins.pathExists { outPath = "/m/f"; }"#),
            "true"
        );
        assert_eq!(
            coerce_fs_run(r#"builtins.pathExists { outPath = "/m/absent"; }"#),
            "false"
        );
        assert_eq!(
            coerce_fs_run(r#"builtins.readFileType { outPath = "/m/f"; }"#),
            r#""regular""#
        );
        assert_eq!(
            coerce_fs_run(r#"builtins.attrNames (builtins.readDir { outPath = "/m/d"; })"#),
            r#"[ "default.nix" ]"#
        );
        assert_eq!(coerce_fs_run(r#"import { outPath = /m/d; }"#), "42");

        // What still fails, with cppnix's wording. The message says "to a
        // string" and not "to a path": `coerceToPath`'s tail is
        // `coerceToString`, and that is where the refusal is raised, so
        // cppnix has no path-specific text here.
        assert!(
            coerce_fs_run("builtins.pathExists 3").contains("cannot coerce an integer to a string"),
            "unexpected: {}",
            coerce_fs_run("builtins.pathExists 3")
        );
        assert!(coerce_fs_run("builtins.pathExists true").contains("cannot coerce a Boolean"));
        assert!(coerce_fs_run("builtins.pathExists null").contains("cannot coerce null"));
        assert!(coerce_fs_run("builtins.pathExists [ ]").contains("cannot coerce a list"));
        // A set with neither attribute is the one case that really is about a
        // set, and it comes from the coercion, not from the path check.
        assert!(
            coerce_fs_run("builtins.pathExists { }").contains("cannot coerce a set to a string")
        );

        // What the coercion produces has to be absolute -- including when it
        // came from a plain string argument, which this backend used to hand
        // to the host as written.
        assert!(
            coerce_fs_run(r#"builtins.pathExists "relative""#)
                .contains("string 'relative' doesn't represent an absolute path"),
            "unexpected: {}",
            coerce_fs_run(r#"builtins.pathExists "relative""#)
        );
        assert!(
            coerce_fs_run(r#"builtins.pathExists { outPath = "relative"; }"#)
                .contains("string 'relative' doesn't represent an absolute path")
        );
        assert!(
            coerce_fs_run(r#"builtins.pathExists """#)
                .contains("string '' doesn't represent an absolute path")
        );

        // A type error, not a catchable one: cppnix's `tryEval` catches
        // `throw` and `assert` and nothing else, so this propagates.
        assert!(
            coerce_fs_run("(builtins.tryEval (builtins.pathExists 3)).success")
                .contains("cannot coerce an integer to a string")
        );
    }

    /// `builtins.toJSON` hands a `__toString` result to `coerceToString` with
    /// both flags off (`value-to-json.cc`, `tryAttrsToString(pos, v, context,
    /// false, false)`), so the result may be a set that coerces further. The
    /// Rust backend used to accept only a string or a path back (ENG-12670).
    ///
    /// `copyToStore` off is the half worth a test: the `nPath` arm one level
    /// up in the same function *does* copy, so getting the flag wrong here
    /// produces a store path in a JSON document that cppnix fills with a
    /// source path -- and that document is what reaches a derivation.
    #[test]
    fn to_json_coerces_a_to_string_result_the_way_cppnix_does() {
        assert_eq!(
            coerce_fs_run(r#"builtins.toJSON { __toString = self: { outPath = "/x"; }; }"#),
            r#""\"/x\"""#
        );
        assert_eq!(
            coerce_fs_run(
                r#"builtins.toJSON { __toString = self: { __toString = s: "/deep"; }; }"#
            ),
            r#""\"/deep\"""#
        );
        assert_eq!(
            coerce_fs_run(r#"builtins.toJSON { __toString = self: "plain"; }"#),
            r#""\"plain\"""#
        );
        // No store copy, and so no context either: `/m/f` renders as itself.
        assert_eq!(
            coerce_fs_run(r#"builtins.toJSON { __toString = self: /m/f; }"#),
            r#""\"/m/f\"""#
        );
        assert_eq!(
            coerce_fs_run(r#"builtins.toJSON { __toString = self: { outPath = /m/f; }; }"#),
            r#""\"/m/f\"""#
        );
        assert_eq!(
            coerce_fs_run(r#"builtins.hasContext (builtins.toJSON { __toString = self: /m/f; })"#),
            "false"
        );
        // The sibling arm, for contrast: a path reached as a *value* rather
        // than through `__toString` is copied, because `printValueAsJSON`'s
        // `nPath` case runs with `copyToStore` on. Both arms in one test, so
        // a change that unified them fails here.
        assert_eq!(
            coerce_fs_run("builtins.toJSON /m/f"),
            r#""\"/nix/store/h-m-f\"""#
        );
        assert_eq!(
            coerce_fs_run("builtins.hasContext (builtins.toJSON /m/f)"),
            "true"
        );
        assert_eq!(
            coerce_fs_run(r#"builtins.toJSON { outPath = /m/f; }"#),
            r#""\"/nix/store/h-m-f\"""#
        );

        // `coerceMore` is off, so everything it would have allowed is still
        // an error, with cppnix's wording.
        assert!(
            coerce_fs_run(r#"builtins.toJSON { __toString = self: 3; }"#)
                .contains("cannot coerce an integer to a string"),
            "unexpected: {}",
            coerce_fs_run(r#"builtins.toJSON { __toString = self: 3; }"#)
        );
        assert!(
            coerce_fs_run(r#"builtins.toJSON { __toString = self: null; }"#)
                .contains("cannot coerce null to a string")
        );
        assert!(
            coerce_fs_run(r#"builtins.toJSON { __toString = self: [ ]; }"#)
                .contains("cannot coerce a list to a string")
        );
        assert!(
            coerce_fs_run(r#"builtins.toJSON { __toString = self: { }; }"#)
                .contains("cannot coerce a set to a string")
        );
    }

    /// ENG-13147: strings are byte sequences, as cppnix's are. Every
    /// expected value here was measured against cppnix (nix 2.34.7+ix, this
    /// change) rather than derived from this implementation. The canonical
    /// way to mint a non-UTF-8 string out of pure-UTF-8 source is
    /// `builtins.substring 0 1 "ä"`: slicing the two-byte a-umlaut
    /// keeps only its lead byte 0xC3.
    mod byte_strings {
        use super::*;
        use crate::host::RealFs;
        use crate::vm::VmError;

        fn machine(src: &str) -> std::result::Result<Vm, String> {
            let settings = Settings::default();
            let module = compile::compile_source(src, "/m", compile::Origin::String, &settings)
                .map_err(|e| format!("compile failed: {e:?}"))?;
            let mut vm = Vm::with_settings(settings);
            vm.start_module(&Rc::new(module));
            Ok(vm)
        }

        fn message(e: &VmError) -> String {
            match e {
                VmError::Throw(c) => c.message.clone(),
                VmError::Unimplemented(r) => format!("unimplemented: {r}"),
            }
        }

        /// The forced value of `src`, or the error's message.
        fn value(src: &str) -> std::result::Result<Value, String> {
            let mut vm = machine(src)?;
            drive(&mut vm, &RealFs).map_err(|e| message(&e))
        }

        /// The bytes the printer writes for `src` -- the `nix-instantiate
        /// --eval` rendering -- or the error's message.
        fn printed(src: &str) -> std::result::Result<Vec<u8>, String> {
            let mut vm = machine(src)?;
            let v = drive(&mut vm, &RealFs).map_err(|e| message(&e))?;
            vm.start_print(v);
            match drive(&mut vm, &RealFs) {
                Ok(Value::Str(s)) => Ok(s.bytes().to_vec()),
                Ok(other) => Err(format!("printer produced {other:?}")),
                Err(e) => Err(message(&e)),
            }
        }

        /// The bytes of a string-valued `src`, or the error's message.
        fn bytes_of(src: &str) -> std::result::Result<Vec<u8>, String> {
            match value(src)? {
                Value::Str(s) => Ok(s.bytes().to_vec()),
                other => Err(format!("not a string: {other:?}")),
            }
        }

        #[test]
        fn string_length_counts_bytes() {
            // cppnix: 2 -- the a-umlaut is two bytes, not one character.
            assert!(matches!(
                value(r#"builtins.stringLength "ä""#),
                Ok(Value::Int(2))
            ));
            // A round trip through interpolation keeps the raw byte.
            assert!(matches!(
                value(r#"builtins.stringLength "a${builtins.substring 0 1 "ä"}x""#),
                Ok(Value::Int(3))
            ));
        }

        #[test]
        fn substring_slices_bytes() {
            assert_eq!(bytes_of(r#"builtins.substring 0 1 "ä""#), Ok(vec![0xC3]));
        }

        #[test]
        fn the_printer_writes_the_bytes_raw() {
            // cppnix nix-instantiate prints the bytes 22 C3 22: no repair,
            // no escape.
            assert_eq!(
                printed(r#"builtins.substring 0 1 "ä""#),
                Ok(vec![b'"', 0xC3, b'"'])
            );
            assert_eq!(
                printed(r#""a${builtins.substring 0 1 "ä"}x""#),
                Ok(vec![b'"', b'a', 0xC3, b'x', b'"'])
            );
        }

        #[test]
        fn hash_string_hashes_the_bytes() {
            // cppnix: sha256 of the single byte 0xC3.
            assert_eq!(
                bytes_of(r#"builtins.hashString "sha256" (builtins.substring 0 1 "ä")"#),
                Ok(b"ae3f4619b0413d70d3004b9131c3752153074e45725be13b9a148978895e359e".to_vec())
            );
        }

        #[test]
        fn match_and_split_are_byte_wise() {
            // cppnix (POSIX ERE over bytes): `.` is one byte, so a two-byte
            // character needs two of them.
            assert_eq!(printed(r#"builtins.match "." "ä""#), Ok(b"null".to_vec()));
            assert_eq!(printed(r#"builtins.match ".." "ä""#), Ok(b"[ ]".to_vec()));
            assert_eq!(
                printed(r#"builtins.split "." "ä""#),
                Ok(br#"[ "" [ ] "" [ ] "" ]"#.to_vec())
            );
        }

        #[test]
        fn equality_is_on_the_bytes() {
            // cppnix: true -- a-umlaut and o-umlaut both lead with 0xC3.
            assert!(matches!(
                value(r#"builtins.substring 0 1 "ä" == builtins.substring 0 1 "ö""#),
                Ok(Value::Bool(true))
            ));
        }

        /// The error message `src` fails with, or what it wrongly produced.
        fn err_of(src: &str) -> String {
            match value(src) {
                Err(m) => m,
                Ok(v) => format!("no error: {v:?}"),
            }
        }

        #[test]
        fn to_json_raises_nlohmanns_error_after_the_walk() {
            // Both messages verbatim from cppnix, including nlohmann's
            // reject-at-the-byte-it-was-reading index: `61 C3 78` fails at
            // index 2 (the `x`), not at the `C3` that led the sequence in.
            assert_eq!(
                err_of(r#"builtins.toJSON (builtins.substring 0 1 "ä")"#),
                "JSON serialization error: [json.exception.type_error.316] \
                 incomplete UTF-8 string; last byte: 0xC3"
            );
            assert_eq!(
                err_of(r#"builtins.toJSON "a${builtins.substring 0 1 "ä"}x""#),
                "JSON serialization error: [json.exception.type_error.316] \
                 invalid UTF-8 byte at index 2: 0x78"
            );
            // cppnix builds the whole document before it serialises, so an
            // eval error inside the value beats the serialization error.
            assert_eq!(
                err_of(r#"builtins.toJSON [ (builtins.substring 0 1 "ä") (throw "boom") ]"#),
                "boom"
            );
            // Valid multibyte text passes through raw, not `\u`-escaped.
            assert_eq!(
                bytes_of(r#"builtins.toJSON "日""#),
                Ok("\"日\"".as_bytes().to_vec())
            );
        }

        #[test]
        fn to_xml_writes_the_bytes_raw() {
            // cppnix writes the byte into the attribute unrepaired; the
            // resulting document is exactly as (in)valid as cppnix's.
            let mut want =
                b"<?xml version='1.0' encoding='utf-8'?>\n<expr>\n  <string value=\"".to_vec();
            want.push(0xC3);
            want.extend_from_slice(b"\" />\n</expr>\n");
            assert_eq!(
                bytes_of(r#"builtins.toXML (builtins.substring 0 1 "ä")"#),
                Ok(want)
            );
        }
    }
}

/// The parked-task scheduler: does an evaluation that is waiting on the world
/// let another one run?
#[cfg(test)]
mod scheduler {
    use super::{Settings, drive, drive_concurrent};
    use crate::compile;
    use crate::host::{FileType, Host, StoreError, ThreadedHost};
    use crate::vm::Vm;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// How long a fetch pretends to take.
    ///
    /// Long enough that the difference between one stall and two is far
    /// outside the noise of starting a thread and compiling a one-line
    /// expression, short enough that the test costs a fifth of a second.
    /// The assertions below are stated as fractions of it rather than as
    /// absolute milliseconds so that a slower machine moves both numbers.
    const STALL: Duration = Duration::from_millis(200);
    /// A string carrying one `!out!<drv>` context element, which is what
    /// makes a read of it an import from derivation. Same fixture as the
    /// `realise` tests above; copied rather than shared because those live in
    /// a nested module and a `pub(super)` there would widen it for no reason.
    const WITH_CONTEXT: &str = r#"builtins.appendContext "/nix/store/00000000000000000000000000000000-out" { "/nix/store/11111111111111111111111111111111-x.drv" = { outputs = [ "out" ]; }; }"#;

    /// A host whose fetch sleeps, and which counts how many sleeps were
    /// running at once.
    ///
    /// `Send + Sync` and no interior mutability that is not atomic, because
    /// [`ThreadedHost`] will call `fetch` from a worker thread -- which is
    /// the bound the wrapper exists to make someone state.
    #[derive(Default)]
    struct SlowFetch {
        /// How many fetches are in their sleep right now.
        concurrent: AtomicUsize,
        /// The most that were ever in their sleep at the same time. This is
        /// the measurement the test is really about: a wall-clock number can
        /// be explained away by a fast machine, a peak of 2 cannot.
        peak: AtomicUsize,
    }

    impl Host for SlowFetch {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            get_env,
            copy_to_store,
            ensure_path,
            find_file,
            nix_path,
            trace,
            warn,
            file_type_resolved
        );
        fn read_file(&self, p: &crate::value2::PathValue) -> Result<String, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn read_dir(
            &self,
            p: &crate::value2::PathValue,
        ) -> Result<Vec<(String, FileType)>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn path_exists_checked(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(false)
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(&self, p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn fetch(&self, request: &crate::task::FetchRequest) -> Result<String, StoreError> {
            let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(STALL);
            self.concurrent.fetch_sub(1, Ordering::SeqCst);
            Ok(format!(
                "/nix/store/0000000000000000000000000000000a-{}",
                request.name
            ))
        }
    }

    /// An expression that fetches `name` and reduces the answer to a length,
    /// so the fetch is forced and the result is a plain integer.
    fn fetching(name: &str) -> String {
        format!(
            "builtins.stringLength (builtins.fetchurl {{ url = \"http://example.invalid/{name}\"; \
             name = \"{name}\"; }})"
        )
    }

    /// A VM seeded with `src`, ready to be driven.
    fn machine(settings: &Settings, src: &str) -> Result<Vm, String> {
        let module = compile::compile_source(src, "/m", compile::Origin::String, settings)
            .map_err(|e| format!("compile failed: {e:?}"))?;
        let mut vm = Vm::with_settings(settings.clone());
        vm.start_module(&Rc::new(module));
        Ok(vm)
    }

    fn rendered(outcome: Result<crate::value2::Value, crate::vm::VmError>) -> String {
        match outcome {
            Ok(value) => format!("{value:?}"),
            Err(error) => format!("{error:?}"),
        }
    }

    /// Two evaluations that each stall on a fetch take about as long as one
    /// of them, not both.
    ///
    /// The measurement this test exists to make, and it prints both numbers
    /// on failure so a regression says how far it slipped rather than only
    /// that it did.
    #[test]
    fn two_evaluations_that_stall_overlap_instead_of_queueing() -> std::result::Result<(), String> {
        let settings = Settings::default();
        let host = ThreadedHost::new(SlowFetch::default());

        let mut a = machine(&settings, &fetching("a"))?;
        let mut b = machine(&settings, &fetching("b"))?;
        let began = Instant::now();
        let together = drive_concurrent(vec![
            (&mut a, &host as &dyn Host),
            (&mut b, &host as &dyn Host),
        ]);
        let overlapped = began.elapsed();
        let peak_together = host.inner().peak.swap(0, Ordering::SeqCst);

        let mut c = machine(&settings, &fetching("c"))?;
        let mut d = machine(&settings, &fetching("d"))?;
        let began = Instant::now();
        let one = drive(&mut c, &host);
        let two = drive(&mut d, &host);
        let queued = began.elapsed();
        let peak_apart = host.inner().peak.swap(0, Ordering::SeqCst);

        // Every arm produced the same answer, so the timings are comparing
        // the same work.
        let answers: Vec<String> = together
            .into_iter()
            .chain([one, two])
            .map(rendered)
            .collect();
        for answer in &answers {
            if answer != "Int(45)" {
                return Err(format!("expected four fetched lengths, got {answers:?}"));
            }
        }

        // Printed, not only asserted: a threshold that passes tells you
        // nothing about how much room is left, and this is the number a PR
        // quotes. `cargo test -- --nocapture` shows it.
        println!(
            "overlap: one stall {STALL:?}; two evaluations together {overlapped:?} \
             (peak in flight {peak_together}), apart {queued:?} (peak in flight {peak_apart})"
        );

        // The timings are printed, not judged: overlap is decided by the
        // peak counts below. A ratio bound on
        // the two arms was tried and failed a run that overlapped perfectly
        // (peak 2, 350ms together against 457ms apart): the machine was
        // loaded (the nix sandbox runs this beside a C++ build, under a
        // darwin switch), and load stretches the fixed cost of building and
        // driving two machines, which the together arm pays concurrently and
        // the sequential arm pays twice, so no fixed ratio separates "did
        // not overlap" from "overlapped on a busy machine". The one clock
        // check kept is a control on the sequential arm, which load can only
        // lengthen.
        if queued <= STALL * 9 / 5 {
            return Err(format!(
                "two sequential evaluations took only {queued:?}, under 1.8 stalls ({:?}); the \
                 sequential arm is not measuring what it claims",
                STALL * 9 / 5
            ));
        }

        // And the reason, which no clock can be talked out of: two fetches
        // were inside their sleep at the same time when the evaluations were
        // driven together, and never more than one when they were not.
        if peak_together != 2 {
            return Err(format!(
                "driven together, the peak number of fetches in flight was {peak_together}, not 2 \
                 (overlapped in {overlapped:?})"
            ));
        }
        if peak_apart != 1 {
            return Err(format!(
                "driven one after the other, the peak number of fetches in flight was \
                 {peak_apart}, not 1"
            ));
        }
        Ok(())
    }

    /// A store that stalls in `realise`, the way a real build does.
    ///
    /// Separate from [`SlowFetch`] rather than a flag on it, because the
    /// question this measures is a different one: `Realise` is the only slow
    /// question whose payload is `Rc`-based, so it is the only one whose
    /// worker has to rebuild its argument on the far side of the channel.
    #[derive(Default)]
    struct SlowBuild {
        concurrent: AtomicUsize,
        peak: AtomicUsize,
        /// Every `realise` that reached this store.
        calls: AtomicUsize,
    }

    impl Host for SlowBuild {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            store_text,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            fetch_tree,
            not_async,
        );
        crate::host::host_stubs!(
            get_env,
            copy_to_store,
            find_file,
            nix_path,
            trace,
            warn,
            file_type_resolved
        );
        /// `appendContext` validates each key against the store before
        /// attaching it, so a fixture that builds a context has to answer.
        fn ensure_path(&self, _p: &str) -> Result<(), StoreError> {
            Ok(())
        }

        fn read_file(&self, _p: &crate::value2::PathValue) -> Result<String, String> {
            Ok("built".to_owned())
        }
        fn read_dir(
            &self,
            p: &crate::value2::PathValue,
        ) -> Result<Vec<(String, FileType)>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn path_exists_checked(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(true)
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(&self, _p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
            Ok(Some(FileType::Regular))
        }
        fn realise(
            &self,
            context: &[crate::value2::ContextElem],
        ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
            // The context has to survive the trip: a worker that rebuilt it
            // wrongly would still return a map and the test would pass on the
            // timing alone.
            if context.len() != 1 {
                return Err(StoreError::Failed(format!(
                    "the worker rebuilt the context as {} element(s), not 1",
                    context.len()
                )));
            }
            let shape = context
                .iter()
                .map(crate::value2::ContextElem::display)
                .collect::<Vec<_>>()
                .join(" ");
            if !shape.starts_with("!out!/nix/store/") {
                return Err(StoreError::Failed(format!(
                    "the worker rebuilt the context as {shape:?}"
                )));
            }
            self.calls.fetch_add(1, Ordering::SeqCst);
            let now = self.concurrent.fetch_add(1, Ordering::SeqCst) + 1;
            self.peak.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(STALL);
            self.concurrent.fetch_sub(1, Ordering::SeqCst);
            Ok(std::collections::BTreeMap::new())
        }
    }

    /// Import from derivation overlaps too, and the context arrives intact.
    ///
    /// The variant with most to gain, because `Realise` runs a *build*: under
    /// the old single-slot design one IFD stalled every root in the process.
    /// It is also the variant that could go wrong quietly, since its argument
    /// is the one payload that cannot cross a thread as it stands -- see
    /// `SendContextElem`. So the fixture checks the rebuilt context before it
    /// answers, and a worker that mangled it fails the test as a build error
    /// rather than as a slow one.
    #[test]
    fn two_builds_overlap_and_the_context_survives_the_trip() -> std::result::Result<(), String> {
        let settings = crate::eval::settings_with_store();
        let host = ThreadedHost::new(SlowBuild::default());
        let src = format!("builtins.readFile ({WITH_CONTEXT})");

        let mut a = machine(&settings, &src)?;
        let mut b = machine(&settings, &src)?;
        let began = Instant::now();
        let together = drive_concurrent(vec![
            (&mut a, &host as &dyn Host),
            (&mut b, &host as &dyn Host),
        ]);
        let overlapped = began.elapsed();
        let peak = host.inner().peak.swap(0, Ordering::SeqCst);

        let answers: Vec<String> = together.into_iter().map(rendered).collect();
        for answer in &answers {
            if !answer.contains("built") {
                return Err(format!(
                    "expected two reads of a built path, got {answers:?}"
                ));
            }
        }
        println!(
            "realise overlap: one stall {STALL:?}; two builds together {overlapped:?} \
             (peak in flight {peak})"
        );
        if peak != 2 {
            return Err(format!(
                "the peak number of builds in flight was {peak}, not 2 (took {overlapped:?})"
            ));
        }
        // Two builds that did not overlap take two full stalls whatever the
        // load; anything under that is proof of overlap on any machine.
        if overlapped >= STALL * 2 {
            return Err(format!(
                "two overlapping builds took {overlapped:?}, not less than two stalls ({:?})",
                STALL * 2
            ));
        }
        Ok(())
    }

    /// The realise memo covers the begun route too: the second ask for a
    /// context this evaluation has built is not begun (no thread, no stall),
    /// and its answer is the first build's: two reads, and the store saw one
    /// build.
    #[test]
    fn a_context_built_on_a_worker_is_not_begun_again() -> std::result::Result<(), String> {
        let settings = crate::eval::settings_with_store();
        let host = ThreadedHost::new(SlowBuild::default());
        let src = format!("let p = {WITH_CONTEXT}; in builtins.readFile p + builtins.readFile p");
        let mut vm = machine(&settings, &src)?;
        let began = Instant::now();
        let answer = rendered(drive(&mut vm, &host));
        let took = began.elapsed();
        // `rendered` prints the value's debug form, as the neighbouring
        // tests read it.
        if !answer.contains("builtbuilt") {
            return Err(format!(
                "expected both reads of the built path, got {answer}"
            ));
        }
        // The count is the proof; the time is a note. A second begun build
        // would be a second call whatever the machine's load, while a bound
        // on `took` would fail on a loaded CI machine with the memo working.
        let calls = host.inner().calls.load(Ordering::SeqCst);
        println!(
            "realise memo on the begun route: two reads, {calls} build(s), {took:?} (one stall {STALL:?})"
        );
        if calls != 1 {
            return Err(format!(
                "the store was asked to build {calls} times, not once"
            ));
        }
        Ok(())
    }

    /// Overlapping does not change any answer.
    ///
    /// Two different expressions, run together and run apart, produce the
    /// same four strings. Tier 1 output does not depend on servicing order,
    /// and this is the cheap end of saying so -- `maintainers/ix/drv-parity.sh`
    /// is the expensive end.
    #[test]
    fn overlapping_does_not_change_an_answer() -> std::result::Result<(), String> {
        let settings = Settings::default();
        let host = ThreadedHost::new(SlowFetch::default());
        let sources = [
            fetching("one"),
            "builtins.concatStringsSep \",\" (builtins.attrNames { b = 1; a = 2; c = 3; })"
                .to_owned(),
        ];

        let apart: Vec<String> = sources
            .iter()
            .map(|src| {
                let mut vm = machine(&settings, src)?;
                Ok(rendered(drive(&mut vm, &host)))
            })
            .collect::<Result<_, String>>()?;

        let mut vms = sources
            .iter()
            .map(|src| machine(&settings, src))
            .collect::<Result<Vec<_>, String>>()?;
        let together: Vec<String> = drive_concurrent(
            vms.iter_mut()
                .map(|vm| (vm, &host as &dyn Host))
                .collect::<Vec<_>>(),
        )
        .into_iter()
        .map(rendered)
        .collect();

        if apart != together {
            return Err(format!("apart {apart:?} but together {together:?}"));
        }
        Ok(())
    }

    /// A host that mints a ticket and never answers it fails the evaluation
    /// rather than hanging, and leaves the machine visibly parked.
    ///
    /// Both guards in one test, because the second state is what the first
    /// one leaves behind. Neither is reachable from a correct host, which is
    /// why they are worth a deliberately incorrect one: a scheduler whose
    /// error paths have never run is a scheduler with two ways to hang.
    #[test]
    fn a_host_that_abandons_a_ticket_fails_rather_than_hangs() -> std::result::Result<(), String> {
        /// Mints tickets, answers none.
        struct Abandons;
        impl Host for Abandons {
            crate::host::host_stubs!(settle);
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
            );
            crate::host::host_stubs!(
                get_env,
                copy_to_store,
                ensure_path,
                find_file,
                nix_path,
                trace,
                warn,
                file_type_resolved
            );
            fn read_file(&self, p: &crate::value2::PathValue) -> Result<String, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn read_dir(
                &self,
                p: &crate::value2::PathValue,
            ) -> Result<Vec<(String, FileType)>, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn path_exists_checked(
                &self,
                _p: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                self.file_type_resolved(path)
                    .map(|kind| kind == crate::host::FileType::Directory)
            }
            fn file_type(&self, p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
                Err(format!("path '{p}' does not exist"))
            }
            fn begin(&self, _question: &crate::host::Slow<'_>) -> Option<crate::host::Ticket> {
                Some(crate::host::Ticket(1))
            }
            fn collect(
                &self,
                _ticket: crate::host::Ticket,
                _block: bool,
            ) -> Option<crate::host::SlowAnswer> {
                None
            }
        }

        let settings = Settings::default();
        let host = Abandons;
        let mut vm = machine(&settings, &fetching("a"))?;
        crate::perf::reset();
        let abandoned = rendered(drive(&mut vm, &host));
        if !abandoned.contains("abandoned the 'fetchurl' question") {
            return Err(format!(
                "expected an abandoned-question failure, got {abandoned:?}"
            ));
        }
        if cfg!(feature = "perf") {
            let kind = crate::purity::QUESTION_KINDS
                .iter()
                .position(|name| *name == "Fetch")
                .ok_or_else(|| "Fetch is missing from QUESTION_KINDS".to_owned())?;
            if crate::perf::by_kind()[kind] != 1
                || crate::perf::slow_by_kind()[kind] != 1
                || crate::perf::abandoned_by_kind()[kind] != 1
                || crate::perf::by_kind_ns()[kind] != 0
                || crate::perf::slow_by_kind_ns()[kind] != 0
            {
                return Err(
                    "an abandoned ticket was not counted as one begun ask, one abandonment, and no completed latency"
                        .to_owned(),
                );
            }
        }

        // The machine is still parked on that suspension, and a driver handed
        // it now says so instead of stepping it for ever.
        if vm.outstanding() != 1 {
            return Err(format!(
                "expected the abandoned suspension to still be open, found {}",
                vm.outstanding()
            ));
        }
        let stuck = rendered(drive(&mut vm, &host));
        if !stuck.contains("parked on 1 suspension(s)") {
            return Err(format!("expected a parked-machine failure, got {stuck:?}"));
        }
        Ok(())
    }

    /// A host that begins nothing is driven exactly as it was before.
    ///
    /// `Host::begin` defaults to `None`, so this is every host that has not
    /// opted in -- including the recorder's inner host in a build with no
    /// asynchronous embedder. It must still answer, through the same
    /// synchronous path, and the fetches must not overlap.
    #[test]
    fn a_host_with_no_asynchronous_path_still_answers() -> std::result::Result<(), String> {
        let settings = Settings::default();
        let host = SlowFetch::default();
        let mut a = machine(&settings, &fetching("a"))?;
        let mut b = machine(&settings, &fetching("b"))?;
        let answers: Vec<String> = drive_concurrent(vec![
            (&mut a, &host as &dyn Host),
            (&mut b, &host as &dyn Host),
        ])
        .into_iter()
        .map(rendered)
        .collect();
        if answers != vec!["Int(45)".to_owned(), "Int(45)".to_owned()] {
            return Err(format!("expected two fetched lengths, got {answers:?}"));
        }
        let peak = host.peak.load(Ordering::SeqCst);
        if peak != 1 {
            return Err(format!(
                "a host with no asynchronous path had {peak} fetches in flight at once"
            ));
        }
        Ok(())
    }
}

/// Fan-out within one evaluation (ENG-13150): when a forcing walk's child
/// parks on a slow question the host began, the walk's next child runs as a
/// sibling strand -- and none of it may depend on which answer arrived first.
#[cfg(test)]
mod fanout {
    use super::{Settings, drive, vm_debug_without_pos};
    use crate::compile;
    use crate::host::{FileType, Host, Slow, SlowAnswer, StoreError, Ticket};
    use crate::readset::RecordingHost;
    use crate::value2::Value;
    use crate::vm::Vm;
    use std::collections::HashMap;
    use std::rc::Rc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::time::Duration;

    /// Two fetches forced by one `toJSON` walk, in program order. The first
    /// is the fixture's slow one; [`FanHost`] makes the second one's answer
    /// arrive first, so anything that leaked arrival order into delivery
    /// order fails every test on this fixture.
    const TWO_FETCHES: &str = r#"builtins.toJSON [
      (builtins.fetchurl { url = "http://example.invalid/slow"; name = "slow"; })
      (builtins.fetchurl { url = "http://example.invalid/fast"; name = "fast"; })
    ]"#;

    /// The same walk with a second child that traces and then fails. The
    /// trace is the witness that the strand ran *while* the first child's
    /// fetch was still in flight; the throw is what must land in the slot
    /// rather than in the scheduler.
    const FETCH_THEN_THROW: &str = r#"builtins.toJSON [
      (builtins.fetchurl { url = "http://example.invalid/slow"; name = "slow"; })
      (builtins.trace "second ran" (throw "boom"))
    ]"#;

    fn store_path_for(name: &str) -> String {
        format!("/nix/store/0000000000000000000000000000000a-{name}")
    }

    fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        mutex
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A host that can answer fetches in the background and writes down every
    /// call in the order it happened.
    ///
    /// The out-of-order arrival is engineered rather than raced: when `gated`,
    /// the worker answering the `slow` fetch waits until the `fast` one has
    /// answered, so "the second question's answer arrives first" is a fact of
    /// the fixture and not a sleep the test usually wins.
    struct FanHost {
        /// `begin` accepts fetches when true; the same host with this off is
        /// the sequential baseline, answering everything inline.
        asynchronous: bool,
        events: Arc<Mutex<Vec<String>>>,
        next: AtomicU64,
        pending: Mutex<HashMap<u64, (String, mpsc::Receiver<SlowAnswer>)>>,
        /// The slow worker waits on this before answering.
        gate_rx: Mutex<Option<mpsc::Receiver<()>>>,
        /// The fast worker fires this after answering.
        gate_tx: Mutex<Option<mpsc::Sender<()>>>,
        /// When set, every worker holds its answer until this many questions
        /// have been begun. See [`FanHost::latched`].
        latch: Option<Arc<Latch>>,
    }

    /// The begun-count latch [`FanHost::latched`] workers wait on.
    struct Latch {
        expected: usize,
        begun: Mutex<usize>,
        all_begun: Condvar,
    }

    impl FanHost {
        /// A host whose `slow` answer arrives strictly after its `fast` one.
        fn gated(asynchronous: bool) -> Self {
            let (tx, rx) = mpsc::channel();
            FanHost {
                gate_rx: Mutex::new(Some(rx)),
                gate_tx: Mutex::new(Some(tx)),
                ..Self::ungated(asynchronous)
            }
        }

        /// A host whose workers answer as soon as they run, for fixtures with
        /// only one fetch in them.
        fn ungated(asynchronous: bool) -> Self {
            FanHost {
                asynchronous,
                events: Arc::new(Mutex::new(Vec::new())),
                next: AtomicU64::new(1),
                pending: Mutex::new(HashMap::new()),
                gate_rx: Mutex::new(None),
                gate_tx: Mutex::new(None),
                latch: None,
            }
        }

        /// A host all of whose workers hold their answers until `expected`
        /// questions have been begun. On this host "every sibling was begun
        /// before any answer arrived" is a fact of the fixture when the test
        /// passes, and a visible interleaving in the event log when it does
        /// not: a drive whose begins stall (because the cascade broke and
        /// the next begin was waiting on the first answer) hits the workers'
        /// 10s timeout instead of deadlocking, and the log says which begins
        /// are missing.
        fn latched(expected: usize) -> Self {
            FanHost {
                latch: Some(Arc::new(Latch {
                    expected,
                    begun: Mutex::new(0),
                    all_begun: Condvar::new(),
                })),
                ..Self::ungated(true)
            }
        }

        fn note(&self, event: String) {
            lock(&self.events).push(event);
        }

        fn events(&self) -> Vec<String> {
            lock(&self.events).clone()
        }
    }

    impl Host for FanHost {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            lock_flake,
            fetch_tree,
        );
        crate::host::host_stubs!(
            get_env,
            copy_to_store,
            ensure_path,
            find_file,
            nix_path,
            file_type_resolved
        );
        fn read_file(&self, p: &crate::value2::PathValue) -> Result<String, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn read_dir(
            &self,
            p: &crate::value2::PathValue,
        ) -> Result<Vec<(String, FileType)>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn path_exists_checked(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            Ok(false)
        }
        fn dir_exists_checked(
            &self,
            path: &crate::value2::PathValue,
        ) -> std::result::Result<bool, String> {
            self.file_type_resolved(path)
                .map(|kind| kind == crate::host::FileType::Directory)
        }
        fn file_type(&self, p: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
            Err(format!("path '{p}' does not exist"))
        }
        fn trace(&self, message: &str) {
            self.note(format!("trace {message}"));
        }
        fn warn(&self, message: &str) {
            self.note(format!("warn {message}"));
        }
        fn fetch(&self, request: &crate::task::FetchRequest) -> Result<String, StoreError> {
            self.note(format!("fetch {}", request.name));
            Ok(store_path_for(&request.name))
        }

        fn begin(&self, question: &Slow<'_>) -> Option<Ticket> {
            if !self.asynchronous {
                return None;
            }
            let Slow::Fetch(request) = question else {
                return None;
            };
            let name = request.name.clone();
            self.note(format!("begin {name}"));
            if let Some(latch) = &self.latch {
                *lock(&latch.begun) += 1;
                latch.all_begun.notify_all();
            }
            let latch = self.latch.clone();
            let answer = store_path_for(&name);
            let events = Arc::clone(&self.events);
            let wait_for = if name == "slow" {
                lock(&self.gate_rx).take()
            } else {
                None
            };
            let signal = if name == "fast" {
                lock(&self.gate_tx).take()
            } else {
                None
            };
            let (tx, rx) = mpsc::channel();
            let id = self.next.fetch_add(1, Ordering::SeqCst);
            let worker_name = name.clone();
            drop(std::thread::spawn(move || {
                if let Some(gate) = wait_for {
                    // Answer only after the fast fetch has. The timeout is a
                    // failure mode, not a schedule: it only fires if the fast
                    // fetch was never begun, and then the event log says so.
                    let _ = gate.recv_timeout(Duration::from_secs(10));
                }
                if let Some(latch) = latch {
                    // Hold the answer until every sibling is begun. Timeout
                    // as above: it fires only when the cascade stalled.
                    drop(latch.all_begun.wait_timeout_while(
                        lock(&latch.begun),
                        Duration::from_secs(10),
                        |begun| *begun < latch.expected,
                    ));
                }
                lock(&events).push(format!("answered {worker_name}"));
                let _ = tx.send(SlowAnswer::Store(Ok(answer)));
                if let Some(signal) = signal {
                    let _ = signal.send(());
                }
            }));
            lock(&self.pending).insert(id, (name, rx));
            Some(Ticket(id))
        }

        fn collect(&self, ticket: Ticket, block: bool) -> Option<SlowAnswer> {
            let (name, rx) = lock(&self.pending).remove(&ticket.0)?;
            let received = if block {
                rx.recv().ok()
            } else {
                rx.try_recv().ok()
            };
            match received {
                Some(answer) => {
                    // Noted when the answer is handed over, not when it was
                    // asked for: this is the moment delivery happens, which
                    // is the order the tests are about.
                    self.note(format!("collect {name}"));
                    Some(answer)
                }
                None => {
                    if !block {
                        lock(&self.pending).insert(ticket.0, (name, rx));
                    }
                    None
                }
            }
        }
    }

    /// The scheduler settles every finished job exactly once, value or error,
    /// and a settle failure is an error only where there was a value to lose:
    /// a failed evaluation keeps its own error and the settle's is a warning,
    /// never silence. Every deferred host effect (the queued derivation
    /// writes of the bridge host) rests on this; the C API used to flush per
    /// entry point and missed one.
    #[test]
    fn a_finished_job_settles_once() {
        use crate::host::Host;
        use std::cell::{Cell, RefCell};

        struct Settling {
            settles: Cell<u32>,
            refuse: bool,
            warnings: RefCell<Vec<String>>,
        }
        impl Settling {
            fn new(refuse: bool) -> Self {
                Self {
                    settles: Cell::new(0),
                    refuse,
                    warnings: RefCell::new(Vec::new()),
                }
            }
        }
        impl Host for Settling {
            crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
            fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
                self.read_file(path).map(String::into_bytes)
            }
            crate::host::host_stubs!(
                realise,
                store_text,
                write_derivation,
                store_filtered,
                fetch,
                lock_flake,
                fetch_tree,
                not_async,
            );
            crate::host::host_stubs!(
                file_type_resolved,
                get_env,
                copy_to_store,
                ensure_path,
                find_file,
                nix_path,
                trace
            );
            fn read_file(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<String, String> {
                Err(format!("path '{path}' does not exist"))
            }
            fn read_dir(
                &self,
                path: &crate::value2::PathValue,
            ) -> std::result::Result<Vec<(String, crate::host::FileType)>, String> {
                Err(format!("path '{path}' does not exist"))
            }
            fn path_exists_checked(
                &self,
                _path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(false)
            }
            fn dir_exists_checked(
                &self,
                _path: &crate::value2::PathValue,
            ) -> std::result::Result<bool, String> {
                Ok(false)
            }
            fn file_type(
                &self,
                _path: &crate::value2::PathValue,
            ) -> std::result::Result<Option<crate::host::FileType>, String> {
                Ok(None)
            }
            fn settle(&self) -> Result<(), crate::host::StoreError> {
                self.settles.set(self.settles.get() + 1);
                if self.refuse {
                    Err(crate::host::StoreError::Failed(
                        "the store refused the batch".to_owned(),
                    ))
                } else {
                    Ok(())
                }
            }
            fn warn(&self, message: &str) {
                self.warnings.borrow_mut().push(message.to_owned());
            }
        }

        let host = Settling::new(false);
        assert_eq!(run(&host, r#""a" + "b""#).as_deref(), Ok("ab"));
        assert_eq!(
            host.settles.get(),
            1,
            "one settle for one finished evaluation"
        );
        assert!(host.warnings.borrow().is_empty());

        let refusing = Settling::new(true);
        let text = run(&refusing, r#""a" + "b""#).unwrap();
        assert!(
            text.contains(
                "settling the evaluation's deferred store effects: the store refused the batch"
            ),
            "a value whose deferred effects the store refused is not an answer: {text}"
        );
        assert_eq!(refusing.settles.get(), 1);
        assert!(
            refusing.warnings.borrow().is_empty(),
            "the refusal IS the error, not a warning"
        );

        let failing = Settling::new(true);
        let text = run(&failing, r#"throw "boom""#).unwrap();
        assert!(
            text.contains("boom"),
            "the evaluation's own error is the one reported: {text}"
        );
        assert!(!text.contains("deferred store effects"), "{text}");
        assert_eq!(
            failing.settles.get(),
            1,
            "a failed evaluation still settles"
        );
        let warnings = failing.warnings.borrow();
        assert_eq!(warnings.len(), 1, "and the refusal is heard: {warnings:?}");
        assert!(
            warnings[0].contains("settling the failed evaluation's deferred store effects: the store refused the batch"),
            "{warnings:?}"
        );
    }

    /// Evaluate `src` against `host`: the string value itself on success, the
    /// position-scrubbed debug form of the error otherwise.
    fn run(host: &dyn Host, src: &str) -> Result<String, String> {
        run_debugged(host, src, vm_debug_without_pos)
    }

    /// A trailing slash selects cppnix's full-resolution directory check.
    /// Its error outcome must survive the evaluator: the old implementation
    /// folded every `file_type_resolved` failure into `false`, so a vanished
    /// mounted root could satisfy this test only after becoming observable.
    #[test]
    fn trailing_slash_path_exists_propagates_host_errors() -> Result<(), String> {
        use crate::host::{FileType, FnHost, PathReadHooks};

        fn read_file(_path: &crate::value2::PathValue) -> Result<String, String> {
            Err("not asked".to_owned())
        }
        fn exists(_path: &crate::value2::PathValue) -> Result<bool, String> {
            Ok(false)
        }
        fn dir_exists(_path: &crate::value2::PathValue) -> Result<bool, String> {
            Err("mounted root disappeared".to_owned())
        }
        fn read_dir(_path: &crate::value2::PathValue) -> Result<Vec<(String, FileType)>, String> {
            Err("not asked".to_owned())
        }
        fn file_type(_path: &crate::value2::PathValue) -> Result<Option<FileType>, String> {
            Ok(None)
        }
        fn file_type_resolved(_path: &crate::value2::PathValue) -> Result<FileType, String> {
            Err("mounted root disappeared through the old route".to_owned())
        }

        let host = FnHost {
            path_reads: Some(PathReadHooks {
                read_file,
                path_exists: exists,
                dir_exists,
                read_dir,
                file_type,
                file_type_resolved,
            }),
            ..FnHost::default()
        };
        let answer = run(&host, "builtins.pathExists \"/gone/\"")?;
        if !answer.contains("mounted root disappeared") {
            return Err(format!(
                "trailing-slash existence swallowed its host error: {answer}"
            ));
        }
        Ok(())
    }

    /// Like [`run`], but an error keeps its position: for the tests whose
    /// claim is "the very same failure", where the position is part of the
    /// sameness.
    fn run_keeping_positions(host: &dyn Host, src: &str) -> Result<String, String> {
        run_debugged(host, src, |error| format!("{error:?}"))
    }

    fn run_debugged(
        host: &dyn Host,
        src: &str,
        debug: impl Fn(&crate::vm::VmError) -> String,
    ) -> Result<String, String> {
        let settings = Settings::default();
        let module = compile::compile_source(src, "/m", compile::Origin::String, &settings)
            .map_err(|e| format!("compile failed: {e:?}"))?;
        let mut vm = Vm::with_settings(settings);
        vm.start_module(&Rc::new(module));
        Ok(match drive(&mut vm, host) {
            Ok(Value::Str(s)) => s.expect_text(),
            Ok(other) => format!("{other:?}"),
            Err(error) => debug(&error),
        })
    }

    /// The evaluate-then-render embedder flow: a first drive for the module's
    /// value, a second for its printed form. This is the flow that trips over
    /// a slot the first drive left blackholed -- the second drive is where a
    /// poisoned value surfaces.
    fn eval_then_print(host: &dyn Host, src: &str) -> Result<String, String> {
        let settings = Settings::default();
        let module = compile::compile_source(src, "/m", compile::Origin::String, &settings)
            .map_err(|e| format!("compile failed: {e:?}"))?;
        let mut vm = Vm::with_settings(settings);
        vm.start_module(&Rc::new(module));
        let value = match drive(&mut vm, host) {
            Ok(v) => v,
            Err(error) => return Ok(format!("eval-err: {}", vm_debug_without_pos(&error))),
        };
        vm.start_print(value);
        Ok(match drive(&mut vm, host) {
            Ok(Value::Str(s)) => format!("ok: {}", s.expect_text()),
            Ok(other) => format!("ok: {other:?}"),
            Err(error) => format!("print-err: {}", vm_debug_without_pos(&error)),
        })
    }

    /// What both drives must produce for [`TWO_FETCHES`].
    fn two_fetches_json() -> String {
        format!(
            "[\"{}\",\"{}\"]",
            store_path_for("slow"),
            store_path_for("fast")
        )
    }

    /// (a) Both questions are begun before either answer is collected, and
    /// the answers are delivered in ask order even though they arrived in
    /// the other one.
    #[test]
    fn two_strands_overlap_before_either_answer_is_collected() -> std::result::Result<(), String> {
        let host = FanHost::gated(true);
        let out = run(&host, TWO_FETCHES)?;
        if out != two_fetches_json() {
            return Err(format!("fan-out drive produced {out:?}"));
        }
        // Every step of this sequence is causally ordered by the fixture --
        // eval-thread events by program order, worker events by the gate --
        // so the whole log is deterministic, and it says: both begun, then
        // both arrived (fast first), then both collected (slow first).
        let events = host.events();
        if events
            != [
                "begin slow",
                "begin fast",
                "answered fast",
                "answered slow",
                "collect slow",
                "collect fast",
            ]
        {
            return Err(format!("the drive did not overlap: {events:?}"));
        }
        Ok(())
    }

    /// (b) The recorded read set is byte-identical between the sequential
    /// drive and the fan-out drive, on the fixture whose answers arrive out
    /// of order.
    #[test]
    fn the_read_set_is_byte_identical_between_sequential_and_fanout_drives()
    -> std::result::Result<(), String> {
        let sequential_inner = FanHost::gated(false);
        let sequential = RecordingHost::new(&sequential_inner);
        let sequential_out = run(&sequential, TWO_FETCHES)?;
        let sequential_set = sequential.take();
        if sequential_inner.events() != ["fetch slow", "fetch fast"] {
            return Err(format!(
                "the baseline was not sequential: {:?}",
                sequential_inner.events()
            ));
        }

        let fanout_inner = FanHost::gated(true);
        let fanout = RecordingHost::new(&fanout_inner);
        let fanout_out = run(&fanout, TWO_FETCHES)?;
        let fanout_set = fanout.take();
        // The overlap really happened and the answers really arrived out of
        // order; without this the equality below would also pass for a
        // fan-out that silently never engaged.
        if fanout_inner.events()
            != [
                "begin slow",
                "begin fast",
                "answered fast",
                "answered slow",
                "collect slow",
                "collect fast",
            ]
        {
            return Err(format!(
                "the fan-out drive did not overlap: {:?}",
                fanout_inner.events()
            ));
        }

        if sequential_out != fanout_out {
            return Err(format!(
                "the two drives disagree: {sequential_out:?} vs {fanout_out:?}"
            ));
        }
        // `ReadSet` equality is structural over (question, answer-digest)
        // pairs in log order, so equal here means the serialized bytes and
        // the memo key are equal too.
        if sequential_set != fanout_set {
            return Err(format!(
                "the read sets diverged:\n  sequential: {sequential_set:?}\n  fan-out:    {fanout_set:?}"
            ));
        }
        Ok(())
    }

    /// (c) A host that begins nothing sees the previous behaviour exactly:
    /// the same questions, inline, in program order, and the same value.
    #[test]
    fn a_host_that_begins_nothing_is_driven_exactly_as_before() -> std::result::Result<(), String> {
        let host = FanHost::gated(false);
        let out = run(&host, TWO_FETCHES)?;
        if out != two_fetches_json() {
            return Err(format!("sequential drive produced {out:?}"));
        }
        // Two inline fetches and nothing else: no begin, no collect, no
        // worker, and the second is asked only after the first was answered.
        let events = host.events();
        if events != ["fetch slow", "fetch fast"] {
            return Err(format!("expected two inline fetches, got {events:?}"));
        }
        Ok(())
    }

    /// (d) A strand that fails memoizes the failure into its slot and the
    /// walk re-raises it exactly where the sequential drive raises it.
    #[test]
    fn a_failing_strand_memoizes_and_re_raises_exactly_as_sequential()
    -> std::result::Result<(), String> {
        let sequential = FanHost::ungated(false);
        // Positions kept deliberately: "exactly as sequential" includes the
        // position the error is reported at, not just its message.
        let sequential_out = run_keeping_positions(&sequential, FETCH_THEN_THROW)?;
        if !sequential_out.contains("boom") {
            return Err(format!("the baseline did not throw: {sequential_out:?}"));
        }
        // Sequentially the second child runs only after the fetch answered.
        if sequential.events() != ["fetch slow", "trace second ran"] {
            return Err(format!(
                "unexpected sequential order: {:?}",
                sequential.events()
            ));
        }

        let fanout = FanHost::ungated(true);
        let fanout_out = run_keeping_positions(&fanout, FETCH_THEN_THROW)?;
        if fanout_out != sequential_out {
            return Err(format!(
                "the failure changed shape: {sequential_out:?} vs {fanout_out:?}"
            ));
        }
        // The strand ran -- and failed -- while the fetch was still in
        // flight (its trace lands before the collect), and the failure was
        // held in the slot rather than aborting the run: the fetch's answer
        // was still collected, and only then did the walk re-raise.
        let events = fanout.events();
        let begin = events.iter().position(|e| e == "begin slow");
        let trace = events.iter().position(|e| e == "trace second ran");
        let collect = events.iter().position(|e| e == "collect slow");
        if !(begin.is_some() && begin < trace && trace < collect) {
            return Err(format!("expected begin < trace < collect, got {events:?}"));
        }
        Ok(())
    }

    /// A `tryEval` catches the first child's throw past the walk, so the
    /// second child's strand is still in flight when the root finishes --
    /// and the second element stays reachable through `xs`, so the print
    /// drive forces the very slot the strand was forcing (review C1).
    const ABANDONING_TRY_EVAL: &str = r#"let xs = [
      (builtins.seq (builtins.fetchurl { url = "http://example.invalid/slow"; name = "slow"; }) (throw "later"))
      (builtins.fetchurl { url = "http://example.invalid/fast"; name = "fast"; })
    ]; in builtins.seq (builtins.tryEval (builtins.toJSON xs)) { second = builtins.elemAt xs 1; }"#;

    /// A strand still in flight when the root's value is ready is drained,
    /// not abandoned: abandoning it mid-force would leave its slot
    /// blackholed forever inside the returned value, and the print drive
    /// would report a cycle that is not there.
    #[test]
    fn a_strand_in_flight_when_the_root_finishes_is_drained_not_abandoned()
    -> std::result::Result<(), String> {
        let sequential = eval_then_print(&FanHost::gated(false), ABANDONING_TRY_EVAL)?;
        if !sequential.starts_with("ok: ") {
            return Err(format!("the baseline did not print: {sequential:?}"));
        }
        let fanout = eval_then_print(&FanHost::gated(true), ABANDONING_TRY_EVAL)?;
        if fanout != sequential {
            return Err(format!(
                "fan-out diverged from sequential: {sequential:?} vs {fanout:?}"
            ));
        }
        Ok(())
    }

    /// A refusal on a speculative strand must not fail a run the sequential
    /// drive completes: here the program abandons the subtree holding the
    /// unimplemented builtin, so sequentially it is never forced at all.
    /// The refusal is memoized into the strand's slot instead -- re-forcing
    /// it would re-raise, but nothing here ever does.
    #[test]
    fn a_speculative_refusal_does_not_fail_a_run_sequential_completes()
    -> std::result::Result<(), String> {
        const SRC: &str = r#"(builtins.tryEval (builtins.toJSON [
          (builtins.seq (builtins.fetchurl { url = "http://example.invalid/slow"; name = "slow"; }) (throw "later"))
          (builtins.fetchMercurial { url = "http://example.invalid"; })
        ])).success"#;
        let sequential = run(&FanHost::ungated(false), SRC)?;
        let fanout = run(&FanHost::ungated(true), SRC)?;
        if fanout != sequential {
            return Err(format!(
                "fan-out diverged from sequential: {sequential:?} vs {fanout:?}"
            ));
        }
        Ok(())
    }

    /// A force cycle whose halves are entered by different fibers -- each
    /// parks waiting on the slot the other holds -- is the same program
    /// error a single chain reports, and must keep cppnix's wording rather
    /// than surface as an internal defect.
    #[test]
    fn a_force_cycle_split_across_two_fibers_is_still_infinite_recursion()
    -> std::result::Result<(), String> {
        const SRC: &str = r#"let
          a = builtins.seq (builtins.fetchurl { url = "http://example.invalid/slow"; name = "slow"; }) (builtins.head [ b ]);
          b = builtins.head [ a ];
        in builtins.toJSON [ a b ]"#;
        let sequential = run(&FanHost::ungated(false), SRC)?;
        if !sequential.contains("infinite recursion") {
            return Err(format!("the baseline did not cycle: {sequential:?}"));
        }
        let fanout = run(&FanHost::ungated(true), SRC)?;
        if fanout != sequential {
            return Err(format!(
                "fan-out diverged from sequential: {sequential:?} vs {fanout:?}"
            ));
        }
        Ok(())
    }

    /// Three fetches in one walk. On a [`FanHost::latched`] host no answer
    /// arrives until all three are begun, so the evaluation only finishes
    /// promptly if the begins cascade without waiting on an answer: the
    /// first begun question seeds sibling two off the walk's offer, sibling
    /// two's begin seeds sibling three. With the offer a single slot instead
    /// of a queue, the third begin waited for the first answer and overlap
    /// capped at two builds however many siblings were pending.
    const THREE_FETCHES: &str = r#"builtins.toJSON [
      (builtins.fetchurl { url = "http://example.invalid/a"; name = "a"; })
      (builtins.fetchurl { url = "http://example.invalid/b"; name = "b"; })
      (builtins.fetchurl { url = "http://example.invalid/c"; name = "c"; })
    ]"#;

    /// The same three, reached through the plain printer -- the walk
    /// `nix-instantiate --eval --strict` renders with -- instead of
    /// `toJSON`'s deepwalk. This is the shape of the ifd-overlap acceptance
    /// gate: N import-from-derivation results in one printed list. It went
    /// strictly serial before the printer published a fan-out offer at all.
    const THREE_FETCHES_PRINTED: &str = r#"[
      (builtins.fetchurl { url = "http://example.invalid/a"; name = "a"; })
      (builtins.fetchurl { url = "http://example.invalid/b"; name = "b"; })
      (builtins.fetchurl { url = "http://example.invalid/c"; name = "c"; })
    ]"#;

    /// What a latched drive's event log must look like: all `expected`
    /// begins first, in program order, before any answer -- that prefix is
    /// the overlap claim -- and the collects in that same order among
    /// themselves, which is the delivery claim. The `answered` events are
    /// worker-side notes the latch releases all at once, so their order --
    /// including relative to the collects they race with -- is deliberately
    /// not asserted.
    fn assert_latched_fanout(events: &[String], names: &[&str]) -> std::result::Result<(), String> {
        let n = names.len();
        let begins: Vec<String> = names.iter().map(|w| format!("begin {w}")).collect();
        if events.get(..n) != Some(&begins[..]) {
            return Err(format!("the begins did not cascade: {events:?}"));
        }
        let collects: Vec<&String> = events
            .iter()
            .filter(|e| e.starts_with("collect "))
            .collect();
        let expected: Vec<String> = names.iter().map(|w| format!("collect {w}")).collect();
        if collects.len() != n
            || collects
                .iter()
                .zip(&expected)
                .any(|(got, want)| *got != want)
        {
            return Err(format!("delivery left ask order: {events:?}"));
        }
        Ok(())
    }

    /// Every sibling's question is begun before the first answer arrives,
    /// and delivery still happens in ask order.
    #[test]
    fn every_sibling_is_begun_before_the_first_answer_arrives() -> std::result::Result<(), String> {
        let host = FanHost::latched(3);
        let out = run(&host, THREE_FETCHES)?;
        let expected = format!(
            "[\"{}\",\"{}\",\"{}\"]",
            store_path_for("a"),
            store_path_for("b"),
            store_path_for("c")
        );
        if out != expected {
            return Err(format!("the latched drive produced {out:?}"));
        }
        assert_latched_fanout(&host.events(), &["a", "b", "c"])
    }

    /// The plain printer fans out the way the deepwalk does, and prints
    /// byte-for-byte what the sequential drive prints.
    #[test]
    fn the_plain_printer_fans_out_like_the_deepwalk() -> std::result::Result<(), String> {
        let sequential_host = FanHost::gated(false);
        let sequential = eval_then_print(&sequential_host, THREE_FETCHES_PRINTED)?;
        if sequential_host.events() != ["fetch a", "fetch b", "fetch c"] {
            return Err(format!(
                "the baseline was not sequential: {:?}",
                sequential_host.events()
            ));
        }

        let host = FanHost::latched(3);
        let fanout = eval_then_print(&host, THREE_FETCHES_PRINTED)?;
        if fanout != sequential {
            return Err(format!(
                "the printed forms diverged: {sequential:?} vs {fanout:?}"
            ));
        }
        assert_latched_fanout(&host.events(), &["a", "b", "c"])
    }

    /// Each element runs a whole nested walk (`toJSON` here; `derivation`'s
    /// attribute walk on the real bridge) before its fetch is asked.
    const NESTED_FETCHES: &str = r#"[
      (builtins.seq (builtins.toJSON { inner = "a"; }) (builtins.fetchurl { url = "http://example.invalid/a"; name = "a"; }))
      (builtins.seq (builtins.toJSON { inner = "b"; }) (builtins.fetchurl { url = "http://example.invalid/b"; name = "b"; }))
      (builtins.seq (builtins.toJSON { inner = "c"; }) (builtins.fetchurl { url = "http://example.invalid/c"; name = "c"; }))
    ]"#;

    /// A walk nested inside a printed element publishes its own offer while
    /// it runs; it must set the printer's aside and put it back rather than
    /// replace it. Replaced for good, the machine has nothing to seed at the
    /// moment the element's fetch finally parks -- which is how
    /// `[ (import (derivation ...)) ... ]` stayed serial through the real
    /// bridge while every un-nested fixture here was green.
    #[test]
    fn a_nested_walk_sets_the_outer_offer_aside_and_puts_it_back() -> std::result::Result<(), String>
    {
        let sequential = eval_then_print(&FanHost::gated(false), NESTED_FETCHES)?;
        let host = FanHost::latched(3);
        let fanout = eval_then_print(&host, NESTED_FETCHES)?;
        if fanout != sequential {
            return Err(format!(
                "the printed forms diverged: {sequential:?} vs {fanout:?}"
            ));
        }
        assert_latched_fanout(&host.events(), &["a", "b", "c"])
    }
}
