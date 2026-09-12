//! Run several evaluations at once, through the crate's own scheduler.
//!
//! # Why this exists rather than a loop over [`nix_eval_rs::eval::drive`]
//!
//! [`nix_eval_rs::eval::drive_concurrent`] is the crate's answer to "several
//! independent evaluations, overlapping the time they spend waiting on the
//! world", and `drive` is written as a one-job call of it. Going through the
//! concurrent form for one job as well as for many means the driver has one
//! code path, and that path is the one under load.
//!
//! # What overlaps today: nothing, and this is not a hedge
//!
//! The scheduler parks a job only on a [`nix_eval_rs::host::Slow`] question,
//! and there are exactly four: `Fetch`, `FetchTree`, `Flake`, `Realise`
//! (`eval.rs`'s `begin_slow` returns `Ok(None)` for every other variant, so
//! everything else is answered inline and cannot yield). This driver's host
//! refuses all four by name -- three because CLAUDE.md keeps the fetchers and
//! flake locking in the C++ bridge, one because a local store cannot realise.
//!
//! So on today's driver the scheduler runs each job to completion in turn and
//! **no wall clock improves**. Claiming a speedup here would be claiming one
//! for a code path that is never taken; the honest statement is that the
//! wiring is in place and correct, which
//! [`tests::the_driver_overlaps_slow_questions_when_the_host_has_an_async_path`]
//! demonstrates against a host that does answer a slow question, measuring
//! peak in-flight. The day a Rust fetcher lands behind
//! [`crate::host::DriverHost`], the overlap arrives with no change here.
//!
//! # Two passes, not one
//!
//! Evaluating and then rendering are separate scheduler runs. A `Vm` can only
//! be seeded with one piece of work at a time (`start_module`, then
//! `start_print`), and printing is itself iterative on the machine so a deep
//! value cannot blow the native stack. Doing all the evaluating first also
//! keeps the printing pass out of the way of any overlap the first pass gets.

use nix_eval_rs::compile;
use nix_eval_rs::eval::{EvalError, Settings, drive_concurrent, map_vm_error};
use nix_eval_rs::host::Host;
use nix_eval_rs::value2::Value;
use nix_eval_rs::vm::{Vm, VmError};
use std::rc::Rc;

/// One thing to evaluate.
#[derive(Clone, Debug)]
pub struct Request {
    /// How the caller names this job. Carried through so an [`Outcome`] can
    /// be reported without the caller matching up positions by hand -- which
    /// it could, since the vectors correspond, but a label in the failure
    /// text is what makes a gate log readable.
    pub label: String,
    /// The source text actually compiled.
    ///
    /// Not necessarily the file's own bytes: `main.rs` puts `import <path>`
    /// here for a file input, and wraps the whole thing again for `-A` and
    /// for `instantiate`. `from_file` still names the file, so positions
    /// reported against the wrapper are attributed to it -- a Tier 2
    /// inaccuracy, and the reason this field is documented as "what is
    /// compiled" rather than "the file".
    pub source: String,
    /// What a relative path in `source` resolves against. cppnix uses the
    /// file's directory for a file and the working directory for `--expr`.
    pub base_dir: String,
    /// The path `source` came from, or `None` for `--expr`. It decides what
    /// `__curPos` reports and how errors are located, so it is not cosmetic:
    /// `--expr` gets `Origin::String` and a null position, exactly as cppnix
    /// does.
    pub from_file: Option<String>,
}

/// How to turn the final value into bytes on stdout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Render {
    /// The value printed the way `nix-instantiate --eval --strict` prints it:
    /// the whole structure forced, strings quoted.
    Strict,
    /// A string value emitted bare, with no quotes and no trailing newline
    /// handling of its own -- `nix eval --raw`. Anything that is not a string
    /// is an error rather than a coercion, because the two callers of this
    /// (`.drvPath`, `.outPath`) are always strings and a silent coercion
    /// would turn a wrong-shape result into a plausible-looking path.
    Raw,
}

/// Why a job produced no value.
#[derive(Clone, Debug)]
pub enum Failure {
    /// The rust evaluator, or this driver's host, does not implement
    /// something the program asked for.
    ///
    /// Separate from [`Failure::Error`] because the two mean opposite things
    /// to a differential gate: a refusal is a known gap that scores as
    /// `unimplemented`, an error is a divergence to investigate. Collapsing
    /// them would let a regression hide in the gap column.
    Unimplemented { token: String, detail: String },
    /// A genuine evaluation, parse or compile failure.
    Error(String),
}

