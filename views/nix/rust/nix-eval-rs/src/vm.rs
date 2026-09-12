//! The evaluator's machine. Everything the interpreter does lives on an
//! explicit heap-allocated frame stack: forcing a thunk, applying a closure,
//! running a builtin and walking a value all push frames rather than host
//! stack. Nothing here recurses on the host stack proportionally to the Nix
//! value or call depth, so a 100k-deep list is an ordinary allocation rather
//! than a SIGSEGV.
//!
//! The loop is poll-shaped: `poll` runs until the program produces a value or
//! asks the scheduler for something (`Step::Perform` / `Step::NeedPath`), and
//! the scheduler answers with `resume`. The VM itself performs no IO, and a
//! suspension is a plain return from `poll` rather than a stack unwind.
//!
//! A suspension is the running [`Fiber`] -- the frame chain plus its flow and
//! call depth -- set aside in a table under the token that will wake it. So a
//! machine with a question open has *nothing on it* rather than a chain it
//! must not touch, and `poll` on one answers [`Step::Idle`] instead of
//! failing. That is what lets [`crate::eval::drive_concurrent`] leave an
//! evaluation waiting on a fetch and go run a different one, without keeping
//! its own record of which evaluations are parked.
//!
//! One fiber per machine. [`Fiber`] says what it would take for that to stop
//! being true and why it is not worth doing yet.

use crate::builtins;
use crate::ir::{Const, Module, Op, Param};
use crate::print;
use crate::refusal::{Refusal, RefusalToken};
use crate::task::{NeedPath, Task, Yield};
use crate::value2::{
    AttrMap, AttrOrigin, BuiltinData, ClosureData, Env, EnvNode, NixStr, Slot, SlotState, Sym,
    Value, type_name,
};
use rustc_hash::FxHashMap;
use rustc_hash::FxHashSet;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::rc::Rc;

/// An error the language can observe (tryEval) or report. `catchable` marks
/// the classes tryEval intercepts (throw and assert, not abort or type
/// errors), mirroring cppnix.
#[derive(Debug, Clone)]
pub struct Catchable {
    pub message: String,
    pub catchable: bool,
    pub kind: ErrKind,
    /// Where the failure happened, filled in by [`Vm::advance`] from the
    /// instruction that was running. `None` for a failure raised with no unit
    /// running -- a bad call into the C ABI, a scheduler error -- and for one
    /// raised from a module compiled with no positions.
    ///
    /// Attached on the way out rather than at the `VmError::eval` call site,
    /// because the sites are everywhere (`vm.rs`, `task.rs`, `primops_*.rs`,
    /// `drvstrict.rs`) and only the interpreter knows where the program was.
    /// A builtin therefore gets the position of the call that invoked it,
    /// which is also where cppnix reports a primop failure.
    /// Boxed, and that is a perf decision rather than a style one. `VmError`
    /// is the `Err` of every `Result` the interpreter threads through `?`, so
    /// its size is paid by the SUCCESS path on every op. An inline
    /// `Option<SrcPos>` is 32 bytes and took `VmError` from 32 to 64; a
    /// synthetic 400k-iteration fold measured +2.9% cpu for it, on a branch
    /// whose whole point was to cost nothing until something fails. The box
    /// puts the allocation on the unwind path, where one malloc is free
    /// beside rendering an error.
    pub pos: Option<Box<SrcPos>>,
}

/// A source position, resolved to what cppnix would print for it.
///
/// Line and column and not the byte offset the IR carries, because the offset
/// is meaningless without the module that produced it and this outlives the
/// module: it crosses the C ABI and is rendered by the bridge. The
/// resolution happens once, at the point the error escapes the unit it
/// happened in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SrcPos {
    /// The file, or `None` for text with no file behind it, which cppnix
    /// prints as `«string»` (`Pos::print`).
    pub file: Option<Rc<str>>,
    pub line: u32,
    pub column: u32,
}

impl SrcPos {
    /// The position of instruction `ip` of `unit` in `module`, or `None` when
    /// the module records none for it.
    #[must_use]
    pub fn of(module: &Module, unit: u32, ip: usize) -> Option<SrcPos> {
        let offset = *module.units.get(unit as usize)?.spans.get(ip)?;
        let (line, column) = module.line_col(offset)?;
        Some(SrcPos {
            file: match &module.origin {
                crate::ir::SrcOrigin::File(path) => Some(Rc::from(path.as_str())),
                crate::ir::SrcOrigin::String => None,
            },
            line,
            column,
        })
    }
}

/// Which cppnix exception class a failure corresponds to. The bridge needs
/// it to raise the matching C++ type and trace note: cppnix reports a throw
/// as ThrownError under "while calling the 'throw' builtin", and a reader
/// (or a differ classifying by text) cannot tell a throw from any other
/// failure without it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrKind {
    /// An ordinary evaluation error (type errors, missing attributes, ...).
    Eval,
    /// `builtins.throw`.
    Thrown,
    /// A failed `assert`.
    Assertion,
    /// Import from derivation was disabled by the embedder.
    ImportFromDerivation,
    /// A function auto-called at the top level (`--arg`/`--argstr` and the
    /// formals' defaults) has a formal with neither. cppnix's
    /// `MissingArgumentError`.
    MissingArgument,
}

#[derive(Debug)]
pub enum VmError {
    Throw(Catchable),
    Unimplemented(crate::refusal::Refusal),
}

impl VmError {
    pub fn eval(msg: impl Into<String>) -> Self {
        VmError::Throw(Catchable {
            message: msg.into(),
            catchable: false,
            kind: ErrKind::Eval,
            pos: None,
        })
    }

    /// `builtins.throw`: catchable by tryEval, reported as cppnix's
    /// ThrownError.
    pub fn thrown(msg: impl Into<String>) -> Self {
        VmError::Throw(Catchable {
            message: msg.into(),
            catchable: true,
            kind: ErrKind::Thrown,
            pos: None,
        })
    }

    /// A failed assertion: catchable, reported as cppnix's AssertionError.
    pub fn assertion(msg: impl Into<String>) -> Self {
        VmError::Throw(Catchable {
            message: msg.into(),
            catchable: true,
            kind: ErrKind::Assertion,
            pos: None,
        })
    }

    /// cppnix's `IFDError`, which command walkers may catch separately.
    pub fn import_from_derivation(msg: impl Into<String>) -> Self {
        VmError::Throw(Catchable {
            message: msg.into(),
            catchable: false,
            kind: ErrKind::ImportFromDerivation,
            pos: None,
        })
    }
}

pub type Result<T> = std::result::Result<T, VmError>;

/// What one `poll` returns. `Perform` and `NeedPath` hand control to the
/// scheduler, which answers through `Vm::resume`; the frame chain stays
/// intact across the gap, so a suspended evaluation is resumable and (once
/// frames serialize) snapshotable.
#[derive(Debug)]
pub enum Step {
    Done(Value),
    Perform {
        domain: String,
        request: Vec<u8>,
        resume: ResumeToken,
    },
    NeedPath {
        need: NeedPath,
        resume: ResumeToken,
    },
    /// This evaluation is waiting on an answer and has nothing else it could
    /// be doing.
    ///
    /// The state the single suspension slot could not express. It used to be
    /// impossible to `poll` a machine with a question open -- the call
    /// returned "internal: poll while a suspension is open" -- so a driver
    /// could not ask "are you waiting?", it had to remember. A scheduler
    /// running several evaluations would then hold its own copy of which
    /// ones were parked, and two records of one fact are two records that
    /// can disagree. Now the machine answers, and there is one record.
    Idle {
        /// How many suspensions are outstanding. A scheduler that sees
        /// `Idle { outstanding: 0 }` has a bug rather than a wait: the VM
        /// only reports idle when something is owed.
        outstanding: usize,
    },
}

/// Names one outstanding suspension. Minted by a suspend, spent by `resume`;
/// a stale token is refused rather than silently answering the wrong wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ResumeToken(u64);

/// One frame chain, with the small amount of state that belongs to it rather
/// than to the machine.
///
/// # Why this is a struct rather than four fields on `Vm`
///
/// Because a suspension is a *table* rather than a slot, and what the table
/// has to hold is exactly this. When an evaluation asks the scheduler a
/// question it is set aside whole -- frames, flow, depth -- and the machine
/// is left with nothing on it; when the answer arrives the fiber comes back
/// with its chain untouched. That is the same "the frame chain stays intact
/// across the gap" property `poll` always had, said in a type instead of in
/// a comment, and it is what lets [`Vm::poll`] be called on a parked machine
/// and answer [`Step::Idle`] rather than fail.
///
/// # Why there is normally only one of them, and what a second one may be
///
/// A `Vec<Fiber>` and a run queue let one evaluation force independent
/// subtrees at once, which is the only thing that can make *its own* host
/// questions overlap. Two facts about this crate constrain how that is
/// allowed to happen, and both are load-bearing:
///
/// * `crate::readset::ReadSet::key` hashes the question sequence **in ask
///   order** (`question_order_is_part_of_the_key` asserts it). A fiber
///   schedule driven by how fast the world answered would make that order,
///   and so the memo key, a property of the network rather than of the
///   program -- not a wrong answer, but a cache that never hits again.
/// * `Sym` is an index into this VM's interner, assigned in first-intern
///   order, and `Sym` order is observable: attrset equality forces the values
///   before the first differing key (`crate::task`'s `DeepEq::shallow`),
///   `deepSeq` walks values in descending `Sym`, and `builtins.path`,
///   `fetchurl` and `fetchTree` report the first unsupported attribute in
///   `Sym` order. A schedule-dependent intern order therefore changes which
///   `throw` a program raises, which is a Tier 1 difference and not a
///   reordered warning.
///
/// So a second fiber exists only under two rules (ENG-13150). First, a
/// sibling strand is seeded ([`Vm::spawn_root`]) only at a point the program
/// itself determines -- a walk parking on a slow question the host agreed to
/// answer in the background -- never because an answer happened to arrive.
/// Second, answers are delivered strictly in resume-token order (mint order,
/// which is ask order; `crate::eval::drive_concurrent` owns that rule), so
/// the whole interleaving is a function of the program and of which
/// questions the host's `begin` accepted, and of nothing about timing. A
/// host that begins nothing -- every production embedder today -- never has
/// a second fiber and behaves bit-identically to the single-slot machine.
/// The intern order and the memo key DO change when a host opts in, but they
/// change the same way on every run against that host, which is the property
/// both bullets actually need.
struct Fiber {
    frames: Vec<Frame>,
    flow: Flow,
    /// Which code unit this chain is executing, and how far into it.
    ///
    /// Only the failure path reads it, and it exists because nothing else can
    /// answer the question: `advance` pops the unit frame before
    /// `advance_unit` runs its ops, so a failure inside an op has already lost
    /// the frame -- and with it the instruction -- by the time `unwind` walks
    /// what is left. Without it the position offered is the *caller's*
    /// instruction, which is a plausible wrong line rather than a missing one,
    /// and a reader cannot tell those apart.
    ///
    /// Per fiber and not per `Vm`, because a fiber parked on a path question
    /// lets another one run: a machine-wide cursor would hand the parked
    /// fiber's error the running fiber's instruction.
    ///
    /// The module and unit are written once per unit entry; `at_ip` is the one
    /// per-instruction write in the interpreter's innermost loop.
    at: Option<(Rc<Module>, u32)>,
    at_ip: usize,
    /// Closure bodies currently on this chain.
    call_depth: u32,
    /// A path question raised by the frame on top, waiting to leave `poll`.
    pending_need: Option<NeedPath>,
    /// A strand seeded by [`Vm::spawn_root`] rather than by a `start_*`.
    ///
    /// Its result is not the machine's result: what it produces lands in the
    /// slot it was spawned to force -- a value memoized, a `throw` recorded
    /// as `SlotState::Failed` -- so `poll` discards its completion instead
    /// of returning `Step::Done`, and swallows a catchable failure instead
    /// of failing the run, because the chain that would have forced that
    /// slot will re-raise the memoized error at the same place it would
    /// have raised the original.
    detached: bool,
}

impl Fiber {
    const fn new() -> Self {
        Fiber {
            frames: Vec::new(),
            flow: Flow::Advance,
            at: None,
            at_ip: 0,
            call_depth: 0,
            pending_need: None,
            detached: false,
        }
    }

    /// No frames, nothing to deliver, nothing to ask: this fiber is not a
    /// piece of work but the absence of one. `poll` uses it as the "no root
    /// is on the machine" state, so "the current fiber" needs no `Option`
    /// and no separate flag that could disagree with the frames.
    const fn is_idle(&self) -> bool {
        self.frames.is_empty() && matches!(self.flow, Flow::Advance) && self.pending_need.is_none()
    }
}

/// Whether this frame chain is mid-force on `slot`: an entered force frame
/// is the one and only marker of "this chain blackholed it and will write it
/// back". Identity is the cell, so two slots sharing an `Rc` are one slot.
fn holds_entered_force(frames: &[Frame], slot: &Slot) -> bool {
    frames.iter().any(
        |frame| matches!(frame, Frame::Force(f) if f.entered && Rc::ptr_eq(&f.slot.0, &slot.0)),
    )
}

/// One operand-stack entry: strict value, lazy slot, or the soft-select miss
/// marker.
#[derive(Debug, Clone)]
pub enum StackEntry {
    Val(Value),
    Lazy(Slot),
    Miss,
}

/// Where a completed sub-evaluation's value lands in the frame that asked
/// for it: appended, or written back over the entry that was forced in place.
#[derive(Debug)]
enum Dest {
    Push,
    Stack(usize),
}

#[derive(Debug)]
struct UnitFrame {
    module: Rc<Module>,
    unit: u32,
    ip: usize,
    stack: Vec<StackEntry>,
    env: Env,
    dest: Dest,
    /// This unit is a closure body entered by an application, so it counts
    /// against `max_call_depth`. Thunk bodies and the module entry do not:
    /// forcing a thunk is not a call, and counting it would trip the limit on
    /// programs cppnix accepts.
    is_call: bool,
}

/// Forcing one slot. `entered` marks that the slot has been blackholed and
/// its thunk is running above us, which is what makes write-back and failure
/// memoization on the way out unambiguous.
#[derive(Debug)]
struct ForceFrame {
    slot: Slot,
    entered: bool,
}

#[derive(Debug)]
struct ApplyFrame {
    f: Value,
    arg: Slot,
}

/// Boxing the task keeps a `Frame` at the size of `UnitFrame`: frames are
/// popped by value in `advance`/`deliver` and pushed back in
/// `advance_task_step`, and a `Task` (Print, Coerce, Builtin{Cont}) is
/// several Vecs and sets wide. `frames_and_yields_are_not_task_sized` pins
/// the shape.
enum Frame {
    Unit(UnitFrame),
    Force(ForceFrame),
    Apply(ApplyFrame),
    Task(Box<Task>),
}

/// The one piece of interpreter state outside the frames: what to do next.
enum Flow {
    /// Advance the topmost frame.
    Advance,
    /// Hand this value to the topmost frame (or halt when there is none).
    Deliver(Value),
    /// Unwind, letting `Force` frames memoize failures and `tryEval` catch.
    Unwind(VmError),
}

/// Matches cppnix's `max-call-depth` default (eval-settings.hh). The limit
/// exists because this VM keeps its frames on the heap rather than the host
/// stack, so unbounded recursion is not a SIGSEGV that the OS stops but an
/// allocation loop that keeps going: `(x: x x) (x: x x)` reached 67 GB before
/// being killed (ENG-12432). A guard is the only thing that makes such a
/// program fail rather than take the machine down with it.
pub const DEFAULT_MAX_CALL_DEPTH: u32 = 10_000;

/// How many `poll` iterations pass between interrupt checks.
///
/// The check itself is an atomic load behind a function pointer, so the
/// stride exists to keep it off the hot path rather than because the check is
/// expensive. A unit's op count is bounded by the source that compiled it --
/// every jump this IR emits is forward, so a unit cannot loop and terminates
/// in at most its own length -- and therefore a bounded number of `poll`
/// iterations bounds the work between two checks. The exception is a single
/// builtin step that is itself unbounded: `genList` with a huge count, or the
/// recursing builtins rung B already names. Those are not covered, and are
/// named in the ladder rather than papered over.
///
/// **ENG-12539's `Op::GetLocal` fast path made each `poll` iteration do more,
/// and the bound survives it.** A forced local no longer returns here to be
/// forced, so a unit runs further per iteration; what keeps this sound is the
/// forward-jump property above rather than any particular yield frequency.
/// Re-measured after #82 landed: the gate still reports the process dying 5s
/// after a SIGTERM sent at 5s. Moving the check inside `advance_unit`'s own
/// loop would bound it in ops rather than in units, which is tighter, and it
/// would put a decrement in the hottest loop in the crate to buy a bound
/// nothing currently needs. If a unit ever gets long enough to matter, that
/// is the change to make.
const INTERRUPT_CHECK_STRIDE: u32 = 2048;

/// Whether the embedder wants this evaluation to stop.
///
/// **This is deliberately not a `Host` question**, unlike every other way the
/// evaluator reaches outside itself. A `Host` question is part of what the
/// expression *means*: it is recorded in a read set, and a memoised result is
/// only valid for the same answers. An interrupt is a fact about the process,
/// not about the expression -- the same program interrupted and not
/// interrupted has the same value -- so recording it would key results on
/// something that has nothing to do with them, and answering it through the
/// scheduler would defeat the point, since the case that needs it is
/// precisely a computation that never returns to the scheduler.
/// How a VM finds out whether the operator asked it to stop.
///
/// A boxed closure rather than a bare `fn` because the embedder that supplies
/// it is usually crossing the C ABI, where the answer needs a context pointer
/// to reach; capturing it here is what keeps that pointer out of a global.
/// One allocation per VM that has one.
pub type InterruptHook = Box<dyn Fn() -> bool>;

/// The `derivation` wrapper's source, the same file cppnix embeds through
/// `derivation.nix.gen.hh`. Included rather than copied so the two evaluators
/// cannot drift: an edit to the file changes both, and because
/// [`Vm::import_module`] keys on the text, it also changes the compile-cache
/// key without anyone having to remember to bump one.
const DERIVATION_INTERNAL: &str = include_str!("../../../src/libexpr/primops/derivation.nix");

/// What that source is called when it appears in a message. cppnix adds it to
/// an in-memory accessor under this name (`EvalState::derivationInternal`), so
/// the spelling is the one a reader will have seen in a cppnix trace.
const DERIVATION_INTERNAL_PATH: &str = "/derivation-internal.nix";

/// The source `nix_path_cell` compiles: one op, reached through the ordinary
/// compiler so there is one path from the name to the op rather than two.
const NIX_PATH_GLOBAL: &str = "__nixPath";

/// Source of [`Vm::id`]. Process-wide so two VMs on different threads never
/// share an id, which is what lets a module's link table trust the id alone.
static NEXT_VM_ID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

/// What that one-line module is called in a message.
const NIX_PATH_GLOBAL_PATH: &str = "/nix-path-global.nix";

/// How many pending children a forcing walk publishes into
/// [`Vm::set_fanout_offer`] at each child force.
///
/// A bound on the publishing side, not the consuming one: the walk
/// republishes at every child force, so an unbounded offer would make each
/// republication scan and clone the walk's whole remaining worklist --
/// quadratic over a large rendered tree that never parks at all. Sixteen
/// keeps republication O(1) while still letting a burst of
/// import-from-derivation siblings all be in flight together; a seventeenth
/// pending build waits one round, which is the walk resuming and
/// republishing, not a serial evaluation.
pub(crate) const FANOUT_WIDTH: usize = 16;

pub struct Vm {
    /// Which VM this is, for the per-module link tables (`Module::linked`):
    /// a table built by another VM's interner must not answer for this one.
    /// Process-unique, from [`NEXT_VM_ID`].
    id: u64,
    /// Global interner; module-local symbols map through `msym`.
    ///
    /// `Rc<str>` rather than `String` so a name is allocated once and the
    /// index below shares that allocation instead of holding a second copy
    /// of every name in the program (ENG-12861).
    interner: Vec<Rc<str>>,
    /// Where each interned name already sits.
    ///
    /// A hash map and not an ordered one. This is probed 8,514,655 times on
    /// a minimal NixOS toplevel evaluation, and a `BTreeMap` pays O(log n)
    /// *full string comparisons* per probe -- about 17 of them at this
    /// interner's size -- to maintain an ordering nothing wants: symbols are
    /// compared by index everywhere else in the VM, and nothing iterates
    /// this in sorted order.
    ///
    /// `Rc<str>` keys look up by `&str` through `Borrow`, so a hit allocates
    /// nothing and a miss bumps a refcount rather than copying the name.
    interner_idx: FxHashMap<Rc<str>, Sym>,
    /// The fiber on the machine right now. An idle one means no root is
    /// running and `poll` should take the next runnable one.
    cur: Fiber,
    /// Every suspension the scheduler owes an answer for, and the fiber each
    /// one will wake.
    ///
    /// This replaced a single `Option<u64>`, and that single slot was the
    /// reason a slow question stalled everything: with one open suspension
    /// permitted there was nowhere to put a second question and no second
    /// chain to run while the first waited, so the driver had to block
    /// inside `answer_path`. This table and `runnable` below are the whole
    /// mechanism; nothing else about the machine changed.
    suspended: FxHashMap<u64, Fiber>,
    /// Fibers that have been answered and are waiting for a turn.
    ///
    /// One deep while there is one fiber, and a queue rather than a second
    /// `Option` because "answered" and "running" are different states and
    /// collapsing them is what made `resume` have to reach into the running
    /// chain. `resume` now hands the fiber back and `poll` picks it up, so
    /// the two halves of a suspension are symmetric.
    runnable: VecDeque<Fiber>,
    /// Fibers parked on a slot that another fiber is in the middle of
    /// forcing, woken when that force writes the slot back.
    ///
    /// This is what makes a blackholed slot mean two different things once
    /// [`Vm::spawn_root`] exists. On one chain, hitting your own blackhole is
    /// a genuine cycle and stays "infinite recursion encountered"; hitting a
    /// blackhole some *other* chain owns is a rendezvous, and the honest
    /// answer is to wait for the value the owner is already computing. A
    /// `Vec` rather than a map keyed on the slot because it is empty in
    /// every single-fiber evaluation and short in every other one, and the
    /// scan happens only on the blackhole path, which is not a hot one.
    slot_waiters: Vec<(Slot, Fiber)>,
    /// The forcing walk's standing offer: the children it would force next,
    /// in program order, published so the scheduler can seed them as sibling
    /// strands if the child being forced now parks on a slow question the
    /// host began (ENG-13150).
    ///
    /// An offer and not a spawn, because whether anything may overlap is the
    /// scheduler's decision: only a `Host::begin` that returns a ticket
    /// consumes an entry ([`Vm::take_fanout_offer`]), so under a host that
    /// begins nothing the offer is only ever replaced and the evaluation
    /// stays exactly sequential. Replaced wholesale at each child force,
    /// cleared by `unwind` so an abandoned walk cannot leave a stale child
    /// behind for a later question to spawn.
    ///
    /// A queue and not a single slot, because consumption cascades while the
    /// walk is parked: the first begun question spawns sibling one, sibling
    /// one parks on its own slow question and that begin spawns sibling two,
    /// and so on -- one spawn per ticket. With only the immediate next child
    /// offered, overlap capped at two builds no matter how many siblings
    /// were waiting, which is what made N import-from-derivation builds take
    /// ~N/2 build times instead of ~one. Bounded by [`FANOUT_WIDTH`] at the
    /// publishing side.
    fanout_offer: VecDeque<Slot>,
    /// The root's finished value, held back while spawned strands drain.
    ///
    /// A strand abandoned mid-force leaves its slot blackholed forever --
    /// the thunk's body was moved out and lives only in the dropped fiber
    /// -- and that slot can be reachable from the value being returned, so
    /// a later force of it would report a cycle that is not there. `poll`
    /// therefore refuses to say [`Step::Done`] while any strand is still
    /// runnable, suspended, or waiting on a slot: the value parks here, the
    /// strands run to their memoized ends, and only then does `Done` carry
    /// it out (ENG-13150 review C1).
    pending_done: Option<Value>,
    next_token: u64,
    /// Compiled modules: every file this machine imports and every top-level
    /// program a session compiles through it. Owned here because the compile
    /// happens inside the machine (`import` is an op), and the cache it fills
    /// has to be the cache the next process reads: behind a store
    /// ([`crate::modcache::ModuleCache::persistent`]) a cold process decodes
    /// the imports its predecessor compiled instead of compiling them again.
    ///
    /// Keyed by the content compiled, never by the path it came from
    /// (`modcache::request`). Without a cache a file imported from n places is
    /// compiled n times, which is what cppnix's path-keyed cache avoids; keying
    /// on content buys the same sharing and also makes the cache safe to keep
    /// across evaluations and processes. A path-keyed entry is only valid
    /// while the file behind the path has not changed, which is true within
    /// one evaluation and false for anything that outlives one, and getting
    /// that wrong serves a pre-edit answer to a post-edit request. Here an edit
    /// changes the text, so it changes the key, so it misses: invalidation is
    /// content addressing rather than a separate mechanism.
    modules: crate::modcache::ModuleCache,
    /// Modules this VM was started on, weakly, for the test probes above the
    /// origin slabs: see `Vm::probed_modules`.
    #[cfg(test)]
    started_modules: Vec<std::rc::Weak<Module>>,
    /// Whether the last run ended because the operator interrupted it.
    ///
    /// An interrupt is not a property of the expression, so a memoising
    /// caller has to be able to tell it apart from an evaluation that failed
    /// on its own merits and refuse to store it. Without this the outcome is
    /// an ordinary `EvalError::Eval` carrying cppnix's wording, and
    /// `session::evaluate` memoised it: one Ctrl-C with `eval-cache-dir` set
    /// made that expression answer "interrupted by the user" for ever, on
    /// every later run, from a cache the operator had no reason to suspect.
    interrupted: bool,
    /// `hashDerivationModulo` of every derivation this evaluation produced,
    /// keyed by its `.drv` path: cppnix's process-global `drvHashes`, scoped
    /// to one VM.
    ///
    /// It exists because nothing writes the `.drv` files. `hashDerivationModulo`
    /// recurses into every input derivation and would read them back off a
    /// store, and under read-only mode there is nothing there to read --
    /// cppnix's own comment at `primops.cc:1937` says the insert is "required
    /// in read-only mode" for exactly that reason. Every input of a derivation
    /// built during an evaluation was built earlier in the same evaluation, so
    /// the answer is always already here; the case that is not is a `.drv` from
    /// outside the evaluation, which must be refused by name rather than sent
    /// to a store. That is `drvstrict`'s behaviour and not this map's, so
    /// `drvstrict::tests::a_drv_from_outside_the_evaluation_is_refused_by_name`
    /// is what holds it -- if that test ever goes green on a store read, this
    /// table stops being the only source of an input's hash.
    drv_hashes: BTreeMap<String, crate::drvpath::DrvHash>,
    /// The `.drv` paths this evaluation has already handed to the host to
    /// write. cppnix writes a derivation every time `derivationStrict`
    /// builds it, and nixpkgs builds the same one many times over (the
    /// home-manager closure: 73k writes for 23k distinct derivations); the
    /// store answered every repeat "already valid" at the price of a round
    /// trip. Here the repeat is not asked: the answer is the same bytes'
    /// same path, and the first ask is what put it in the store. Keyed by
    /// path because the path is the content: two derivations with one path
    /// are one derivation.
    drvs_written: FxHashSet<String>,
    /// Compiled regular expressions by translated pattern: cppnix's
    /// `EvalState::regexCache`, scoped to one VM.
    ///
    /// `builtins.match` and `builtins.split` are called with a handful of
    /// distinct patterns and hundreds of thousands of subjects (nixpkgs
    /// version parsing, `lib.strings` helpers), and compiling a pattern costs
    /// more than matching it against a short subject. Before this table the
    /// darwin toplevel spent ~2.7% of its main thread inside
    /// `RegexBuilder::build` recompiling the same few patterns (round 8
    /// profile, goals/rust-eval.md). Keyed on the exact bytes handed to the
    /// builder, after anchoring and bracket translation, so `match` and
    /// `split` of the same source pattern are two entries that never answer
    /// for each other. Only successful compiles are stored: an invalid
    /// pattern is an error every time, as in cppnix. A `Regex` clone is an
    /// `Arc` bump, so a hit allocates nothing.
    regexes: FxHashMap<Box<[u8]>, regex::bytes::Regex>,
    /// Argument hashes already counted for the two high-volume store
    /// questions. They are per VM because one VM is one evaluator session;
    /// nothing invalidates them during that session. Hash collisions can only
    /// undercount diagnostics and cannot affect evaluation.
    #[cfg(feature = "perf")]
    store_path_question_args: FxHashSet<[u8; 16]>,
    #[cfg(feature = "perf")]
    write_drv_question_args: FxHashSet<[u8; 16]>,
    /// The `perf::reset_epoch` the two sets above were last cleared at,
    /// so a perf reset empties them along with the counters they feed.
    #[cfg(feature = "perf")]
    question_args_epoch: u64,
    /// The `builtins` set and the `derivation` cell, built on first use and
    /// then shared, which is what cppnix does: both live in `staticBaseEnv`,
    /// constructed once per `EvalState`, and every reference is that one
    /// value rather than a fresh copy.
    ///
    /// Sharing them is a correctness point before it is a speed one -- a
    /// forced cell stays forced, so `builtins.derivation` evaluates the
    /// wrapper once per VM as cppnix evaluates it once per process -- but the
    /// speed is why it was found. Rebuilding the set costs ~200 interns, ~150
    /// `format!` allocations for the unimplemented names, and one blake3 over
    /// the whole wrapper source (`derivation_cell` goes through
    /// `import_module`, which keys on the text). `Op::BuiltinsSet` runs once
    /// per `builtins.x` *evaluated*, so an inner loop mentioning two of them
    /// paid all of that twice per iteration: on the rung E expression those
    /// three lines were 37% of the profile.
    builtins_value: Option<Value>,
    derivation_slot: Option<Slot>,
    /// Iterations left before the next interrupt check. See
    /// [`INTERRUPT_CHECK_STRIDE`].
    interrupt_countdown: u32,
    /// How to ask whether the operator interrupted this evaluation, or `None`
    /// when nobody can be asked. See [`Vm::set_interrupt`].
    interrupt: Option<InterruptHook>,
    /// Everything outside the source text and the read set that decides what
    /// this evaluation produces, taken once when the `Vm` was built.
    ///
    /// A value and not a read of [`crate::eval`]'s statics, for two reasons
    /// that turn out to be one. The memo key is computed from a
    /// [`crate::eval::Settings`] snapshot, so an evaluation that kept reading
    /// the statics could be filed under a key describing settings it did not
    /// actually run under -- the answer and its label disagreeing is the one
    /// thing a cache may not do. And a test whose behaviour depends on a
    /// process global is a test whose behaviour depends on what every other
    /// test is doing, which is ENG-12939.
    ///
    /// Snapshotting also settles the question cppnix already settled the same
    /// way: `settings.thisSystem` and friends are read once when the
    /// `EvalState` is built, so an expression cannot see one change under it.
    settings: crate::eval::Settings,
    import_cache_enabled: bool,
}

pub(crate) enum ImportEntry {
    Cached(Value),
    Evaluate {
        module: Rc<Module>,
        key: Option<crate::import_cache::ImportKey>,
    },
}

impl Vm {
    /// A VM that will evaluate under `settings`, with a compile cache that
    /// dies with it.
    ///
    /// One of two constructors, both taking the settings, and deliberately:
    /// there is no `Vm::with_settings(crate::eval::Settings::default())` and
    /// no `Default`, because the two callers mean different things and the
    /// difference is invisible at a call site that does not
    /// say. Production wants the process configuration
    /// ([`Vm::from_process_settings`]); a test wants a configuration it chose
    /// ([`crate::eval::Settings::default`]), so that no other test can move it.
    /// A defaulted constructor would let either be written by accident,
    /// silently -- an embedder evaluating against `/nix/store` when the store
    /// is elsewhere computes wrong output paths and reports nothing.
    pub fn with_settings(settings: crate::eval::Settings) -> Self {
        Self::with_modules(settings, crate::modcache::ModuleCache::in_memory())
    }

    /// A VM that compiles through `modules`: in production the session's
    /// on-disk cache ([`crate::modcache::ModuleCache::persistent`]), so a cold
    /// process decodes the imports its predecessor compiled instead of
    /// compiling them again. [`Vm::with_settings`] is this with a cache that
    /// dies with the machine.
    pub fn with_modules(
        settings: crate::eval::Settings,
        modules: crate::modcache::ModuleCache,
    ) -> Self {
        Vm {
            import_cache_enabled: true,
            id: NEXT_VM_ID.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
            interner: Vec::new(),
            interner_idx: FxHashMap::default(),
            cur: Fiber::new(),
            suspended: FxHashMap::default(),
            runnable: VecDeque::new(),
            slot_waiters: Vec::new(),
            fanout_offer: VecDeque::new(),
            pending_done: None,
            next_token: 0,
            modules,
            #[cfg(test)]
            started_modules: Vec::new(),
            interrupted: false,
            drv_hashes: BTreeMap::new(),
            drvs_written: FxHashSet::default(),
            regexes: FxHashMap::default(),
            #[cfg(feature = "perf")]
            store_path_question_args: FxHashSet::default(),
            #[cfg(feature = "perf")]
            write_drv_question_args: FxHashSet::default(),
            #[cfg(feature = "perf")]
            question_args_epoch: crate::perf::reset_epoch(),
            builtins_value: None,
            derivation_slot: None,
            interrupt_countdown: INTERRUPT_CHECK_STRIDE,
            interrupt: None,
            settings,
        }
    }

    /// Tell this VM how to find out whether it has been interrupted. Without
    /// one, nothing interrupts the evaluation and the behaviour is what it
    /// was before ENG-12533.
    ///
    /// Per VM and not per process. It used to be a `static RwLock<Option<fn>>`
    /// installed by `set_interrupt_hook`, which meant one evaluation arming an
    /// interrupt armed every other evaluation in the process, and a test that
    /// forgot to clear it left every later test killable by whatever flag it
    /// had set.
    pub fn set_interrupt(&mut self, hook: InterruptHook) {
        self.interrupt = Some(hook);
    }

    /// Ask the embedder whether the operator wants this stopped. Distinct
    /// from [`Vm::interrupted`], which reports whether this VM already
    /// stopped for that reason.
    fn check_interrupt(&self) -> bool {
        self.interrupt.as_ref().is_some_and(|f| f())
    }

    /// A VM configured the way the embedder configured this process.
    ///
    /// The production constructor. Every read of [`crate::eval`]'s statics on
    /// an evaluation path funnels through here, which is what makes "when are
    /// the settings read" a question with one answer.
    #[must_use]
    pub fn from_process_settings() -> Self {
        Self::with_settings(crate::eval::Settings::current())
    }

    /// The configuration this VM is evaluating under. See the field.
    #[must_use]
    pub fn settings(&self) -> &crate::eval::Settings {
        &self.settings
    }

    /// The compile cache this machine imports through. See the field.
    #[must_use]
    pub fn modules(&self) -> &crate::modcache::ModuleCache {
        &self.modules
    }

    /// The compile cache, to drain its corruption reports.
    pub fn modules_mut(&mut self) -> &mut crate::modcache::ModuleCache {
        &mut self.modules
    }

    /// Compile a program through this machine's cache, under its settings.
    ///
    /// What `import` does for a file, and what a session does for the
    /// top-level expression before deciding whether to run it
    /// ([`crate::session::evaluate`] keys the result memo on the module's
    /// id). Here rather than on the cache so the settings the compile sees are
    /// the machine's own: a caller passing them alongside could pass somebody
    /// else's.
    pub fn compile(
        &mut self,
        text: &str,
        base_dir: &str,
        origin: crate::compile::Origin<'_>,
    ) -> std::result::Result<crate::modcache::Compiled, crate::modcache::CacheError> {
        self.modules.compile(text, base_dir, origin, &self.settings)
    }

    /// The `builtins` attrset, built once per VM. See the fields.
    ///
    /// No staleness check on this path, and that is the point: the set is a
    /// function of the settings alone, and the settings cannot move while a
    /// `Vm` holds them. The one thing that replaces them
    /// ([`Vm::reload_settings_from_process`]) drops the cached set, so this is
    /// a plain memo rather than a comparison per `builtins` reference.
    ///
    /// It used to compare a hand-written `EmbedderInputs` witness listing the
    /// three constants somebody remembered could move. That missed
    /// `pure-eval`, which decides which *keys* the set has -- so a `Vm` whose
    /// purity changed served a set naming `currentSystem` under pure
    /// evaluation. Invalidating on the whole settings value covers every
    /// field, including the next one.
    pub fn builtins_value(&mut self) -> Result<Value> {
        if let Some(v) = &self.builtins_value {
            return Ok(v.clone());
        }
        let v = builtins::builtins_set(self)?;
        self.builtins_value = Some(v.clone());
        Ok(v)
    }

    /// Re-take the process configuration, for an embedder that configured
    /// something after this `Vm` was built.
    ///
    /// The C ABI has one process-global channel for configuration and a
    /// session that outlives any single call, so `ixe_set_nix_version` may
    /// legitimately arrive after `ixe_session_new`. Re-taking per evaluation
    /// is what keeps that working while the settings still hold still *within*
    /// an evaluation -- which is the property the memo key needs, since the
    /// key is one snapshot and the evaluation must not run under another.
    ///
    /// Anything derived from the settings is dropped here rather than checked
    /// later. One invalidation, at the one moment the settings can change.
    /// `path_reads` is carried over rather than re-taken. It is the one field
    /// of [`crate::eval::Settings`] that is not process state: whether a file
    /// read goes through an embedder is a property of the host this machine
    /// was built for, fixed when its session was created, and
    /// `Settings::current()` has no way to know it. Re-taking it would answer
    /// `Direct` for a session that has an embedder, which turns every read
    /// under `pure-eval` into a refusal -- with the message "this evaluator
    /// has no embedder to read through" on a run that has one.
    pub fn reload_settings_from_process(&mut self) {
        let mut settings = crate::eval::Settings::current();
        settings.path_reads = self.settings.path_reads;
        if self.settings == settings {
            return;
        }
        self.settings = settings;
        self.builtins_value = None;
        self.derivation_slot = None;
    }

    /// The modulo hashes of the derivations produced so far. See the field.
    pub fn drv_hashes_mut(&mut self) -> &mut BTreeMap<String, crate::drvpath::DrvHash> {
        &mut self.drv_hashes
    }

    /// Record that `drv_path` is being handed to the host to write: `true`
    /// the first time in this evaluation, `false` for a repeat. See the
    /// field.
    pub fn note_drv_written(&mut self, drv_path: &str) -> bool {
        if self.drvs_written.contains(drv_path) {
            return false;
        }
        self.drvs_written.insert(drv_path.to_owned())
    }

    /// The compiled regular expression for `pattern`, compiling it through
    /// `compile` on the first request and serving the table afterwards. See
    /// the field for why this exists and what the key is.
    pub fn regex(
        &mut self,
        pattern: &[u8],
        compile: impl FnOnce(&[u8]) -> Result<regex::bytes::Regex>,
    ) -> Result<regex::bytes::Regex> {
        if let Some(rx) = self.regexes.get(pattern) {
            return Ok(rx.clone());
        }
        let rx = compile(pattern)?;
        self.regexes.insert(pattern.into(), rx.clone());
        Ok(rx)
    }

    /// Count a distinct argument to `StorePath` or `WriteDrv` once for this
    /// VM. The perf module retains only cardinalities and remains IO-free.
    pub fn note_question_argument(&mut self, need: &NeedPath) {
        #[cfg(not(feature = "perf"))]
        let _ = need;
        #[cfg(feature = "perf")]
        {
            let epoch = crate::perf::reset_epoch();
            if epoch != self.question_args_epoch {
                self.store_path_question_args.clear();
                self.write_drv_question_args.clear();
                self.question_args_epoch = epoch;
            }
            match need {
                NeedPath::StorePath(path) => {
                    let argument = crate::readset::question_argument_digest(
                        &crate::readset::Question::CopyToStore(path.clone()),
                    );
                    if self.store_path_question_args.insert(argument) {
                        crate::perf::note_unique_store_path();
                    }
                }
                NeedPath::WriteDrv {
                    name,
                    aterm,
                    expected,
                } => {
                    // The question the recorder files for this operation,
                    // framed exactly as it files it: the ATerm by its
                    // content address and the path the write is expected to
                    // produce, which is the answer a successful write gives.
                    let argument = crate::readset::question_argument_digest(
                        &crate::readset::Question::WriteDrv {
                            name: name.clone(),
                            answer: crate::readset::WriteDrvAnswer::Written(expected.clone()),
                            aterm: ix_kernel::ObjId::of(aterm.as_bytes()),
                        },
                    );
                    if self.write_drv_question_args.insert(argument) {
                        crate::perf::note_unique_write_drv();
                    }
                }
                _ => {}
            }
        }
    }

    /// Set the call-depth ceiling, mirroring cppnix's `max-call-depth`.
    pub fn set_max_call_depth(&mut self, depth: u32) {
        self.settings.max_call_depth = depth;
    }

    /// For the coercion walk, which cppnix bounds with the same setting it
    /// bounds calls with (`addCallDepth`, `eval.cc:2614`) and which lives in
    /// another module here.
    pub fn max_call_depth(&self) -> u32 {
        self.settings.max_call_depth
    }

    /// Whether the machine stopped because the operator interrupted it. See
    /// the field: a memoising caller must not store what this run produced.
    #[must_use]
    pub const fn interrupted(&self) -> bool {
        self.interrupted
    }

    /// Forget a previous run's interrupt, at the start of the next one.
    ///
    /// The flag is per-run, not per-machine. A VM outlives one evaluation in
    /// the persistent server and behind the handle API, so a flag that only
    /// ever went true would make every later evaluation on that machine
    /// refuse to memoise -- a cache that silently stops working after the
    /// first Ctrl-C, which is a worse failure than the one it guards because
    /// nothing goes wrong visibly.
    pub const fn clear_interrupted(&mut self) {
        self.interrupted = false;
    }

    /// The compiled module for an imported file, compiling it on first use.
    /// Compilation is pure, so it happens here rather than in the scheduler.
    ///
    /// The base directory is part of the key alongside the text because
    /// `compile_source` makes path literals absolute against it, so the same
    /// text under two directories is two different modules.
    pub fn import_module(
        &mut self,
        path: &crate::value2::PathValue,
        text: &str,
        base: &str,
    ) -> Result<Rc<Module>> {
        self.compile_import(path, text, base)
            .map(|compiled| compiled.module)
    }

    fn compile_import(
        &mut self,
        path: &crate::value2::PathValue,
        text: &str,
        base: &str,
    ) -> Result<crate::modcache::Compiled> {
        let origin = match &path.root {
            crate::value2::Root::Ambient => crate::compile::Origin::File(path.as_ref()),
            crate::value2::Root::Mounted(mount_point) => crate::compile::Origin::MountedFile {
                path: path.as_ref(),
                mount_point,
            },
        };
        self.compile_cached_identity(text, base, origin)
    }

    pub(crate) fn set_import_cache_enabled(&mut self, enabled: bool) -> bool {
        std::mem::replace(&mut self.import_cache_enabled, enabled)
    }

    /// The source has already been read through the scheduler. Only certified
    /// closed scalar computations may skip their existing root force here.
    pub(crate) fn import_entry(
        &mut self,
        path: &crate::value2::PathValue,
        text: &str,
        base: &str,
    ) -> Result<ImportEntry> {
        let compiled = self.compile_import(path, text, base)?;
        let mut key = None;
        if self.import_cache_enabled
            && let Some(store) = self.modules.store()
        {
            match crate::import_cache::probe(
                store,
                compiled.id,
                &compiled.module,
                &self.settings,
                self.cur.call_depth,
            ) {
                Ok(crate::import_cache::Probe::Hit(value)) => {
                    crate::perf::note_import_cache_hit();
                    return Ok(ImportEntry::Cached(value));
                }
                Ok(crate::import_cache::Probe::Miss(request)) => {
                    crate::perf::note_import_cache_miss();
                    key = Some(request);
                }
                Ok(crate::import_cache::Probe::Damaged {
                    key: request,
                    warning,
                }) => {
                    crate::perf::note_import_cache_miss();
                    self.modules.note_cache_warning(warning);
                    key = Some(request);
                }
                Ok(crate::import_cache::Probe::Ineligible) => {}
                Err(error) => self.modules.note_cache_warning(error),
            }
        }
        Ok(ImportEntry::Evaluate {
            module: compiled.module,
            key,
        })
    }

    pub(crate) fn complete_import(&mut self, key: &crate::import_cache::ImportKey, value: &Value) {
        if self.import_cache_enabled
            && let Some(store) = self.modules.store()
            && let Err(error) = crate::import_cache::insert(store, key, value)
        {
            self.modules.note_cache_warning(error);
        }
    }