impl Failure {
    /// The one line this failure prints on stderr.
    ///
    /// The refusal spelling matches the bridge's, `rust-eval unimplemented:
    /// [token] detail`, so a harness that already greps the bridge's stderr
    /// for a refusal reads this driver with no second pattern -- and
    /// `lang-diff.sh` and `drv-parity.sh` both do exactly that grep.
    #[must_use]
    pub fn message(&self) -> String {
        match self {
            Failure::Unimplemented { token, detail } => {
                format!("rust-eval unimplemented: [{token}] {detail}")
            }
            Failure::Error(text) => text.clone(),
        }
    }

    /// Whether this is a refusal, which a caller counts separately from an
    /// error.
    #[must_use]
    pub fn is_unimplemented(&self) -> bool {
        matches!(self, Failure::Unimplemented { .. })
    }
}

/// What one [`Request`] produced.
#[derive(Clone, Debug)]
pub struct Outcome {
    pub label: String,
    /// Bytes, not text: cppnix writes a rendered value to stdout byte-wise,
    /// and a non-UTF-8 string (ENG-13147) renders to exactly those bytes.
    pub result: Result<Vec<u8>, Failure>,
}

/// Where one job has got to.
///
/// The `Vm` is boxed because it is by far the largest variant and an unboxed
/// one would make every `Slot` in the vector that size, including the
/// finished ones.
enum Slot {
    Running(Box<Vm>),
    Done(Vec<u8>),
    Failed(Failure),
}

/// One job's answer from a scheduler pass, kept beside the index it belongs
/// to.
///
/// A named struct and not `(usize, Result<..>)`: the fork denies
/// `anonymous_tuple_return_type`, and the reason it does applies here --
/// `slot` and `value` are not interchangeable and a positional pair invites
/// swapping them at one of the two call sites.
struct Produced {
    slot: usize,
    value: Result<Value, VmError>,
}

/// Evaluate every request, then render every value, and return one
/// [`Outcome`] per request **in the order they were given**.
///
/// Order is part of the contract because a caller reports results positionally
/// and a compacted vector would silently attribute one job's answer to
/// another. A job that fails to compile therefore still occupies its slot.
pub fn evaluate(
    requests: &[Request],
    settings: &Settings,
    host: &dyn Host,
    render: Render,
) -> Vec<Outcome> {
    let mut slots: Vec<Slot> = requests
        .iter()
        .map(|request| {
            let origin = match &request.from_file {
                Some(path) => compile::Origin::File(path),
                None => compile::Origin::String,
            };
            match compile::compile_source(&request.source, &request.base_dir, origin, settings) {
                Ok(module) => {
                    let mut vm = Vm::with_settings(settings.clone());
                    vm.start_module(&Rc::new(module));
                    Slot::Running(Box::new(vm))
                }
                Err(error) => Slot::Failed(failure_of(&EvalError::from(error))),
            }
        })
        .collect();

    // Pass one: evaluate. Everything still running goes to the scheduler
    // together, which is the whole point of the driver.
    let mut values: Vec<Option<Value>> = requests.iter().map(|_| None).collect();
    for produced in drive_running(&mut slots, host) {
        match produced.value {
            Ok(value) => {
                if let Some(place) = values.get_mut(produced.slot) {
                    *place = Some(value);
                }
            }
            Err(error) => {
                if let Some(place) = slots.get_mut(produced.slot) {
                    *place = Slot::Failed(failure_of(&map_vm_error(error)));
                }
            }
        }
    }

    // Pass two: render. `Render::Raw` never enters the machine, since a
    // string is already the answer; `Render::Strict` seeds the printer, which
    // can itself ask the host (a path in a value is coerced, a thunk deeper
    // in the structure is forced) and so belongs in the scheduler too.
    for (index, value) in values.into_iter().enumerate() {
        let Some(value) = value else { continue };
        match render {
            Render::Raw => {
                let rendered = match &value {
                    Value::Str(s) => Ok(s.bytes().to_vec()),
                    other => Err(Failure::Error(format!(
                        "expected a string, got {}",
                        nix_eval_rs::value2::type_name(other)
                    ))),
                };
                if let Some(place) = slots.get_mut(index) {
                    *place = match rendered {
                        Ok(text) => Slot::Done(text),
                        Err(failure) => Slot::Failed(failure),
                    };
                }
            }
            Render::Strict => {
                if let Some(Slot::Running(vm)) = slots.get_mut(index) {
                    vm.start_print(value);
                }
            }
        }
    }
    if render == Render::Strict {
        for produced in drive_running(&mut slots, host) {
            let filled = match produced.value {
                Ok(Value::Str(s)) => Slot::Done(s.bytes().to_vec()),
                Ok(other) => Slot::Failed(Failure::Error(format!(
                    "the printer returned {}, not a string",
                    nix_eval_rs::value2::type_name(&other)
                ))),
                Err(error) => Slot::Failed(failure_of(&map_vm_error(error))),
            };
            if let Some(place) = slots.get_mut(produced.slot) {
                *place = filled;
            }
        }
    }

    slots
        .into_iter()
        .zip(requests)
        .map(|(slot, request)| Outcome {
            label: request.label.clone(),
            result: match slot {
                Slot::Done(text) => Ok(text),
                Slot::Failed(failure) => Err(failure),
                // A slot still `Running` after both passes means the machine
                // was seeded and the scheduler did not return it, which is a
                // defect in this function rather than in the program being
                // evaluated. Say so rather than reporting an empty answer.
                Slot::Running(_) => Err(Failure::Error(String::from(
                    "internal: the driver left an evaluation unfinished",
                ))),
            },
        })
        .collect()
}