    /// The compiled module for a program the *embedder* supplied rather than
    /// a file the evaluation imported: today, `call-flake.nix` on its way to
    /// `builtins.getFlake`.
    ///
    /// [`crate::compile::Origin::String`] and not `File`, which is the whole
    /// reason this is not `import_module`. cppnix's origin for `call-flake.nix`
    /// is «flakes-internal», a `MemorySourceAccessor` and not a filesystem
    /// path, so `__curPos` inside it is `null`; naming a path here would
    /// invent one and make `__curPos` answer a file that does not exist. It is
    /// also what `rustEvaluandOf` does for the same program on the command-line
    /// seam -- it sends an empty `file` -- so the two seams compile one
    /// program one way.
    pub fn internal_module(&mut self, text: &str, base: &str) -> Result<Rc<Module>> {
        // `Origin::String` is the key's origin, and it compiles `__curPos` to
        // null regardless. Two internal programs with the same text under the
        // same base are the same module, which is what we want.
        self.compile_cached(text, base, crate::compile::Origin::String)
    }

    fn compile_cached(
        &mut self,
        text: &str,
        base: &str,
        origin: crate::compile::Origin<'_>,
    ) -> Result<Rc<Module>> {
        self.compile_cached_identity(text, base, origin)
            .map(|compiled| compiled.module)
    }

    fn compile_cached_identity(
        &mut self,
        text: &str,
        base: &str,
        origin: crate::compile::Origin<'_>,
    ) -> Result<crate::modcache::Compiled> {
        // The key is `modcache::request`'s: base directory, origin, the
        // settings fingerprint, and the text. The origin carries the file's
        // path, because `__curPos` compiles to the name of the file it is
        // written in and two files in one directory with the same text are two
        // modules (ENG-12713); for a mounted file it carries the root too, so
        // the same store spelling compiled once through rootFS and once through
        // a mounted accessor is two modules with different relative path
        // constants.
        match self.compile(text, base, origin) {
            Ok(compiled) => Ok(compiled),
            Err(crate::modcache::CacheError::Compile(error)) => Err(match error {
                crate::compile::CompileError::Unimplemented(w) => VmError::Unimplemented(w),
                crate::compile::CompileError::UndefinedVariable(n) => {
                    VmError::eval(format!("undefined variable '{n}'"))
                }
                crate::compile::CompileError::Parse(m) => VmError::eval(format!(
                    "in imported file '{}': {m}",
                    origin.source_path().unwrap_or("")
                )),
                // Not prefixed with the file, unlike a parse error: cppnix
                // raises these while parsing the import too, but as a plain
                // `Error` whose message already names the offending path.
                crate::compile::CompileError::Eval(m) => VmError::eval(m),
            }),
            // The cache itself failed under a compile that succeeded or was
            // never reached: the store's directory refused an I/O, or a row
            // names an object that is gone. Not turned into a recompile: a
            // cache that quietly re-performs everything looks exactly like a
            // cold one, and the session-level compile fails the same way
            // (`session::compile_failure`).
            Err(error) => Err(VmError::eval(format!("compile cache: {error}"))),
        }
    }

    /// A cell over the `derivation` wrapper, for the two places cppnix's
    /// `addConstant("derivation", ...)` puts it: the bare global and the
    /// `builtins` set.
    ///
    /// Not a primop. cppnix evaluates the source file into a value at startup,
    /// after the builtins exist, because the file uses them. Here the compile
    /// happens on first use and is then served from the module cache, which is
    /// keyed on the text, so it costs one compile per VM however many
    /// derivations an expression builds.
    ///
    /// One cell per VM, lazy until something forces it. cppnix binds the
    /// *evaluated* file into `staticBaseEnv` once per `EvalState`
    /// (`evalFile(derivationInternal, *vDerivation)`), so every reference
    /// there is one value and forcing it twice is not a thing that happens;
    /// sharing the cell here is that, arrived at lazily. Nothing forces the
    /// wrapper until something applies it, so an expression that never
    /// mentions a derivation never pays for one.
    pub fn derivation_cell(&mut self) -> Result<Slot> {
        if let Some(slot) = &self.derivation_slot {
            return Ok(slot.clone());
        }
        let path = crate::value2::PathValue::ambient(DERIVATION_INTERNAL_PATH);
        let module = self.import_module(&path, DERIVATION_INTERNAL, "/")?;
        let entry = module.entry;
        let slot = Slot::thunk(module, entry, Rc::new(crate::value2::EnvNode::Root));
        self.derivation_slot = Some(slot.clone());
        Ok(slot)
    }

    /// A lazy cell over `__nixPath`, for the `builtins` set.
    ///
    /// The same trick `derivation_cell` uses: the value is not a primop and
    /// not something a `Slot` can hold unevaluated, so it is a thunk over a
    /// one-op module compiled from source. Lazy for the reason cppnix's is
    /// eager and this one cannot be -- asking the embedder costs a scheduler
    /// round trip, and an expression that never mentions a search path should
    /// not pay for one, nor record it in a read set.
    pub fn nix_path_cell(&mut self) -> Result<Slot> {
        let path = crate::value2::PathValue::ambient(NIX_PATH_GLOBAL_PATH);
        let module = self.import_module(&path, NIX_PATH_GLOBAL, "/")?;
        let entry = module.entry;
        Ok(Slot::thunk(
            module,
            entry,
            Rc::new(crate::value2::EnvNode::Root),
        ))
    }

    /// The static attributes of a set under construction: the values
    /// `MkAttrs` popped, zipped with the attr site's static names, sorted
    /// once into the map they become.
    ///
    /// No string is read and nothing is interned: each module symbol goes
    /// through the link table, one indexed load after the module's first
    /// use. Source order is not symbol order, so the pairs are sorted here,
    /// one `sort_unstable` over `(u32, pointer)` pairs, instead of one
    /// shifting insert per name. The duplicate check is the compiler's
    /// ("already defined" is a compile error for static names) repeated as an
    /// internal invariant, because a site whose names do not match its values
    /// would land here first; after the sort a repeat is two equal
    /// neighbours.
    fn static_attrs(
        &mut self,
        module: &Rc<Module>,
        names: &[(u32, u32)],
        values: Vec<Slot>,
    ) -> Result<AttrMap> {
        if names.len() != values.len() {
            return Err(VmError::eval(format!(
                "internal: MkAttrs pops {} static values but its site names {}",
                values.len(),
                names.len()
            )));
        }
        let mut entries: Vec<(Sym, Slot)> = Vec::with_capacity(names.len());
        for (&(sym, _), v) in names.iter().zip(values) {
            entries.push((self.msym(module, sym)?, v));
        }
        entries.sort_unstable_by_key(|(k, _)| *k);
        let repeat = entries.windows(2).find_map(|w| match w {
            [(a, _), (b, _)] if a == b => Some(*a),
            _ => None,
        });
        if let Some(g) = repeat {
            return Err(VmError::eval(format!(
                "attribute '{}' already defined",
                self.sym_name(g)
            )));
        }
        Ok(AttrMap::from_sorted(entries))
    }

    /// Add the dynamic (name, value) pairs to a set under construction, with
    /// cppnix's name rules: a `null` name is skipped, anything but a string
    /// is a type error, and a repeat is "already defined". Returns where each
    /// name that landed was written, for the set's dynamic origin.
    fn insert_dynamic_attrs(
        &mut self,
        map: &mut AttrMap,
        pairs: Vec<(Value, Slot)>,
        dynamic_names: &[(u32, u32)],
    ) -> Result<Vec<(Sym, u32)>> {
        if pairs.len() != dynamic_names.len() {
            return Err(VmError::eval(format!(
                "internal: {} dynamic attribute pairs but the site names {}",
                pairs.len(),
                dynamic_names.len()
            )));
        }
        let mut positions = Vec::with_capacity(dynamic_names.len());
        for (pair_index, ((k, v), &(site_pair, pos))) in
            pairs.into_iter().zip(dynamic_names).enumerate()
        {
            if usize::try_from(site_pair) != Ok(pair_index) {
                return Err(VmError::eval(
                    "internal: dynamic attribute site is not in pair order",
                ));
            }
            // cppnix skips a dynamic binding whose name evaluates to null, so
            // `{ ${null} = true; }` is the empty set rather than an error.
            if matches!(k, Value::Null) {
                continue;
            }
            let Value::Str(s) = &k else {
                return Err(VmError::eval(format!(
                    "expected a string but found {}: {k}",
                    type_name(&k)
                )));
            };
            // eval.cc:1434, "while evaluating the name of a dynamic
            // attribute": same refusal as the select path, different
            // expression.
            crate::primops_pure::refuse_context(s)?;
            // Borrowed, not copied: the interner allocates on a miss only.
            let name = crate::primops_pure::text_of(s)?;
            crate::perf::note_attr_name_intern();
            let sym = self.intern(name);
            if map.insert(sym, v).is_some() {
                return Err(VmError::eval(format!("attribute '{name}' already defined")));
            }
            positions.push((sym, pos));
        }
        Ok(positions)
    }

    fn dynamic_attr_origin(
        &mut self,
        module: &Rc<Module>,
        site: &crate::ir::AttrSite,
        positions: Vec<(Sym, u32)>,
        fallback: Option<AttrOrigin>,
    ) -> Result<AttrOrigin> {
        if site.dynamic_names().is_empty() {
            return Err(VmError::eval(
                "internal: dynamic MkAttrs has no runtime pair site",
            ));
        }
        AttrOrigin::dynamic(Rc::clone(module), positions.into_boxed_slice(), fallback)
            .ok_or_else(|| VmError::eval("too many live dynamic attribute origins"))
    }

    pub(crate) fn static_attr_origin(&self, module: &Rc<Module>, unit: u32, ip: u32) -> AttrOrigin {
        AttrOrigin {
            module: Rc::clone(module),
            unit,
            ip,
        }
    }

    pub(crate) fn attr_origin_position(
        &self,
        origin: &AttrOrigin,
        name: &str,
        sym: Sym,
    ) -> Option<crate::value2::AttrPosition> {
        origin.position_of(name, sym)
    }

    /// Flatten the positions selected by `left // right` into one projected
    /// origin. The sorted two-pointer walk makes the same choice as the value
    /// merge: right wins equality, and left supplies names right lacks.
    fn projected_update_origin(
        &self,
        owner: &Rc<Module>,
        left: &crate::value2::Attrs,
        right: &crate::value2::Attrs,
    ) -> Result<Option<AttrOrigin>> {
        if left.is_empty() {
            return Ok(right.origin.clone());
        }
        if right.is_empty() {
            return Ok(left.origin.clone());
        }
        // The names left supplies are the ones right lacks; right supplies
        // all of its own. Each side is then resolved as one sorted run, so a
        // projected side (the accumulator of a fold) answers in a single
        // linear walk, and the two runs are disjoint and merge back into one
        // sorted projection.
        let mut left_only: Vec<Sym> = Vec::with_capacity(left.len());
        let mut left_entries = left.keys().copied().peekable();
        let mut right_entries = right.keys().copied().peekable();
        while let (Some(&left_sym), Some(&right_sym)) = (left_entries.peek(), right_entries.peek())
        {
            match left_sym.cmp(&right_sym) {
                std::cmp::Ordering::Less => {
                    left_only.push(left_sym);
                    left_entries.next();
                }
                std::cmp::Ordering::Greater => {
                    right_entries.next();
                }
                std::cmp::Ordering::Equal => {
                    left_entries.next();
                    right_entries.next();
                }
            }
        }
        left_only.extend(left_entries);
        let right_all: Vec<Sym> = right.keys().copied().collect();
        let mut lefts = Vec::with_capacity(left_only.len());
        if let Some(origin) = left.origin.as_ref() {
            origin.positions_of(&left_only, |sym| self.sym_name(sym), &mut lefts);
        }
        let mut rights = Vec::with_capacity(right_all.len());
        if let Some(origin) = right.origin.as_ref() {
            origin.positions_of(&right_all, |sym| self.sym_name(sym), &mut rights);
        }
        if lefts.is_empty() && rights.is_empty() {
            return Ok(None);
        }
        let mut positions = Vec::with_capacity(lefts.len().saturating_add(rights.len()));
        let mut lefts = lefts.into_iter().peekable();
        let mut rights = rights.into_iter().peekable();
        while let (Some(l), Some(r)) = (lefts.peek(), rights.peek()) {
            let next = if l.sym() < r.sym() {
                lefts.next()
            } else {
                rights.next()
            };
            positions.extend(next);
        }
        positions.extend(lefts);
        positions.extend(rights);
        let origin = AttrOrigin::projected(Rc::clone(owner), positions.into_boxed_slice())
            .ok_or_else(|| VmError::eval("too many live projected attribute origins"))?;
        Ok(Some(origin))
    }

    /// Every module this VM can still see: its import memo, plus the modules
    /// it was started on that something still holds (a value handle keeps
    /// its module alive through the origins it names). Weak on purpose: a
    /// started module nothing holds is gone, and a probe that kept it alive
    /// would hide exactly the leak it exists to catch.
    #[cfg(test)]
    fn probed_modules(&self) -> Vec<Rc<Module>> {
        let mut all: Vec<Rc<Module>> = self.modules.loaded().cloned().collect();
        for started in self
            .started_modules
            .iter()
            .filter_map(std::rc::Weak::upgrade)
        {
            if !all.iter().any(|known| Rc::ptr_eq(known, &started)) {
                all.push(started);
            }
        }
        all
    }

    #[cfg(test)]
    pub(crate) fn live_dynamic_attr_origins(&self) -> usize {
        self.probed_modules()
            .iter()
            .map(|module| module.dynamic_attr_origins.live_len())
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn dynamic_attr_origin_slots(&self) -> usize {
        self.probed_modules()
            .iter()
            .map(|module| module.dynamic_attr_origins.slot_len())
            .sum()
    }

    pub fn intern(&mut self, s: &str) -> Sym {
        // Counted before the lookup, not after a miss: ENG-12861 is about the
        // cost of the BTreeMap probe, which a hit pays in full.
        crate::perf::note_intern();
        if let Some(&i) = self.interner_idx.get(s) {
            return i;
        }
        crate::perf::note_intern_miss();
        let i = self.interner.len() as Sym;
        let name: Rc<str> = Rc::from(s);
        self.interner.push(Rc::clone(&name));
        self.interner_idx.insert(name, i);
        i
    }

    pub fn sym_name(&self, s: Sym) -> &str {
        self.interner
            .get(s as usize)
            .map(|name| &**name)
            .unwrap_or("<sym?>")
    }

    /// Map a module-local symbol to the global interner.
    ///
    /// An indexed load from the module's link table once this VM has linked
    /// the module; the first name it needs links every symbol at once (one
    /// intern each, and an intern only allocates on a miss). See
    /// [`crate::value2::LinkedSymbols`] for why the table is per VM.
    fn msym(&mut self, module: &Module, sym: u32) -> Result<Sym> {
        if let Some(global) = module.linked.get(self.id, sym) {
            return Ok(global);
        }
        let table: Box<[Sym]> = module
            .symbols
            .iter()
            .map(|name| self.intern(name))
            .collect();
        module.linked.set(self.id, table);
        module
            .linked
            .get(self.id, sym)
            .ok_or_else(|| VmError::eval("internal: bad symbol index"))
    }

    // -- scheduler surface -------------------------------------------------

    /// Seed the machine with a module's entry unit.
    pub fn start_module(&mut self, module: &Rc<Module>) {
        self.reset();
        #[cfg(test)]
        if !self
            .started_modules
            .iter()
            .filter_map(std::rc::Weak::upgrade)
            .any(|seen| Rc::ptr_eq(&seen, module))
        {
            self.started_modules.push(Rc::downgrade(module));
        }
        let env: Env = Rc::new(EnvNode::Root);
        self.push_unit(module.clone(), module.entry, env);
    }

    /// Seed the machine with the strict printer over an already-evaluated
    /// value. The result is a `Value::Str` holding the rendered text; going
    /// through the machine is what keeps printing a deep structure iterative.
    pub fn start_print(&mut self, v: Value) {
        self.start_task(Task::Print(print::Print::new(v)));
    }

    /// Seed the machine with one task over an already-evaluated value, the
    /// general form of [`Vm::start_print`]. The embedder needs it to run a
    /// renderer other than the default printer (`builtins.toJSON`, the
    /// `toString` coercion) over a value it is holding, without inventing a
    /// second driver for each.
    pub fn start_task(&mut self, task: Task) {
        self.reset();
        self.push_task(task);
    }

    /// The one place a fresh task becomes a frame: boxed here, once, and moved
    /// as one word from then on. A task that already lives in a `Box` (a
    /// continuation being re-queued, a sub-task from [`Yield::Sub`]) goes back
    /// on the stack in the box it arrived in, never through here.
    fn push_task(&mut self, task: Task) {
        self.cur.frames.push(Frame::Task(Box::new(task)));
    }

    /// Seed the machine with forcing one slot. The result is that slot's
    /// value in weak head normal form, and the slot memoises it, so a second
    /// force is free and a failure is re-raised rather than re-run -- the
    /// same contract a thunk forced from inside an expression gets.
    ///
    /// This is what makes selection lazy across the C ABI: an embedder can
    /// hold a whole attribute set and force one field of it without the
    /// siblings ever being entered.
    pub fn start_force(&mut self, slot: Slot) {
        self.reset();
        self.cur.frames.push(Frame::Force(ForceFrame {
            slot,
            entered: false,
        }));
    }

    /// Drop whatever the machine was doing. Every `start_*` begins here, so
    /// a fresh piece of work never inherits a half-finished frame chain or an
    /// open suspension from the last one.
    ///
    /// The table and the queue are cleared too, not just the running chain: a
    /// parked fiber left behind would be resumable by a token minted for an
    /// evaluation that no longer exists. [`Vm::spawn_root`] is the one
    /// seeding that must NOT come here, for exactly the reason the `start_*`
    /// family must: it adds a strand *beside* the parked fibers, and
    /// clearing the table would orphan the answers the scheduler still owes
    /// them.
    fn reset(&mut self) {
        self.cur = Fiber::new();
        self.suspended.clear();
        self.runnable.clear();
        self.slot_waiters.clear();
        self.fanout_offer.clear();
        self.pending_done = None;
    }

    /// Seed one more root: a detached frame chain forcing `slot`, beside
    /// whatever the machine is already doing.
    ///
    /// Every `start_*` resets first, because each of them means "the last
    /// evaluation is over". This one means the opposite -- the evaluation is
    /// mid-flight, parked on a slow question, and has a sibling it could
    /// usefully be forcing in the meantime -- so it must not reset: the
    /// parked fibers stay parked, their tokens stay live, and the new strand
    /// simply joins the run queue.
    ///
    /// The strand is detached (see [`Fiber::detached`]): its result is the
    /// slot's memoized state, not the machine's `Step::Done`, and whoever
    /// forces the slot next observes exactly what forcing it themselves
    /// would have produced -- including a memoized failure, re-raised.
    pub fn spawn_root(&mut self, slot: Slot) {
        let mut fiber = Fiber::new();
        fiber.detached = true;
        fiber.frames.push(Frame::Force(ForceFrame {
            slot,
            entered: false,
        }));
        self.runnable.push_back(fiber);
    }

    /// Publish (or withdraw, with an empty vector) the forcing walk's
    /// pending children. See the field.
    pub(crate) fn set_fanout_offer(&mut self, slots: Vec<Slot>) {
        self.fanout_offer = slots.into();
    }

    /// Set the standing offer aside, for a nested walk about to publish its
    /// own. The nested walk holds what this returns and hands it back to
    /// [`Vm::restore_fanout_offer`] when it finishes, so the offer is
    /// dynamically scoped to the innermost walk that is still publishing.
    ///
    /// Without the bracket, a nested walk simply overwrote the outer one's
    /// offer -- which is how `[ (import (derivation ...)) ... ]` went back
    /// to serial: the printer offered the pending elements, `derivation`'s
    /// attribute walk replaced them with its own attributes (all long
    /// forced by the time they could matter), and when the import finally
    /// parked on its realise the machine had nothing left to seed. The
    /// walk that abandons its saved queue on an error path loses nothing:
    /// `unwind` clears the live offer, and the outer walk republishes at
    /// its next child force.
    pub(crate) fn save_fanout_offer(&mut self) -> VecDeque<Slot> {
        std::mem::take(&mut self.fanout_offer)
    }

    /// Put back the offer a nested walk set aside. See
    /// [`Vm::save_fanout_offer`].
    pub(crate) fn restore_fanout_offer(&mut self, saved: VecDeque<Slot>) {
        self.fanout_offer = saved;
    }

    /// Consume the walk's next offered child still worth spawning, if there
    /// is one. Called by the scheduler at the one moment overlap is known
    /// to be safe and useful: a slow question was just begun, so the machine
    /// would otherwise sit idle until its answer arrives. One child per
    /// begun question, so the number of strands in flight never exceeds the
    /// number of tickets the host chose to hand out.
    pub(crate) fn take_fanout_offer(&mut self) -> Option<Slot> {
        // An already-forced child has nothing left to overlap; a strand over
        // it would only deliver the memo and vanish.
        while let Some(slot) = self.fanout_offer.pop_front() {
            if slot.peek().is_none() {
                return Some(slot);
            }
        }
        None
    }

    /// Take the next runnable fiber onto the machine, if there is one.
    fn take_next(&mut self) -> bool {
        match self.runnable.pop_front() {
            Some(f) => {
                self.cur = f;
                true
            }
            None => false,
        }
    }

    /// Set the current fiber aside under `token`, leaving the machine empty.
    fn park(&mut self, token: u64) {
        let parked = std::mem::replace(&mut self.cur, Fiber::new());
        self.suspended.insert(token, parked);
    }

    /// Whether this machine is waiting on the scheduler for anything.
    ///
    /// A driver running several evaluations asks this instead of keeping its
    /// own note of which ones are parked.
    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.suspended.len()
    }

    /// Run until the program finishes, asks the scheduler for something, or
    /// has nothing left it can do.
    ///
    /// Unlike the single-slot version this may be called with a suspension
    /// open: that is the case it exists for. What it will not do is invent
    /// work -- with the chain parked it returns [`Step::Idle`], and the
    /// scheduler has to answer something before there is anything to do.
    pub fn poll(&mut self) -> Result<Step> {
        loop {
            if self.cur.is_idle() && !self.take_next() {
                if !self.suspended.is_empty() {
                    return Ok(Step::Idle {
                        outstanding: self.suspended.len(),
                    });
                }
                // Nothing is runnable and nothing awaits an answer. If the
                // root already finished, hand its value out now: any fibers
                // still parked on each other's slots are spawned strands
                // deadlocked on a genuine cycle in work the program
                // abandoned, and dropping them leaves those slots
                // blackholed -- which is what makes a later force of one
                // report the cycle, exactly as the sequential evaluation
                // does when it reaches the same slot.
                if let Some(v) = self.pending_done.take() {
                    self.slot_waiters.clear();
                    return Ok(Step::Done(v));
                }
                // No root value either, yet fibers wait on slots whose
                // entered force can only be held by another waiter: a force
                // cycle split across chains. cppnix's wording, not the
                // internal-defect one -- the program is wrong, the machine
                // is not.
                if !self.slot_waiters.is_empty() {
                    return Err(VmError::eval("infinite recursion encountered"));
                }
                return Err(VmError::eval("internal: nothing left to advance"));
            }
            // A deliberate divergence from cppnix, stated as one: cppnix
            // checks no interrupt during evaluation (`rg checkInterrupt
            // src/libexpr` finds no site in `eval.cc`), so a runaway
            // evaluation there is unkillable too and is only noticed at the
            // first checkpoint afterwards, which on this path is printing.
            // A poll machine can do better for free, and an operator who can
            // kill a runaway is better served than one who cannot (ENG-12533).
            self.interrupt_countdown = self.interrupt_countdown.saturating_sub(1);
            if self.interrupt_countdown == 0 {
                self.interrupt_countdown = INTERRUPT_CHECK_STRIDE;
                if self.check_interrupt() {
                    // cppnix's own wording for `Interrupted`, and not
                    // catchable: `tryEval` swallowing a SIGTERM would turn
                    // "the operator asked this to stop" into a value.
                    self.interrupted = true;
                    return Err(VmError::eval("interrupted by the user"));
                }
            }
            if let Some(need) = self.cur.pending_need.take() {
                let resume = self.mint_token();
                self.park(resume.0);
                return Ok(Step::NeedPath { need, resume });
            }
            match std::mem::replace(&mut self.cur.flow, Flow::Advance) {
                Flow::Advance => {
                    if let Err(e) = self.advance() {
                        self.cur.flow = Flow::Unwind(e);
                    }
                }
                Flow::Deliver(v) => {
                    if self.cur.frames.is_empty() {
                        let detached = self.cur.detached;
                        self.cur = Fiber::new();
                        if detached {
                            // A spawned strand's value already lives in the
                            // slot it forced (and its waiters are awake);
                            // `Step::Done` belongs to the root the embedder
                            // seeded.
                            continue;
                        }
                        if !self.runnable.is_empty()
                            || !self.suspended.is_empty()
                            || !self.slot_waiters.is_empty()
                        {
                            // The root finished first, but strands are still
                            // in flight. Returning `Done` here would abandon
                            // them mid-force with their slots blackholed --
                            // see `pending_done`. Park the value and drain
                            // them; the loop hands it out when the machine
                            // is truly empty. Never taken single-fiber: with
                            // one chain, its own finish empties everything.
                            self.pending_done = Some(v);
                            continue;
                        }
                        return Ok(Step::Done(v));
                    }
                    if let Err(e) = self.deliver(v) {
                        self.cur.flow = Flow::Unwind(e);
                    }
                }
                Flow::Unwind(e) => {
                    if let Some(err) = self.unwind(e) {
                        let detached = self.cur.detached;
                        self.cur = Fiber::new();
                        // A failure on a spawned strand is not the run's
                        // failure: `unwind` memoized it into the strand's
                        // entered slots on the way out -- `Failed` for a
                        // throw, `Unimplemented` for a refusal -- and the
                        // chain that would have forced those slots
                        // re-raises it there, exactly as if the strand had
                        // never run. In particular a refusal on speculative
                        // work the program then abandons must not fail a
                        // run the sequential drive completes (ENG-13150
                        // review C2). Interrupts do not pass through here;
                        // they return from `poll` directly and still kill
                        // the whole run.
                        if detached {
                            continue;
                        }
                        return Err(err);
                    }
                }
            }
        }
    }

    /// Suspend the running root for an effect the scheduler must run. Its
    /// frame chain is set aside intact; the answer arrives through `resume`,
    /// and until then the machine is free to run a sibling.
    pub fn suspend_perform(&mut self, domain: String, request: Vec<u8>) -> Step {
        let resume = self.mint_token();
        self.park(resume.0);
        Step::Perform {
            domain,
            request,
            resume,
        }
    }

    /// Answer an outstanding suspension. The value is delivered to the frame
    /// that suspended, exactly as a completed sub-evaluation would be.
    ///
    /// The answered fiber goes to the back of the run queue rather than
    /// straight onto the machine: a scheduler answering three questions in a
    /// row should not have the third one displace a root that has been
    /// waiting since the first.
    pub fn resume(&mut self, token: ResumeToken, value: Value) -> Result<()> {
        let mut fiber = self.unpark(token)?;
        fiber.flow = Flow::Deliver(value);
        self.runnable.push_back(fiber);
        Ok(())
    }

    /// Lift the fiber a token names off the suspension table.
    ///
    /// A token that is not in the table is refused rather than applied to
    /// whatever happens to be running. That check used to be an equality
    /// against the one open slot; against a table it also catches the case
    /// that slot could not represent -- answering the same token twice, which
    /// with several questions in flight is a live mistake for a scheduler to
    /// make rather than a theoretical one.
    fn unpark(&mut self, token: ResumeToken) -> Result<Fiber> {
        self.suspended
            .remove(&token.0)
            .ok_or_else(|| VmError::eval("internal: resume with a stale token"))
    }

    /// Resume with a failure rather than an answer: the scheduler could not
    /// answer the question, and the evaluation fails from the point that
    /// asked it.
    ///
    /// It has to go through the frame chain rather than out of `poll`,
    /// because that is where the language's one barrier lives. A scheduler
    /// that returned the error itself would skip `Task::catch`, so
    /// `builtins.tryEval <nosuchentry>` would abort the whole evaluation
    /// where cppnix returns `{ success = false; ... }` -- which is what it
    /// did until ENG-12557, invisibly, because no `Host` question could
    /// produce a catchable error before search paths did (cppnix raises a
    /// search path miss as a `ThrownError`, `eval.cc:3413`, and `tryEval`
    /// catches exactly those and assertions).
    ///
    /// Unwinding is also what marks the forced slot `Failed`, so a second
    /// force of the same thunk reports the same error rather than asking
    /// again.
    pub fn resume_error(&mut self, token: ResumeToken, error: VmError) -> Result<()> {
        let mut fiber = self.unpark(token)?;
        fiber.flow = Flow::Unwind(error);
        self.runnable.push_back(fiber);
        Ok(())
    }

    /// A token no suspension will ever be filed under.
    ///
    /// `wrapping_add` was harmless while one slot held the only live token --
    /// a wrapped counter could only collide with itself. Against a table a
    /// collision would hand one root's answer to another, so the counter
    /// saturates and the machine refuses to mint past the end instead. At
    /// one token per host question that end is 18 quintillion questions
    /// away, so this is a statement that the case was considered rather than
    /// a limit anyone will meet.
    fn mint_token(&mut self) -> ResumeToken {
        self.next_token = self.next_token.saturating_add(1);
        debug_assert!(
            !self.suspended.contains_key(&self.next_token),
            "token counter wrapped onto a live suspension"
        );
        ResumeToken(self.next_token)
    }

    // -- the loop ----------------------------------------------------------

    fn advance(&mut self) -> Result<()> {
        let Some(frame) = self.cur.frames.pop() else {
            return Err(VmError::eval("internal: nothing left to advance"));
        };
        // Attributed here and not in `unwind`, because this is the only place
        // that knows which op was running. `self.at_ip` is set before the
        // fetch, so it names the op that failed; a `Frame::Unit` on the chain
        // has already advanced past it, which is why `unwind`'s fallback
        // steps back one and why it is a fallback.
        //
        // Every arm and not just `Unit`, because a builtin that raises --
        // `throw`, `abort`, an argument type error -- runs in a `Task`, and
        // those are the errors a user sees most. Attributing only `Unit` left
        // `throw` with no position at the top level and with the enclosing
        // unit's first op inside one.
        let mut outcome = match frame {
            Frame::Unit(u) => self.advance_unit(u),
            Frame::Force(f) => self.advance_force(f),
            Frame::Apply(a) => self.advance_apply(a),
            Frame::Task(t) => self.advance_task(t, None),
        };
        // Mutated through the reference rather than matched by value and
        // rebuilt. `advance` is called once per frame step, so the rebuild
        // moved the whole `Result` twice on the SUCCESS path as well; in place
        // the success path reads one discriminant and falls through.
        if let Err(VmError::Throw(c)) = &mut outcome
            && c.pos.is_none()
        {
            c.pos = self.here().map(Box::new);
        }
        outcome
    }

    fn deliver(&mut self, v: Value) -> Result<()> {
        let Some(frame) = self.cur.frames.pop() else {
            return Err(VmError::eval("internal: delivery with no frame"));
        };
        match frame {
            Frame::Unit(mut u) => {
                match std::mem::replace(&mut u.dest, Dest::Push) {
                    Dest::Push => u.stack.push(StackEntry::Val(v)),
                    Dest::Stack(i) => {
                        let Some(e) = u.stack.get_mut(i) else {
                            return Err(VmError::eval("internal: stale force destination"));
                        };
                        *e = StackEntry::Val(v);
                    }
                }
                self.cur.frames.push(Frame::Unit(u));
                self.cur.flow = Flow::Advance;
                Ok(())
            }
            Frame::Force(f) => {
                if !f.entered {
                    return Err(VmError::eval("internal: delivery to an unentered force"));
                }
                *f.slot.0.borrow_mut() = SlotState::Value(v.clone());
                self.wake_slot_waiters(&f.slot);
                self.cur.flow = Flow::Deliver(v);
                Ok(())
            }
            Frame::Apply(a) => self.advance_apply(a),
            Frame::Task(t) => self.advance_task(t, Some(v)),
        }
    }

    /// Pop frames until something catches. `Force` frames memoize the failure
    /// into their slot on the way past, which is how cppnix makes re-forcing a
    /// throwing thunk rethrow the same error.
    fn unwind(&mut self, mut e: VmError) -> Option<VmError> {
        // The walk that published the standing offer is being popped (or
        // caught past); a stale child left here would let a later question
        // seed a strand over a subtree the program abandoned -- work the
        // sequential evaluation never does. The walk re-publishes at its
        // next child force, so a caught error costs one park's worth of
        // overlap and nothing else.
        self.fanout_offer.clear();
        while let Some(frame) = self.cur.frames.pop() {
            // The first unit frame off the chain is the innermost one, so the
            // first position offered is the deepest, and nothing below can
            // overwrite it. Costs a `spans` lookup and a binary search over
            // the line table ONCE per failing evaluation, on a path that is
            // already unwinding; a successful evaluation never reaches it.
            if let Frame::Unit(u) = &frame
                && let VmError::Throw(c) = &mut e
                && c.pos.is_none()
            {
                // `ip` has already been advanced past the op that pushed the
                // sub-frame this error came out of, so the op to name is the
                // one before it. `advance` gets this exactly right and runs
                // first; this only catches an error raised outside the frame
                // dispatch, where nothing set `self.at_ip`.
                c.pos = SrcPos::of(&u.module, u.unit, u.ip.saturating_sub(1)).map(Box::new);
            }
            match frame {
                Frame::Force(f) => {
                    if f.entered {
                        match &e {
                            VmError::Throw(c) => {
                                *f.slot.0.borrow_mut() = SlotState::Failed(Rc::new(c.clone()));
                                // A failure is a write-back too: a sibling
                                // waiting on this slot re-forces it, finds
                                // `Failed`, and re-raises -- the
                                // memoized-failure contract, kept across
                                // fibers.
                                self.wake_slot_waiters(&f.slot);
                            }
                            // A refusal is memoized only off a spawned
                            // strand, where `poll` swallows it and the run
                            // continues: whoever forces this slot later
                            // re-raises the refusal with its original
                            // token, exactly where the sequential drive
                            // would first have hit it (ENG-13150 review
                            // C2). On the root chain the run dies with the
                            // refusal right now and the embedder falls
                            // back, so writing the slot would change
                            // nothing observable -- not writing it keeps
                            // the single-fiber machine byte-identical.
                            VmError::Unimplemented(r) if self.cur.detached => {
                                *f.slot.0.borrow_mut() =
                                    SlotState::Unimplemented(Rc::new(r.clone()));
                                self.wake_slot_waiters(&f.slot);
                            }
                            VmError::Unimplemented(_) => {}
                        }
                    }
                }
                Frame::Task(t) => {
                    if let Some(v) = t.catch(self, &e) {
                        self.cur.flow = Flow::Deliver(v);
                        return None;
                    }
                }
                Frame::Unit(u) => self.retire_unit(&u),
                Frame::Apply(_) => {}
            }
        }
        Some(e)
    }

    /// Whether any fiber other than the current one is mid-force on `slot`.
    ///
    /// Only consulted on the blackhole path, so it may afford to look at
    /// every parked chain: the owner of a blackholed slot is a fiber holding
    /// an *entered* force frame on it, and it is parked in exactly one of
    /// these three places (or running, which the caller already checked).
    fn another_fiber_is_forcing(&self, slot: &Slot) -> bool {
        self.suspended
            .values()
            .any(|fiber| holds_entered_force(&fiber.frames, slot))
            || self
                .runnable
                .iter()
                .any(|fiber| holds_entered_force(&fiber.frames, slot))
            || self
                .slot_waiters
                .iter()
                .any(|(_, fiber)| holds_entered_force(&fiber.frames, slot))
    }

    /// Move everyone parked on `slot` to the run queue. Called at the two
    /// places an entered force writes its slot back (a value delivered, a
    /// catchable failure memoized), which are the two moments a waiter's
    /// re-force stops meeting a blackhole.
    fn wake_slot_waiters(&mut self, slot: &Slot) {
        if self.slot_waiters.is_empty() {
            return;
        }
        let mut kept = Vec::with_capacity(self.slot_waiters.len());
        for (waited, fiber) in std::mem::take(&mut self.slot_waiters) {
            if Rc::ptr_eq(&waited.0, &slot.0) {
                self.runnable.push_back(fiber);
            } else {
                kept.push((waited, fiber));
            }
        }
        self.slot_waiters = kept;
    }

    fn advance_force(&mut self, f: ForceFrame) -> Result<()> {
        if f.entered {
            return Err(VmError::eval("internal: re-entered a running force"));
        }
        let state = std::mem::replace(&mut *f.slot.0.borrow_mut(), SlotState::Blackhole);
        match state {
            SlotState::Value(v) => {
                *f.slot.0.borrow_mut() = SlotState::Value(v.clone());
                self.cur.flow = Flow::Deliver(v);
                Ok(())
            }
            // Blackholed: someone is already forcing it. Which someone is
            // the whole question. A force frame further down THIS chain
            // makes it a genuine cycle; a force frame on a sibling strand
            // (ENG-13150) makes it a rendezvous, and this chain parks until
            // the owner writes the slot back. The own-chain check runs
            // first so a real cycle inside a strand still says what cppnix
            // says, and the fallback when no owner is found anywhere is the
            // cycle error too -- which keeps every single-fiber evaluation,
            // where no sibling can exist, byte-identical to the machine
            // before strands did.
            SlotState::Blackhole => {
                if holds_entered_force(&self.cur.frames, &f.slot)
                    || !self.another_fiber_is_forcing(&f.slot)
                {
                    return Err(VmError::eval("infinite recursion encountered"));
                }
                // Wait for the owner: put the (un-entered) force back on top
                // so re-dispatch on wake finds the slot's final state, and
                // park the whole chain beside the slot.
                let slot = f.slot.clone();
                self.cur.frames.push(Frame::Force(f));
                let parked = std::mem::replace(&mut self.cur, Fiber::new());
                self.slot_waiters.push((slot, parked));
                Ok(())
            }
            SlotState::Unimplemented(refusal) => {
                let err = (*refusal).clone();
                *f.slot.0.borrow_mut() = SlotState::Unimplemented(refusal);
                Err(VmError::Unimplemented(err))
            }
            SlotState::Failed(c) => {
                let err = (*c).clone();
                *f.slot.0.borrow_mut() = SlotState::Failed(c);
                Err(VmError::Throw(err))
            }
            SlotState::Thunk { module, unit, env } => {
                self.cur.frames.push(Frame::Force(ForceFrame {
                    slot: f.slot.clone(),
                    entered: true,
                }));
                self.push_unit(module, unit, env);
                self.cur.flow = Flow::Advance;
                Ok(())
            }
            SlotState::PendingApply { f: func, args } => {
                self.cur.frames.push(Frame::Force(ForceFrame {
                    slot: f.slot.clone(),
                    entered: true,
                }));
                self.push_task(Task::apply_chain(func, args));
                self.cur.flow = Flow::Advance;
                Ok(())
            }
        }
    }

    fn advance_apply(&mut self, a: ApplyFrame) -> Result<()> {
        match &a.f {
            Value::Closure(c) => {
                let c = c.clone();
                let unit = c
                    .module
                    .units
                    .get(c.unit as usize)
                    .ok_or_else(|| VmError::eval("internal: bad closure unit"))?;
                // Borrowed through the closure's own `Rc<Module>`, not cloned:
                // the `Param` carries the formals `Vec`, and cloning it once
                // per application was one allocation per call before any
                // argument was looked at.
                match &unit.param {
                    Some(Param::Ident(_)) => {
                        let frame = EnvNode::frame(c.env.clone(), vec![a.arg.clone()]);
                        self.push_call_unit(c.module.clone(), c.unit, frame)?;
                        self.cur.flow = Flow::Advance;
                        Ok(())
                    }
                    Some(Param::Formals {
                        fields,
                        ellipsis,
                        bind,
                    }) => {
                        let Some(v) = a.arg.peek() else {
                            // Destructuring needs the argument itself; come
                            // back to this same dispatch once it is forced.
                            let slot = a.arg.clone();
                            self.cur.frames.push(Frame::Apply(a));
                            self.cur.frames.push(Frame::Force(ForceFrame {
                                slot,
                                entered: false,
                            }));
                            self.cur.flow = Flow::Advance;
                            return Ok(());
                        };
                        let map = match &v {
                            Value::Attrs(m) => m.clone(),
                            other => {
                                return Err(VmError::eval(format!(
                                    "expected a set but found {}: {other}",
                                    type_name(other)
                                )));
                            }
                        };
                        // Slot order: fields in declaration order, then @.
                        // Built empty and filled below: a default's thunk
                        // captures the frame it will be looked up in.
                        let frame = EnvNode::frame(c.env.clone(), Vec::new());
                        // A formal's name is a module symbol, and the link
                        // table maps it to this VM's numbering with one
                        // indexed load (`msym`), the path `Op::Select` takes.
                        // Until 2026-09-04 this cloned the name `String` and
                        // re-interned it on every call: 50.7M interns on a
                        // NixOS toplevel, most of them here and in `MkAttrs`.
                        let mut slots =
                            Vec::with_capacity(fields.len() + usize::from(bind.is_some()));
                        let mut matched = 0usize;
                        for formal in fields {
                            let g = self.msym(&c.module, formal.sym)?;
                            match map.get(&g) {
                                Some(s) => {
                                    matched += 1;
                                    slots.push(s.clone());
                                }
                                None => match formal.default {
                                    Some(default_unit) => slots.push(Slot::thunk(
                                        c.module.clone(),
                                        default_unit,
                                        frame.clone(),
                                    )),
                                    None => {
                                        return Err(VmError::eval(format!(
                                            "function called without required argument '{}'",
                                            self.sym_name(g)
                                        )));
                                    }
                                },
                            }
                        }
                        if bind.is_some() {
                            slots.push(a.arg.clone());
                        }
                        // cppnix (`callFunction`, `attrsUsed != args.size()`):
                        // every argument matched a formal iff the counts
                        // agree, so the search for the unexpected one runs
                        // only on the failing call. Formal names are distinct
                        // (the parser rejects `{ a, a }`), so no argument
                        // matches twice.
                        if !*ellipsis && matched != map.len() {
                            // Symbols against symbols; the text compare this
                            // replaces allocated the key's name per argument.
                            for k in map.keys() {
                                let mut known = false;
                                for formal in fields {
                                    if self.msym(&c.module, formal.sym)? == *k {
                                        known = true;
                                        break;
                                    }
                                }
                                if !known {
                                    return Err(VmError::eval(format!(
                                        "function called with unexpected argument '{}'",
                                        self.sym_name(*k)
                                    )));
                                }
                            }
                        }
                        EnvNode::fill(&frame, slots)?;
                        self.push_call_unit(c.module.clone(), c.unit, frame)?;
                        self.cur.flow = Flow::Advance;
                        Ok(())
                    }
                    None => Err(VmError::eval("internal: closure without param")),
                }
            }
            Value::Builtin(b) => {
                let arity = builtins::TABLE
                    .get(b.idx as usize)
                    .ok_or_else(|| VmError::eval("internal: bad builtin index"))?
                    .arity;
                let mut args = b.args.clone();
                args.push(a.arg.clone());
                if args.len() < arity {
                    self.cur.flow =
                        Flow::Deliver(Value::Builtin(Rc::new(BuiltinData { idx: b.idx, args })));
                } else {
                    self.push_task(Task::builtin(b.idx, args));
                    self.cur.flow = Flow::Advance;
                }
                Ok(())
            }
            // A set with a `__functor` attribute is callable: cppnix rewrites
            // `set arg` into `set.__functor set arg` (`eval.cc:1880`), which
            // is how `stdenv.mkDerivation` and everything else built on
            // `lib.makeOverridable` is applied. Checked here rather than
            // earlier because it is the fallback: a set without the attribute
            // is still the type error below.
            Value::Attrs(m) => {
                let sym = self.intern("__functor");
                match m.get(&sym) {
                    Some(functor) => {
                        let task = Task::Functor(crate::task::Functor::new(
                            a.f.clone(),
                            functor.clone(),
                            a.arg.clone(),
                        ));
                        self.push_task(task);
                        self.cur.flow = Flow::Advance;
                        Ok(())
                    }
                    None => Err(VmError::eval(format!(
                        "attempt to call something which is not a function but {}",
                        type_name(&a.f)
                    ))),
                }
            }
            other => Err(VmError::eval(format!(
                "attempt to call something which is not a function but {}",
                type_name(other)
            ))),
        }
    }

    fn advance_task(&mut self, t: Box<Task>, incoming: Option<Value>) -> Result<()> {
        match self.advance_task_step(t, incoming)? {
            None => Ok(()),
            Some(need) => {
                self.cur.pending_need = Some(need);
                Ok(())
            }
        }
    }