/// Hand every `Running` slot to the scheduler at once and return what each
/// produced, tagged with its slot.
///
/// The borrow is why this is a function rather than inline code: the
/// scheduler wants `&mut Vm` for every job simultaneously, so the indices
/// have to be gathered first and the results matched back afterwards.
fn drive_running(slots: &mut [Slot], host: &dyn Host) -> Vec<Produced> {
    let mut indices: Vec<usize> = Vec::new();
    let mut vms: Vec<&mut Vm> = Vec::new();
    for (index, slot) in slots.iter_mut().enumerate() {
        if let Slot::Running(vm) = slot {
            indices.push(index);
            vms.push(vm.as_mut());
        }
    }
    if vms.is_empty() {
        return Vec::new();
    }
    let jobs: Vec<(&mut Vm, &dyn Host)> = vms.into_iter().map(|vm| (vm, host)).collect();
    drive_concurrent(jobs)
        .into_iter()
        .zip(indices)
        .map(|(value, slot)| Produced { slot, value })
        .collect()
}

/// The one place an [`EvalError`] becomes a [`Failure`], so the
/// refusal/error split is decided once.
fn failure_of(error: &EvalError) -> Failure {
    match error {
        EvalError::Unimplemented(refusal) => Failure::Unimplemented {
            token: format!("{:?}", refusal.token),
            detail: refusal.detail.clone(),
        },
        // The position is dropped rather than rendered: this driver's failure
        // text is compared against the C++ CLI's by rust-driver-parity, and
        // cppnix's CLI does not put an `at file:line:col` into this string.
        EvalError::Eval(_, message, _) => Failure::Error(message.clone()),
        EvalError::Parse(message) => Failure::Error(message.clone()),
    }
}

#[cfg(test)]
mod tests {
    use super::{Render, Request, evaluate};
    use nix_eval_rs::eval::Settings;
    use nix_eval_rs::host::{
        FileType, Host, Links, LookupError, RealFs, Slow, SlowAnswer, StoreError, StorePathResult,
        Ticket,
    };
    use nix_eval_rs::value2::PathValue;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    /// How long a stalled question takes. Long enough that the difference
    /// between overlapping and not is far outside timer noise, short enough
    /// that the test costs under a second.
    const STALL: Duration = Duration::from_millis(200);

    /// A host whose `fetch` takes [`STALL`], answered off-thread so the
    /// scheduler can park on it.
    ///
    /// This stands in for the Rust fetcher this driver does not have. Without
    /// it the concurrency wiring is untestable, because
    /// [`crate::host::DriverHost`] refuses every question the scheduler is
    /// able to park on -- see this module's own documentation.
    struct StallHost {
        inflight: Arc<AtomicUsize>,
        peak: Arc<AtomicUsize>,
        next: AtomicUsize,
        pending: Mutex<HashMap<u64, std::thread::JoinHandle<String>>>,
    }

    impl StallHost {
        fn new() -> Self {
            StallHost {
                inflight: Arc::new(AtomicUsize::new(0)),
                peak: Arc::new(AtomicUsize::new(0)),
                next: AtomicUsize::new(1),
                pending: Mutex::new(HashMap::new()),
            }
        }
    }