    /// Returns the path question the task asked for, if any; the caller turns
    /// it into the `Step` that leaves `poll`.
    fn advance_task_step(
        &mut self,
        mut t: Box<Task>,
        incoming: Option<Value>,
    ) -> Result<Option<NeedPath>> {
        let y = match t.step(self, incoming) {
            Ok(y) => y,
            Err(e) => {
                // Put the frame back so `unwind` still gives it the chance to
                // catch, which is what makes tryEval's barrier unconditional.
                self.cur.frames.push(Frame::Task(t));
                return Err(e);
            }
        };
        // The one place a task machine's yield is dispatched, so counting
        // here is complete in the way the question count is.
        crate::perf::note_yield(match y {
            Yield::Done(_) => 0,
            Yield::Force(_) => 1,
            Yield::Apply(..) => 2,
            Yield::Sub(_) => 3,
            Yield::Need(_) => 4,
        });
        match y {
            Yield::Need(need) => {
                self.cur.frames.push(Frame::Task(t));
                return Ok(Some(need));
            }
            Yield::Done(v) => self.cur.flow = Flow::Deliver(v),
            Yield::Force(slot) => {
                // Already-forced slots skip the ForceFrame round trip and
                // deliver straight to the task; measured 2.7 forces per cpp
                // thunk (ENG-13149), so most forces land here. Failed and
                // Unimplemented slots take the frame, which re-raises the
                // memoized outcome.
                self.cur.frames.push(Frame::Task(t));
                if let Some(v) = slot.peek() {
                    self.cur.flow = Flow::Deliver(v);
                } else {
                    self.cur.frames.push(Frame::Force(ForceFrame {
                        slot,
                        entered: false,
                    }));
                    self.cur.flow = Flow::Advance;
                }
            }
            Yield::Apply(f, arg) => {
                self.cur.frames.push(Frame::Task(t));
                self.cur.frames.push(Frame::Apply(ApplyFrame { f, arg }));
                self.cur.flow = Flow::Advance;
            }
            Yield::Sub(sub) => {
                self.cur.frames.push(Frame::Task(t));
                self.cur.frames.push(Frame::Task(sub));
                self.cur.flow = Flow::Advance;
            }
        }
        Ok(None)
    }

    fn push_unit(&mut self, module: Rc<Module>, unit: u32, env: Env) {
        self.cur.frames.push(Frame::Unit(UnitFrame {
            module,
            unit,
            ip: 0,
            stack: Vec::new(),
            env,
            dest: Dest::Push,
            is_call: false,
        }));
    }

    /// Enter a closure body. This is the one push that counts against
    /// `max_call_depth`, and the check happens here rather than on frame
    /// count because the frame stack also grows for reasons that are not
    /// recursion (a long attrset literal, a deep list) and cppnix does not
    /// refuse those.
    fn push_call_unit(&mut self, module: Rc<Module>, unit: u32, env: Env) -> Result<()> {
        if self.cur.call_depth >= self.settings.max_call_depth {
            // cppnix's wording, from EvalErrorStackOverflow in
            // eval-error.hh, so the class a differ reads off the message is
            // the same on both arms.
            return Err(VmError::eval("stack overflow; max-call-depth exceeded"));
        }
        self.cur.call_depth += 1;
        self.cur.frames.push(Frame::Unit(UnitFrame {
            module,
            unit,
            ip: 0,
            stack: Vec::new(),
            env,
            dest: Dest::Push,
            is_call: true,
        }));
        Ok(())
    }

    /// Where the interpreter last was, as cppnix would print it.
    ///
    /// `None` when no unit has run yet, or when the module carries no
    /// position for that instruction -- a module compiled before positions
    /// existed, or an op the compiler could not place.
    fn here(&self) -> Option<SrcPos> {
        let (module, unit) = self.cur.at.as_ref()?;
        SrcPos::of(module, *unit, self.cur.at_ip)
    }

    /// A unit frame is leaving the stack for good. Re-pushing a frame to wait
    /// on a sub-evaluation (`yield_force`, `yield_task`, `deliver`) is not a
    /// retirement and must not come through here, or the count drifts down
    /// and the limit stops holding.
    fn retire_unit(&mut self, u: &UnitFrame) {
        if u.is_call {
            self.cur.call_depth = self.cur.call_depth.saturating_sub(1);
        }
    }

    fn yield_force(&mut self, u: UnitFrame, slot: Slot) -> Result<()> {
        // An already-forced slot delivers straight to the unit's dest, the
        // exact value the ForceFrame's Value arm would produce, without the
        // frame push, the extra advance dispatch, or the blackhole swap.
        // Failed and Unimplemented slots still take the frame so the
        // memoized outcome is re-raised (ENG-13149: 2.7 forces per cpp
        // thunk, so re-forces dominate).
        self.cur.frames.push(Frame::Unit(u));
        if let Some(v) = slot.peek() {
            self.cur.flow = Flow::Deliver(v);
            return Ok(());
        }
        self.cur.frames.push(Frame::Force(ForceFrame {
            slot,
            entered: false,
        }));
        self.cur.flow = Flow::Advance;
        Ok(())
    }

    /// Suspend a code unit for a path answer the scheduler must produce.
    /// The caller has already pointed `u.dest` at the stack entry the answer
    /// replaces, so resuming re-runs the op with the answer in place: the
    /// same resumption shape a force uses, and for the same reason (no
    /// per-op phase bookkeeping to get wrong).
    fn yield_path(&mut self, u: UnitFrame, need: NeedPath) -> Result<()> {
        self.cur.frames.push(Frame::Unit(u));
        self.cur.pending_need = Some(need);
        self.cur.flow = Flow::Advance;
        Ok(())
    }

    fn yield_task(&mut self, u: UnitFrame, t: Task) -> Result<()> {
        self.cur.frames.push(Frame::Unit(u));
        self.push_task(t);
        self.cur.flow = Flow::Advance;
        Ok(())
    }

    // -- executing one code unit -------------------------------------------

    fn advance_unit(&mut self, mut u: UnitFrame) -> Result<()> {
        // Once per entry, not once per op: the module and unit are fixed for
        // the life of the frame, and only the instruction moves.
        //
        // And not even once per entry when the module has not changed, which
        // is the common case -- a whole evaluation of one file enters
        // thousands of units all belonging to it. `Rc::clone` plus the drop of
        // the old handle is two refcount writes; `Rc::ptr_eq` is a pointer
        // compare, and a unit index is a `u32` store either way.
        match &mut self.cur.at {
            Some((module, unit)) if Rc::ptr_eq(module, &u.module) => *unit = u.unit,
            slot => *slot = Some((Rc::clone(&u.module), u.unit)),
        }
        loop {
            self.cur.at_ip = u.ip;
            let fetched = u
                .module
                .units
                .get(u.unit as usize)
                .and_then(|c| c.ops.get(u.ip))
                .copied();
            let Some(op) = fetched else {
                // Falling off the end returns the stack top, as `Ret` does.
                if let Some(s) = strict_gap(&mut u, 1) {
                    return self.yield_force(u, s);
                }
                let v = pop_value(&mut u.stack)?;
                self.retire_unit(&u);
                self.cur.flow = Flow::Deliver(v);
                return Ok(());
            };
            // The innermost statement in the evaluator. Compiled out entirely
            // without the `perf-ops` feature, which is why the feature exists:
            // see the on/off pair in maintainers/ix/perf-counter-overhead.md.
            crate::perf::note_op(&op);
            match op {
                Op::Ret => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let v = pop_value(&mut u.stack)?;
                    self.retire_unit(&u);
                    self.cur.flow = Flow::Deliver(v);
                    return Ok(());
                }
                Op::Const(idx) => {
                    let c = u
                        .module
                        .consts
                        .get(idx as usize)
                        .ok_or_else(|| VmError::eval("internal: bad const index"))?;
                    if let Const::Str(s) = c {
                        check_no_nul(s)?;
                    }
                    u.stack.push(StackEntry::Val(const_value(c)));
                    u.ip += 1;
                }
                Op::GetLocal { depth, slot } => {
                    let s = lookup_local(&u.env, depth, slot)?;
                    u.ip += 1;
                    // A slot that already holds a value needs no force frame.
                    // The general path costs two frame pushes and two pops to
                    // reach `advance_force`, which for `SlotState::Value`
                    // clones the value and hands it straight back here; this
                    // is that clone without the round trip (ENG-12539). Every
                    // other state still goes the long way, so blackholing,
                    // failure memoization and cycle detection are untouched.
                    match s.peek() {
                        Some(v) => u.stack.push(StackEntry::Val(v)),
                        None => return self.yield_force(u, s),
                    }
                }
                Op::GetLocalLazy { depth, slot } => {
                    let s = lookup_local(&u.env, depth, slot)?;
                    u.stack.push(StackEntry::Lazy(s));
                    u.ip += 1;
                }
                Op::Builtin { idx } => {
                    u.stack.push(StackEntry::Val(builtins::mk_value(idx)));
                    u.ip += 1;
                }
                Op::BuiltinsSet => {
                    let v = self.builtins_value()?;
                    u.stack.push(StackEntry::Val(v));
                    u.ip += 1;
                }
                Op::DerivationGlobal => {
                    let cell = self.derivation_cell()?;
                    u.stack.push(StackEntry::Lazy(cell));
                    u.ip += 1;
                }
                Op::NixPathGlobal => {
                    u.ip += 1;
                    return self.yield_task(u, Task::NixPath);
                }
                Op::Thunk { unit } => {
                    u.stack.push(StackEntry::Lazy(Slot::thunk(
                        u.module.clone(),
                        unit,
                        u.env.clone(),
                    )));
                    u.ip += 1;
                }
                Op::Closure { unit } => {
                    u.stack
                        .push(StackEntry::Val(Value::Closure(Rc::new(ClosureData {
                            module: u.module.clone(),
                            unit,
                            env: u.env.clone(),
                        }))));
                    u.ip += 1;
                }
                Op::Apply => {
                    // The callee sits under the argument; only it has to be
                    // strict here, because a lambda's argument stays lazy.
                    if let Some(s) = strict_at(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let f = entry_value(&u, 2)?;
                    let arg = pop_slot(&mut u.stack)?;
                    u.stack.pop();
                    u.ip += 1;
                    self.cur.frames.push(Frame::Unit(u));
                    self.cur.frames.push(Frame::Apply(ApplyFrame { f, arg }));
                    self.cur.flow = Flow::Advance;
                    return Ok(());
                }
                Op::PushEnv { n } => {
                    let mut slots = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        slots.push(pop_slot(&mut u.stack)?);
                    }
                    slots.reverse();
                    // Bindings were compiled against the frame they live in:
                    // thunks on the stack captured the OLD env, but let/rec
                    // bodies need self-reference. compile_bindings compiled
                    // value thunks inside the new scope, so re-point them at
                    // the new frame.
                    let frame = EnvNode::frame(u.env.clone(), Vec::new());
                    let repointed: Vec<Slot> = slots
                        .into_iter()
                        .map(|s| repoint_thunk(&s, &frame))
                        .collect();
                    EnvNode::fill(&frame, repointed)?;
                    u.env = frame;
                    u.ip += 1;
                }
                Op::PopEnv => {
                    u.env = match &*u.env {
                        EnvNode::Frame { up, .. } | EnvNode::With { up, .. } => up.clone(),
                        EnvNode::Root => return Err(VmError::eval("internal: env underflow")),
                    };
                    u.ip += 1;
                }
                Op::PushWith => {
                    let subject = pop_slot(&mut u.stack)?;
                    u.env = Rc::new(EnvNode::With {
                        up: u.env.clone(),
                        subject,
                    });
                    u.ip += 1;
                }
                Op::JumpIfFalse { target } => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let v = pop_value(&mut u.stack)?;
                    match v {
                        Value::Bool(false) => u.ip += 1 + target as usize,
                        Value::Bool(true) => u.ip += 1,
                        other => {
                            return Err(VmError::eval(format!(
                                "expected a Boolean but found {}: {other}",
                                type_name(&other)
                            )));
                        }
                    }
                }
                Op::Jump { target } => u.ip += 1 + target as usize,
                Op::Add | Op::Sub | Op::Mul | Op::Div => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    // `+` is cppnix's `ExprConcatStrings` with `forceString`
                    // off, so it dispatches on the LEFT operand alone: only a
                    // number there is arithmetic, and a string, a path and an
                    // attribute set all coerce both sides to a string. A set
                    // operand has to become a string before `arith` can do
                    // anything with it, and becoming one is a *call*, so it
                    // leaves as a task and the op re-runs with the answer in
                    // its place.
                    //
                    // cppnix's `copyToStore` for the whole concatenation is
                    // `firstType == nString` (`eval.cc`,
                    // `ExprConcatStrings::eval`), so a path is copied into the
                    // store under a string and left as its own source path
                    // under a set:
                    //
                    //     "/pre" + ./f                 copies ./f, fails if
                    //                                  it does not exist
                    //     { outPath = "/pre"; } + ./f  "/pre" ++ "/abs/f"
                    //
                    // That flag is read off the left operand, and coercing the
                    // left operand replaces it with a string, so the operands
                    // are settled RIGHT TO LEFT: the path demotion and the
                    // right-hand set are both decided while the left is still
                    // whatever it started as. Left-first would coerce the set,
                    // see a string on the left on the next pass, and copy a
                    // path cppnix does not copy.
                    if matches!(op, Op::Add) && !lhs_is_number(&u) {
                        let copy_to_store = lhs_is_string(&u);
                        if copy_to_store {
                            if let Some(p) = store_copy_after_string(&mut u) {
                                return self.yield_path(u, NeedPath::StorePath(p));
                            }
                        } else if lhs_is_attrs(&u) {
                            demote_path_to_string(&mut u);
                        }
                        if let Some(set) = set_operand_of_concat(&mut u) {
                            return self.yield_task(
                                u,
                                Task::concat_coerce(Slot::value(set), copy_to_store),
                            );
                        }
                    }
                    let r = pop_value(&mut u.stack)?;
                    let l = pop_value(&mut u.stack)?;
                    let out = self.arith(op, l, r)?;
                    u.stack.push(StackEntry::Val(out));
                    u.ip += 1;
                }
                Op::Eq | Op::Neq => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let r = pop_value(&mut u.stack)?;
                    let l = pop_value(&mut u.stack)?;
                    u.ip += 1;
                    let negate = matches!(op, Op::Neq);
                    return self.yield_task(u, Task::deep_eq(l, r, negate));
                }
                Op::Lt | Op::Leq | Op::Gt | Op::Geq => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let r = pop_value(&mut u.stack)?;
                    let l = pop_value(&mut u.stack)?;
                    u.ip += 1;
                    // Only `<` exists underneath: the other three are it with
                    // the operands swapped, negated, or both.
                    let (a, b, negate) = match op {
                        Op::Lt => (l, r, false),
                        Op::Gt => (r, l, false),
                        Op::Leq => (r, l, true),
                        _ => (l, r, true),
                    };
                    return self.yield_task(u, Task::compare(a, b, negate));
                }
                Op::Not => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    match pop_value(&mut u.stack)? {
                        Value::Bool(b) => u.stack.push(StackEntry::Val(Value::Bool(!b))),
                        other => {
                            return Err(VmError::eval(format!(
                                "expected a Boolean but found {}: {other}",
                                type_name(&other)
                            )));
                        }
                    }
                    u.ip += 1;
                }
                Op::Negate => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let neg = match pop_value(&mut u.stack)? {
                        Value::Int(n) => n
                            .checked_neg()
                            .map(Value::Int)
                            .ok_or_else(|| VmError::eval("integer overflow in negation"))?,
                        Value::Float(x) => Value::Float(-x),
                        other => {
                            return Err(VmError::eval(format!(
                                "expected an integer or float but found {}",
                                type_name(&other)
                            )));
                        }
                    };
                    u.stack.push(StackEntry::Val(neg));
                    u.ip += 1;
                }
                Op::ConcatStrings { n } => {
                    if let Some(s) = strict_gap(&mut u, n as usize) {
                        return self.yield_force(u, s);
                    }
                    // A path inside a string is copied into the store and
                    // interpolates as the store path, never as the source
                    // path: cppnix coerces it with `copyToStore` set
                    // (eval.cc:2582), which `ExprConcatStrings` passes
                    // whenever the concatenation started with a string --
                    // always here, because the compiler only emits this op
                    // for string literals. The VM performs no IO, so the copy
                    // leaves through the scheduler and its answer comes back
                    // over the part's own stack entry (ENG-12447).
                    if let Some(p) = store_copy_gap(&mut u, n as usize) {
                        return self.yield_path(u, NeedPath::StorePath(p));
                    }
                    // A set coerces through `__toString` or `outPath`, which
                    // can call a function and can step through further sets,
                    // so it cannot be decided inline the way the scalars
                    // below are. It leaves as a task whose answer replaces
                    // this part, and the op re-runs one part further along:
                    // the same resumption shape the force and the store copy
                    // above use.
                    if let Some(set) = attrs_coerce_gap(&mut u, n as usize) {
                        return self.yield_task(u, Task::interpolate(set));
                    }
                    let mut parts = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        parts.push(pop_value(&mut u.stack)?);
                    }
                    parts.reverse();
                    let mut out: Vec<u8> = Vec::new();
                    // The result depends on everything the parts depended on:
                    // cppnix copies each part's context into the result as it
                    // coerces it, which is what makes a derivation built from
                    // an interpolated store path depend on that path.
                    let mut context = BTreeSet::new();
                    for p in parts {
                        if let Value::Str(s) = &p
                            && let Some(c) = s.context()
                        {
                            context.extend(c.iter().cloned());
                        }
                        out.extend_from_slice(&coerce_interpolated(&p)?);
                    }
                    check_no_nul(&out)?;
                    u.stack
                        .push(StackEntry::Val(Value::Str(NixStr::with_context(
                            out, context,
                        ))));
                    u.ip += 1;
                }
                Op::ConcatPath { n } => {
                    if let Some(s) = strict_gap(&mut u, n as usize) {
                        return self.yield_force(u, s);
                    }
                    // No store copy, and that is the whole difference from
                    // `ConcatStrings`. cppnix's `copyToStore` for a whole
                    // concatenation is `firstType == nString`
                    // (`eval.cc:2320`), and the first part of an interpolated
                    // path literal is a path, so a path part contributes its
                    // own spelling rather than a store path it was copied to.
                    //
                    // A set still coerces through `__toString` or `outPath`,
                    // which is a call, so it leaves as a task and the op
                    // re-runs with the answer in its place -- the same
                    // resumption shape `ConcatStrings` uses, with
                    // `copy_to_store` off for the reason above.
                    if let Some(set) = attrs_coerce_gap(&mut u, n as usize) {
                        return self.yield_task(u, Task::concat_coerce(set, false));
                    }
                    let mut parts = Vec::with_capacity(n as usize);
                    for _ in 0..n {
                        parts.push(pop_value(&mut u.stack)?);
                    }
                    parts.reverse();
                    if !matches!(parts.first(), Some(Value::Path(_))) {
                        return Err(VmError::eval(
                            "path concatenation did not start with a path",
                        ));
                    }
                    let mut out = String::new();
                    for part in &parts {
                        // cppnix collects the context of every part and
                        // refuses at the end if any is non-empty
                        // (`eval.cc:2334`); refusing at the first is the same
                        // outcome one part earlier, and the parts are visited
                        // in cppnix's own order.
                        refuse_context_under_a_path(part)?;
                        // A path is text in this backend; a non-UTF-8 part
                        // refuses by name rather than being repaired.
                        out.push_str(crate::primops_pure::text_of_bytes(&coerce_interpolated(
                            part,
                        )?)?);
                    }
                    check_no_nul(&out)?;
                    // Only the finished text is canonicalized, which is why
                    // the compiler leaves the prefix's trailing slash on.
                    // cppnix's per-part `canonicalizePath = !first` reaches
                    // only a path value in a later position, and every path
                    // this evaluator holds is already normalized.
                    u.stack.push(StackEntry::Val(Value::Path(Rc::new(
                        crate::value2::PathValue::normalized(crate::value2::Root::Ambient, &out),
                    ))));
                    u.ip += 1;
                }
                Op::MkList { n } => {
                    let items = pop_slots(&mut u.stack, n)?;
                    u.stack.push(StackEntry::Val(Value::list(items)));
                    u.ip += 1;
                }
                Op::ConcatLists => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let r = pop_value(&mut u.stack)?;
                    let l = pop_value(&mut u.stack)?;
                    match (l, r) {
                        (Value::List(a), Value::List(b)) => {
                            let mut out = (*a).clone();
                            out.extend(b.iter().cloned());
                            u.stack.push(StackEntry::Val(Value::list(out)));
                        }
                        (l, _) => {
                            return Err(VmError::eval(format!(
                                "expected a list but found {}",
                                type_name(&l)
                            )));
                        }
                    }
                    u.ip += 1;
                }
                Op::MkAttrs { statics, dynamics } => {
                    // Dynamic names are strict, values stay lazy: forcing a
                    // value here would make `{ a = throw "x"; } ? a` throw.
                    if let Some(s) = strict_names(&mut u, dynamics) {
                        return self.yield_force(u, s);
                    }
                    let ip = u32::try_from(u.ip).map_err(|_| {
                        VmError::eval(
                            "internal: instruction index does not fit below AttrOrigin::FORMALS",
                        )
                    })?;
                    let pairs = pop_attr_pairs(&mut u.stack, dynamics)?;
                    let values = pop_slots(&mut u.stack, statics)?;
                    // Where the set was written, for `unsafeGetAttrPos`: a
                    // static set's origin is two `u32`s and one refcount; a
                    // dynamic set puts only its now-known dynamic names in a
                    // reclaiming slab slot and falls back to the compiled
                    // static site for the others. `{}` has no site
                    // (`Emit::attr_site` files nothing for it); every set
                    // that builds something has one.
                    let (map, origin) = if statics > 0 || dynamics > 0 {
                        let site = attr_site(&u.module, u.unit, ip)?;
                        let mut map = self.static_attrs(&u.module, site.static_names(), values)?;
                        let origin = if dynamics > 0 {
                            let positions =
                                self.insert_dynamic_attrs(&mut map, pairs, site.dynamic_names())?;
                            let fallback = (statics > 0)
                                .then(|| self.static_attr_origin(&u.module, u.unit, ip));
                            self.dynamic_attr_origin(&u.module, site, positions, fallback)?
                        } else {
                            self.static_attr_origin(&u.module, u.unit, ip)
                        };
                        (map, origin)
                    } else {
                        (
                            AttrMap::new(),
                            self.static_attr_origin(&u.module, u.unit, ip),
                        )
                    };
                    u.stack.push(StackEntry::Val(Value::Attrs(Rc::new(
                        crate::value2::Attrs::at(map, origin),
                    ))));
                    u.ip += 1;
                }
                Op::MkAttrsOnto { n } => {
                    if let Some(s) = strict_names(&mut u, n) {
                        return self.yield_force(u, s);
                    }
                    // The base sits under the pairs and is a set already, but
                    // forcing it here rather than assuming so keeps the op
                    // usable from anywhere the stack shape is right.
                    if let Some(s) = strict_at(&mut u, 2 * usize::from(n) + 1) {
                        return self.yield_force(u, s);
                    }
                    let pairs = pop_attr_pairs(&mut u.stack, n)?;
                    let base = pop_value(&mut u.stack)?;
                    let Value::Attrs(base) = base else {
                        return Err(VmError::eval(format!(
                            "expected a set but found {}",
                            type_name(&base)
                        )));
                    };
                    // The map is copied and extended, and the set is built
                    // once from the result, so the census sees one set at its
                    // final width rather than a clone of the base plus
                    // uncounted appends.
                    let mut map = (**base).clone();
                    let ip = u32::try_from(u.ip).map_err(|_| {
                        VmError::eval(
                            "internal: instruction index does not fit below AttrOrigin::FORMALS",
                        )
                    })?;
                    let site = attr_site(&u.module, u.unit, ip)?;
                    let positions =
                        self.insert_dynamic_attrs(&mut map, pairs, site.dynamic_names())?;
                    // The dynamic names this op adds were written here. Names
                    // it did not add keep the post-`Update` base origin, so an
                    // override position remains available after appending a
                    // dynamic binding.
                    let fallback = base.origin.clone();
                    let origin = self.dynamic_attr_origin(&u.module, site, positions, fallback)?;
                    u.stack.push(StackEntry::Val(Value::Attrs(Rc::new(
                        crate::value2::Attrs::at(map, origin),
                    ))));
                    u.ip += 1;
                }
                Op::Update => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let r = pop_value(&mut u.stack)?;
                    let l = pop_value(&mut u.stack)?;
                    match (l, r) {
                        (Value::Attrs(a), Value::Attrs(b)) => {
                            // One flat `(name, position)` projection for the
                            // result, so a `//` fold never retains a chain of
                            // prior result origins; the bindings are one merge
                            // pass over two sorted maps.
                            let origin = self.projected_update_origin(&u.module, &a, &b)?;
                            let mut out = crate::value2::Attrs::new(a.update(&b));
                            out.origin = origin;
                            u.stack.push(StackEntry::Val(Value::Attrs(Rc::new(out))));
                        }
                        (l, r) => {
                            let bad = if matches!(l, Value::Attrs(_)) { r } else { l };
                            return Err(VmError::eval(format!(
                                "expected a set but found {}",
                                type_name(&bad)
                            )));
                        }
                    }
                    u.ip += 1;
                }
                Op::Select { sym } => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let g = self.msym(&u.module, sym)?;
                    let set = pop_value(&mut u.stack)?;
                    let slot = self.select_strict(&set, g)?;
                    u.ip += 1;
                    return self.yield_force(u, slot);
                }
                Op::SelectDyn => {
                    if let Some(s) = strict_gap(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let name = pop_value(&mut u.stack)?;
                    let set = pop_value(&mut u.stack)?;
                    let g = self.attr_sym(&name)?;
                    let slot = self.select_strict(&set, g)?;
                    u.ip += 1;
                    return self.yield_force(u, slot);
                }
                Op::SelectSoft { sym } => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let g = self.msym(&u.module, sym)?;
                    let top = u.stack.pop().ok_or_else(stack_underflow)?;
                    u.ip += 1;
                    match top {
                        StackEntry::Miss => u.stack.push(StackEntry::Miss),
                        StackEntry::Val(set) => match select(&set, g) {
                            Some(slot) => return self.yield_force(u, slot),
                            None => u.stack.push(StackEntry::Miss),
                        },
                        StackEntry::Lazy(_) => return Err(unforced()),
                    }
                }
                Op::SelectSoftDyn => {
                    if let Some(s) = strict_at(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    if let Some(s) = strict_at(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let name = pop_value(&mut u.stack)?;
                    let g = self.attr_sym(&name)?;
                    let top = u.stack.pop().ok_or_else(stack_underflow)?;
                    u.ip += 1;
                    match top {
                        StackEntry::Miss => u.stack.push(StackEntry::Miss),
                        StackEntry::Val(set) => match select(&set, g) {
                            Some(slot) => return self.yield_force(u, slot),
                            None => u.stack.push(StackEntry::Miss),
                        },
                        StackEntry::Lazy(_) => return Err(unforced()),
                    }
                }
                Op::OrDefault => {
                    let default = pop_slot(&mut u.stack)?;
                    let scrutinee = u.stack.pop().ok_or_else(stack_underflow)?;
                    u.ip += 1;
                    match scrutinee {
                        StackEntry::Miss => return self.yield_force(u, default),
                        other => u.stack.push(other),
                    }
                }
                Op::HasAttr { sym } => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    let g = self.msym(&u.module, sym)?;
                    let top = u.stack.pop().ok_or_else(stack_underflow)?;
                    u.stack
                        .push(StackEntry::Val(Value::Bool(has_attr(&top, g))));
                    u.ip += 1;
                }
                Op::HasAttrDyn => {
                    if let Some(s) = strict_at(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    if let Some(s) = strict_at(&mut u, 2) {
                        return self.yield_force(u, s);
                    }
                    let name = pop_value(&mut u.stack)?;
                    let g = self.attr_sym(&name)?;
                    let top = u.stack.pop().ok_or_else(stack_underflow)?;
                    u.stack
                        .push(StackEntry::Val(Value::Bool(has_attr(&top, g))));
                    u.ip += 1;
                }
                Op::ResolveWith { sym } => {
                    let g = self.msym(&u.module, sym)?;
                    let env = u.env.clone();
                    u.ip += 1;
                    return self.yield_task(u, Task::resolve_with(env, g));
                }
                Op::CallBuiltin { .. } => {
                    return Err(VmError::Unimplemented(Refusal::new(
                        RefusalToken::UnsupportedOp,
                        "CallBuiltin",
                    )));
                }
                Op::Assert => {
                    if let Some(s) = strict_gap(&mut u, 1) {
                        return self.yield_force(u, s);
                    }
                    match pop_value(&mut u.stack)? {
                        Value::Bool(true) => {}
                        Value::Bool(false) => {
                            return Err(VmError::assertion("assertion failed"));
                        }
                        other => {
                            return Err(VmError::eval(format!(
                                "expected a Boolean but found {}",
                                type_name(&other)
                            )));
                        }
                    }
                    u.ip += 1;
                }
            }
        }
    }

    fn attr_sym(&mut self, name: &Value) -> Result<Sym> {
        match name {
            Value::Str(s) => {
                // cppnix's `getName` (eval.cc:247) forces a dynamic attribute
                // name with `forceStringNoCtx`, so `set.${"${./f}"}` is an
                // error there and must be one here. An attribute name cannot
                // record a dependency, so accepting it would lose one.
                crate::primops_pure::refuse_context(s)?;
                // Attribute names live in the interner, which is text: a
                // non-UTF-8 dynamic name refuses by name.
                Ok(self.intern(crate::primops_pure::text_of(s)?))
            }
            // cppnix's wording, verbatim: "expected a string but found a set:
            // { }". The differ reads an error's class out of its text, and
            // this shape is what puts it in the `type` class.
            other => Err(VmError::eval(format!(
                "expected a string but found {}: {other}",
                type_name(other)
            ))),
        }
    }

    fn arith(&mut self, op: Op, l: Value, r: Value) -> Result<Value> {
        // String/path + coerces per cppnix.
        if matches!(op, Op::Add) {
            match (&l, &r) {
                (Value::Str(a), _) => {
                    let b = coerce_interpolated(&r)?;
                    // `+` is cppnix's ExprConcatStrings, so it unions contexts
                    // exactly as interpolation does.
                    let mut context = BTreeSet::new();
                    for side in [&l, &r] {
                        if let Value::Str(s) = side
                            && let Some(c) = s.context()
                        {
                            context.extend(c.iter().cloned());
                        }
                    }
                    let mut bytes = a.bytes().to_vec();
                    bytes.extend_from_slice(&b);
                    return Ok(Value::Str(NixStr::with_context(bytes, context)));
                }
                (Value::Path(a), _) => {
                    // A path stays canonical: cppnix normalizes the result,
                    // so `/foo/bar + "/../xyzzy"` is `/foo/xyzzy`. A string on
                    // the left does not, which is why the two orders differ.
                    //
                    // The result is a path and a path carries no context, so a
                    // right-hand side that refers to a store path has nowhere
                    // to record the dependency. cppnix refuses rather than
                    // dropping it, and so does this.
                    refuse_context_under_a_path(&r)?;
                    let b = coerce_interpolated(&r)?;
                    let b = crate::primops_pure::text_of_bytes(&b)?;
                    return Ok(Value::Path(Rc::new(crate::value2::PathValue::normalized(
                        crate::value2::Root::Ambient,
                        &format!("{a}{b}"),
                    ))));
                }
                // Only a number on the left is arithmetic. cppnix reaches
                // `coerceToString` for everything else and refuses there, with
                // `coerceMore` off, so a Boolean, a null, a list and a
                // function are all rejected by name rather than as the wrong
                // sort of number. A set never arrives: `Op::Add` coerces it
                // out first.
                (Value::Int(_) | Value::Float(_), _) => {}
                (other, _) => {
                    return Err(VmError::eval(format!(
                        "cannot coerce {} to a string",
                        type_name(other)
                    )));
                }
            }
        }
        match (&l, &r) {
            (Value::Int(a), Value::Int(b)) => {
                let (a, b) = (*a, *b);
                let (out, verb) = match op {
                    Op::Add => (a.checked_add(b), "adding"),
                    Op::Sub => (a.checked_sub(b), "subtracting"),
                    Op::Mul => (a.checked_mul(b), "multiplying"),
                    Op::Div => {
                        if b == 0 {
                            return Err(VmError::eval("division by zero"));
                        }
                        (a.checked_div(b), "dividing")
                    }
                    _ => (None, "computing"),
                };
                let sign = match op {
                    Op::Add => "+",
                    Op::Sub => "-",
                    Op::Mul => "*",
                    _ => "/",
                };
                out.map(Value::Int).ok_or_else(|| {
                    VmError::eval(format!("integer overflow in {verb} {a} {sign} {b}"))
                })
            }
            _ => {
                let a = num_f64(&l)?;
                let b = num_f64(&r)?;
                let x = match op {
                    Op::Add => a + b,
                    Op::Sub => a - b,
                    Op::Mul => a * b,
                    Op::Div => a / b,
                    _ => return Err(VmError::eval("internal: bad arith op")),
                };
                Ok(Value::Float(x))
            }
        }
    }

    fn select_strict(&mut self, set: &Value, sym: Sym) -> Result<Slot> {
        match select(set, sym) {
            Some(s) => Ok(s),
            None => match set {
                Value::Attrs(_) => Err(VmError::eval(format!(
                    "attribute '{}' missing",
                    self.sym_name(sym)
                ))),
                other => Err(VmError::eval(format!(
                    "expected a set but found {}: {other}",
                    type_name(other)
                ))),
            },
        }
    }
}

fn select(set: &Value, sym: Sym) -> Option<Slot> {
    match set {
        Value::Attrs(map) => map.get(&sym).cloned(),
        _ => None,
    }
}

fn has_attr(e: &StackEntry, sym: Sym) -> bool {
    match e {
        StackEntry::Val(Value::Attrs(map)) => map.contains_key(&sym),
        _ => false,
    }
}

/// Force the topmost lazy entry among the top `k`, recording where its value
/// goes. Returns `None` when they are all strict and the op may run. Ops call
/// this before touching the stack, so re-running the op after the force is
/// what resumption costs -- no per-op phase bookkeeping.
fn strict_gap(u: &mut UnitFrame, k: usize) -> Option<Slot> {
    for off in 1..=k {
        if let Some(s) = strict_at(u, off) {
            return Some(s);
        }
    }
    None
}

/// The leftmost of the top `k` entries holding a path, which a string
/// coercion has to turn into a store path before it can run. Returns `None`
/// when there is none left and the op may proceed; records where the answer
/// goes, as `strict_at` does, so resuming overwrites that entry and the op
/// re-runs one path further along.
///
/// Leftmost first because that is the order cppnix coerces the parts in, so
/// two unwritable paths in one string report the same one on both arms.
fn store_copy_gap(u: &mut UnitFrame, k: usize) -> Option<Rc<crate::value2::PathValue>> {
    let base = u.stack.len().checked_sub(k)?;
    for i in base..u.stack.len() {
        if let Some(StackEntry::Val(Value::Path(p))) = u.stack.get(i) {
            let p = Rc::clone(p);
            u.dest = Dest::Stack(i);
            return Some(p);
        }
    }
    None
}

/// The leftmost of the top `k` entries holding an attribute set, which has to
/// be coerced through `__toString` or `outPath` before the concatenation can
/// run. Records where the answer goes, as the sibling gaps do, so resuming
/// overwrites that entry and the op re-runs one part further along.
fn attrs_coerce_gap(u: &mut UnitFrame, k: usize) -> Option<Slot> {
    let base = u.stack.len().checked_sub(k)?;
    for i in base..u.stack.len() {
        if let Some(StackEntry::Val(v @ Value::Attrs(_))) = u.stack.get(i) {
            let slot = Slot::value(v.clone());
            u.dest = Dest::Stack(i);
            return Some(slot);
        }
    }
    None
}

/// The `+` case of the same rule: a path on top of a string is copied, a path
/// under anything is not.
/// The operand of `+` that has to leave `arith` to become a string.
///
/// `+` is cppnix's `ExprConcatStrings` with `forceString` off, and its string
/// branch coerces every part with `coerceToString` -- under which a set
/// becomes a string through `__toString` or `outPath`. That is a *call*, so
/// it cannot happen inside `arith`, which is why this exists rather than
/// another arm there.
///
/// In cppnix the first element decides the branch, so a set on the left makes
/// the whole expression a string concatenation: `{ outPath = "/x"; } + "a"` is
/// `"/xa"` and not an arithmetic error.
///
/// The RIGHT operand is offered first even so. The left operand's type is also
/// what sets `copyToStore` for every part, and coercing it replaces it with a
/// string; settling the right one first means that flag is read while the left
/// is still a set. One at a time is enough either way, because the op re-runs
/// after each answer.
///
/// `u.dest` points at the entry the answer replaces, so resuming re-runs the
/// op with a string in place -- the same resumption shape
/// [`store_copy_after_string`] uses, and for the same reason.
fn set_operand_of_concat(u: &mut UnitFrame) -> Option<Value> {
    let top = u.stack.len().checked_sub(1)?;
    let left = top.checked_sub(1)?;
    for i in [top, left] {
        if let StackEntry::Val(v) = u.stack.get(i)?
            && matches!(v, Value::Attrs(_))
        {
            let v = v.clone();
            u.dest = Dest::Stack(i);
            return Some(v);
        }
    }
    None
}

/// Is the left operand of a two-operand op a number? cppnix's
/// `ExprConcatStrings` runs arithmetic only when the first part is one and
/// coerces every part to a string otherwise, so this is the whole of `+`'s
/// dispatch.
fn lhs_is_number(u: &UnitFrame) -> bool {
    matches!(
        lhs(u),
        Some(StackEntry::Val(Value::Int(_) | Value::Float(_)))
    )
}

/// Is the left operand a string? cppnix's `copyToStore` for the whole
/// concatenation is exactly this test.
fn lhs_is_string(u: &UnitFrame) -> bool {
    matches!(lhs(u), Some(StackEntry::Val(Value::Str(_))))
}

fn lhs_is_attrs(u: &UnitFrame) -> bool {
    matches!(lhs(u), Some(StackEntry::Val(Value::Attrs(_))))
}

fn lhs(u: &UnitFrame) -> Option<&StackEntry> {
    u.stack.get(u.stack.len().checked_sub(2)?)
}

/// Replace a path on top of the stack with its own source path as a string,
/// which is what cppnix's `coerceToString` does to it with `copyToStore` off.
///
/// Nothing leaves the machine, because no copy happens: only the value's type
/// changes, and the `coerce_interpolated` that would have run produces these
/// same bytes. It exists so that coercing a set on the left cannot later make
/// this path look like the right-hand side of a string, which would copy it.
fn demote_path_to_string(u: &mut UnitFrame) {
    let Some(i) = u.stack.len().checked_sub(1) else {
        return;
    };
    let Some(top) = u.stack.get_mut(i) else {
        return;
    };
    if let StackEntry::Val(Value::Path(p)) = top {
        *top = StackEntry::Val(Value::Str(p.to_string().into()));
    }
}

fn store_copy_after_string(u: &mut UnitFrame) -> Option<Rc<crate::value2::PathValue>> {
    let top = u.stack.len().checked_sub(1)?;
    let StackEntry::Val(Value::Str(_)) = u.stack.get(top.checked_sub(1)?)? else {
        return None;
    };
    let StackEntry::Val(Value::Path(p)) = u.stack.get(top)? else {
        return None;
    };
    let p = Rc::clone(p);
    u.dest = Dest::Stack(top);
    Some(p)
}

/// As `strict_gap`, for exactly one position (1 is the top of the stack).
fn strict_at(u: &mut UnitFrame, off: usize) -> Option<Slot> {
    let i = u.stack.len().checked_sub(off)?;
    let StackEntry::Lazy(s) = u.stack.get(i)? else {
        return None;
    };
    let s = s.clone();
    u.dest = Dest::Stack(i);
    Some(s)
}

/// The attribute-position site for one set-building op.
fn attr_site(module: &Module, unit: u32, ip: u32) -> Result<&crate::ir::AttrSite> {
    module
        .units
        .get(unit as usize)
        .and_then(|unit| {
            unit.attr_sites
                .binary_search_by_key(&ip, |site| site.ip)
                .ok()
                .and_then(|i| unit.attr_sites.get(i))
        })
        .ok_or_else(|| {
            VmError::eval(format!(
                "internal: set-building op at unit {unit} ip {ip} has no attribute site"
            ))
        })
}

/// The `n` values on top of the stack, in push order: a list's items, or a
/// set's static values (whose names are in its attr site, same order).
fn pop_slots(stack: &mut Vec<StackEntry>, n: u16) -> Result<Vec<Slot>> {
    let mut slots = Vec::with_capacity(usize::from(n));
    for _ in 0..n {
        slots.push(pop_slot(stack)?);
    }
    slots.reverse();
    Ok(slots)
}

/// The `n` dynamic (name, value) pairs on top of the stack, in source order.
///
/// Shared by `MkAttrs` and `MkAttrsOnto` so the two cannot drift about what a
/// pair is; the ONLY difference between them is what map the pairs land in.
fn pop_attr_pairs(stack: &mut Vec<StackEntry>, n: u16) -> Result<Vec<(Value, Slot)>> {
    let mut pairs = Vec::with_capacity(usize::from(n));
    for _ in 0..n {
        let v = pop_slot(stack)?;
        let k = pop_value(stack)?;
        pairs.push((k, v));
    }
    pairs.reverse();
    Ok(pairs)
}

/// The attribute-name half of the `n` dynamic (name, value) pairs on top of
/// the stack, topmost pair first (the order `MkAttrs` and `MkAttrsOnto` pop
/// them in). Static names are not on the stack and have nothing to force.
fn strict_names(u: &mut UnitFrame, n: u16) -> Option<Slot> {
    for j in 0..usize::from(n) {
        if let Some(s) = strict_at(u, 2 + 2 * j) {
            return Some(s);
        }
    }
    None
}

fn num_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        other => Err(VmError::eval(format!(
            "expected an integer or float but found {}",
            type_name(other)
        ))),
    }
}

/// cppnix refuses any string carrying an interior NUL, because a Nix string
/// reaches the store and the OS as a C string. Checked where strings are
/// built rather than where they are printed, so the failure names the input.
pub fn check_no_nul(s: impl AsRef<[u8]>) -> Result<()> {
    let s = s.as_ref();
    if s.contains(&0) {
        return Err(VmError::eval(format!(
            "input string '{}' cannot be represented as Nix string because it contains null bytes",
            String::from_utf8_lossy(s).replace('\0', "\u{2400}")
        )));
    }
    Ok(())
}

/// cppnix's `coerceToString` with `coerceMore` off, for the parts it can
/// settle without leaving the machine.
///
/// Both spellings of concatenation reach it, because interpolation and `+`
/// are one `ExprConcatStrings` in cppnix: a number is refused in `"a" + 1`
/// exactly as it is in `"${1}"`. A set is absent because coercing one can call
/// `__toString`; the ops send that out as a task before they get here.
pub fn coerce_interpolated(v: &Value) -> Result<Vec<u8>> {
    match v {
        Value::Str(s) => Ok(s.bytes().to_vec()),
        Value::Path(p) => Ok(p.as_bytes().to_vec()),
        other => Err(VmError::eval(format!(
            "cannot coerce {} to a string",
            type_name(other)
        ))),
    }
}

/// A path result carries no context, so a part that refers to a store path has
/// nowhere to record the dependency. cppnix refuses rather than dropping it
/// (`eval.cc:2334`):
///
/// ```console
/// $ nix-instantiate --eval -E '/x + (builtins.appendContext "/y" \
///     { "/nix/store/1zqzq0d31c1cnjf6z8pfr2h2p1c5s5f0-x" = { path = true; }; })'
/// error: a string that refers to a store path cannot be appended to a path
/// ```
///
/// One function because both spellings of path concatenation reach the rule
/// and they are one `ExprConcatStrings` in cppnix: `+` with a path on the
/// left, and an interpolated path literal (`Op::ConcatPath`). Two copies is
/// how one of them ends up silently dropping the reference, which is what
/// this backend used to do for both.
pub fn refuse_context_under_a_path(part: &Value) -> Result<()> {
    if let Value::Str(text) = part
        && text.context().is_some_and(|c| !c.is_empty())
    {
        return Err(VmError::eval(
            "a string that refers to a store path cannot be appended to a path",
        ));
    }
    Ok(())
}

/// The memoized value of a slot the machine has already forced. Reaching the
/// error means an interpreter bug, not a Nix-level one: every value a builtin
/// or task is handed went through a `Force` frame first.
pub fn forced(s: &Slot) -> Result<Value> {
    s.peek()
        .ok_or_else(|| VmError::eval("internal: value read before it was forced"))
}

fn stack_underflow() -> VmError {
    VmError::eval("internal: stack underflow")
}

fn miss_escaped() -> VmError {
    VmError::eval("internal: miss escaped selection")
}

fn unforced() -> VmError {
    VmError::eval("internal: unforced stack entry")
}