    impl Host for StallHost {
        fn read_file_bytes(&self, path: &PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        fn read_file(&self, path: &PathValue) -> Result<String, String> {
            RealFs.read_file(path)
        }
        fn read_dir(&self, path: &PathValue) -> Result<Vec<(String, FileType)>, String> {
            RealFs.read_dir(path)
        }
        fn path_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
            RealFs.path_exists_checked(path)
        }
        fn dir_exists_checked(&self, path: &PathValue) -> Result<bool, String> {
            RealFs.dir_exists_checked(path)
        }
        fn file_type(&self, path: &PathValue) -> Result<Option<FileType>, String> {
            RealFs.file_type(path)
        }
        fn file_type_resolved(&self, path: &PathValue) -> Result<FileType, String> {
            RealFs.file_type_resolved(path)
        }
        fn get_env(&self, name: &str) -> Option<String> {
            RealFs.get_env(name)
        }
        fn copy_to_store(&self, path: &PathValue) -> Result<String, StoreError> {
            RealFs.copy_to_store(path)
        }
        fn store_path(&self, path: &PathValue) -> Result<StorePathResult, StoreError> {
            RealFs.store_path(path)
        }
        fn ensure_path(&self, path: &str) -> Result<(), StoreError> {
            RealFs.ensure_path(path)
        }
        fn valid_paths(
            &self,
            paths: &[String],
        ) -> Result<std::collections::BTreeSet<String>, StoreError> {
            RealFs.valid_paths(paths)
        }
        fn sealed_paths(&self, objects: &[String]) -> Result<Links, StoreError> {
            RealFs.sealed_paths(objects)
        }
        fn allow_paths(&self, paths: &[String]) -> Result<(), StoreError> {
            RealFs.allow_paths(paths)
        }
        fn allow_closures(&self, outputs: &[String]) -> Result<(), StoreError> {
            RealFs.allow_closures(outputs)
        }
        fn settle(&self) -> Result<(), StoreError> {
            Ok(())
        }
        fn nix_path(&self) -> Result<Vec<nix_eval_rs::task::SearchPathEntry>, LookupError> {
            Ok(Vec::new())
        }
        fn trace(&self, _message: &str) {}
        fn warn(&self, _message: &str) {}
        fn find_file(
            &self,
            _entries: &[nix_eval_rs::task::SearchPathEntry],
            name: &str,
        ) -> Result<PathValue, LookupError> {
            Err(LookupError::NotFound(name.to_owned()))
        }

        fn fetch(&self, _request: &nix_eval_rs::task::FetchRequest) -> Result<String, StoreError> {
            std::thread::sleep(STALL);
            Ok(String::from(
                "/nix/store/00000000000000000000000000000000-stalled",
            ))
        }

        // The effects this test host has no opinion about.
        //
        // Spelled out because `nix-eval-rs`'s `host_stubs!` is `#[cfg(test)]`
        // and `pub(crate)`, so it does not cross the crate boundary. They
        // stopped having default bodies in ENG-13107, deliberately: a default
        // that a leaf inherits is the same mechanism by which a WRAPPER
        // inherits one by accident and quietly refuses on behalf of the host
        // behind it. This is a leaf, so refusing is the truth.
        fn store_text(
            &self,
            _name: &str,
            _contents: &str,
            _references: &[String],
        ) -> Result<String, StoreError> {
            Err(StoreError::NoStore)
        }
        fn write_derivation(&self, _name: &str, _aterm: &str) -> Result<String, StoreError> {
            Err(StoreError::NoStore)
        }
        fn store_filtered(
            &self,
            _copy: &nix_eval_rs::task::FilteredCopy,
        ) -> Result<String, StoreError> {
            Err(StoreError::NoStore)
        }
        fn fetch_tree(
            &self,
            _request: &nix_eval_rs::task::FetchTreeRequest,
        ) -> Result<String, StoreError> {
            Err(StoreError::NoStore)
        }
        fn lock_flake(&self, _flake_ref: &str) -> Result<nix_eval_rs::host::FlakeCall, StoreError> {
            Err(StoreError::NoStore)
        }
        fn parse_flake_ref(&self, _flake_ref: &str) -> Result<String, StoreError> {
            Err(StoreError::NoStore)
        }
        fn flake_ref_to_string(
            &self,
            _attrs: &std::collections::BTreeMap<String, nix_eval_rs::task::TreeAttr>,
        ) -> Result<String, StoreError> {
            Err(StoreError::NoStore)
        }
        fn realise(
            &self,
            _context: &[nix_eval_rs::value2::ContextElem],
        ) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
            Err(StoreError::NoStore)
        }