fn pop_slot(stack: &mut Vec<StackEntry>) -> Result<Slot> {
    match stack.pop().ok_or_else(stack_underflow)? {
        StackEntry::Val(v) => Ok(Slot::value(v)),
        StackEntry::Lazy(s) => Ok(s),
        StackEntry::Miss => Err(miss_escaped()),
    }
}

fn pop_value(stack: &mut Vec<StackEntry>) -> Result<Value> {
    match stack.pop().ok_or_else(stack_underflow)? {
        StackEntry::Val(v) => Ok(v),
        StackEntry::Lazy(_) => Err(unforced()),
        StackEntry::Miss => Err(miss_escaped()),
    }
}

fn entry_value(u: &UnitFrame, off: usize) -> Result<Value> {
    let i = u.stack.len().checked_sub(off).ok_or_else(stack_underflow)?;
    match u.stack.get(i) {
        Some(StackEntry::Val(v)) => Ok(v.clone()),
        Some(StackEntry::Lazy(_)) => Err(unforced()),
        Some(StackEntry::Miss) => Err(miss_escaped()),
        None => Err(stack_underflow()),
    }
}

/// Walks the chain by reference: the version before this cloned the `Rc` at
/// every hop, two refcount writes per hop per variable read, on a path the
/// profile put at 1.5% of the edit run before any of the slot's own work
/// (dev-compute-4, 2026-09-04). Only the slot found is cloned.
fn lookup_local(env: &Env, depth: u16, slot: u16) -> Result<Slot> {
    let mut node: &EnvNode = env;
    let mut d = depth;
    loop {
        match node {
            EnvNode::Frame { up, slots } => {
                if d == 0 {
                    return slots
                        .borrow()
                        .get(slot as usize)
                        .cloned()
                        .ok_or_else(|| VmError::eval("internal: bad local slot"));
                }
                d -= 1;
                node = &**up;
            }
            EnvNode::With { up, .. } => {
                if d == 0 {
                    return Err(VmError::eval("internal: local depth hit with-scope"));
                }
                d -= 1;
                node = &**up;
            }
            EnvNode::Root => return Err(VmError::eval("internal: local depth underflow")),
        }
    }
}

/// Rebase a thunk captured at compile-fill time onto the frame it belongs
/// to (let/rec self-reference).
fn repoint_thunk(s: &Slot, frame: &Env) -> Slot {
    let inner = s.0.borrow();
    match &*inner {
        SlotState::Thunk { module, unit, .. } => Slot::thunk(module.clone(), *unit, frame.clone()),
        _ => s.clone(),
    }
}

fn const_value(c: &Const) -> Value {
    match c {
        Const::Int(n) => Value::Int(*n),
        Const::Float(x) => Value::Float(*x),
        Const::Bool(b) => Value::Bool(*b),
        Const::Null => Value::Null,
        Const::Str(s) => Value::Str(s.clone().into()),
        Const::Path(path) => Value::Path(Rc::new(path.clone())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ir::CodeUnit;

    /// `Task` is the fat state machine. Boxing it keeps every hot move
    /// (`advance`/`deliver` pop a `Frame`, `advance_task_step` pushes it back,
    /// every `Task::step` returns a `Yield`) at the size of the small
    /// variants. Relative bounds, not constants: the property is "no bigger
    /// than the variants that must be inline", whatever those measure.
    #[test]
    fn frames_and_yields_are_not_task_sized() {
        use std::mem::{align_of, size_of};
        let task = size_of::<Task>();
        let (frame, unit) = (size_of::<Frame>(), size_of::<UnitFrame>());
        assert!(
            frame < task,
            "Frame is {frame} bytes, Task {task}: a Task rides inline"
        );
        assert!(
            frame <= unit + align_of::<Frame>(),
            "Frame is {frame} bytes, UnitFrame {unit}: more than a tag's worth of slack"
        );
        let yld = size_of::<crate::task::Yield>();
        let largest = size_of::<(Value, Slot)>().max(size_of::<crate::task::NeedPath>());
        assert!(
            yld < task,
            "Yield is {yld} bytes, Task {task}: a Task rides inline"
        );
        assert!(
            yld <= largest + align_of::<crate::task::Yield>(),
            "Yield is {yld} bytes, its largest non-task variant {largest}: more than a tag's worth of slack"
        );
    }

    /// Evaluate `src` with an explicit call-depth ceiling, bypassing the
    /// process-global in `eval.rs` so these tests stay independent of each
    /// other under a parallel test runner.
    fn eval_at_depth(src: &str, depth: u32) -> std::result::Result<String, String> {
        let module = Rc::new(
            crate::compile::compile_source(
                src,
                ".",
                crate::compile::Origin::String,
                &crate::eval::Settings::default(),
            )
            .map_err(|e| format!("{e:?}"))?,
        );
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        vm.set_max_call_depth(depth);
        vm.start_module(&module);
        let value =
            crate::eval::drive(&mut vm, &crate::host::RealFs).map_err(|e| format!("{e:?}"))?;
        vm.start_print(value);
        match crate::eval::drive(&mut vm, &crate::host::RealFs) {
            Ok(Value::Str(s)) => Ok(s.expect_text()),
            Ok(other) => Err(format!("non-string print: {other:?}")),
            Err(e) => Err(format!("{e:?}")),
        }
    }

    /// A repeated `//` owns one flat projection for the accumulator. While an
    /// update is replacing it, only the old and new slots may coexist; after
    /// the returned accumulator drops, neither may remain live.
    #[test]
    fn an_update_fold_keeps_bounded_projected_origin_storage() -> std::result::Result<(), String> {
        const ATTRIBUTE_COUNT: usize = 64;
        const UPDATE_COUNT: usize = 512;

        let bindings = (0..ATTRIBUTE_COUNT)
            .map(|index| format!("a{index} = {index};"))
            .collect::<Vec<_>>()
            .join(" ");
        let source = format!(
            "let initial = {{ {bindings} }}; rhs = {{ a0 = 999; }}; \
             in builtins.foldl' \
             (acc: ignored: acc // rhs) initial \
             (builtins.genList (ignored: null) {UPDATE_COUNT})"
        );
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let module = vm
            .import_module(
                &crate::value2::ambient_path("/positions/bounded-fold.nix"),
                &source,
                "/positions",
            )
            .map_err(|error| format!("compile failed: {error:?}"))?;
        vm.start_module(&module);
        let value = crate::eval::drive(&mut vm, &crate::host::RealFs)
            .map_err(|error| format!("evaluation failed: {error:?}"))?;

        assert_eq!(
            vm.live_dynamic_attr_origins(),
            1,
            "only the returned accumulator should retain a projected origin"
        );
        assert!(
            vm.dynamic_attr_origin_slots() <= 2,
            "a flat fold used {} slab slots for {UPDATE_COUNT} updates",
            vm.dynamic_attr_origin_slots()
        );

        let Value::Attrs(attrs) = &value else {
            return Err(format!("fold returned {value:?}, not an attribute set"));
        };
        let origin = attrs
            .origin
            .as_ref()
            .ok_or_else(|| "fold accumulator has no projected origin".to_owned())?;
        for index in [0, ATTRIBUTE_COUNT / 2, ATTRIBUTE_COUNT - 1] {
            let name = format!("a{index}");
            let sym = vm.intern(&name);
            let position = origin
                .position_of(&name, sym)
                .ok_or_else(|| format!("no projected position for {name}"))?;
            let binding = if index == 0 {
                "a0 = 999".to_owned()
            } else {
                format!("{name} = {index}")
            };
            let expected = source
                .find(&binding)
                .ok_or_else(|| format!("fixture has no binding for {name}"))?;
            assert_eq!(position.offset as usize, expected, "position for {name}");
            assert!(
                Rc::ptr_eq(&position.module, &module),
                "{name} position belongs to another module"
            );
        }

        drop(value);
        assert_eq!(
            vm.live_dynamic_attr_origins(),
            0,
            "dropping the accumulator did not reclaim its projected origin"
        );
        Ok(())
    }

    /// The attribute-name intern counter counts what the VM actually does,
    /// not what a test hands it directly.
    ///
    /// Calling `perf::note_attr_name_intern` in a perf test would prove only
    /// that a `Cell` increments. This evaluates real attrsets and reads the
    /// counter afterwards: building a static set interns no attribute name
    /// (its names come from the attr site through the link table, which
    /// interned the module's symbols once when the module was linked, and
    /// that is not this counter; before, every attribute of every set built
    /// re-interned its name, 50.7M times on one NixOS toplevel), and a
    /// dynamic name interns exactly once per set built. The static arm is the
    /// one that matters and needs its control: the dynamic arm is what proves
    /// the counter can count at all.
    ///
    /// Serialised against the other counter tests by taking the same lock
    /// they do, because the counters are thread-local but this binary runs
    /// tests on one thread per case and `reset` is global to the thread.
    #[test]
    fn static_attribute_names_are_not_re_interned_when_a_set_is_built() {
        crate::perf::reset();
        let before = crate::perf::snapshot();
        assert_eq!(before.attr_name_interns, 0, "reset left a residue");

        // Static: three attributes in the outer set, two in the inner, a rec
        // and an inherit, all built (toJSON forces everything).
        let out = eval_at_depth(
            r#"let x = 0; in builtins.toJSON { a = 1; b = 2; c = rec { d = 3; e = d; inherit x; }; }"#,
            100,
        );
        assert!(out.is_ok(), "{out:?}");
        let after_static = crate::perf::snapshot();
        if !after_static.ops_counted {
            assert_eq!(
                after_static.attr_name_interns, 0,
                "counted without the perf-ops feature"
            );
            return;
        }
        assert_eq!(
            after_static.attr_name_interns, 0,
            "a static set interned an attribute name at run time"
        );

        // Dynamic control: two run-time names, one intern each, plus the
        // static sibling that interns nothing.
        let out = eval_at_depth(
            r#"let k = "d"; in builtins.toJSON { a = 1; ${k} = 2; ${k + "2"} = 3; }"#,
            100,
        );
        assert!(out.is_ok(), "{out:?}");
        let after_dynamic = crate::perf::snapshot();
        assert_eq!(
            after_dynamic.attr_name_interns, 2,
            "expected one intern per dynamic attribute built"
        );
        assert!(
            after_dynamic.attr_name_interns <= after_dynamic.interns,
            "attr_name_interns ({}) must be a subset of interns ({})",
            after_dynamic.attr_name_interns,
            after_dynamic.interns
        );
    }

    /// The VM zips a set's popped values with its attr site's static names by
    /// POSITION, so the one thing that must hold is that each value lands
    /// under the name written next to it, for every emitter: a plain set, a
    /// rec set, `inherit`, `inherit (e)`, `__overrides`, and a mixed dynamic
    /// set. Names are chosen out of text order (`z` before `a`) so a table
    /// that was sorted, or reversed, would swap values and fail here.
    #[test]
    fn every_set_emitter_lands_each_value_under_its_own_name() {
        for (src, want) in [
            (
                "let x = 9; s = { z = 1; inherit x; a = 2; m = 3; }; in [ s.z s.x s.a s.m ]",
                "[ 1 9 2 3 ]",
            ),
            (
                "let x = 9; s = rec { z = 1; inherit x; a = z + 1; m = a + 1; }; in [ s.z s.x s.a s.m ]",
                "[ 1 9 2 3 ]",
            ),
            (
                "let e = { p = 5; q = 6; }; s = { z = 1; inherit (e) q p; a = 2; }; in [ s.z s.q s.p s.a ]",
                "[ 1 6 5 2 ]",
            ),
            (
                "let s = rec { z = 1; __overrides = { a = 20; n = 7; }; a = 2; m = a + 1; }; in [ s.z s.a s.m s.n ]",
                "[ 1 20 21 7 ]",
            ),
            (
                "let k = \"d\"; s = { z = 1; ${k} = 4; a = 2; }; in [ s.z s.d s.a ]",
                "[ 1 4 2 ]",
            ),
        ] {
            let out = eval_at_depth(src, 100);
            assert_eq!(
                out.as_deref().map_err(|e| format!("{e:?}")),
                Ok(want),
                "{src}"
            );
        }
    }

    /// WriteDrv uniqueness uses the operation a RecordingHost records, not the
    /// evaluator-only expected path. Changing an actual recorded field must
    /// still produce a second argument digest.
    #[test]
    fn write_drv_uniqueness_matches_the_recording_question() {
        crate::perf::reset();
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let question = |expected: &str, aterm: &str| NeedPath::WriteDrv {
            name: "diagnostic".to_owned(),
            aterm: aterm.to_owned(),
            expected: expected.to_owned(),
        };

        vm.note_question_argument(&question("/nix/store/first.drv", "aterm"));
        vm.note_question_argument(&question("/nix/store/second.drv", "aterm"));
        vm.note_question_argument(&question("/nix/store/second.drv", "other-aterm"));

        let expected = if cfg!(feature = "perf") { 2 } else { 0 };
        assert_eq!(crate::perf::snapshot().write_drv_unique, expected);
    }

    /// Evaluate an interpolated path literal as if the file containing it
    /// lived in `base_dir`, and render the result.
    ///
    /// `base_dir` is explicit because it is half the semantics: cppnix's
    /// `path_start` calls `absPath(literal, &state->basePath)`, and
    /// `basePath` is the directory of the file being parsed, never the
    /// process working directory. A helper defaulting it would test the one
    /// thing that cannot go wrong.
    fn render_at(src: &str, base_dir: &str) -> std::result::Result<String, String> {
        render_with(src, base_dir, &crate::eval::Settings::default())
    }

    /// As `render_at`, with the settings named rather than defaulted. Only
    /// worth spelling out for a setting the answer depends on -- `home_dir`
    /// is the one today, and taking it from the environment would make the
    /// expected string differ per machine.
    fn render_with(
        src: &str,
        base_dir: &str,
        settings: &crate::eval::Settings,
    ) -> std::result::Result<String, String> {
        let module = Rc::new(
            crate::compile::compile_source(src, base_dir, crate::compile::Origin::String, settings)
                .map_err(|e| format!("{e:?}"))?,
        );
        let mut vm = Vm::with_settings(settings.clone());
        vm.start_module(&module);
        let value =
            crate::eval::drive(&mut vm, &crate::host::RealFs).map_err(|e| format!("{e:?}"))?;
        vm.start_print(value);
        match crate::eval::drive(&mut vm, &crate::host::RealFs) {
            Ok(Value::Str(s)) => Ok(s.expect_text()),
            Ok(other) => Err(format!("non-string print: {other:?}")),
            Err(e) => Err(format!("{e:?}")),
        }
    }

    /// `./${v}/x` resolves against the directory of the file that wrote it.
    /// Two files with the same text name two different paths, and neither
    /// depends on where `nix` was invoked (`parser.y`, `path_start`, which
    /// passes `state->basePath` to `absPath`).
    ///
    /// The two base directories are what make this a test: a compiler that
    /// used the process cwd, or that dropped the base entirely, gives the
    /// same answer for both rows and passes any single-row version of it.
    #[test]
    fn an_interpolated_path_resolves_against_the_defining_files_directory() {
        for (base, want) in [
            ("/a/dir", "/a/dir/3.13/gh.patch"),
            ("/other", "/other/3.13/gh.patch"),
        ] {
            assert_eq!(
                render_at(r#"let v = "3.13"; in ./${v}/gh.patch"#, base).as_deref(),
                Ok(want.to_string().as_str()),
                "base {base}"
            );
        }
    }

    /// The prefix keeps the trailing slash the parser puts back, and only the
    /// finished text is canonicalized.
    ///
    /// Each row fails differently under the obvious wrong implementations: a
    /// prefix canonicalized to `/base` gives `/base3.13`, and canonicalizing
    /// per part instead of once at the end gives `/base/a/b` for the `..`
    /// row rather than cppnix's `/base/b`.
    #[test]
    fn the_prefix_keeps_its_slash_and_only_the_result_is_canonicalized() {
        for (src, want) in [
            (r#"let v = "3.13"; in ./${v}"#, "/base/3.13"),
            (r#"let v = "3.13"; in ./${v}.patch"#, "/base/3.13.patch"),
            (r#"let v = "sub"; in ./x/${v}/y"#, "/base/x/sub/y"),
            (r#"let a = "p"; b = "q"; in ./${a}${b}"#, "/base/pq"),
            (r#"let v = "a/../b"; in ./${v}"#, "/base/b"),
            // An absolute start ignores the base entirely, as cppnix's
            // absolute branch does (it canonicalizes against the root).
            (r#"let v = "x"; in /abs/${v}/y"#, "/abs/x/y"),
        ] {
            assert_eq!(render_at(src, "/base").as_deref(), Ok(want), "{src}");
        }
    }

    /// A path is not copied into the store, and a path part inside one keeps
    /// its own spelling: cppnix's `copyToStore` for a concatenation is
    /// `firstType == nString` (`eval.cc:2320`), which is false here. The
    /// contrast row is the same interpolation inside a string, which does
    /// copy -- that is ENG-12447 and is why this cannot be one op with a
    /// runtime type check.
    #[test]
    fn a_path_part_inside_a_path_is_not_copied_to_the_store() {
        assert_eq!(
            render_at(r#"let p = /x/y; in ./${p}"#, "/base").as_deref(),
            Ok("/base/x/y")
        );
    }

    /// A string carrying a store-path context has nowhere to record the
    /// dependency once the result is a path, so cppnix refuses rather than
    /// dropping it.
    ///
    /// Driven through the rule rather than through source for the reason
    /// `a_path_refuses_a_right_hand_side_carrying_context` gives below: every
    /// way of writing a context in Nix goes through a builtin wanting the
    /// embedder's store directory, and that is a `OnceLock` a test cannot set
    /// without setting it for the rest of the process. `Op::ConcatPath` and
    /// `arith` both call this one function, which is what makes covering it
    /// once enough -- and a second copy of the check is exactly what the
    /// function exists to prevent.
    #[test]
    fn a_context_carrying_part_cannot_be_appended_to_a_path() {
        let mut context = BTreeSet::new();
        context.insert(crate::value2::ContextElem::Opaque(
            "/nix/store/1zqzq0d31c1cnjf6z8pfr2h2p1c5s5f0-x".into(),
        ));
        let refused = refuse_context_under_a_path(&Value::Str(NixStr::with_context(
            b"3.13".as_slice(),
            context,
        )));
        assert!(
            matches!(&refused, Err(e) if format!("{e:?}")
                .contains("a string that refers to a store path cannot be appended to a path")),
            "{refused:?}"
        );
        // Without a context the same part passes, so the check is refusing
        // the context and not the type.
        assert!(refuse_context_under_a_path(&Value::Str(NixStr::from("3.13"))).is_ok());
        // And a plain interpolated path still evaluates, so wiring the rule
        // into the op did not make every part refuse.
        assert_eq!(
            render_at(r#"let v = "3.13"; in ./${v}"#, "/base").as_deref(),
            Ok("/base/3.13")
        );
    }

    /// `~/...` expands against the home directory the embedder supplied,
    /// and the interpolated form takes the same prefix as the literal one --
    /// cppnix builds both in `parser.y` from one `getHome()` call
    /// (`parser.y:465`), so a backend that resolved only the literal form
    /// would answer two different paths for `~/x` and `~/${"x"}`.
    ///
    /// The base directory is `/base` and plays no part: a `~` path is
    /// absolute from the home directory, never relative to the file.
    #[test]
    fn a_home_path_expands_against_the_supplied_home_directory() {
        let settings = crate::eval::Settings {
            home_dir: Some("/home/nixer".to_owned()),
            ..crate::eval::Settings::default()
        };
        assert_eq!(
            render_with(r#"let v = "x"; in ~/dir/${v}"#, "/base", &settings).as_deref(),
            Ok("/home/nixer/dir/x")
        );
        assert_eq!(
            render_with("~/dir/x", "/base", &settings).as_deref(),
            Ok("/home/nixer/dir/x")
        );
    }

    /// With no home directory to expand against, the answer is cppnix's
    /// error rather than a guess -- `getHomeOf` throws "cannot determine
    /// user\'s home directory" (`users.cc:31`) and the crate cannot invent a
    /// path, because inventing one resolves an import to the wrong file
    /// instead of to none.
    #[test]
    fn a_home_path_without_a_home_directory_says_so() {
        let got = render_at("~/dir/x", "/base");
        assert!(
            got.as_ref()
                .err()
                .is_some_and(|e| e.contains("cannot determine user's home directory")),
            "{got:?}"
        );
    }

    /// Under `pure-eval` the path is refused whatever `$HOME` says, because
    /// the answer would differ per machine and feed a hash
    /// (`parser.y:455`). Checked with a home directory present, so it is
    /// the purity rule firing and not the absence of one.
    #[test]
    fn a_home_path_is_rejected_under_pure_eval() {
        let settings = crate::eval::Settings {
            home_dir: Some("/home/nixer".to_owned()),
            pure_eval: true,
            ..crate::eval::Settings::default()
        };
        let got = render_with("~/dir/x", "/base", &settings);
        assert!(
            got.as_ref()
                .err()
                .is_some_and(|e| e.contains("can not be resolved in pure mode")),
            "{got:?}"
        );
    }

    /// The parser lints at `fatal`, message for message with cppnix
    /// (`parser.y:372-466`). The texts are pinned verbatim because the
    /// lang-diff gate classifies these failures as `unknown`, where the
    /// comparison is byte equality of the terminal `error:` line -- a word
    /// moved here is a mismatch there. Measured on cppnix at e64631c27
    /// (`tests/functional/lang/eval-fail-{url-literal,short-path-literal,abs-path-fatal,home-path-fatal}`).
    #[test]
    fn fatal_parser_lints_reject_what_cppnix_rejects() {
        let url_fatal = crate::eval::Settings {
            lint_url_literals: crate::eval::Diagnose::Fatal,
            ..crate::eval::Settings::default()
        };
        let got = render_with("http://example.com", "/base", &url_fatal);
        // `render_with` debug-formats the compile error, so the message's
        // inner quotes arrive escaped; the two halves pin the full text.
        assert!(
            got.as_ref().err().is_some_and(|e| {
                e.contains("URL literals are disallowed. Consider using a string literal")
                    && e.contains("http://example.com")
                    && e.contains("instead (lint-url-literals)")
            }),
            "{got:?}"
        );

        let abs_fatal = crate::eval::Settings {
            lint_absolute_path_literals: crate::eval::Diagnose::Fatal,
            ..crate::eval::Settings::default()
        };
        let got = render_with("/tmp/foo", "/base", &abs_fatal);
        assert!(
            got.as_ref().err().is_some_and(|e| e.contains(
                "absolute path literals are not portable. Consider replacing path \
                 literal '/tmp/foo' by a string, relative path, or parameter \
                 (lint-absolute-path-literals)"
            )),
            "{got:?}"
        );
        // The home form has its own wording under the same setting
        // (`parser.y:461`), and fires only once `pure-eval` has had its say.
        let home_abs_fatal = crate::eval::Settings {
            home_dir: Some("/home/nixer".to_owned()),
            ..abs_fatal.clone()
        };
        let got = render_with("~/foo", "/base", &home_abs_fatal);
        assert!(
            got.as_ref().err().is_some_and(|e| e.contains(
                "home path literals are not portable. Consider replacing path \
                 literal '~/foo' by a string, relative path, or parameter \
                 (lint-absolute-path-literals)"
            )),
            "{got:?}"
        );

        let short_fatal = crate::eval::Settings {
            lint_short_path_literals: crate::eval::Diagnose::Fatal,
            ..crate::eval::Settings::default()
        };
        let got = render_with("test/subdir", "/base", &short_fatal);
        assert!(
            got.as_ref().err().is_some_and(|e| e.contains(
                "relative path literal 'test/subdir' should be prefixed with '.' for \
                 clarity: './test/subdir' (lint-short-path-literals)"
            )),
            "{got:?}"
        );

        // `path_start` is shared with the interpolated form, so the lint
        // reaches `/x/${...}` exactly as it reaches `/x/y`.
        let got = render_with("/tmp/${\"x\"}", "/base", &abs_fatal);
        assert!(
            got.as_ref()
                .err()
                .is_some_and(|e| e.contains("absolute path literals are not portable")),
            "{got:?}"
        );
    }

    /// What a fatal lint permits, it permits: `./x` and `../x` never trip
    /// the short-path lint (`parser.y:442` returns before diagnosing), a
    /// relative path is not an absolute one, and a quoted URL lexes as a
    /// string. These are the five eval-okay corpus cases the bridge used to
    /// refuse wholesale (`eval-okay-*-fatal`).
    #[test]
    fn fatal_parser_lints_permit_what_cppnix_permits() {
        let all_fatal = crate::eval::Settings {
            lint_url_literals: crate::eval::Diagnose::Fatal,
            lint_short_path_literals: crate::eval::Diagnose::Fatal,
            lint_absolute_path_literals: crate::eval::Diagnose::Fatal,
            ..crate::eval::Settings::default()
        };
        assert_eq!(
            render_with("\"http://example.com\"", "/base", &all_fatal).as_deref(),
            Ok("\"http://example.com\"")
        );
        assert_eq!(
            render_with("./test/subdir", "/base", &all_fatal).as_deref(),
            Ok("/base/test/subdir")
        );
        assert_eq!(
            render_with("../test/subdir", "/base/lang", &all_fatal).as_deref(),
            Ok("/base/test/subdir")
        );
    }

    /// `warn` evaluates like `ignore`: the value is the same and only cppnix
    /// prints a diagnostic (tier-2 warning text this backend does not
    /// carry). Pinned so an over-eager future lint cannot quietly turn
    /// `warn` into a failure.
    #[test]
    fn warn_parser_lints_do_not_change_the_value() {
        let all_warn = crate::eval::Settings {
            lint_url_literals: crate::eval::Diagnose::Warn,
            lint_short_path_literals: crate::eval::Diagnose::Warn,
            lint_absolute_path_literals: crate::eval::Diagnose::Warn,
            ..crate::eval::Settings::default()
        };
        assert_eq!(
            render_with("http://example.com", "/base", &all_warn).as_deref(),
            Ok("\"http://example.com\"")
        );
        assert_eq!(
            render_with("test/subdir", "/base", &all_warn).as_deref(),
            Ok("/base/test/subdir")
        );
        assert_eq!(
            render_with("/tmp/foo", "/base", &all_warn).as_deref(),
            Ok("/tmp/foo")
        );
    }

    /// With the feature off, `|>` is cppnix's feature-is-disabled error
    /// (`lexer.l` via `requireExperimentalFeature`), byte for byte -- the
    /// lang-diff gate compares this terminal line under `unknown`. Measured
    /// on cppnix at e64631c27 (`eval-fail-pipe-operators`).
    #[test]
    fn pipe_operators_off_is_the_feature_disabled_error() {
        let got = render_at("1 |> 2", "/base");
        assert!(
            got.as_ref().err().is_some_and(|e| e.contains(
                "experimental Nix feature 'pipe-operators' is disabled; add \
                 '--extra-experimental-features pipe-operators' to enable it"
            )),
            "{got:?}"
        );
        let got = render_at("2 <| 1", "/base");
        assert!(
            got.as_ref()
                .err()
                .is_some_and(|e| e.contains("experimental Nix feature 'pipe-operators'")),
            "{got:?}"
        );
    }

    /// With the feature on, both operators are sugar for a call and nothing
    /// else (`parser.y:287-295`): `a |> f` is `f a` with `|>` associating
    /// left, `f <| a` is `f a` with `<|` associating right, and the argument
    /// is a thunk exactly as an `ExprCall`'s is. Chains and laziness pinned
    /// against cppnix under `--extra-experimental-features pipe-operators`.
    #[test]
    fn pipe_operators_on_desugar_to_application() {
        let pipes = crate::eval::Settings {
            pipe_operators: true,
            ..crate::eval::Settings::default()
        };
        assert_eq!(
            render_with("1 |> (x: x + 1)", "/base", &pipes).as_deref(),
            Ok("2")
        );
        assert_eq!(
            render_with("(x: x + 1) <| 1", "/base", &pipes).as_deref(),
            Ok("2")
        );
        // Left-associative: ((1 |> f) |> g) = g (f 1).
        assert_eq!(
            render_with("1 |> (x: x + 1) |> (y: y * 3)", "/base", &pipes).as_deref(),
            Ok("6")
        );
        // Right-associative: f <| (g <| 2) = f (g 2).
        assert_eq!(
            render_with("(x: x + 1) <| (y: y * 3) <| 2", "/base", &pipes).as_deref(),
            Ok("7")
        );
        // The argument is a thunk: a function that never forces it never
        // sees the throw.
        assert_eq!(
            render_with("(builtins.throw \"boom\") |> (x: \"ok\")", "/base", &pipes).as_deref(),
            Ok("\"ok\"")
        );
    }

    /// `builtins.toPath` coerces to an absolute path and returns a STRING,
    /// canonicalized. Every row is measured cppnix output (`nix-instantiate
    /// --eval --strict`):
    ///
    /// - string inputs are canonicalized, not passed through -- `rootPath`'s
    ///   `CanonPath` collapses `.`, `..` and doubled slashes, so an
    ///   implementation that returns the string it checked gives
    ///   `"/a/./b//c/../d"` for the first row;
    /// - the result really is a string (`typeOf` row), so an implementation
    ///   that returns a path value prints the same on most rows and is only
    ///   caught there;
    /// - a set coerces through `__toString` or `outPath`, like the rest of
    ///   the path family;
    /// - a relative string is the coercion's own refusal, whose terminal
    ///   line `eval-fail-to-path.err.exp` compares byte-for-byte;
    /// - bare `toPath` stays undefined: cppnix registers only `__toPath`,
    ///   so a TABLE entry that leaked into the global scope would turn an
    ///   `undefined variable` into an answer.
    #[test]
    fn to_path_canonicalizes_and_returns_a_string() {
        for (src, want) in [
            (r#"builtins.toPath "/a/./b//c/../d""#, r#""/a/b/d""#),
            (r#"builtins.toPath "/""#, r#""/""#),
            (r#"builtins.typeOf (builtins.toPath "/x")"#, r#""string""#),
            ("builtins.toPath ./sub/file.nix", r#""/base/sub/file.nix""#),
            (
                r#"builtins.toPath { __toString = _: "/abs/x"; }"#,
                r#""/abs/x""#,
            ),
            (r#"builtins.toPath { outPath = "/out/y"; }"#, r#""/out/y""#),
        ] {
            assert_eq!(
                render_at(src, "/base").as_deref(),
                Ok(want.to_string().as_str()),
                "expr {src}"
            );
        }
        let Err(err) = render_at(r#"builtins.toPath "foo/bar""#, "/base") else {
            unreachable!("a relative string is an error")
        };
        assert!(
            err.contains("string 'foo/bar' doesn't represent an absolute path"),
            "got {err}"
        );
        // `render_with` debug-formats compile errors, so the variant name is
        // the text to match.
        let Err(bare) = render_at("toPath", "/base") else {
            unreachable!("the bare name is unbound")
        };
        assert!(
            bare.contains(r#"UndefinedVariable("toPath")"#),
            "got {bare}"
        );
    }

    /// Under `parse-toml-timestamps`, a TOML date or time is a
    /// `{ _type = "timestamp"; value = "..."; }` set whose string is
    /// toml11's *normalized* form, not the source text. The rows mirror
    /// `eval-okay-fromTOML-timestamps.nix` and the expectations its `.exp`
    /// (plus two direct measurements; see `toml_datetime_text`):
    ///
    /// - lowercase `t` and a space both come back as `T` (odt4, odt8);
    /// - fractions pad to 3, 6 or 9 digits and a tenth digit is truncated
    ///   (lt2, odt5, odt8, odt11, odt14);
    /// - a zero offset prints `Z` even when spelled `+00:00` (odt15).
    #[test]
    fn from_toml_timestamps_match_toml11s_normalization() {
        let toml_ts = crate::eval::Settings {
            parse_toml_timestamps: true,
            ..crate::eval::Settings::default()
        };
        let src = "builtins.fromTOML ''\n\
                   odt1 = 1979-05-27T07:32:00Z\n\
                   odt2 = 1979-05-27T00:32:00-07:00\n\
                   odt4 = 1979-05-27 07:32:00Z\n\
                   odt5 = 1979-05-27 07:32:00.1Z\n\
                   odt8 = 1979-05-27t07:32:00.1234Z\n\
                   odt11 = 1979-05-27 07:32:00.1234567Z\n\
                   odt14 = 1979-05-27t07:32:00.1234567891Z\n\
                   odt15 = 1979-05-27T07:32:00+05:45\n\
                   odt16 = 1979-05-27T07:32:00+00:00\n\
                   ldt1 = 1979-05-27T07:32:00\n\
                   ld1 = 1979-05-27\n\
                   lt1 = 07:32:00\n\
                   lt2 = 00:32:00.1\n\
                   ''";
        let ts = |s: &str| format!("{{ _type = \"timestamp\"; value = \"{s}\"; }}");
        let want = format!(
            "{{ ld1 = {}; ldt1 = {}; lt1 = {}; lt2 = {}; odt1 = {}; odt11 = {}; odt14 = {}; \
             odt15 = {}; odt16 = {}; odt2 = {}; odt4 = {}; odt5 = {}; odt8 = {}; }}",
            ts("1979-05-27"),
            ts("1979-05-27T07:32:00"),
            ts("07:32:00"),
            ts("00:32:00.100"),
            ts("1979-05-27T07:32:00Z"),
            ts("1979-05-27T07:32:00.123456700Z"),
            ts("1979-05-27T07:32:00.123456789Z"),
            ts("1979-05-27T07:32:00+05:45"),
            ts("1979-05-27T07:32:00Z"),
            ts("1979-05-27T00:32:00-07:00"),
            ts("1979-05-27T07:32:00Z"),
            ts("1979-05-27T07:32:00.100Z"),
            ts("1979-05-27T07:32:00.123400Z"),
        );
        assert_eq!(
            render_with(src, "/base", &toml_ts).as_deref(),
            Ok(want.as_str())
        );
        // With the feature off the same document is a refusal, wrapped the
        // way cppnix's parse visitor wraps it; the corpus compares this
        // terminal line byte-for-byte (`eval-fail-fromTOML-timestamps`).
        let Err(err) = render_at("builtins.fromTOML ''d = 1979-05-27''", "/base") else {
            unreachable!("a timestamp with the feature off is an error")
        };
        assert!(
            err.contains("while parsing TOML: Dates and times are not supported"),
            "got {err}"
        );
    }

    /// A path on the left keeps the result a path, and a path carries no
    /// context, so a right-hand side that refers to a store path has nowhere
    /// to record the dependency. cppnix refuses rather than dropping it:
    ///
    ///     $ nix-instantiate --eval -E '/x + (builtins.appendContext "/y" \
    ///         { "/nix/store/1zqzq0d31c1cnjf6z8pfr2h2p1c5s5f0-x" = { path = true; }; })'
    ///     error: a string that refers to a store path cannot be appended to a path
    ///
    /// Reachable two ways now that a set operand coerces -- through a string
    /// carrying a context, and through the set itself -- and this backend
    /// used to answer a path for both, silently losing the reference.
    ///
    /// Driven through `arith` rather than through source because every way of
    /// writing a context in Nix goes through a builtin that wants the
    /// embedder's store directory, and that is a `OnceLock` this test would
    /// be setting for every other test in the process.
    #[test]
    fn a_path_refuses_a_right_hand_side_carrying_context() {
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let mut context = BTreeSet::new();
        context.insert(crate::value2::ContextElem::Opaque(
            "/nix/store/1zqzq0d31c1cnjf6z8pfr2h2p1c5s5f0-x".into(),
        ));
        let refused = vm.arith(
            Op::Add,
            Value::Path(crate::value2::ambient_path("/x")),
            Value::Str(NixStr::with_context(b"/y".as_slice(), context)),
        );
        assert!(
            matches!(&refused, Err(e) if format!("{e:?}")
                .contains("a string that refers to a store path cannot be appended to a path")),
            "want cppnix's refusal; got: {refused:?}"
        );

        // The same append with no context is an ordinary path, so the check
        // above is refusing the context and not the operand. cppnix answers
        // `/x/y` for this and for the set spelling below.
        let plain = vm.arith(
            Op::Add,
            Value::Path(crate::value2::ambient_path("/x")),
            Value::Str(NixStr::from("/y")),
        );
        assert!(
            matches!(&plain, Ok(Value::Path(p)) if p.path.as_ref() == "/x/y"),
            "want the path /x/y; got: {plain:?}"
        );
        assert_eq!(
            eval_at_depth(r#"/x + { outPath = "/y"; }"#, 1000),
            Ok("/x/y".to_owned())
        );
    }

    #[test]
    fn path_addition_rebuilds_under_the_ambient_root() {
        let mount = "/nix/store/00000000000000000000000000000000-source";
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let original = Rc::new(crate::value2::PathValue::new(
            crate::value2::Root::mounted(mount),
            format!("{mount}/a"),
        ));
        let same_spelling = vm
            .arith(
                Op::Add,
                Value::Path(Rc::clone(&original)),
                Value::Str(NixStr::from("")),
            )
            .expect("append empty string");
        assert!(matches!(
            same_spelling,
            Value::Path(path)
                if path.root == crate::value2::Root::Ambient
                    && path.path == original.path
                    && path.as_ref() != original.as_ref()
        ));
        let result = vm.arith(
            Op::Add,
            Value::Path(Rc::clone(&original)),
            Value::Str(NixStr::from("/../outside")),
        );
        let Ok(Value::Path(path)) = result else {
            unreachable!("path addition did not return a path")
        };
        assert_eq!(path.root, crate::value2::Root::Ambient);
        assert_eq!(path.as_ref().as_ref(), format!("{mount}/outside"));
        assert_ne!(
            original.as_ref(),
            path.as_ref(),
            "a mounted p must differ from p + empty-or-string because cppnix rebuilds under rootFS"
        );
    }

    #[test]
    fn interpolated_mounted_path_rebuilds_under_the_ambient_root() {
        let mount = "/nix/store/00000000000000000000000000000000-source";
        let source = crate::value2::PathValue::new(
            crate::value2::Root::mounted(mount),
            format!("{mount}/module.nix"),
        );
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let module = vm
            .import_module(&source, r#"./${"a/../b"}"#, mount)
            .expect("compile mounted interpolation");
        vm.start_module(&module);
        let value = crate::eval::drive(&mut vm, &crate::host::RealFs)
            .expect("evaluate mounted interpolation");

        assert!(matches!(
            value,
            Value::Path(path)
                if path.root == crate::value2::Root::Ambient
                    && path.as_ref().as_ref() == format!("{mount}/b")
        ));
    }

    #[test]
    fn compile_memo_separates_identical_spellings_by_module_root() {
        let path = "/nix/store/00000000000000000000000000000000-source/m.nix";
        let mount = "/nix/store/00000000000000000000000000000000-source";
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let ambient = vm
            .import_module(&crate::value2::PathValue::ambient(path), "./dep.nix", mount)
            .expect("compile ambient module");
        let mounted = vm
            .import_module(
                &crate::value2::PathValue::new(crate::value2::Root::mounted(mount), path),
                "./dep.nix",
                mount,
            )
            .expect("compile mounted module");

        assert!(!Rc::ptr_eq(&ambient, &mounted));
        assert_eq!(ambient.root, crate::value2::Root::Ambient);
        assert_eq!(mounted.root, crate::value2::Root::mounted(mount));
    }

    /// ENG-12432. This VM holds its frames on the heap, so a self-application
    /// does not fault the way cppnix's host-stack recursion does; it
    /// allocates, and it reached 67 GB before something killed it. The point
    /// of the limit is that the program now fails, in bounded memory and
    /// bounded time, with the error cppnix reports for the same input.
    #[test]
    fn self_application_fails_instead_of_allocating_forever() {
        let outcome = eval_at_depth("(x: x x) (x: x x)", 1000);
        assert!(
            matches!(&outcome, Err(e) if e.contains("stack overflow; max-call-depth exceeded")),
            "want a failure in cppnix's wording, so a differ reads the same class; got: {outcome:?}"
        );
    }

    /// The counter must fall as calls return, not only rise as they are made.
    /// 50k calls one after another nest two deep; if a return path forgot to
    /// decrement, this trips the limit and the guard would be refusing
    /// programs cppnix accepts. That is the failure worth catching here,
    /// because it looks like a working limit from every other angle.
    #[test]
    fn sequential_calls_do_not_accumulate_depth() {
        let src = "builtins.foldl' (a: b: a + b) 0 (builtins.genList (x: x) 50000)";
        assert_eq!(eval_at_depth(src, 100), Ok("1249975000".to_owned()));
    }

    /// Unwinding pops unit frames without going through the ordinary return
    /// path, so it needs its own decrement. A caught throw inside a loop is
    /// the shape that exercises it: without the unwind-side decrement the
    /// depth ratchets up once per iteration and the limit trips.
    #[test]
    fn a_caught_failure_gives_its_depth_back() {
        // The throw has to happen with closure bodies in flight, or nothing
        // with `is_call` set is on the stack when the unwind runs and the
        // test passes whether or not the decrement is there. Two nested
        // applications is what makes the drift 2 per iteration; the first
        // version of this test threw straight out of `tryEval`, exercised no
        // call frame at all, and stayed green with the decrement deleted.
        let src = "builtins.foldl' \
                   (a: b: a + (if (builtins.tryEval ((g: g 1) (y: throw \"x\"))).success \
                               then 1 else 0)) \
                   0 (builtins.genList (x: x) 2000)";
        assert_eq!(eval_at_depth(src, 100), Ok("0".to_owned()));
    }

    /// The ceiling is the configured one, not a constant compiled in: the
    /// corpus passes `--max-call-depth` and both arms have to honour it.
    #[test]
    fn the_configured_ceiling_is_the_one_enforced() {
        let src = "let f = n: if n == 0 then 0 else 1 + f (n - 1); in f 200";
        assert_eq!(eval_at_depth(src, 10_000), Ok("200".to_owned()));
        let outcome = eval_at_depth(src, 50);
        assert!(
            matches!(&outcome, Err(e) if e.contains("stack overflow; max-call-depth exceeded")),
            "200 nested calls must not fit under a ceiling of 50; got: {outcome:?}"
        );
    }

    /// The scheduler seam, driven from where a handler sits: suspend, watch
    /// the machine report itself parked rather than fail, answer, and watch
    /// the frame that was mid-unit consume the answer as an ordinary operand.
    #[test]
    fn a_suspension_resumes_into_the_running_frame() -> std::result::Result<(), String> {
        // One unit that adds 40 to whatever is already on its stack.
        let module = Rc::new(Module {
            consts: vec![Const::Int(40)],
            symbols: Vec::new(),
            units: vec![CodeUnit {
                spans: vec![crate::ir::NO_POS; 3],
                ops: vec![Op::Const(0), Op::Add, Op::Ret],
                param: None,
                attr_sites: Vec::new(),
            }],
            entry: 0,
            origin: crate::ir::SrcOrigin::String,
            root: crate::value2::Root::Ambient,
            line_starts: Vec::new(),
            dynamic_attr_origins: crate::value2::DynamicAttrOriginSlab::default(),
            linked: crate::value2::LinkedSymbols::default(),
        });
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        vm.start_module(&module);

        let Step::Perform {
            domain,
            request,
            resume,
        } = vm.suspend_perform("test".to_owned(), b"ping".to_vec())
        else {
            return Err("expected a Perform suspension".to_owned());
        };
        assert_eq!(domain, "test");
        assert_eq!(request, b"ping".to_vec());

        // Parked, not broken. This used to be an error -- "poll while a
        // suspension is open" -- which is what forced a driver to keep its
        // own record of which evaluations were waiting.
        let Ok(Step::Idle { outstanding }) = vm.poll() else {
            return Err("a parked machine must report itself idle".to_owned());
        };
        assert_eq!(outstanding, 1);
        vm.resume(resume, Value::Int(2))
            .map_err(|_| "resume rejected a live token".to_owned())?;
        // Spending the token twice is refused rather than answering a wait
        // that no longer exists.
        if vm.resume(resume, Value::Int(2)).is_ok() {
            return Err("a spent token must not resume again".to_owned());
        }

        let Ok(Step::Done(Value::Int(n))) = vm.poll() else {
            return Err("expected the unit to finish".to_owned());
        };
        assert_eq!(n, 42);
        Ok(())
    }
}

#[cfg(test)]
mod builtins_set_tests {
    use super::Vm;
    use crate::value2::Value;
    use std::rc::Rc;

    /// One set per `Vm`, shared by every reference to it. Without this the
    /// compiler's fold would still leave first-class `builtins` -- and every
    /// `with builtins;` -- paying for ~160 interns and slot allocations per
    /// occurrence. ENG-12539.
    #[test]
    fn the_builtins_set_is_built_once_per_vm() {
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let first = vm.builtins_value();
        let second = vm.builtins_value();
        assert!(
            matches!(
                (&first, &second),
                (Ok(Value::Attrs(_)), Ok(Value::Attrs(_)))
            ),
            "builtins_set did not produce an attrset twice: {first:?} {second:?}"
        );
        if let (Ok(Value::Attrs(a)), Ok(Value::Attrs(b))) = (&first, &second) {
            assert!(Rc::ptr_eq(a, b), "the set was rebuilt on the second read");
        }
    }
}