        fn begin(&self, question: &Slow<'_>) -> Option<Ticket> {
            // Only the variant this test exercises. Returning `None` for the
            // rest is exactly what the default does: the scheduler answers
            // them inline.
            if !matches!(question, Slow::Fetch(_)) {
                return None;
            }
            let id = self.next.fetch_add(1, Ordering::SeqCst) as u64;
            let inflight = Arc::clone(&self.inflight);
            let peak = Arc::clone(&self.peak);
            let handle = std::thread::spawn(move || {
                let now = inflight.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                std::thread::sleep(STALL);
                inflight.fetch_sub(1, Ordering::SeqCst);
                String::from("/nix/store/00000000000000000000000000000000-stalled")
            });
            if let Ok(mut pending) = self.pending.lock() {
                pending.insert(id, handle);
                Some(Ticket(id))
            } else {
                None
            }
        }

        fn collect(&self, ticket: Ticket, block: bool) -> Option<SlowAnswer> {
            let handle = {
                let mut pending = self.pending.lock().ok()?;
                // Without `block` the scheduler is asking whether the answer
                // is ready, and a host that blocked here would defeat the
                // whole mechanism -- so report "not yet" unless it is
                // finished.
                if !block && !pending.get(&ticket.0).is_some_and(|h| h.is_finished()) {
                    return None;
                }
                pending.remove(&ticket.0)?
            };
            let path = handle.join().ok()?;
            Some(SlowAnswer::Store(Ok(path)))
        }
    }

    /// Two jobs that each stall are in flight at the same moment.
    ///
    /// Peak in-flight is the whole assertion. The stall starts inside `begin`,
    /// so a peak of 2 means both sleeps ran concurrently and the answers cost
    /// one stall however the scheduler orders its waits; a serial scheduler
    /// begins the second question only after collecting the first and peaks
    /// at 1. An elapsed-time bound on top of that measured the machine's load
    /// (the gate failed at 350ms against a 300ms bound during two concurrent
    /// nix builds, 2026-09-04) and nothing about the scheduler.
    #[test]
    fn the_driver_overlaps_slow_questions_when_the_host_has_an_async_path() -> Result<(), String> {
        let host = StallHost::new();
        let peak = Arc::clone(&host.peak);
        let requests: Vec<Request> = (0..2)
            .map(|i| Request {
                label: format!("job{i}"),
                source: String::from(
                    "builtins.fetchurl { url = \"https://example.invalid/x\"; name = \"x\"; }",
                ),
                base_dir: String::from("/"),
                from_file: None,
            })
            .collect();

        let outcomes = evaluate(&requests, &Settings::default(), &host, Render::Strict);

        if outcomes.len() != 2 {
            return Err(format!("got {} outcomes", outcomes.len()));
        }
        for outcome in &outcomes {
            if let Err(failure) = &outcome.result {
                return Err(format!("{} failed: {}", outcome.label, failure.message()));
            }
        }
        let seen = peak.load(Ordering::SeqCst);
        if seen < 2 {
            return Err(format!(
                "peak in-flight was {seen}; the two stalls did not overlap"
            ));
        }
        Ok(())
    }

    /// A job that cannot compile keeps its position, so the caller's labels
    /// still line up with its requests.
    #[test]
    fn outcomes_stay_beside_their_requests_when_one_fails_to_compile() -> Result<(), String> {
        let host = RealFs;
        let requests = vec![
            Request {
                label: String::from("first"),
                source: String::from("1 + 1"),
                base_dir: String::from("/"),
                from_file: None,
            },
            Request {
                label: String::from("broken"),
                source: String::from("let in"),
                base_dir: String::from("/"),
                from_file: None,
            },
            Request {
                label: String::from("third"),
                source: String::from("\"tail\""),
                base_dir: String::from("/"),
                from_file: None,
            },
        ];
        let outcomes = evaluate(&requests, &Settings::default(), &host, Render::Strict);
        let labels: Vec<&str> = outcomes.iter().map(|o| o.label.as_str()).collect();
        if labels != ["first", "broken", "third"] {
            return Err(format!("labels came back as {labels:?}"));
        }
        if outcomes.first().and_then(|o| o.result.as_ref().ok()) != Some(&b"2".to_vec()) {
            return Err(format!("first produced {:?}", outcomes.first()));
        }
        if outcomes.get(1).is_some_and(|o| o.result.is_ok()) {
            return Err(String::from("the broken job reported success"));
        }
        if outcomes.get(2).and_then(|o| o.result.as_ref().ok()) != Some(&b"\"tail\"".to_vec()) {
            return Err(format!("third produced {:?}", outcomes.get(2)));
        }
        Ok(())
    }
}
