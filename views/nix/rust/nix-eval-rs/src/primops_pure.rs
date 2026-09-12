//! The builtin driver, the continuation cursor it steps, and every primop
//! that reaches nothing outside its arguments.
//!
//! Reaching nothing outside its arguments is the whole of what puts a primop
//! here rather than in [`crate::primops_host`], and it is not a stylistic
//! split: it is the same line read sets and memo keys are drawn on. A result
//! computed in this module depends on the argument values and on nothing a
//! `Host` could answer differently tomorrow, so it is safe to memoise against
//! a read set that records no question at all. That is why the boundary is
//! held by a test rather than by this paragraph --
//! `builtins::purity_tests::each_primop_lives_on_its_own_side_of_the_boundary`
//! fails the build when a TABLE entry is implemented on the wrong side.
//!
//! The driver sits here, and serves both halves: it forces a builtin's strict
//! arguments in cppnix's order, runs the body, and turns what the body hands
//! back into the machine's next step. For a host primop that step is a
//! `Yield::Need`, so the driver routes host questions without any primop in
//! this module asking one.
//!
//! Every body here is either pure over already-forced arguments or a `Cont`:
//! a cursor the machine steps, handing back one forced value or one
//! application result at a time. Nothing calls back into the interpreter, so
//! `map` over a million-element list, `sort` with a Nix comparator and
//! `deepSeq` over a self-referential attrset all run flat.

use crate::builtins::{ArgType, Kind, TABLE};
use crate::refusal::{Refusal, RefusalToken};
use crate::task::{NeedPath, Task, Yield};
use crate::value2::{AttrMap, AttrOrigin, Attrs, ContextElem, NixStr, Slot, Sym, Value, type_name};
use crate::vm::{Result, Vm, VmError, forced};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, VecDeque, btree_map};
use std::rc::Rc;

/// How a builtin body starts: with an answer, with one thing to evaluate
/// whose value is the answer, or with a continuation to drive.
pub enum Begin {
    Done(Value),
    Force(Slot),
    Sub(Task),
    Cont(Cont),
}

/// One literal run or one `to`-index in a `replaceStrings` plan. Planning the
/// substitution before any `to` is forced is what keeps unused replacements
/// lazy, which the corpus checks with a `throw` in an unreachable slot.
pub enum Piece {
    Lit(Vec<u8>),
    Use(usize),
}

pub enum Cont {
    /// Walking the builtin's `strict` list: `Args(k)` means the first `k`
    /// steps have been forced and the value of step `k - 1` has just come
    /// back, waiting to be type-checked. A cursor into that list rather than
    /// an argument index, because the order there is cppnix's forcing order
    /// and is not always left to right.
    Args(usize),
    /// The coercion of strict step `k - 1`'s argument, at position `pos`, is
    /// running as a sub-task. Its result replaces that argument and the walk
    /// resumes at `Args(k)`. See [`crate::builtins::ArgType::Coerce`].
    CoerceArg {
        k: usize,
        pos: usize,
    },
    /// Run the builtin body.
    Start,
    /// Hand back the next value delivered.
    Result,
    Filter {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        out: Vec<Slot>,
    },
    /// `any` (want = true) and `all` (want = false): stop at the first
    /// element whose predicate equals `want`.
    AnyAll {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        want: bool,
    },
    ConcatMap {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        out: Vec<Slot>,
    },
    /// `half` holds `f acc` between the two applications of one step.
    Foldl {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        acc: Slot,
        half: Option<Value>,
        /// Set once the list is exhausted and the accumulator is being
        /// forced: without it the forced value arrives looking like the
        /// result of `f acc` and gets applied to a list element that is not
        /// there.
        finishing: bool,
    },
    /// Insertion sort: cppnix's sort is stable and its comparator is a Nix
    /// function that can throw, which rules the standard sorts out.
    Sort {
        f: Value,
        items: Vec<Slot>,
        next: usize,
        sorted: Vec<Slot>,
        probe: usize,
        half: Option<Value>,
    },
    GroupBy {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        out: BTreeMap<Sym, Vec<Slot>>,
    },
    Partition {
        f: Value,
        items: Rc<Vec<Slot>>,
        i: usize,
        right: Vec<Slot>,
        wrong: Vec<Slot>,
    },
    Elem {
        x: Slot,
        items: Rc<Vec<Slot>>,
        i: usize,
    },
    /// Force every element, then finish purely.
    ForceEach {
        items: Rc<Vec<Slot>>,
        i: usize,
        vals: Vec<Value>,
        finish: Finish,
    },
    ListToAttrs {
        items: Rc<Vec<Slot>>,
        i: usize,
        out: BTreeMap<Sym, Slot>,
        origins: Vec<(Sym, AttrOrigin)>,
        cur: Option<Rc<crate::value2::Attrs>>,
        name_sym: Sym,
        value_sym: Sym,
    },
    Replace {
        stage: u8,
        froms: Vec<Vec<u8>>,
        plan: Vec<Piece>,
        used: Vec<usize>,
        k: usize,
        tos: BTreeMap<usize, Vec<u8>>,
        /// The subject's context plus that of every replacement actually
        /// used, accumulated across the yields because the replacements are
        /// forced one at a time. cppnix merges exactly these two and drops
        /// the `from` strings' contexts, which is why they are not here.
        context: BTreeSet<ContextElem>,
    },
    /// `seen` is keyed on cell identity, the way cppnix's forceValueDeep is:
    /// without it a self-referential attrset never bottoms out. `tail` marks
    /// the switch to the second argument, which cppnix forces only once the
    /// first is fully deep, so a throw in either lands in that order.
    ///
    /// Each slot carries its nesting depth, because `forceValueDeep` opens
    /// every level with `addCallDepth` (`eval.cc:2421`) and this walk is flat,
    /// so nothing else would stop it. Without the depth this answered `1` for
    /// a 20,000-deep list that cppnix refuses (ENG-12900).
    DeepSeq {
        work: Vec<(Slot, usize)>,
        seen: BTreeSet<usize>,
        tail: bool,
        /// The depth of the slot currently being forced, so the children that
        /// come back in `incoming` know theirs.
        depth: usize,
    },
    TryEval {
        started: bool,
    },
    /// Waiting on the scheduler's answer to a question already built. For
    /// the builtins whose argument is not a path -- `getEnv`, `toFile` --
    /// and so needs no coercion.
    Ask {
        asked: bool,
        need: NeedPath,
    },
    /// A path-family argument: coerced to an absolute path, its context
    /// realised, then asked about. `mk` is applied last rather than at
    /// construction, because the path is not known until the coercion
    /// finishes -- which for a set means running `__toString` -- and can move
    /// again when the realisation answers. See [`coerce_to_path`] and
    /// [`PathPhase`].
    Path {
        phase: PathPhase,
        mk: fn(Rc<crate::value2::PathValue>) -> NeedPath,
    },
    /// `toPath`'s coercion: the front half of [`Cont::Path`] -- coerce the
    /// argument to an absolute path -- and then stop, because the coerced
    /// string *is* the answer. cppnix's `prim_toPath` calls `coerceToPath`
    /// directly rather than `realisePath` (`primops.cc`, `prim_toPath`), so
    /// the context is copied onto the result, never realised, and no
    /// question reaches the scheduler.
    ToPath {
        stage: PathStage,
    },
    /// A coercion the body started rather than the driver, and what to do
    /// with the string it produces. One user, `dirOf`, and the reason is in
    /// [`crate::builtins::ArgType::Coerce`]: cppnix's `prim_dirOf` tests the
    /// forced argument for `nPath` and returns a path unchanged, so the body
    /// needs the value and cannot have it replaced by its coercion.
    CoerceBody {
        slot: Slot,
        flags: crate::print::CoerceFlags,
        started: bool,
        finish: Finish,
    },
    /// A continuation owned by `primops_host`, so its state machines live
    /// beside the builtins they belong to rather than here.
    Ext(crate::primops_host::Ext),
    /// `genericClosure`'s worklist walk. Its own variant rather than one of
    /// the shared cursors above, because it is the only builtin here that
    /// carries a queue and a key set across steps.
    Generic(Generic),
    /// `import`, in three steps: ask the scheduler for the file, compile it
    /// and force its entry, then hand that value back. Three and not two --
    /// the forced module value returns to this same continuation, and a
    /// two-state version parses it as if it were the scheduler's answer.
    Import {
        stage: ImportStage,
    },
}

/// How far a path-family argument has got on its way to a question.
///
/// Three states and not two, because cppnix's `realisePath`
/// (`primops.cc:167`) is three steps: coerce the value to a path, realise
/// whatever context it carried, and only then read. The middle step can be a
/// derivation build, so it suspends like any other question and needs a state
/// the machine can resume into.
#[derive(Clone)]
pub enum PathPhase {
    /// Coercing the argument. See [`PathStage`].
    Coerce(PathStage),
    /// A [`NeedPath::Realise`] is out; this is the path the rewrites coming
    /// back get applied to.
    Realising(Rc<crate::value2::PathValue>),
    /// The question itself is out; the next value is its answer.
    Asked,
}

/// How far the coercion of a path-family argument has got. Two states and
/// not one, because cppnix's `coerceToPath` is not a function of the forced
/// argument: a set is coerced by applying `__toString` or by following
/// `outPath`, and both of those need the machine.
#[derive(Clone, Copy)]
pub enum PathStage {
    /// The argument as the driver forced it, not yet coerced.
    Value,
    /// The string a [`Task::coerce_to_path`] sub-task made of a set.
    Coerced,
}

/// Where `import` is. The path stages come first because `import` reaches
/// its file through `realisePath` like the rest of the family
/// (`primops.cc`, `prim_import`), so its argument coerces the same way.
pub enum ImportStage {
    Path(PathStage),
    /// A [`NeedPath::Realise`] is out. Held separately from
    /// [`PathPhase::Realising`] rather than reusing it, because `import` has
    /// two more states after the read that the plain family does not.
    Realising(Rc<crate::value2::PathValue>),
    /// The scheduler's answer, to be compiled.
    Answer,
    /// The compiled module's forced entry value and optional closed-result key.
    Value(Option<crate::import_cache::ImportKey>),
    Derivation(Slot),
}

pub type Finish = fn(&mut Vm, &[Value], &[Slot]) -> Result<Value>;

// -- the driver -------------------------------------------------------------

pub fn drive(
    vm: &mut Vm,
    idx: u16,
    args: &mut [Slot],
    cont: &mut Cont,
    incoming: Option<Value>,
) -> Result<Yield> {
    let mut incoming = incoming;
    loop {
        // A coercion the walk started has finished: its result is the
        // argument from here on, so the body reads a string whatever was
        // written there. Replacing the argument rather than passing the
        // string alongside it is what makes the tag impossible to declare and
        // then not honour.
        if let Cont::CoerceArg { k, pos } = *cont {
            let coerced = incoming
                .take()
                .ok_or_else(|| VmError::eval("internal: coerced builtin argument lost"))?;
            *args
                .get_mut(pos)
                .ok_or_else(|| VmError::eval("internal: builtin argument out of range"))? =
                Slot::value(coerced);
            *cont = Cont::Args(k);
        }
        if let Cont::Args(k) = cont {
            let strict = TABLE.get(idx as usize).map(|b| b.strict).unwrap_or(&[]);
            // The step before this one asked for a value and it has arrived,
            // so this is cppnix's moment to check its type: after that
            // argument is forced and before the next one is. Raising here and
            // not in the body is what makes a type error beat a `throw` in a
            // later argument, which is how cppnix orders the two (ENG-12674).
            if let Some(&(pos, ty)) = k.checked_sub(1).and_then(|prev| strict.get(prev)) {
                if let ArgType::Coerce(flags) = ty {
                    // Coercing a string is the identity -- cppnix's `nString`
                    // arm copies the context and returns the bytes
                    // (`eval.cc`, `coerceToString`), whatever the flags say --
                    // so the machine runs only for the values that need it.
                    // The one thing skipped with it is cppnix's
                    // `addCallDepth` for this level, which can only matter to
                    // an expression already one frame from the ceiling.
                    let v = argv(args, pos)?;
                    if !matches!(v, Value::Str(_)) {
                        let slot = arg(args, pos)?.clone();
                        *cont = Cont::CoerceArg { k: *k, pos };
                        return Ok(Yield::sub(Task::coerce_as_primop(slot, flags)));
                    }
                } else {
                    ty.check(vm, &argv(args, pos)?)?;
                }
            }
            if let Some(&(pos, _)) = strict.get(*k) {
                let s = args
                    .get(pos)
                    .cloned()
                    .ok_or_else(|| VmError::eval("internal: builtin argument out of range"))?;
                *k += 1;
                return Ok(Yield::Force(s));
            }
        }
        if matches!(cont, Cont::Args(_)) {
            *cont = Cont::Start;
            continue;
        }
        if matches!(cont, Cont::Start) {
            let b = TABLE
                .get(idx as usize)
                .ok_or_else(|| VmError::eval("internal: bad builtin index"))?;
            match &b.kind {
                Kind::Pure(f) => return Ok(Yield::Done(f(vm, args)?)),
                Kind::Start(g) => match g(vm, args)? {
                    Begin::Done(v) => return Ok(Yield::Done(v)),
                    Begin::Force(s) => {
                        *cont = Cont::Result;
                        return Ok(Yield::Force(s));
                    }
                    Begin::Sub(t) => {
                        *cont = Cont::Result;
                        return Ok(Yield::sub(t));
                    }
                    Begin::Cont(c) => {
                        *cont = c;
                        incoming = None;
                        continue;
                    }
                },
            }
        }
        if matches!(cont, Cont::Result) {
            return incoming
                .take()
                .map(Yield::Done)
                .ok_or_else(|| VmError::eval("internal: builtin result lost"));
        }
        return step_cont(vm, args, cont, incoming.take());
    }
}

fn step_cont(
    vm: &mut Vm,
    args: &[Slot],
    cont: &mut Cont,
    incoming: Option<Value>,
) -> Result<Yield> {
    match cont {
        Cont::Args(_) | Cont::CoerceArg { .. } | Cont::Start | Cont::Result => {
            Err(VmError::eval("internal: driver state reached the walker"))
        }
        Cont::CoerceBody {
            slot,
            flags,
            started,
            finish,
        } => {
            if !*started {
                *started = true;
                return Ok(Yield::sub(Task::coerce_as_primop(slot.clone(), *flags)));
            }
            let coerced =
                incoming.ok_or_else(|| VmError::eval("internal: body coercion lost its result"))?;
            Ok(Yield::Done(finish(vm, &[coerced], args)?))
        }
        Cont::Ext(e) => crate::primops_host::step(vm, args, e, incoming),
        Cont::Generic(g) => g.step(vm, args, incoming),
        Cont::Filter { f, items, i, out } => {
            if let Some(v) = incoming {
                if want_bool(&v)? {
                    out.push(nth(items, *i)?);
                }
                *i += 1;
            }
            if *i >= items.len() {
                return Ok(Yield::Done(Value::list(std::mem::take(out))));
            }
            Ok(Yield::Apply(f.clone(), nth(items, *i)?))
        }
        Cont::AnyAll { f, items, i, want } => {
            if let Some(v) = incoming {
                if want_bool(&v)? == *want {
                    return Ok(Yield::Done(Value::Bool(*want)));
                }
                *i += 1;
            }
            if *i >= items.len() {
                return Ok(Yield::Done(Value::Bool(!*want)));
            }
            Ok(Yield::Apply(f.clone(), nth(items, *i)?))
        }
        Cont::ConcatMap { f, items, i, out } => {
            if let Some(v) = incoming {
                out.extend(want_list(&v)?.iter().cloned());
                *i += 1;
            }
            if *i >= items.len() {
                return Ok(Yield::Done(Value::list(std::mem::take(out))));
            }
            Ok(Yield::Apply(f.clone(), nth(items, *i)?))
        }
        Cont::Foldl {
            f,
            items,
            i,
            acc,
            half,
            finishing,
        } => {
            if *finishing {
                return incoming
                    .map(Yield::Done)
                    .ok_or_else(|| VmError::eval("internal: foldl' lost its accumulator"));
            }
            if let Some(v) = incoming {
                if half.is_none() {
                    *half = Some(v.clone());
                    return Ok(Yield::Apply(v, nth(items, *i)?));
                }
                *half = None;
                *acc = Slot::value(v);
                *i += 1;
            }
            if *i >= items.len() {
                // The initial accumulator is never forced by foldl' itself,
                // so an empty list hands it straight back unforced; anything
                // later that wants a value forces it then.
                return match acc.peek() {
                    Some(v) => Ok(Yield::Done(v)),
                    None => {
                        *finishing = true;
                        Ok(Yield::Force(acc.clone()))
                    }
                };
            }
            Ok(Yield::Apply(f.clone(), acc.clone()))
        }
        Cont::Sort {
            f,
            items,
            next,
            sorted,
            probe,
            half,
        } => {
            if let Some(v) = incoming {
                if half.is_none() {
                    *half = Some(v.clone());
                    return Ok(Yield::Apply(v, nth(sorted, *probe)?));
                }
                *half = None;
                if want_bool(&v)? {
                    let item = nth(items, *next)?;
                    sorted.insert(*probe, item);
                    *next += 1;
                    *probe = 0;
                } else {
                    *probe += 1;
                }
            }
            loop {
                if *next >= items.len() {
                    return Ok(Yield::Done(Value::list(std::mem::take(sorted))));
                }
                if *probe >= sorted.len() {
                    let item = nth(items, *next)?;
                    sorted.push(item);
                    *next += 1;
                    *probe = 0;
                    continue;
                }
                return Ok(Yield::Apply(f.clone(), nth(items, *next)?));
            }
        }
        Cont::GroupBy { f, items, i, out } => {
            if let Some(v) = incoming {
                // The grouping function's return becomes an attribute name,
                // which cppnix forces with `forceStringNoCtx`: a name that
                // referred to a store path would be a dependency the attrset
                // cannot record.
                let key = want_text_no_ctx(&v)?;
                let sym = vm.intern(&key);
                out.entry(sym).or_default().push(nth(items, *i)?);
                *i += 1;
            }
            if *i >= items.len() {
                let map: BTreeMap<Sym, Slot> = std::mem::take(out)
                    .into_iter()
                    .map(|(k, v)| (k, Slot::value(Value::list(v))))
                    .collect();
                return Ok(Yield::Done(Value::Attrs(Rc::new(Attrs::new(map)))));
            }
            Ok(Yield::Apply(f.clone(), nth(items, *i)?))
        }
        Cont::Partition {
            f,
            items,
            i,
            right,
            wrong,
        } => {
            if let Some(v) = incoming {
                if want_bool(&v)? {
                    right.push(nth(items, *i)?);
                } else {
                    wrong.push(nth(items, *i)?);
                }
                *i += 1;
            }
            if *i >= items.len() {
                let mut map = BTreeMap::new();
                let r = vm.intern("right");
                map.insert(r, Slot::value(Value::list(std::mem::take(right))));
                let w = vm.intern("wrong");
                map.insert(w, Slot::value(Value::list(std::mem::take(wrong))));
                return Ok(Yield::Done(Value::Attrs(Rc::new(Attrs::new(map)))));
            }
            Ok(Yield::Apply(f.clone(), nth(items, *i)?))
        }
        Cont::Elem { x, items, i } => {
            if matches!(incoming, Some(Value::Bool(true))) {
                return Ok(Yield::Done(Value::Bool(true)));
            }
            if incoming.is_some() {
                *i += 1;
            }
            if *i >= items.len() {
                return Ok(Yield::Done(Value::Bool(false)));
            }
            Ok(Yield::sub(Task::deep_eq_slots(x.clone(), nth(items, *i)?)))
        }
        Cont::ForceEach {
            items,
            i,
            vals,
            finish,
        } => {
            if let Some(v) = incoming {
                vals.push(v);
                *i += 1;
            }
            if *i >= items.len() {
                let f = *finish;
                return Ok(Yield::Done(f(vm, vals, args)?));
            }
            Ok(Yield::Force(nth(items, *i)?))
        }
        Cont::ListToAttrs {
            items,
            i,
            out,
            origins,
            cur,
            name_sym,
            value_sym,
        } => {
            match (incoming, cur.take()) {
                (Some(v), None) => {
                    let m = want_attrs(&v)?;
                    let ns = m
                        .get(name_sym)
                        .cloned()
                        .ok_or_else(|| VmError::eval("attribute 'name' missing"))?;
                    *cur = Some(m);
                    return Ok(Yield::Force(ns));
                }
                (Some(v), Some(m)) => {
                    let name = want_text_no_ctx(&v)?;
                    let sym = vm.intern(&name);
                    // First binding wins in cppnix.
                    if let btree_map::Entry::Vacant(slot) = out.entry(sym) {
                        let val = m
                            .get(value_sym)
                            .cloned()
                            .ok_or_else(|| VmError::eval("attribute 'value' missing"))?;
                        if let Some(origin) = &m.origin {
                            origins.push((sym, origin.clone()));
                        }
                        slot.insert(val);
                    }
                    *i += 1;
                }
                (None, _) => {}
            }
            if *i >= items.len() {
                let map = std::mem::take(out);
                let attrs = if origins.is_empty() {
                    Attrs::new(map)
                } else {
                    let origin = AttrOrigin::list_to_attrs(
                        std::mem::take(origins).into_boxed_slice(),
                        *value_sym,
                    )
                    .ok_or_else(|| VmError::eval("too many live listToAttrs origins"))?;
                    Attrs::at(map, origin)
                };
                return Ok(Yield::Done(Value::Attrs(Rc::new(attrs))));
            }
            Ok(Yield::Force(nth(items, *i)?))
        }
        Cont::Replace {
            stage,
            froms,
            plan,
            used,
            k,
            tos,
            context,
        } => {
            let from_items = want_list(&argv(args, 0)?)?;
            let to_items = want_list(&argv(args, 1)?)?;
            let mut incoming = incoming;
            if *stage == 0 {
                if let Some(v) = incoming.take() {
                    // cppnix forces these with the context-less overload, so
                    // a `from` string that refers to a store path neither
                    // contributes nor is refused. Matching that rather than
                    // improving on it: the two arms have to agree.
                    froms.push(want_bytes(&v)?.to_vec());
                }
                if froms.len() < from_items.len() {
                    let at = froms.len();
                    return Ok(Yield::Force(nth(&from_items, at)?));
                }
                let subject_arg = argv(args, 2)?;
                let subject_str = want_nix_str(&subject_arg)?;
                let subject = subject_str.bytes().to_vec();
                *context = subject_str.context_set();
                *plan = build_plan(froms, &subject);
                *used = distinct_uses(plan);
                *stage = 1;
            }
            if let Some(v) = incoming.take() {
                let j = *used
                    .get(*k)
                    .ok_or_else(|| VmError::eval("internal: replaceStrings index lost"))?;
                let to_str = want_nix_str(&v)?;
                if let Some(c) = to_str.context() {
                    context.extend(c.iter().cloned());
                }
                tos.insert(j, to_str.bytes().to_vec());
                *k += 1;
            }
            if *k < used.len() {
                let j = *used
                    .get(*k)
                    .ok_or_else(|| VmError::eval("internal: replaceStrings index lost"))?;
                return Ok(Yield::Force(nth(&to_items, j)?));
            }
            let mut out: Vec<u8> = Vec::new();
            for p in plan.iter() {
                match p {
                    Piece::Lit(s) => out.extend_from_slice(s),
                    Piece::Use(j) => {
                        out.extend_from_slice(tos.get(j).map(Vec::as_slice).unwrap_or(b""))
                    }
                }
            }
            Ok(Yield::Done(Value::Str(NixStr::with_context(
                out,
                core::mem::take(context),
            ))))
        }
        Cont::DeepSeq {
            work,
            seen,
            tail,
            depth,
        } => {
            if *tail {
                return incoming
                    .map(Yield::Done)
                    .ok_or_else(|| VmError::eval("internal: deepSeq lost its result"));
            }
            if let Some(v) = incoming {
                // One level down from whatever was just forced. `depth` is
                // the parent's, recorded when it was queued.
                let child = depth.saturating_add(1);
                match v {
                    Value::List(l) => work.extend(l.iter().map(|s| (s.clone(), child))),
                    Value::Attrs(m) => work.extend(m.values().map(|s| (s.clone(), child))),
                    _ => {}
                }
            }
            while let Some((s, d)) = work.pop() {
                if !seen.insert(s.id()) {
                    continue;
                }
                if d > crate::deepwalk::max_depth(vm) {
                    return Err(VmError::eval("stack overflow; max-call-depth exceeded"));
                }
                *depth = d;
                return Ok(Yield::Force(s));
            }
            *tail = true;
            Ok(Yield::Force(arg(args, 1)?.clone()))
        }
        Cont::Ask { asked, need } => {
            if !*asked {
                *asked = true;
                return Ok(Yield::Need(need.clone()));
            }
            incoming
                .map(Yield::Done)
                .ok_or_else(|| VmError::eval("internal: path answer lost"))
        }
        Cont::Path { phase, mk } => match phase {
            PathPhase::Coerce(stage) => match coerce_for_read(args, 0, stage, incoming)? {
                PathReady::Run(y) => Ok(y),
                PathReady::Realise(p, context) => {
                    *phase = PathPhase::Realising(p);
                    Ok(Yield::Need(NeedPath::Realise(context)))
                }
                PathReady::Ready(p) => {
                    *phase = PathPhase::Asked;
                    Ok(Yield::Need(mk(p)))
                }
            },
            PathPhase::Realising(path) => {
                let p = apply_rewrites(Rc::clone(path), incoming)?;
                *phase = PathPhase::Asked;
                Ok(Yield::Need(mk(p)))
            }
            PathPhase::Asked => incoming
                .map(Yield::Done)
                .ok_or_else(|| VmError::eval("internal: path answer lost")),
        },
        Cont::ToPath { stage } => {
            let v = path_arg(args, 0, *stage, incoming)?;
            // Read off whatever value this stage is looking at, exactly as
            // [`coerce_for_read`] does and for the same reason: a set arrives
            // here a second time as the string its `__toString` or `outPath`
            // produced, and that string carries the accumulated context.
            let context = crate::value2::context_of(&v);
            match coerce_to_path(&v, stage)? {
                Coerced::Run(y) => Ok(y),
                // cppnix's `coerceToPath` ends in `rootPath`, whose
                // `CanonPath` collapses `.`, `..` and doubled slashes, so
                // `builtins.toPath "/a/./b//c/../d"` is `"/a/b/d"` -- string
                // inputs are canonicalized, not just path values.
                Coerced::Done(p) => Ok(Yield::Done(Value::Str(NixStr::with_context(
                    crate::value2::normalize_path(p.as_ref()).into_bytes(),
                    context,
                )))),
            }
        }
        Cont::Import { stage } => {
            if let ImportStage::Path(ps) = stage {
                return match coerce_for_read(args, 0, ps, incoming)? {
                    PathReady::Run(y) => Ok(y),
                    PathReady::Realise(p, context) => {
                        *stage = ImportStage::Realising(p);
                        Ok(Yield::Need(NeedPath::Realise(context)))
                    }
                    PathReady::Ready(p) => {
                        *stage = ImportStage::Answer;
                        Ok(Yield::Need(NeedPath::Import(p)))
                    }
                };
            }
            if let ImportStage::Realising(path) = stage {
                let p = apply_rewrites(Rc::clone(path), incoming)?;
                *stage = ImportStage::Answer;
                return Ok(Yield::Need(NeedPath::Import(p)));
            }
            if let ImportStage::Value(key) = stage {
                let value = incoming.ok_or_else(|| VmError::eval("internal: import value lost"))?;
                if let Some(key) = key.take() {
                    vm.complete_import(&key, &value);
                }
                return Ok(Yield::Done(value));
            }
            if let ImportStage::Derivation(argument) = stage {
                let argument = argument.clone();
                let wrapper = incoming.ok_or_else(|| VmError::eval("internal: import wrapper lost"))?;
                *stage = ImportStage::Value(None);
                return Ok(Yield::Apply(wrapper, argument));
            }
            let answer = incoming.ok_or_else(|| VmError::eval("internal: import answer lost"))?;
            let m = want_attrs(&answer)?;
            if let Some(argument) = m.get(&vm.intern("derivation")) {
                *stage = ImportStage::Derivation(argument.clone());
                return Ok(Yield::Force(crate::imported_drv::wrapper(vm)?));
            }
            let key = |vm: &mut Vm, name: &str| -> Result<String> {
                let sym = vm.intern(name);
                let slot = m
                    .get(&sym)
                    .ok_or_else(|| VmError::eval("internal: malformed import answer"))?;
                // Both halves are text by construction: the host resolved the
                // path, and non-UTF-8 source is already `NonUtf8Source`.
                want_text(&forced(slot)?)
            };
            let path = key(vm, "path")?;
            let root =
                crate::value2::Root::from_wire_name(&key(vm, "root")?).map_err(VmError::eval)?;
            let text = key(vm, "text")?;
            let path = crate::value2::PathValue::new(root, path);
            // The imported file's own directory is what its relative paths
            // resolve against, and it is the RESOLVED path's parent: a
            // directory import reads default.nix, so using the argument here
            // would resolve one level too high.
            let base = match path.rfind('/') {
                Some(0) => "/".to_owned(),
                Some(i) => path.get(..i).unwrap_or("/").to_owned(),
                None => ".".to_owned(),
            };
            let (module, key) = match vm.import_entry(&path, &text, &base)? {
                crate::vm::ImportEntry::Cached(value) => return Ok(Yield::Done(value)),
                crate::vm::ImportEntry::Evaluate { module, key } => (module, key),
            };
            *stage = ImportStage::Value(key);
            crate::perf::note_import_entry_force();
            let entry = module.entry;
            Ok(Yield::Force(Slot::thunk(
                module,
                entry,
                Rc::new(crate::value2::EnvNode::Root),
            )))
        }
        Cont::TryEval { started } => {
            if !*started {
                *started = true;
                return Ok(Yield::Force(nth(args, 0)?));
            }
            let v = incoming.ok_or_else(|| VmError::eval("internal: tryEval lost its value"))?;
            Ok(Yield::Done(try_eval_result(vm, true, v)))
        }
    }
}

pub fn try_eval_result(vm: &mut Vm, success: bool, value: Value) -> Value {
    let mut out = BTreeMap::new();
    let s = vm.intern("success");
    out.insert(s, Slot::value(Value::Bool(success)));
    let v = vm.intern("value");
    out.insert(v, Slot::value(value));
    Value::Attrs(Rc::new(Attrs::new(out)))
}

// -- shared helpers ---------------------------------------------------------

pub fn arg(args: &[Slot], i: usize) -> Result<&Slot> {
    args.get(i)
        .ok_or_else(|| VmError::eval("internal: missing builtin argument"))
}

/// The already-forced value of argument `i`.
pub fn argv(args: &[Slot], i: usize) -> Result<Value> {
    forced(arg(args, i)?)
}

fn nth(items: &[Slot], i: usize) -> Result<Slot> {
    items
        .get(i)
        .cloned()
        .ok_or_else(|| VmError::eval("internal: list position lost"))
}

pub(crate) fn want_list(v: &Value) -> Result<Rc<Vec<Slot>>> {
    match v {
        Value::List(l) => Ok(l.clone()),
        other => Err(VmError::eval(format!(
            "expected a list but found {}",
            type_name(other)
        ))),
    }
}

pub(crate) fn want_attrs(v: &Value) -> Result<Rc<crate::value2::Attrs>> {
    match v {
        Value::Attrs(m) => Ok(m.clone()),
        other => Err(VmError::eval(format!(
            "expected a set but found {}",
            type_name(other)
        ))),
    }
}

pub(crate) fn want_int(v: &Value) -> Result<i64> {
    match v {
        Value::Int(n) => Ok(*n),
        other => Err(VmError::eval(format!(
            "expected an integer but found {}",
            type_name(other)
        ))),
    }
}

/// The string's bytes, with no opinion about its context.
///
/// Only correct where the context genuinely does not matter: the result is not
/// a string (`stringLength`), or the caller carries the context itself. A
/// builtin whose result IS a string and which uses this is dropping a
/// dependency silently, which is the ENG-12447 failure mode; use
/// [`want_nix_str`] or [`want_bytes_no_ctx`] instead.
pub(crate) fn want_bytes(v: &Value) -> Result<Rc<[u8]>> {
    Ok(want_nix_str(v)?.bytes_rc())
}

/// The string with its context, for a builtin that propagates.
pub(crate) fn want_nix_str(v: &Value) -> Result<&NixStr> {
    match v {
        Value::Str(s) => Ok(s),
        other => Err(VmError::eval(format!(
            "expected a string but found {}",
            type_name(other)
        ))),
    }
}

/// The bytes of a string that is not allowed to carry a context, which is
/// cppnix's `forceStringNoCtx` (`eval.cc:2541`) and its wording.
///
/// This is not fussiness: an attribute name or a hash input that referred to a
/// store path would be a dependency the result cannot record, so cppnix
/// refuses rather than losing it, and a backend that quietly accepted one
/// would disagree with it on a program cppnix rejects.
pub(crate) fn want_bytes_no_ctx(v: &Value) -> Result<Rc<[u8]>> {
    let s = want_nix_str(v)?;
    refuse_context(s)?;
    Ok(s.bytes_rc())
}

/// The string as text, for the boundaries that are genuinely text-only in
/// this backend: an attribute name headed for the `str`-keyed interner, a
/// path or URL headed for the host, an algorithm name. cppnix accepts
/// arbitrary bytes at every one of them, so a non-UTF-8 string here is a
/// named coverage gap ([`RefusalToken::NonUtf8Boundary`]) and never an
/// evaluation error the cpp backend would not raise.
pub(crate) fn want_text(v: &Value) -> Result<String> {
    Ok(text_of(want_nix_str(v)?)?.to_owned())
}

/// [`want_text`] under cppnix's `forceStringNoCtx`. The context refusal comes
/// first because cppnix raises it first, and because it is an evaluation
/// error where the UTF-8 refusal is only a coverage gap.
pub(crate) fn want_text_no_ctx(v: &Value) -> Result<String> {
    let s = want_nix_str(v)?;
    refuse_context(s)?;
    Ok(text_of(s)?.to_owned())
}

/// The text-only boundary itself, for a caller that already holds the
/// string. See [`want_text`] for why this is a refusal and not an error.
pub(crate) fn text_of(s: &NixStr) -> Result<&str> {
    text_of_bytes(s.bytes())
}

/// [`text_of`] for bytes that never got wrapped in a [`NixStr`], e.g. the
/// concatenation feeding a path.
pub(crate) fn text_of_bytes(b: &[u8]) -> Result<&str> {
    std::str::from_utf8(b).map_err(|_| {
        VmError::Unimplemented(crate::refusal::Refusal::new(
            crate::refusal::RefusalToken::NonUtf8Boundary,
            format!(
                "a non-UTF-8 string ('{}') at a text-only boundary of this backend",
                String::from_utf8_lossy(b)
            ),
        ))
    })
}

/// The refusal half of [`want_bytes_no_ctx`], for a caller that already has the
/// string and only needs the check -- the two places the VM turns a value into
/// an attribute name. Split out so cppnix's wording has one definition; a
/// second copy of this message is a second thing to keep in step.
///
/// cppnix reports the first element of the set (`eval.cc:2544` takes
/// `*v.context()->begin()`), and this set is ordered, so the two name the same
/// one.
pub(crate) fn refuse_context(s: &NixStr) -> Result<()> {
    if let Some(first) = s.context().and_then(|c| c.iter().next()) {
        return Err(VmError::eval(format!(
            "the string '{}' is not allowed to refer to a store path (such as '{}')",
            s.lossy(),
            first.display()
        )));
    }
    Ok(())
}

pub(crate) fn want_bool(v: &Value) -> Result<bool> {
    match v {
        Value::Bool(b) => Ok(*b),
        other => Err(VmError::eval(format!(
            "expected a Boolean but found {}",
            type_name(other)
        ))),
    }
}

// -- list builtins ----------------------------------------------------------

pub fn bi_length(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Ok(Value::Int(want_list(&argv(args, 0)?)?.len() as i64))
}

pub fn bi_head(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let l = want_list(&argv(args, 0)?)?;
    match l.first() {
        Some(s) => Ok(Begin::Force(s.clone())),
        None => Err(VmError::eval("list index 0 is out of bounds")),
    }
}

pub fn bi_tail(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let l = want_list(&argv(args, 0)?)?;
    if l.is_empty() {
        return Err(VmError::eval("'tail' called on an empty list"));
    }
    Ok(Value::list(l.iter().skip(1).cloned().collect()))
}

pub fn bi_elem_at(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let l = want_list(&argv(args, 0)?)?;
    let i = want_int(&argv(args, 1)?)?;
    let s = usize::try_from(i)
        .ok()
        .and_then(|i| l.get(i))
        .ok_or_else(|| VmError::eval(format!("list index {i} is out of bounds")))?;
    Ok(Begin::Force(s.clone()))
}

pub fn bi_elem(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::Elem {
        x: arg(args, 0)?.clone(),
        items,
        i: 0,
    }))
}

/// cppnix builds each element with mkApp, so `map f xs` applies nothing
/// until an element is forced; the corpus catches the difference through
/// `mapAttrs throw`, and the same rule governs map and genList.
pub fn bi_map(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = arg(args, 0)?.clone();
    let items = want_list(&argv(args, 1)?)?;
    let out: Vec<Slot> = items
        .iter()
        .map(|s| Slot::pending(f.clone(), vec![s.clone()]))
        .collect();
    Ok(Begin::Done(Value::list(out)))
}

pub fn bi_filter(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::Filter {
        f,
        items,
        i: 0,
        out: Vec::new(),
    }))
}

pub fn bi_any(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    any_all(args, true)
}

pub fn bi_all(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    any_all(args, false)
}

fn any_all(args: &[Slot], want: bool) -> Result<Begin> {
    let f = argv(args, 0)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::AnyAll {
        f,
        items,
        i: 0,
        want,
    }))
}

pub fn bi_concat_lists(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let items = want_list(&argv(args, 0)?)?;
    Ok(Begin::Cont(Cont::ForceEach {
        items,
        i: 0,
        vals: Vec::new(),
        finish: finish_concat_lists,
    }))
}

fn finish_concat_lists(_vm: &mut Vm, vals: &[Value], _args: &[Slot]) -> Result<Value> {
    let mut out = Vec::new();
    for v in vals {
        out.extend(want_list(v)?.iter().cloned());
    }
    Ok(Value::list(out))
}

pub fn bi_concat_map(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::ConcatMap {
        f,
        items,
        i: 0,
        out: Vec::new(),
    }))
}

pub fn bi_gen_list(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = arg(args, 0)?.clone();
    let n = want_int(&argv(args, 1)?)?;
    let n =
        usize::try_from(n).map_err(|_| VmError::eval(format!("cannot create list of size {n}")))?;
    let out: Vec<Slot> = (0..n)
        .map(|i| Slot::pending(f.clone(), vec![Slot::value(Value::Int(i as i64))]))
        .collect();
    Ok(Begin::Done(Value::list(out)))
}

/// cppnix's foldl' is strict in the accumulator it PRODUCES, not in the one
/// it is given: `foldl' op (throw "x") [ … ]` is fine as long as op ignores it.
pub fn bi_foldl_strict(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let acc = arg(args, 1)?.clone();
    let items = want_list(&argv(args, 2)?)?;
    Ok(Begin::Cont(Cont::Foldl {
        f,
        items,
        i: 0,
        acc,
        half: None,
        finishing: false,
    }))
}

pub fn bi_sort(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let l = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::Sort {
        f,
        items: l.iter().cloned().collect(),
        next: 0,
        sorted: Vec::with_capacity(l.len()),
        probe: 0,
        half: None,
    }))
}

// -- genericClosure ---------------------------------------------------------

/// Where a `genericClosure` run is: which forced value the machine is about
/// to hand back.
enum Stage {
    StartSet,
    Operator,
    /// The element popped off the work queue.
    Elem,
    /// That element's `key` attribute.
    Key,
    /// What the operator returned.
    OpResult,
    /// One element of the operator's result. cppnix forces each before
    /// queueing it, so a throw there surfaces during the closure walk and
    /// not later when the element is inspected.
    OpElem,
}

/// A key already emitted, reduced to what cppnix's `CompareValues` looks at.
/// `Other` keeps the type name so a comparison against it can raise the
/// message cppnix raises, and `List` is a key shape cppnix compares
/// element-wise -- a walk that would have to run through the machine, so it
/// is reported rather than approximated.
enum Key {
    Int(i64),
    Float(f64),
    Str(Vec<u8>),
    Path(String),
    List,
    Other(&'static str),
}

impl Key {
    fn of(v: &Value) -> Key {
        match v {
            Value::Int(n) => Key::Int(*n),
            Value::Float(x) => Key::Float(*x),
            Value::Str(s) => Key::Str(s.bytes().to_vec()),
            Value::Path(p) => Key::Path(p.to_string()),
            Value::List(_) => Key::List,
            other => Key::Other(type_name(other)),
        }
    }

    fn type_name(&self) -> &'static str {
        match self {
            Key::Int(_) => "an integer",
            Key::Float(_) => "a float",
            Key::Str(_) => "a string",
            Key::Path(_) => "a path",
            Key::List => "a list",
            Key::Other(t) => t,
        }
    }

    /// cppnix's `CompareValues`: int and float compare across the two, every
    /// other pair of differing types is an error, and a same-typed pair its
    /// switch has no case for ("values of that type are incomparable").
    fn cmp_nix(&self, other: &Key) -> Result<Ordering> {
        let cmp = |a: f64, b: f64| a.partial_cmp(&b).unwrap_or(Ordering::Equal);
        match (self, other) {
            (Key::Int(a), Key::Int(b)) => Ok(a.cmp(b)),
            (Key::Float(a), Key::Float(b)) => Ok(cmp(*a, *b)),
            (Key::Int(a), Key::Float(b)) => Ok(cmp(*a as f64, *b)),
            (Key::Float(a), Key::Int(b)) => Ok(cmp(*a, *b as f64)),
            (Key::Str(a), Key::Str(b)) => Ok(a.cmp(b)),
            (Key::Path(a), Key::Path(b)) => Ok(a.cmp(b)),
            (Key::List, _) | (_, Key::List) => Err(VmError::Unimplemented(Refusal::new(
                RefusalToken::UnorderedComparison,
                "builtins.genericClosure with list keys",
            ))),
            (a, b) if a.type_name() == b.type_name() => Err(VmError::eval(format!(
                "cannot compare {0} with {0}; values of that type are incomparable",
                a.type_name()
            ))),
            (a, b) => Err(VmError::eval(format!(
                "cannot compare {} with {}",
                a.type_name(),
                b.type_name()
            ))),
        }
    }
}

pub struct Generic {
    /// The `startSet` slot, held until the driver's first step: a `Begin::Cont`
    /// arrives with nothing forced yet, so the first thing this does is ask
    /// for it.
    start: Option<Slot>,
    stage: Stage,
    work: VecDeque<Slot>,
    res: Vec<Slot>,
    /// Emitted keys in `CompareValues` order. A sorted `Vec` searched by
    /// hand rather than a `BTreeMap`, because the comparison is fallible and
    /// `Ord` is not: cppnix gets the same behavior from `std::map`, whose
    /// comparator throws out of the insert.
    keys: Vec<Key>,
    op: Option<Value>,
    /// The element being processed, kept because its `key` is forced between
    /// popping it and emitting it.
    cur: Option<Slot>,
    /// The operator's result, forced element by element before queueing.
    pending: Vec<Slot>,
    pi: usize,
}

/// cppnix reads `startSet` and `operator` out of the one attrset argument,
/// and an empty `startSet` returns before `operator` is looked at all.
pub fn bi_generic_closure(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let attrs = want_attrs(&argv(args, 0)?)?;
    let sym = vm.intern("startSet");
    let start = attrs
        .get(&sym)
        .cloned()
        .ok_or_else(|| VmError::eval("attribute 'startSet' missing"))?;
    Ok(Begin::Cont(Cont::Generic(Generic {
        start: Some(start),
        stage: Stage::StartSet,
        work: VecDeque::new(),
        res: Vec::new(),
        keys: Vec::new(),
        op: None,
        cur: None,
        pending: Vec::new(),
        pi: 0,
    })))
}

impl Generic {
    fn step(&mut self, vm: &mut Vm, args: &[Slot], incoming: Option<Value>) -> Result<Yield> {
        let Some(v) = incoming else {
            // The driver's first step into a fresh continuation, before
            // anything has been forced.
            let s = self
                .start
                .take()
                .ok_or_else(|| VmError::eval("internal: genericClosure restarted"))?;
            return Ok(Yield::Force(s));
        };
        match self.stage {
            Stage::StartSet => {
                let items = want_list(&v)?;
                // cppnix hands the startSet value straight back when it is
                // empty, which is why a missing `operator` is not an error
                // for an empty closure.
                if items.is_empty() {
                    return Ok(Yield::Done(v));
                }
                self.work.extend(items.iter().cloned());
                let attrs = want_attrs(&argv(args, 0)?)?;
                let sym = vm.intern("operator");
                let op = attrs
                    .get(&sym)
                    .cloned()
                    .ok_or_else(|| VmError::eval("attribute 'operator' missing"))?;
                self.stage = Stage::Operator;
                Ok(Yield::Force(op))
            }
            Stage::Operator => {
                if !matches!(v, Value::Closure(_) | Value::Builtin(_)) {
                    return Err(VmError::eval(format!(
                        "expected a function but found {}",
                        type_name(&v)
                    )));
                }
                self.op = Some(v);
                self.next_element()
            }
            Stage::Elem => {
                let attrs = want_attrs(&v)?;
                let sym = vm.intern("key");
                let key = attrs
                    .get(&sym)
                    .cloned()
                    .ok_or_else(|| VmError::eval("attribute 'key' missing"))?;
                self.stage = Stage::Key;
                Ok(Yield::Force(key))
            }
            Stage::Key => {
                if !insert_key(&mut self.keys, Key::of(&v))? {
                    // Already closed over: cppnix skips the element without
                    // calling the operator on it.
                    return self.next_element();
                }
                let cur = self
                    .cur
                    .clone()
                    .ok_or_else(|| VmError::eval("internal: genericClosure lost its element"))?;
                self.res.push(cur.clone());
                let op = self
                    .op
                    .clone()
                    .ok_or_else(|| VmError::eval("internal: genericClosure lost its operator"))?;
                self.stage = Stage::OpResult;
                Ok(Yield::Apply(op, cur))
            }
            Stage::OpResult => {
                self.pending = want_list(&v)?.iter().cloned().collect();
                self.pi = 0;
                self.force_pending()
            }
            Stage::OpElem => {
                self.pi += 1;
                self.force_pending()
            }
        }
    }

    fn next_element(&mut self) -> Result<Yield> {
        match self.work.pop_front() {
            Some(s) => {
                self.cur = Some(s.clone());
                self.stage = Stage::Elem;
                Ok(Yield::Force(s))
            }
            None => Ok(Yield::Done(Value::list(std::mem::take(&mut self.res)))),
        }
    }

    fn force_pending(&mut self) -> Result<Yield> {
        match self.pending.get(self.pi).cloned() {
            Some(s) => {
                self.stage = Stage::OpElem;
                Ok(Yield::Force(s))
            }
            None => {
                self.work.extend(self.pending.drain(..));
                self.next_element()
            }
        }
    }
}

/// Insert into the sorted key list, reporting whether the key was new.
/// `false` means an equal key is already closed over.
fn insert_key(keys: &mut Vec<Key>, k: Key) -> Result<bool> {
    let mut lo = 0usize;
    let mut hi = keys.len();
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let existing = keys
            .get(mid)
            .ok_or_else(|| VmError::eval("internal: genericClosure key index lost"))?;
        match k.cmp_nix(existing)? {
            Ordering::Less => hi = mid,
            Ordering::Greater => lo = mid + 1,
            Ordering::Equal => return Ok(false),
        }
    }
    keys.insert(lo, k);
    Ok(true)
}

// -- attrset builtins -------------------------------------------------------

pub fn bi_attr_names(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let m = want_attrs(&argv(args, 0)?)?;
    let mut names: Vec<String> = m.keys().map(|k| vm.sym_name(*k).to_owned()).collect();
    names.sort();
    Ok(Value::list(
        names
            .into_iter()
            .map(|n| Slot::value(Value::Str(n.into())))
            .collect(),
    ))
}

pub fn bi_attr_values(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let m = want_attrs(&argv(args, 0)?)?;
    let mut entries: Vec<(String, Slot)> = m
        .iter()
        .map(|(k, s)| (vm.sym_name(*k).to_owned(), s.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(Value::list(entries.into_iter().map(|(_, s)| s).collect()))
}

pub fn bi_get_attr(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let name = want_text_no_ctx(&argv(args, 0)?)?;
    let m = want_attrs(&argv(args, 1)?)?;
    let sym = vm.intern(&name);
    match m.get(&sym) {
        Some(s) => Ok(Begin::Force(s.clone())),
        None => Err(VmError::eval(format!("attribute '{name}' missing"))),
    }
}

pub fn bi_has_attr(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let name = want_text_no_ctx(&argv(args, 0)?)?;
    let m = want_attrs(&argv(args, 1)?)?;
    let sym = vm.intern(&name);
    Ok(Value::Bool(m.contains_key(&sym)))
}

pub fn bi_remove_attrs(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    want_attrs(&argv(args, 0)?)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::ForceEach {
        items,
        i: 0,
        vals: Vec::new(),
        finish: finish_remove_attrs,
    }))
}

fn finish_remove_attrs(vm: &mut Vm, vals: &[Value], args: &[Slot]) -> Result<Value> {
    let m = want_attrs(&argv(args, 0)?)?;
    let mut gone: Vec<Sym> = Vec::with_capacity(vals.len());
    for v in vals {
        let name = want_text_no_ctx(v)?;
        gone.push(vm.intern(&name));
    }
    gone.sort_unstable();
    gone.dedup();
    // `retain` visits names in ascending order and `gone` is sorted, so one
    // cursor over the removal list walks both in a single pass.
    let mut gone = gone.iter().peekable();
    let mut out = (*m).clone();
    out.retain(|k, _| {
        while gone.next_if(|g| *g < k).is_some() {}
        gone.next_if_eq(&k).is_none()
    });
    // The origin travels with the clone, and that is right rather than
    // merely convenient: every attribute still in `out` came from the set
    // this was derived from, so the position it reports is that attribute's
    // own. cppnix keeps them too -- `removeAttrs` copies the `Attr`s it
    // keeps, position included.
    Ok(Value::Attrs(Rc::new(out)))
}

pub fn bi_intersect_attrs(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let a = want_attrs(&argv(args, 0)?)?;
    let b = want_attrs(&argv(args, 1)?)?;
    // Both maps iterate in ascending `Sym` order, so a two-pointer merge
    // finds the intersection in one pass over each -- no tree lookup per
    // key, and the output is born sorted, which is exactly the input
    // `Attrs::from_sorted_iter` asks for.
    let mut out: Vec<(Sym, Slot)> = Vec::new();
    let mut ai = a.keys();
    let mut bi = b.iter();
    let (mut an, mut bn) = (ai.next(), bi.next());
    while let (Some(&ak), Some((&bk, bv))) = (an, bn) {
        match ak.cmp(&bk) {
            std::cmp::Ordering::Less => an = ai.next(),
            std::cmp::Ordering::Greater => bn = bi.next(),
            std::cmp::Ordering::Equal => {
                out.push((bk, bv.clone()));
                an = ai.next();
                bn = bi.next();
            }
        }
    }
    // Every value here is b's, so every position is b's, exactly as in
    // cppnix -- which copies b's `Attr`s, position included.
    let mut result = crate::value2::Attrs::from_sorted_iter(out);
    result.origin = b.origin.clone();
    Ok(Value::Attrs(Rc::new(result)))
}

pub fn bi_cat_attrs(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    // Type- and context-check the name before forcing the list, so the
    // refusal does not depend on how far the list gets.
    want_text_no_ctx(&argv(args, 0)?)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::ForceEach {
        items,
        i: 0,
        vals: Vec::new(),
        finish: finish_cat_attrs,
    }))
}

fn finish_cat_attrs(vm: &mut Vm, vals: &[Value], args: &[Slot]) -> Result<Value> {
    let name = want_text_no_ctx(&argv(args, 0)?)?;
    let sym = vm.intern(&name);
    let mut out = Vec::new();
    for v in vals {
        if let Some(s) = want_attrs(v)?.get(&sym) {
            out.push(s.clone());
        }
    }
    Ok(Value::list(out))
}

pub fn bi_list_to_attrs(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let items = want_list(&argv(args, 0)?)?;
    let name_sym = vm.intern("name");
    let value_sym = vm.intern("value");
    Ok(Begin::Cont(Cont::ListToAttrs {
        items,
        i: 0,
        out: BTreeMap::new(),
        origins: Vec::new(),
        cur: None,
        name_sym,
        value_sym,
    }))
}

pub fn bi_map_attrs(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    // Taken unforced on purpose: `prim_mapAttrs` forces the set and never the
    // function, and `builtins::TABLE` therefore declares no strict position
    // for it. Reading it with `argv` here would reinstate the force the
    // table just dropped (ENG-13124).
    let f = arg(args, 0)?.clone();
    let m = want_attrs(&argv(args, 1)?)?;
    // `m` iterates in `Sym` order, so the result is born sorted.
    let mut out = Vec::with_capacity(m.len());
    for (k, s) in m.iter() {
        let name = Slot::value(Value::Str(vm.sym_name(*k).into()));
        out.push((*k, Slot::pending(f.clone(), vec![name, s.clone()])));
    }
    Ok(Begin::Done(Value::Attrs(Rc::new(Attrs::new(
        AttrMap::from_sorted(out),
    )))))
}

pub fn bi_group_by(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::GroupBy {
        f,
        items,
        i: 0,
        out: BTreeMap::new(),
    }))
}

pub fn bi_partition(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::Partition {
        f,
        items,
        i: 0,
        right: Vec::new(),
        wrong: Vec::new(),
    }))
}

// -- zipAttrsWith -----------------------------------------------------------

/// Every attrset in the list contributes its value to that name's list, in
/// list order. cppnix builds each result entry as an unapplied `f name vals`
/// so an entry nobody reads never calls `f`, which is what makes
/// `zipAttrsWith (n: v: throw n)` on an unread attribute succeed.
pub fn bi_zip_attrs_with(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let f = argv(args, 0)?;
    if !matches!(f, Value::Closure(_) | Value::Builtin(_)) {
        return Err(VmError::eval(format!(
            "expected a function but found {}",
            type_name(&f)
        )));
    }
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Cont(Cont::ForceEach {
        items,
        i: 0,
        vals: Vec::new(),
        finish: zip_finish,
    }))
}

fn zip_finish(vm: &mut Vm, vals: &[Value], args: &[Slot]) -> Result<Value> {
    let f = arg(args, 0)?.clone();
    let mut seen: BTreeMap<Sym, Vec<Slot>> = BTreeMap::new();
    for v in vals {
        for (k, s) in want_attrs(v)?.iter() {
            seen.entry(*k).or_default().push(s.clone());
        }
    }
    // `seen` iterates in `Sym` order, so the result is born sorted.
    let mut out: Vec<(Sym, Slot)> = Vec::with_capacity(seen.len());
    for (sym, items) in seen {
        let name: Rc<str> = vm.sym_name(sym).into();
        out.push((
            sym,
            Slot::pending(
                f.clone(),
                vec![
                    Slot::value(Value::Str(name.into())),
                    Slot::value(Value::list(items)),
                ],
            ),
        ));
    }
    Ok(Value::Attrs(Rc::new(Attrs::new(AttrMap::from_sorted(out)))))
}

// -- unsafeGetAttrPos -------------------------------------------------------

/// `builtins.unsafeGetAttrPos`, arity 2: where an attribute was defined.
///
/// cppnix's shape exactly (`primops.cc`, `prim_unsafeGetAttrPos`): `null`
/// when the set has no such attribute, `null` when it has one with no
/// recorded position (`mkPos` of `noPos`), and otherwise
/// `{ file; line; column; }` with `file` a *string* and the other two
/// integers.
///
/// The set has to know where it came from, which is [`AttrOrigin`]: a set
/// written as a literal does, and a set built by a builtin does not. What
/// each case answers is spelled out on `AttrOrigin` itself. Sets assembled
/// from several origins carry a flat per-name projection, and a name with no
/// selected position stays absent from that projection. This never reports a
/// position belonging to a different attribute.
///
/// The forcing is cppnix's and is where the type errors come from: the name
/// goes through `forceStringNoCtx` and the set through `forceAttrs`, so
/// `builtins.unsafeGetAttrPos "x" 42` is a type error rather than a quiet
/// `null`.
pub fn bi_unsafe_get_attr_pos(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let name = want_text_no_ctx(&argv(args, 0)?)?;
    let attrs = want_attrs(&argv(args, 1)?)?;
    let sym = vm.intern(&name);
    // Absent is `null`, and it is checked before the position: a set can list
    // a name in its site and no longer hold it (`removeAttrs`), and cppnix
    // answers for the attribute rather than for the source text.
    if !attrs.contains_key(&sym) {
        return Ok(Value::Null);
    }
    let Some(origin) = &attrs.origin else {
        return Ok(Value::Null);
    };
    let Some(position) = vm.attr_origin_position(origin, &name, sym) else {
        return Ok(Value::Null);
    };
    let Some((line, column)) = position.module.line_col(position.offset) else {
        return Ok(Value::Null);
    };
    // `null` for text with no file behind it, which is not a shortcut: cppnix
    // builds the record only for a `SourcePath` origin (`eval.cc`'s `mkPos`),
    // so `nix-instantiate --eval -E 'builtins.unsafeGetAttrPos "a" { a = 1; }'`
    // answers `null` on both arms. Verified against the system nix, 2026-08-06.
    let crate::ir::SrcOrigin::File(file) = &position.module.origin else {
        return Ok(Value::Null);
    };
    // cppnix's `file` is a string and not a path, which is visible: it prints
    // unquoted as a path would not, and `builtins.typeOf` says "string".
    let file = file.clone();
    let mut out = BTreeMap::new();
    out.insert(vm.intern("file"), Slot::value(Value::Str(file.into())));
    out.insert(vm.intern("line"), Slot::value(Value::Int(i64::from(line))));
    out.insert(
        vm.intern("column"),
        Slot::value(Value::Int(i64::from(column))),
    );
    Ok(Value::Attrs(Rc::new(Attrs::new(out))))
}

// -- string builtins --------------------------------------------------------

/// Bytes, as cppnix's is: `NixStringContext context; auto s = ...; s->size()`
/// counts bytes, so `stringLength "ä"` is 2.
pub fn bi_string_length(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Ok(Value::Int(want_bytes(&argv(args, 0)?)?.len() as i64))
}

/// Byte offsets, as cppnix's `std::string::substr` is: slicing the middle of
/// a UTF-8 codepoint hands back the raw half, not an empty string -- which is
/// one of the two ways this evaluator mints a non-UTF-8 string out of pure
/// UTF-8 input (the other is `readFile` of a binary).
pub fn bi_substring(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let start = want_int(&argv(args, 0)?)?;
    let len = want_int(&argv(args, 1)?)?;
    let subject = argv(args, 2)?;
    let subject = want_nix_str(&subject)?;
    let s = subject.bytes();
    if start < 0 {
        return Err(VmError::eval("negative start position in 'substring'"));
    }
    let start = start as usize;
    let out: Vec<u8> = if start >= s.len() {
        Vec::new()
    } else if len < 0 {
        s.get(start..).unwrap_or(b"").to_vec()
    } else {
        let end = start.saturating_add(len as usize).min(s.len());
        s.get(start..end).unwrap_or(b"").to_vec()
    };
    // A substring keeps the whole string's context, cppnix's `mkString(...,
    // context)`. `builtins.substring 0 0 s` is the idiom for carrying a
    // dependency with none of the bytes, and it only works because of this.
    Ok(Value::Str(NixStr::with_context(out, subject.context_set())))
}

/// cppnix takes the separator with `forceString` and every element with
/// `coerceToString`, so the separator has to be a string while an element may
/// be anything that coerces -- a path, or a set with `__toString` or
/// `outPath`. The set case is why this cannot be a pure fold over forced
/// values: coercing a set applies `__toString`, which is a call, so the whole
/// join has to be able to suspend. `concatStringsSep " " [ pkgA pkgB ]` is how
/// a package list becomes a shell word list, and it is everywhere in nixpkgs
/// (ENG-12628).
pub fn bi_concat_strings_sep(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let sep_arg = argv(args, 0)?;
    let sep = want_nix_str(&sep_arg)?;
    let items = want_list(&argv(args, 1)?)?;
    Ok(Begin::Sub(Task::coerce_joining(&items, sep)))
}

pub fn bi_replace_strings(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let from = want_list(&argv(args, 0)?)?;
    let to = want_list(&argv(args, 1)?)?;
    want_bytes(&argv(args, 2)?)?;
    if from.len() != to.len() {
        return Err(VmError::eval(
            "'from' and 'to' arguments to 'replaceStrings' have different lengths",
        ));
    }
    Ok(Begin::Cont(Cont::Replace {
        stage: 0,
        froms: Vec::with_capacity(from.len()),
        plan: Vec::new(),
        used: Vec::new(),
        k: 0,
        tos: BTreeMap::new(),
        context: BTreeSet::new(),
    }))
}

/// Byte-wise, because cppnix's `prim_replaceStrings` is: an empty `from`
/// emits ONE BYTE of the subject and advances one byte (`res += s[p]; p++`),
/// so `replaceStrings [""] ["-"] "é"` interleaves the dash between the two
/// bytes of `é`, and a `from` may match half a codepoint.
fn build_plan(froms: &[Vec<u8>], s: &[u8]) -> Vec<Piece> {
    let mut plan = Vec::new();
    let mut lit: Vec<u8> = Vec::new();
    let mut i = 0usize;
    while i <= s.len() {
        let mut matched = false;
        for (j, f) in froms.iter().enumerate() {
            let hit = if f.is_empty() {
                true
            } else {
                s.get(i..)
                    .map(|rest| rest.starts_with(f.as_slice()))
                    .unwrap_or(false)
            };
            if !hit {
                continue;
            }
            if !lit.is_empty() {
                plan.push(Piece::Lit(std::mem::take(&mut lit)));
            }
            plan.push(Piece::Use(j));
            if f.is_empty() {
                // Empty match: emit one source byte and advance.
                if let Some(&b) = s.get(i) {
                    lit.push(b);
                }
                i += 1;
            } else {
                i += f.len();
            }
            matched = true;
            break;
        }
        if !matched {
            if let Some(&b) = s.get(i) {
                lit.push(b);
                i += 1;
            } else {
                break;
            }
        }
    }
    if !lit.is_empty() {
        plan.push(Piece::Lit(lit));
    }
    plan
}

fn distinct_uses(plan: &[Piece]) -> Vec<usize> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for p in plan {
        if let Piece::Use(j) = p
            && seen.insert(*j)
        {
            out.push(*j);
        }
    }
    out
}

pub fn bi_split_version(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let s = want_bytes_no_ctx(&argv(args, 0)?)?;
    Ok(Value::list(
        version_parts(&s)
            .into_iter()
            .map(|p| Slot::value(Value::Str(p.into())))
            .collect(),
    ))
}

// -- hashString -------------------------------------------------------------

const HEX: &[u8; 16] = b"0123456789abcdef";

fn hex_of(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        for nibble in [b >> 4, b & 0x0f] {
            if let Some(c) = HEX.get(usize::from(nibble)) {
                out.push(char::from(*c));
            }
        }
    }
    out
}

/// The five algorithms cppnix's `parseHashAlgo` accepts, rendered base-16
/// without the `sha256:` prefix (`to_string(HashFormat::Base16, false)`).
pub fn bi_hash_string(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    // Both arguments are forceStringNoCtx in cppnix: a hash of a string that
    // referred to a store path would be a dependency the integer result
    // cannot record, so the refusal is the correct answer, not a limitation.
    let algo = want_text_no_ctx(&argv(args, 0)?)?;
    // The subject is BYTES: cppnix hashes `s->data()` whatever it holds, so
    // `hashString "sha256" (substring 0 1 "ä")` digests the lone 0xC3.
    let s = want_bytes_no_ctx(&argv(args, 1)?)?;
    let hex = hash_hex(parse_algo_name(&algo, vm.settings().blake3_hashes)?, &s);
    Ok(Value::Str(hex.as_str().into()))
}

/// The algorithm argument of `hashString` and `hashFile`, both of which
/// reach cppnix's `parseHashAlgo`: an unknown name errors with the full
/// "expect ..." wording, and the experimentally-gated `blake3` is a refusal
/// because cppnix with the feature on answers fine (the same split
/// `drvstrict::hash_parse_error` makes).
pub(crate) fn parse_algo_name(algo: &str, blake3_hashes: bool) -> Result<crate::nixhash::HashAlgo> {
    match crate::nixhash::parse_algo(algo).and_then(|parsed| parsed.require_enabled(blake3_hashes))
    {
        Ok(a) => Ok(a),
        Err(e @ crate::nixhash::HashError::Unsupported(_)) => {
            Err(VmError::Unimplemented(crate::refusal::Refusal::new(
                crate::refusal::RefusalToken::UnimplementedBuiltin,
                e.to_string(),
            )))
        }
        Err(e) => Err(VmError::eval(e.to_string())),
    }
}

/// `hashString(algo, bytes).to_string(Base16, false)`: the rendering both
/// hash builtins share.
pub(crate) fn hash_hex(algo: crate::nixhash::HashAlgo, bytes: &[u8]) -> String {
    use crate::nixhash::HashAlgo;
    use sha2::Digest;
    match algo {
        // cppnix switches to a TBB update at 128,000 bytes. This stays on
        // Blake3's single-threaded convenience call: the digest is identical,
        // and a threading dependency is not justified for a performance-only
        // difference at this evaluator boundary.
        HashAlgo::Blake3 => hex_of(blake3::hash(bytes).as_bytes()),
        HashAlgo::Md5 => hex_of(&md5::Md5::digest(bytes)),
        HashAlgo::Sha1 => hex_of(&sha1::Sha1::digest(bytes)),
        HashAlgo::Sha256 => hex_of(&sha2::Sha256::digest(bytes)),
        HashAlgo::Sha512 => hex_of(&sha2::Sha512::digest(bytes)),
    }
}

// -- match / split ----------------------------------------------------------

/// cppnix builds every pattern with `std::regex::extended`: POSIX ERE, where
/// `[[:space:]]` and friends are bracket classes and there are no
/// lookarounds, lazy quantifiers or backreferences to lose.
///
/// `(?s)` is the one syntax adjustment: in POSIX a period matches every
/// character, newline included, while this crate excludes newline unless
/// asked. eval-okay-regex-match2 catches the difference with
/// `^.*CONFIG_BOARD_DIRECTORY="([a-zA-Z0-9_]+)".*$` over a multi-line
/// subject, which cppnix matches and a newline-excluding period does not.
///
/// The remaining dialect gap is that POSIX picks the longest alternative
/// where this crate picks the leftmost one; no corpus pattern, including
/// the ~200 nixpkgs-derived ones in eval-okay-regex-match2, distinguishes
/// them.
///
/// A **byte** regex with Unicode off, because cppnix's is: `std::regex` over
/// `char` reads one byte per position, so `.` and `[^x]` match single bytes,
/// a non-ASCII literal is its UTF-8 bytes in sequence, and the subject may be
/// any byte string at all. The Unicode-aware `regex::Regex` this used to be
/// matched `builtins.match "." "ä"` where cppnix answers null (two bytes are
/// not one), and could not be handed a non-UTF-8 subject at all. Non-ASCII
/// pattern bytes are rewritten to `\xHH` by [`posix_brackets`] so the
/// translated pattern is pure ASCII and means the same byte sequence in both
/// dialects, inside and outside bracket expressions.
///
/// Served from the VM's regex table ([`Vm::regex`]) so a pattern is compiled
/// once per evaluation however many subjects it is matched against.
fn compile_re(vm: &mut Vm, re: &[u8]) -> Result<regex::bytes::Regex> {
    vm.regex(re, |re| {
        let lossy = || String::from_utf8_lossy(re).into_owned();
        regex::bytes::RegexBuilder::new(&format!("(?s){}", posix_brackets(re)))
            .unicode(false)
            .build()
            .map_err(|_| VmError::eval(format!("invalid regular expression '{}'", lossy())))
    })
}

/// Rewrite POSIX bracket expressions into this crate's class syntax.
///
/// Inside `[...]`, POSIX and the `regex` crate disagree about who is special:
///
/// * `\` is a literal member in POSIX and an escape here, so a class like
///   `[^$\[:space:]]` closes at a different `]` under each reading -- under
///   this crate's, `\[` is an escaped bracket, `:space:` is seven literal
///   characters, the class closes early and a stray `]+` is left over. That
///   exact pattern validates every shell abbreviation in ix, and the raw
///   crate reading failed all 293 of them where cppnix passes all (the
///   whole-ix sweep's six `dev-compute-*` toplevels, ENG-13140 follow-up).
/// * a bare `[` is a literal member in POSIX and opens a nested class here;
/// * `&` and `~` are literal in POSIX and halves of the `&&`/`~~` set
///   operators here.
///
/// So within a bracket expression this escapes `\`, `[`, `&` and `~`, keeps
/// the three POSIX bracket sequences (`[:class:]`, `[=equiv=]`,
/// `[.collate.]`) intact since both dialects read those, and honours the two
/// POSIX positions where `]` is a member rather than the terminator (first,
/// and first after `^`). Everything outside a bracket expression is copied
/// unchanged: there the two dialects agree on everything the corpus has
/// exercised, and a rewrite of agreeing syntax would be pure risk.
///
/// `-` is deliberately NOT escaped: it spells ranges in both dialects, and
/// escaping it would turn `[a-z]` into three characters. The cost is that
/// `--` between two members reads as set difference here and as a literal
/// plus a range bound in POSIX; no corpus pattern spells that, and a pattern
/// that does fails loudly at compile rather than matching differently.
///
/// Byte-wise, and the output is pure ASCII: every pattern byte outside ASCII
/// is rewritten to `\xHH`, which in a Unicode-off bytes regex means exactly
/// that one byte -- the same single position POSIX's byte-char reading gives
/// it, inside a bracket expression ([é] is a class of the TWO bytes 0xC3
/// 0xA9 to cppnix, and becomes `[\xC3\xA9]` here) and outside one (a literal
/// é is the two-byte sequence in both). This is also what lets the pattern
/// itself be a non-UTF-8 byte string, which cppnix accepts.
fn posix_brackets(re: &[u8]) -> String {
    fn push_byte(out: &mut String, b: u8) {
        if b.is_ascii() {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("\\x{b:02X}"));
        }
    }
    let mut out = String::with_capacity(re.len() + 8);
    let mut i = 0;
    while let Some(&c) = re.get(i) {
        if c == b'\\'
            && let Some(&next) = re.get(i + 1)
        {
            // An escape outside a bracket expression means the same
            // thing to both dialects: copy the pair, and do not let the
            // next `[` open a bracket expression.
            if next.is_ascii() {
                out.push('\\');
                out.push(char::from(next));
            } else {
                // POSIX reads backslash-ordinary as the ordinary character,
                // and `\` before an `\xHH` escape would escape the `x`
                // instead; the byte goes in alone.
                push_byte(&mut out, next);
            }
            i += 2;
            continue;
        }
        if c != b'[' {
            push_byte(&mut out, c);
            i += 1;
            continue;
        }
        out.push('[');
        i += 1;
        if re.get(i) == Some(&b'^') {
            out.push('^');
            i += 1;
        }
        if re.get(i) == Some(&b']') {
            // A `]` in first position is a member, not the terminator.
            out.push_str("\\]");
            i += 1;
        }
        while let Some(&m) = re.get(i) {
            if m == b']' {
                break;
            }
            if m == b'['
                && let Some(&delim @ (b':' | b'=' | b'.')) = re.get(i + 1)
            {
                // Find the closing `<delim>]`; absent one, POSIX reads a
                // literal `[`, so fall through to the escape below.
                let mut j = i + 2;
                let mut end = None;
                while let (Some(&a), Some(&b)) = (re.get(j), re.get(j + 1)) {
                    if a == delim && b == b']' {
                        end = Some(j + 2);
                        break;
                    }
                    j += 1;
                }
                if let Some(end) = end {
                    for &b in re.get(i..end).unwrap_or(&[]) {
                        push_byte(&mut out, b);
                    }
                    i = end;
                    continue;
                }
            }
            match m {
                b'\\' => out.push_str("\\\\"),
                b'[' => out.push_str("\\["),
                b'&' => out.push_str("\\&"),
                b'~' => out.push_str("\\~"),
                other => push_byte(&mut out, other),
            }
            i += 1;
        }
        if re.get(i) == Some(&b']') {
            out.push(']');
            i += 1;
        }
    }
    out
}

fn group_list(caps: &regex::bytes::Captures<'_>) -> Value {
    // Group 0 is the whole match, which cppnix drops: "the first match is
    // the whole string". An unmatched group is null, not the empty string.
    let items: Vec<Slot> = (1..caps.len())
        .map(|i| {
            Slot::value(match caps.get(i) {
                Some(m) => Value::Str(m.as_bytes().into()),
                None => Value::Null,
            })
        })
        .collect();
    Value::list(items)
}

/// `std::regex_match`: the pattern must cover the whole subject, so the
/// pattern is anchored rather than searched for.
pub fn bi_match(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    // The pattern is forceStringNoCtx; the subject is not, and the captures
    // cppnix returns carry no context either.
    let re = want_bytes_no_ctx(&argv(args, 0)?)?;
    let s = want_bytes(&argv(args, 1)?)?;
    let mut anchored = Vec::with_capacity(re.len() + 8);
    anchored.extend_from_slice(br"\A(?:");
    anchored.extend_from_slice(&re);
    anchored.extend_from_slice(br")\z");
    // Re-mapped so the message quotes the pattern as written, not the
    // anchored wrapper around it.
    let compiled = compile_re(vm, &anchored).map_err(|_| {
        VmError::eval(format!(
            "invalid regular expression '{}'",
            String::from_utf8_lossy(&re)
        ))
    })?;
    match compiled.captures(&s) {
        Some(caps) => Ok(group_list(&caps)),
        None => Ok(Value::Null),
    }
}

/// Non-matching runs interleaved with one group list per match, starting and
/// ending with a (possibly empty) run: `2 * matches + 1` elements. A pattern
/// that matches nothing hands the subject back as a one-element list.
pub fn bi_split(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let re = want_bytes_no_ctx(&argv(args, 0)?)?;
    let s = want_bytes(&argv(args, 1)?)?;
    let rx = compile_re(vm, &re)?;
    let mut out: Vec<Slot> = Vec::new();
    let mut last = 0usize;
    for caps in rx.captures_iter(&s) {
        let Some(whole) = caps.get(0) else { continue };
        let prefix = s
            .get(last..whole.start())
            .ok_or_else(|| VmError::eval("internal: split lost its place"))?;
        out.push(Slot::value(Value::Str(prefix.into())));
        out.push(Slot::value(group_list(&caps)));
        last = whole.end();
    }
    if out.is_empty() {
        return Ok(Value::list(vec![Slot::value(Value::Str(NixStr::from(
            Rc::clone(&s),
        )))]));
    }
    let suffix = s
        .get(last..)
        .ok_or_else(|| VmError::eval("internal: split lost its place"))?;
    out.push(Slot::value(Value::Str(suffix.into())));
    Ok(Value::list(out))
}

// -- placeholder ------------------------------------------------------------

/// `builtins.placeholder`, arity 1. Pure, and the whole of it is
/// [`crate::drvpath::hash_placeholder`]; `forceStringNoCtx` because cppnix
/// does (`primops.cc:1981`), so a context here is refused rather than
/// silently dropped into a string that goes on to name an output.
pub fn bi_placeholder(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let name = want_bytes_no_ctx(&argv(args, 0)?)?;
    Ok(Value::Str(crate::drvpath::hash_placeholder(&name).into()))
}

// -- string contexts --------------------------------------------------------

/// `builtins.hasContext`: whether the string depends on anything.
pub fn bi_has_context(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let arg0 = argv(args, 0)?;
    Ok(Value::Bool(want_nix_str(&arg0)?.has_context()))
}

/// `builtins.getContext`: the dependencies, as the attrset cppnix builds in
/// `primops/context.cc`.
///
/// One entry per store path, so the three element kinds collapse onto the same
/// key: `path` for an opaque reference, `allOutputs` for a whole derivation,
/// and an `outputs` list for individual outputs. A path can carry more than
/// one of those at once, which is why they are accumulated rather than
/// matched exclusively.
pub fn bi_get_context(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    use crate::value2::ContextElem;
    #[derive(Default)]
    struct Info {
        path: bool,
        all_outputs: bool,
        outputs: Vec<String>,
    }

    let arg0 = argv(args, 0)?;
    let s = want_nix_str(&arg0)?;
    let mut infos: BTreeMap<String, Info> = BTreeMap::new();
    if let Some(context) = s.context() {
        for elem in context.iter() {
            match elem {
                ContextElem::Opaque(p) => infos.entry(p.to_string()).or_default().path = true,
                ContextElem::DrvDeep(p) => {
                    infos.entry(p.to_string()).or_default().all_outputs = true;
                }
                ContextElem::Built { drv, output } => infos
                    .entry(drv.to_string())
                    .or_default()
                    .outputs
                    .push(output.to_string()),
            }
        }
    }

    let mut out: BTreeMap<Sym, Slot> = BTreeMap::new();
    for (path, info) in infos {
        let mut entry: BTreeMap<Sym, Slot> = BTreeMap::new();
        if info.path {
            let k = vm.intern("path");
            entry.insert(k, Slot::value(Value::Bool(true)));
        }
        if info.all_outputs {
            let k = vm.intern("allOutputs");
            entry.insert(k, Slot::value(Value::Bool(true)));
        }
        if !info.outputs.is_empty() {
            // Sorted because the context is a set and its iteration order is
            // this crate's business, not the program's; cppnix's own order
            // comes out sorted too, since it walks a std::set of parsed
            // elements.
            let mut names = info.outputs;
            names.sort();
            let k = vm.intern("outputs");
            entry.insert(
                k,
                Slot::value(Value::list(
                    names
                        .into_iter()
                        .map(|n| Slot::value(Value::Str(n.into())))
                        .collect(),
                )),
            );
        }
        let k = vm.intern(&path);
        out.insert(
            k,
            Slot::value(Value::Attrs(std::rc::Rc::new(Attrs::new(entry)))),
        );
    }
    Ok(Value::Attrs(std::rc::Rc::new(Attrs::new(out))))
}

// -- data formats ----------------------------------------------------------

// `fromJSON` and `fromTOML` are the two decoders in the language, and both
// are pure: the text arrives as an argument and nothing is read to parse it.

pub fn bi_from_json(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    // Bytes: cppnix hands nlohmann the raw string, and both parsers treat a
    // JSON document that is not UTF-8 as a parse error, not a type error.
    let src = want_bytes_no_ctx(&argv(args, 0)?)?;
    let doc: serde_json::Value = serde_json::from_slice(&src)
        .map_err(|e| VmError::eval(format!("while decoding a JSON string: {e}")))?;
    json_to_value(vm, &doc)
}

/// Recursive over the parsed document rather than over Nix values: the depth
/// is whatever serde_json already accepted, so this adds no reach the parser
/// did not already have.
///
/// Shared with the `FetchTree` answer, which arrives as JSON, so that a tree's
/// attributes and `builtins.fromJSON` agree about how a number or a nested
/// object becomes a Nix value. One reader, not two.
pub(crate) fn json_to_value(vm: &mut Vm, j: &serde_json::Value) -> Result<Value> {
    Ok(match j {
        serde_json::Value::Null => Value::Null,
        serde_json::Value::Bool(b) => Value::Bool(*b),
        serde_json::Value::Number(n) => json_number(n)?,
        serde_json::Value::String(s) => {
            crate::vm::check_no_nul(s)?;
            Value::Str(s.as_str().into())
        }
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(Slot::value(json_to_value(vm, it)?));
            }
            Value::list(out)
        }
        serde_json::Value::Object(map) => {
            let mut out = BTreeMap::new();
            for (k, v) in map {
                crate::vm::check_no_nul(k)?;
                let sym = vm.intern(k);
                out.insert(sym, Slot::value(json_to_value(vm, v)?));
            }
            Value::Attrs(Rc::new(Attrs::new(out)))
        }
    })
}

/// cppnix keeps JSON's int/float distinction: `1` is a Nix integer and `1.0`
/// a Nix float. A JSON integer past `i64` is refused rather than silently
/// widened to a float.
fn json_number(n: &serde_json::Number) -> Result<Value> {
    if let Some(i) = n.as_i64() {
        return Ok(Value::Int(i));
    }
    if let Some(u) = n.as_u64() {
        return Err(VmError::eval(format!(
            "unsigned json number {u} outside of Nix integer range"
        )));
    }
    match n.as_f64() {
        Some(f) => Ok(Value::Float(f)),
        None => Err(VmError::eval(format!(
            "json number {n} outside of Nix integer range"
        ))),
    }
}

// -- fromTOML ---------------------------------------------------------------

pub fn bi_from_toml(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let src = want_text_no_ctx(&argv(args, 0)?)?;
    let doc: toml::Value = toml::from_str(&src)
        .map_err(|e| VmError::eval(format!("while parsing TOML: {}", e.message())))?;
    toml_to_value(vm, &doc)
}

fn toml_to_value(vm: &mut Vm, t: &toml::Value) -> Result<Value> {
    Ok(match t {
        toml::Value::Boolean(b) => Value::Bool(*b),
        toml::Value::Integer(i) => Value::Int(*i),
        toml::Value::Float(f) => Value::Float(*f),
        toml::Value::String(s) => {
            // cppnix runs its NUL check inside the parse, so the message
            // arrives wrapped: "while parsing TOML: error: input string ...".
            wrap_toml(crate::vm::check_no_nul(s))?;
            Value::Str(s.as_str().into())
        }
        toml::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for it in items {
                out.push(Slot::value(toml_to_value(vm, it)?));
            }
            Value::list(out)
        }
        toml::Value::Table(map) => {
            let mut out = BTreeMap::new();
            for (k, v) in map {
                wrap_toml(crate::vm::check_no_nul(k))?;
                let sym = vm.intern(k);
                out.insert(sym, Slot::value(toml_to_value(vm, v)?));
            }
            Value::Attrs(Rc::new(Attrs::new(out)))
        }
        // cppnix only turns a TOML date/time into `{ _type = "timestamp"; }`
        // under the `parse-toml-timestamps` experimental feature, and
        // refuses it otherwise -- from inside the parse visitor, which is
        // why the refusal arrives wrapped like a parse error
        // (`primops.cc`, `prim_fromTOML`).
        toml::Value::Datetime(d) => {
            if !vm.settings().parse_toml_timestamps {
                return Err(VmError::eval(
                    "while parsing TOML: Dates and times are not supported",
                ));
            }
            let mut out = BTreeMap::new();
            out.insert(
                vm.intern("_type"),
                Slot::value(Value::Str("timestamp".into())),
            );
            out.insert(
                vm.intern("value"),
                Slot::value(Value::Str(toml_datetime_text(d)?.as_str().into())),
            );
            Value::Attrs(Rc::new(Attrs::new(out)))
        }
    })
}

/// The string cppnix puts in a timestamp set's `value`: toml11's
/// `operator<<` over the parsed datetime, which *normalizes* rather than
/// echoing the source (`primops.cc` streams the value into an
/// `ostringstream`). Measured against `eval-okay-fromTOML-timestamps.exp`
/// and directly on this repo's cppnix:
///
/// - the date/time delimiter is always `T`, whatever the source used;
/// - fractional seconds are padded to 3, 6 or 9 digits (`.1` -> `.100`,
///   `.1234` -> `.123400`, `.1234567` -> `.123456700`), and `.0` -- zero
///   nanoseconds -- is omitted entirely;
/// - a zero offset prints `Z` however it was spelled: `+00:00` and `-00:00`
///   both came back `Z` when measured, and a nonzero one keeps its sign and
///   minutes (`+05:45`).
///
/// Fallible for one seam between the two parsers: the `toml` crate tracks
/// TOML 1.1, where seconds are optional, and cppnix's toml11 tracks 1.0,
/// where `07:32` is not a time at all. Handing a value back for text cppnix
/// refuses would be a value-versus-error divergence, so a missing `second`
/// errors here instead. cppnix's own line depends on where its lexer gives
/// up -- measured `[error] bad time: must be HH:MM:SS.subsec` for
/// `1979-05-27T07:32` but `[error] bad integer: invalid digit after an
/// integer` for a bare `07:32` -- and this mirrors the first, because it is
/// the one that names the actual problem; mimicking a lexer's stumble from a
/// successfully parsed value would be fiction.
fn toml_datetime_text(d: &toml::value::Datetime) -> Result<String> {
    use std::fmt::Write as _;
    let mut out = String::new();
    if let Some(date) = &d.date {
        let _ = write!(out, "{:04}-{:02}-{:02}", date.year, date.month, date.day);
    }
    if let Some(time) = &d.time {
        if d.date.is_some() {
            out.push('T');
        }
        let second = time.second.ok_or_else(|| {
            VmError::eval("while parsing TOML: [error] bad time: must be HH:MM:SS.subsec")
        })?;
        let _ = write!(out, "{:02}:{:02}:{:02}", time.hour, time.minute, second);
        let ns = time.nanosecond.unwrap_or(0);
        if ns != 0 {
            let (digits, div) = if ns % 1_000 != 0 {
                (9, 1)
            } else if ns % 1_000_000 != 0 {
                (6, 1_000)
            } else {
                (3, 1_000_000)
            };
            let _ = write!(out, ".{:0width$}", ns / div, width = digits);
        }
    }
    if let Some(offset) = &d.offset {
        match offset {
            toml::value::Offset::Z => out.push('Z'),
            toml::value::Offset::Custom { minutes: 0 } => out.push('Z'),
            toml::value::Offset::Custom { minutes } => {
                let sign = if *minutes < 0 { '-' } else { '+' };
                let m = minutes.unsigned_abs();
                let _ = write!(out, "{}{:02}:{:02}", sign, m / 60, m % 60);
            }
        }
    }
    Ok(out)
}

fn wrap_toml(r: Result<()>) -> Result<()> {
    match r {
        Err(VmError::Throw(c)) => Err(VmError::eval(format!(
            "while parsing TOML: error: {}",
            c.message
        ))),
        other => other,
    }
}

// -- evaluation control -----------------------------------------------------

pub fn bi_seq(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    argv(args, 0)?;
    argv(args, 1)
}

/// `builtins.toXML` (`primops.cc:2620`), which is `printValueAsXML` with
/// `strict` on and `location` off.
///
/// # Why this is here and `toJSON` is not
///
/// They run the same walk on the same driver ([`crate::deepwalk`]), and they
/// are on opposite sides of the module boundary because the boundary is the
/// purity split and they really do differ there. `toJSON` copies a path into
/// the store on its way past, so it asks the embedder a question and lands in
/// a read set. `toXML` writes a path verbatim -- `v.path().to_string()` at
/// `value-to-xml.cc:89`, no copy -- so it reaches nothing outside its
/// argument and belongs on this side.
///
/// `each_primop_lives_on_its_own_side_of_the_boundary` is what said so: this
/// was first written next to `bi_to_json` and the guard rejected it by name.
pub fn bi_to_xml(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let root = arg(args, 0)?.clone();
    Ok(Begin::Cont(Cont::Ext(crate::primops_host::Ext::DeepWalk(
        Box::new(crate::deepwalk::DeepWalk::new(
            root,
            Box::new(crate::deepwalk::Xml::new()),
        )),
    ))))
}

pub fn bi_deep_seq(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Cont(Cont::DeepSeq {
        work: vec![(arg(args, 0)?.clone(), 0)],
        seen: BTreeSet::new(),
        tail: false,
        depth: 0,
    }))
}

pub fn bi_try_eval(_vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Cont(Cont::TryEval { started: false }))
}

// -- addErrorContext --------------------------------------------------------

/// `builtins.addErrorContext`, arity 2: force the second argument and hand it
/// back, and on failure decorate the error with the first.
///
/// The machine forces argument 1 and leaves argument 0 alone, which is
/// cppnix's own laziness: `prim_addErrorContext` (`primops.cc:1060`) coerces
/// the message only inside the `catch`, so a `throw` in an unused context
/// message never fires on the success path.
///
/// **The decoration is not implemented, and that is a tier-2 divergence
/// rather than a gap being hidden.** cppnix calls `e.addTrace(...)` and
/// rethrows, which adds one line to the trace and changes neither the error's
/// class nor the value of any expression that succeeds. This IR carries no
/// source positions and builds no traces at all (ENG-12137), so there is
/// nothing here to add a line to; the error propagates unchanged and with its
/// class intact, which is the bar CLAUDE.md sets for presentation. When
/// ENG-12137 gives the IR positions this is the second place to revisit,
/// after the printer.
///
/// Said plainly, because "presentation" undersells it and the shadow census
/// now has the number: the context string is evaluated and thrown away, so a
/// program that annotates its failures gets none of them back. ENG-12714 is
/// the same hole seen from the user's side -- `--show-trace` under this
/// backend produces a single line with no position, no call site and no file
/// -- and it is why the ix fleet eval gate cannot name a failing host. This
/// builtin is not the fix for that and cannot be: there is no frame chain for
/// it to write into. It is one of the places that starts working when there
/// is one.
///
/// What would *not* be acceptable is dropping the force: `addErrorContext`
/// is transparent to the value, so returning the argument unforced would make
/// `builtins.seq (builtins.addErrorContext "x" (throw "y")) 1` differ.
pub fn bi_add_error_context(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Done(argv(args, 1)?))
}

// -- paths ------------------------------------------------------------------

/// One step of cppnix's `EvalState::coerceToPath`.
pub(crate) enum Coerced {
    /// The argument is an absolute path, and this is it.
    Done(Rc<crate::value2::PathValue>),
    /// The machine has to run this first; the value it produces arrives at
    /// [`PathStage::Coerced`].
    Run(Yield),
}

/// The value being coerced: the argument itself on the way in, and whatever
/// the sub-task produced on the way back.
fn path_arg(args: &[Slot], at: usize, stage: PathStage, incoming: Option<Value>) -> Result<Value> {
    match stage {
        PathStage::Value => argv(args, at),
        PathStage::Coerced => {
            incoming.ok_or_else(|| VmError::eval("internal: path coercion lost its value"))
        }
    }
}

/// cppnix's `EvalState::coerceToPath` (`eval.cc`, `coerceToPath`), which
/// every builtin taking a path argument runs on it by way of `realisePath`
/// (`primops.cc`). Three cases in that order:
///
/// 1. a path value is returned as it is, with no coercion and no store copy;
/// 2. a set carrying `__toString` has it applied and the *result* coerced
///    again, so `__toString` may return a path, or another set;
/// 3. everything else goes through `coerceToString` with `coerceMore` and
///    `copyToStore` both off -- which is how a set without `__toString`
///    reaches `outPath` -- and what comes out has to be absolute.
///
/// Cases 1 and 2 collapse into the same two arms here because this crate's
/// [`Coerce`](crate::print::Coerce) already implements 2 and 3 together with
/// those flags, and coercing a path *to a string* under them yields the same
/// bytes cppnix returns from case 1.
///
/// The failing arm says "to a string" and not "to a path" on purpose: cppnix
/// has no path-specific message here, because the refusal is raised inside
/// `coerceToString`. `nix-instantiate --eval --expr 'builtins.pathExists 3'`
/// reports `cannot coerce an integer to a string: 3`.
///
/// **The context this coercion sees is not realised here.** cppnix realises
/// it in `realisePath`, the caller, immediately after this returns
/// (`primops.cc:176`). This crate splits it the same way: [`coerce_for_read`]
/// is the caller, and it is what turns a non-empty context into a
/// [`NeedPath::Realise`]. Keeping the two apart matters because `builtins.path`
/// coerces with this and then applies a *different* rule -- cppnix's `addPath`
/// realises only when the path is already in the store (`primops.cc:2947`).
pub(crate) fn coerce_to_path(v: &Value, stage: &mut PathStage) -> Result<Coerced> {
    // cppnix returns a path value directly (case 1), before any coercion, so
    // this arm holds whatever the stage is.
    if let Value::Path(p) = v {
        return Ok(Coerced::Done(Rc::clone(p)));
    }
    if let (Value::Attrs(_), PathStage::Value) = (v, *stage) {
        *stage = PathStage::Coerced;
        return Ok(Coerced::Run(Yield::sub(Task::coerce_to_path(Slot::value(
            v.clone(),
        )))));
    }
    let s = match v {
        // Paths are text in this backend (`Value::Path` holds `Rc<str>`), so
        // a byte string headed for one is a boundary refusal, not a repair.
        Value::Str(s) => text_of(s)?.to_owned(),
        other => {
            return Err(VmError::eval(format!(
                "cannot coerce {} to a string",
                type_name(other)
            )));
        }
    };
    if !s.starts_with('/') {
        return Err(VmError::eval(format!(
            "string '{s}' doesn't represent an absolute path"
        )));
    }
    // `CanonPath(path)` in cppnix's `coerceToPath`: the text is normalised
    // lexically, so `"/x/Cargo.toml/../."` is the path `/x`. Handing the
    // spelling on unnormalised would make an embedder's accessor refuse it
    // as non-canonical where cppnix reads it (the one home-configuration
    // divergence of 2026-09-01).
    Ok(Coerced::Done(Rc::new(
        crate::value2::PathValue::normalized(crate::value2::Root::Ambient, &s),
    )))
}

/// What coercing a path-family argument produced: cppnix's `coerceToPath`
/// followed by the context test its one caller, `realisePath`, makes on the
/// result (`primops.cc:176`).
pub(crate) enum PathReady {
    /// The machine has to run something -- a `__toString` call -- first.
    Run(Yield),
    /// An absolute path, and a context cppnix realises before using it. The
    /// elements are in `BTreeSet` order, which is what
    /// [`NeedPath::Realise`] promises.
    Realise(
        Rc<crate::value2::PathValue>,
        Vec<crate::value2::ContextElem>,
    ),
    /// An absolute path with nothing to realise, which is every path read in
    /// an evaluation that never touched a store.
    Ready(Rc<crate::value2::PathValue>),
}

/// [`coerce_to_path`] plus cppnix's "is there a context to realise" test, for
/// the two continuations that read a path.
///
/// One function rather than the same six lines in each, because the two would
/// otherwise be free to disagree about *when* a context is realised -- and
/// `import` disagreeing with `readFile` about that is a difference nothing in
/// the corpus would catch until a derivation output was imported.
pub(crate) fn coerce_for_read(
    args: &[Slot],
    at: usize,
    stage: &mut PathStage,
    incoming: Option<Value>,
) -> Result<PathReady> {
    let (coerced, context) = coerce_path_with_context(args, at, stage, incoming)?;
    Ok(match coerced {
        Coerced::Run(y) => PathReady::Run(y),
        Coerced::Done(p) if context.is_empty() => PathReady::Ready(p),
        Coerced::Done(p) => PathReady::Realise(p, context.into_iter().collect()),
    })
}

/// Coerce one path-family argument while retaining the context accumulated by
/// the value that supplied its spelling.
///
/// Most path consumers pass the result to [`coerce_for_read`], which realises
/// the context before reading. `builtins.storePath` is the deliberate
/// exception: cppnix validates the store object without realising the input
/// context, then copies that context onto its result. Keeping coercion and
/// context capture here makes those two policies share the mechanism without
/// making either policy a special case inside the other.
pub(crate) fn coerce_path_with_context(
    args: &[Slot],
    at: usize,
    stage: &mut PathStage,
    incoming: Option<Value>,
) -> Result<(Coerced, BTreeSet<ContextElem>)> {
    let v = path_arg(args, at, *stage, incoming)?;
    // Read before the coercion runs, and off whatever value this stage is
    // looking at. A string carries its own context; a set arrives here a
    // second time as the string its `__toString` or `outPath` produced, which
    // carries the accumulated one; a path value carries none, which is right,
    // because a path literal depends on nothing.
    let context = crate::value2::context_of(&v);
    Ok((coerce_to_path(&v, stage)?, context))
}

/// cppnix's `rewriteStrings(path, rewrites)` over the answer to a
/// [`NeedPath::Realise`]: the flat `[from, to, from, to, ...]` list
/// `answer_path` builds out of the embedder's rewrite map.
///
/// Empty for every input-addressed derivation, so this is usually the
/// identity. It is not skipped on that account: under `ca-derivations` the
/// path is a downstream placeholder and reading it unrewritten is reading a
/// path that never exists.
pub(crate) fn apply_rewrites(
    path: Rc<crate::value2::PathValue>,
    answer: Option<Value>,
) -> Result<Rc<crate::value2::PathValue>> {
    let out = rewrite_spelling(path.to_string(), answer)?;
    Ok(Rc::new(path.with_path(out)))
}

/// The string half of [`apply_rewrites`]: cppnix's `rewriteStrings` over one
/// spelling. `findFile` entries are spellings and not path values, which is
/// why this is a function of its own rather than a body inside the other.
pub(crate) fn rewrite_spelling(mut out: String, answer: Option<Value>) -> Result<String> {
    let list =
        want_list(&answer.ok_or_else(|| VmError::eval("internal: realised context answer lost"))?)?;
    if list.len() % 2 != 0 {
        return Err(VmError::eval(
            "internal: realised context has a half rewrite",
        ));
    }
    for pair in list.chunks_exact(2) {
        let (Some(from), Some(to)) = (pair.first(), pair.get(1)) else {
            return Err(VmError::eval(
                "internal: realised context has a half rewrite",
            ));
        };
        let from = want_text(&forced(from)?)?;
        let to = want_text(&forced(to)?)?;
        // cppnix's `rewriteStrings` replaces every occurrence, not the first,
        // and skips an empty needle rather than looping forever
        // (`util.cc`, `rewriteStrings`).
        if !from.is_empty() {
            out = out.replace(&from, &to);
        }
    }
    Ok(out)
}

/// Coerce `slot` with a primop's own `coerceToString` flags and hand the
/// string to `finish`. For the body of a primop that cannot let the driver
/// replace its argument; see [`Cont::CoerceBody`].
pub(crate) fn coerce_in_body(
    slot: Slot,
    flags: crate::print::CoerceFlags,
    finish: Finish,
) -> Result<Begin> {
    Ok(Begin::Cont(Cont::CoerceBody {
        slot,
        flags,
        started: false,
        finish,
    }))
}

pub(crate) fn ask(mk: fn(Rc<crate::value2::PathValue>) -> NeedPath) -> Result<Begin> {
    Ok(Begin::Cont(Cont::Path {
        phase: PathPhase::Coerce(PathStage::Value),
        mk,
    }))
}

/// `builtins.toPath`: coerce the argument to an absolute path and hand it
/// back *as a string* -- cppnix's `prim_toPath` is "Convert the argument to
/// a path and then to a string (confusing, eh?)" (`primops.cc`), deprecated
/// in the manual but with no runtime warning, so none is mirrored here. Not
/// the [`ask`] family: nothing is read, so the context on the result is
/// accumulated, never realised. See [`Cont::ToPath`] for the shape of the
/// answer.
pub fn bi_to_path(_vm: &mut Vm, _args: &[Slot]) -> Result<Begin> {
    Ok(Begin::Cont(Cont::ToPath {
        stage: PathStage::Value,
    }))
}

pub fn bi_function_args(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let f = argv(args, 0)?;
    match &f {
        Value::Closure(c) => {
            let unit = c
                .module
                .units
                .get(c.unit as usize)
                .ok_or_else(|| VmError::eval("internal: bad closure unit"))?;
            let mut out = BTreeMap::new();
            if let Some(crate::ir::Param::Formals { fields, .. }) = &unit.param {
                for formal in fields {
                    let name = c
                        .module
                        .symbols
                        .get(formal.sym as usize)
                        .cloned()
                        .unwrap_or_default();
                    let g = vm.intern(&name);
                    out.insert(g, Slot::value(Value::Bool(formal.default.is_some())));
                }
            }
            // cppnix builds this set with each formal's own position
            // (`primops.cc`, prim_functionArgs), so `unsafeGetAttrPos "x"
            // (functionArgs f)` names where `x` was declared. The site is on
            // the `Param` and not on any instruction, which is what
            // `AttrOrigin::FORMALS` stands for.
            Ok(Value::Attrs(Rc::new(crate::value2::Attrs::at(
                out,
                vm.static_attr_origin(&c.module, c.unit, crate::value2::AttrOrigin::FORMALS),
            ))))
        }
        Value::Builtin(_) => Ok(Value::Attrs(Rc::new(Attrs::default()))),
        other => Err(VmError::eval(format!(
            "'functionArgs' requires a function, got {}",
            type_name(other)
        ))),
    }
}

// -- type tests and arithmetic ----------------------------------------------

pub fn bi_type_of(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let t = match argv(args, 0)? {
        Value::Int(_) => "int",
        Value::Float(_) => "float",
        Value::Bool(_) => "bool",
        Value::Null => "null",
        Value::Str(_) => "string",
        Value::Path(_) => "path",
        Value::List(_) => "list",
        Value::Attrs(_) => "set",
        Value::Closure(_) | Value::Builtin(_) => "lambda",
    };
    Ok(Value::Str(t.into()))
}

macro_rules! type_test {
    ($name:ident, $pat:pat) => {
        pub fn $name(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
            Ok(Value::Bool(matches!(argv(args, 0)?, $pat)))
        }
    };
}

type_test!(bi_is_int, Value::Int(_));
type_test!(bi_is_float, Value::Float(_));
type_test!(bi_is_bool, Value::Bool(_));
type_test!(bi_is_string, Value::Str(_));
type_test!(bi_is_path, Value::Path(_));
type_test!(bi_is_list, Value::List(_));
type_test!(bi_is_attrs, Value::Attrs(_));
type_test!(bi_is_function, Value::Closure(_) | Value::Builtin(_));

pub fn bi_add(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    num_op(vm, args, i64::checked_add, |a, b| a + b)
}

pub fn bi_sub(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    num_op(vm, args, i64::checked_sub, |a, b| a - b)
}

pub fn bi_mul(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    num_op(vm, args, i64::checked_mul, |a, b| a * b)
}

pub fn bi_div(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let a = argv(args, 0)?;
    let b = argv(args, 1)?;
    if matches!(b, Value::Int(0)) {
        return Err(VmError::eval("division by zero"));
    }
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        return x
            .checked_div(*y)
            .map(Value::Int)
            .ok_or_else(|| VmError::eval("integer overflow"));
    }
    let (x, y) = (as_f64(&a)?, as_f64(&b)?);
    Ok(Value::Float(x / y))
}

fn num_op(
    _vm: &mut Vm,
    args: &[Slot],
    int_op: fn(i64, i64) -> Option<i64>,
    float_op: fn(f64, f64) -> f64,
) -> Result<Value> {
    let a = argv(args, 0)?;
    let b = argv(args, 1)?;
    if let (Value::Int(x), Value::Int(y)) = (&a, &b) {
        return int_op(*x, *y)
            .map(Value::Int)
            .ok_or_else(|| VmError::eval("integer overflow"));
    }
    let (x, y) = (as_f64(&a)?, as_f64(&b)?);
    Ok(Value::Float(float_op(x, y)))
}

fn as_f64(v: &Value) -> Result<f64> {
    match v {
        Value::Int(n) => Ok(*n as f64),
        Value::Float(x) => Ok(*x),
        other => Err(VmError::eval(format!(
            "expected an integer or float but found {}",
            type_name(other)
        ))),
    }
}

/// cppnix's lessThan is the same CompareValues `<` uses, so it orders lists
/// lexicographically too; a scalar-only version fails eval-okay-sort, which
/// sorts a list of lists.
pub fn bi_less_than(_vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let a = argv(args, 0)?;
    let b = argv(args, 1)?;
    Ok(Begin::Sub(Task::compare(a, b, false)))
}

pub fn bi_bit_and(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    bit_op(vm, args, |a, b| a & b)
}

pub fn bi_bit_or(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    bit_op(vm, args, |a, b| a | b)
}

pub fn bi_bit_xor(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    bit_op(vm, args, |a, b| a ^ b)
}

fn bit_op(_vm: &mut Vm, args: &[Slot], op: fn(i64, i64) -> i64) -> Result<Value> {
    let a = want_int(&argv(args, 0)?)?;
    let b = want_int(&argv(args, 1)?)?;
    Ok(Value::Int(op(a, b)))
}

pub fn bi_floor(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Ok(Value::Int(as_f64(&argv(args, 0)?)?.floor() as i64))
}

pub fn bi_ceil(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    Ok(Value::Int(as_f64(&argv(args, 0)?)?.ceil() as i64))
}

pub fn bi_compare_versions(_vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let a = want_bytes_no_ctx(&argv(args, 0)?)?;
    let b = want_bytes_no_ctx(&argv(args, 1)?)?;
    Ok(Value::Int(match compare_versions(&a, &b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }))
}

/// cppnix's `DrvName::DrvName(std::string_view)` (`src/libstore/names.cc:23`),
/// which the comment above it states as: the name is everything up to but not
/// including the first dash NOT followed by a letter, and the version is the
/// rest, excluding that dash. No dash qualifies means the whole string is the
/// name and the version is empty, because `DrvName` seeds `name = fullName`
/// and leaves `version` default-constructed.
///
/// Three consequences of the literal loop that a reading of the prose misses,
/// and each one is a corpus case below:
///
///   * `i + 1 < s.size()` is part of the condition, so a trailing dash is not
///     a separator: `"hello-"` parses as the whole string with no version.
///   * the test is `!isalpha`, not "is a digit", so `"a--1"` splits at the
///     FIRST of the two dashes and the version keeps the second one.
///   * `isalpha` in the C locale is ASCII-only, so a dash before any byte
///     above 0x7f separates. Bytes, not `char`s, for that reason.
///
/// The two attributes go in a `BTreeMap<Sym, _>`, whose keys are interner
/// indices rather than names, so this map's own order is the order the two
/// names happened to be interned in. Nothing observes it: `print`, `attrNames`
/// and `attrValues` all sort by the name string, which is cppnix's order.
pub fn bi_parse_drv_name(vm: &mut Vm, args: &[Slot]) -> Result<Value> {
    let s = want_bytes_no_ctx(&argv(args, 0)?)?;
    let crate::drv_name::DrvName { name, version } = crate::drv_name::split(&s);
    let mut out: BTreeMap<Sym, Slot> = BTreeMap::new();
    let name_key = vm.intern("name");
    out.insert(name_key, Slot::value(Value::Str(name.into())));
    let version_key = vm.intern("version");
    out.insert(version_key, Slot::value(Value::Str(version.into())));
    Ok(Value::Attrs(Rc::new(Attrs::new(out))))
}

/// Split into digit and non-digit runs; '.' and '-' separate.
/// Byte-wise, as cppnix's `parseDrvName`/`splitVersion` machinery is: the
/// separators and the digit test are ASCII, and any other byte -- a UTF-8
/// fragment included -- extends the current non-numeric run.
fn version_parts(s: &[u8]) -> Vec<Vec<u8>> {
    let mut parts = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_digit = None::<bool>;
    for &c in s {
        if c == b'.' || c == b'-' {
            if !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
            cur_digit = None;
            continue;
        }
        let d = c.is_ascii_digit();
        if cur_digit.is_some() && cur_digit != Some(d) && !cur.is_empty() {
            parts.push(std::mem::take(&mut cur));
        }
        cur.push(c);
        cur_digit = Some(d);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

/// cppnix's `componentsLT` (`src/libstore/names.cc:76`) applied pairwise, in
/// its own branch order: numeric runs compare numerically, "pre" sorts before
/// everything, a numeric run beats a non-numeric one ("2.3a" < "2.3.1"), and
/// two non-numeric runs compare lexically.
///
/// The parse is `i32` because cppnix's is `string2Int<int>`, and `int` is what
/// decides which branch a component takes: `string2Int` returns `nullopt` when
/// `boost::lexical_cast` overflows (`src/libutil/util.cc:112`), so a run of
/// digits above `INT_MAX` is not a number to cppnix at all and falls through
/// to the non-numeric branches. That is not a rounding difference, it inverts
/// the answer -- `compareVersions "2147483648" "1"` is -1 in cppnix, because
/// the left component is non-numeric and the right one is numeric, and an
/// `i64` parse here made it 1. 91 of 5314 differential cases against
/// nix 2.34.7+ix.h24085346 hit this; see `versions_past_int_max_are_not_numbers`.
fn compare_versions(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (pa, pb) = (version_parts(a), version_parts(b));
    let n = pa.len().max(pb.len());
    // A component that is not text is not a number to cppnix either:
    // `string2Int` over bytes that fail the digit parse returns `nullopt`
    // whatever encoding they were, so the UTF-8 detour here decides nothing.
    let int_of = |bytes: &[u8]| -> Option<i32> {
        std::str::from_utf8(bytes).ok().and_then(|s| s.parse().ok())
    };
    for i in 0..n {
        let x = pa.get(i).map(Vec::as_slice).unwrap_or(b"");
        let y = pb.get(i).map(Vec::as_slice).unwrap_or(b"");
        if x == y {
            continue;
        }
        let xn = int_of(x);
        let yn = int_of(y);
        let ord = match (xn, yn) {
            (Some(xi), Some(yi)) => xi.cmp(&yi),
            _ => {
                if x == b"pre" {
                    Ordering::Less
                } else if y == b"pre" || xn.is_some() {
                    Ordering::Greater
                } else if yn.is_some() {
                    Ordering::Less
                } else {
                    // cppnix compares the `std::string` components, which is
                    // byte-lexicographic, exactly `[u8]`'s `Ord`.
                    x.cmp(y)
                }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}
pub(crate) const STORE_PATH_ESCAPE: &str = "__storePath";

/// Decode one JSON node, honouring [`STORE_PATH_ESCAPE`].
///
/// Containers are walked here so that the escape is recognised at every
/// depth; scalars go to the reader `builtins.fromJSON` uses, so the
/// integer/float rule has one implementation.
pub(crate) fn json_value_with_store_paths(vm: &mut Vm, j: &serde_json::Value) -> Result<Value> {
    match j {
        serde_json::Value::Array(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(Slot::value(json_value_with_store_paths(vm, item)?));
            }
            Ok(Value::list(out))
        }
        serde_json::Value::Object(map) => {
            if let Some(escaped) = map.get(STORE_PATH_ESCAPE) {
                if map.len() != 1 {
                    return Err(VmError::eval(format!(
                        "a JSON object carrying '{STORE_PATH_ESCAPE}' has {} other key(s); \
                         the escape names a store-path string and nothing else",
                        map.len() - 1
                    )));
                }
                let serde_json::Value::String(path) = escaped else {
                    return Err(VmError::eval(format!(
                        "'{STORE_PATH_ESCAPE}' must be a string"
                    )));
                };
                crate::vm::check_no_nul(path)?;
                return Ok(crate::eval::store_path_string(path.clone()));
            }
            let mut out = BTreeMap::new();
            for (k, v) in map {
                crate::vm::check_no_nul(k)?;
                let sym = vm.intern(k);
                out.insert(sym, Slot::value(json_value_with_store_paths(vm, v)?));
            }
            Ok(Value::Attrs(std::rc::Rc::new(Attrs::new(out))))
        }
        scalar => json_to_value(vm, scalar),
    }
}

#[cfg(test)]
mod tests {
    use super::{Coerced, PathStage, coerce_to_path, posix_brackets};

    /// cppnix's `coerceToPath` goes through `CanonPath`, so the text is
    /// normalised: an embedder's accessor refuses a non-canonical spelling
    /// where cppnix reads the path it means.
    #[test]
    fn a_string_coerced_to_a_path_is_normalised_like_canon_path() {
        let mut stage = PathStage::Value;
        let value = crate::value2::Value::Str("/x/crates/Cargo.toml/../.".into());
        let result = coerce_to_path(&value, &mut stage);
        assert!(matches!(
            result,
            Ok(Coerced::Done(path))
                if path.root == crate::value2::Root::Ambient && path.as_ref().as_ref() == "/x/crates"
        ));
    }

    #[test]
    fn a_string_coerced_to_a_path_uses_the_ambient_root() {
        let mut stage = PathStage::Value;
        let value = crate::value2::Value::Str("/tmp/a".into());
        let result = coerce_to_path(&value, &mut stage);
        assert!(matches!(
            result,
            Ok(Coerced::Done(path))
                if path.root == crate::value2::Root::Ambient && path.as_ref().as_ref() == "/tmp/a"
        ));
    }

    fn matched(src: &str) -> String {
        crate::eval::render_str_with(&crate::eval::Settings::default(), src)
    }

    /// What the rewrite emits, checked directly so a failure names the
    /// transformation rather than a whole evaluation.
    #[test]
    fn bracket_rewrites_are_the_expected_strings() {
        // A backslash member stays a member.
        assert_eq!(posix_brackets(br"[a\]+"), r"[a\\]+");
        // A bare `[` member cannot open a nested class.
        assert_eq!(posix_brackets(b"[a[]+"), r"[a\[]+");
        // First-position `]` is a member, in both plain and negated form.
        assert_eq!(posix_brackets(b"[]a]+"), r"[\]a]+");
        assert_eq!(posix_brackets(b"[^]a]+"), r"[^\]a]+");
        // Set-operator halves are members.
        assert_eq!(posix_brackets(b"[a&b~c]"), r"[a\&b\~c]");
        // POSIX bracket sequences survive untouched.
        assert_eq!(posix_brackets(b"[a[:digit:]]"), "[a[:digit:]]");
        // Outside a bracket expression nothing moves, and an escaped `[`
        // does not open one.
        assert_eq!(posix_brackets(br"a\[b]c"), r"a\[b]c");
    }

    /// Every subject here is what this fork's `nix-instantiate` (cpp arm,
    /// clean config) answers `[ ]` for -- a match with no capture groups.
    /// The first row is the exact class that validates every shell
    /// abbreviation in ix: under the raw crate reading its `\[` was an
    /// escaped bracket, the class closed at the wrong `]`, and all 293 keys
    /// failed where cppnix passes all (six whole-ix sweep attrs).
    #[test]
    fn posix_bracket_semantics_match_cppnix() {
        for src in [
            r#"builtins.match ''[^|;&<>()$`'"\[:space:]]+'' "gc" != null"#,
            r#"builtins.match "[a\\]+" "a\\a" != null"#,
            r#"builtins.match "[a[]+" "a[a" != null"#,
            r#"builtins.match "[]a]+" "]a" != null"#,
            r#"builtins.match "[^]a]+" "bc" != null"#,
            r#"builtins.match "[a&b]+" "a&b" != null"#,
            r#"builtins.match "[a[:digit:]]+" "a12" != null"#,
        ] {
            assert_eq!(matched(src), "true", "{src}");
        }
        // And one that must NOT match, so the pass above cannot be a
        // match-everything bug: `&` is a member, not an operator, and `c`
        // is outside the class.
        assert_eq!(matched(r#"builtins.match "[a&b]+" "c" != null"#), "false");
    }

    /// The two-pointer `bi_intersect_attrs` (ENG-13152) against the
    /// implementation it replaced -- `b.iter().filter(a.contains_key)`
    /// into a fresh `BTreeMap` -- over overlapping, disjoint, empty, and
    /// superset inputs. Equality is checked on the parts a caller can
    /// observe: the keys in iteration order, which slot each key maps to
    /// (values and positions must be b's), and the origin.
    #[test]
    fn intersect_attrs_matches_the_filter_implementation() {
        use crate::value2::{AttrOrigin, Attrs, Slot, Sym, Value};
        use std::collections::BTreeMap;
        use std::rc::Rc;

        fn attrs_of(syms: &[Sym], origin: Option<AttrOrigin>) -> Rc<Attrs> {
            let map: BTreeMap<Sym, Slot> = syms
                .iter()
                // The payload does not matter: provenance is checked by
                // `Rc::ptr_eq` on the slots below, which tells a's slot
                // from b's even when both hold the same integer.
                .map(|&s| (s, Slot::value(Value::Int(i64::from(s)))))
                .collect();
            let mut a = Attrs::new(map);
            a.origin = origin;
            Rc::new(a)
        }

        /// The implementation this replaced, verbatim.
        fn reference(a: &Attrs, b: &Attrs) -> Attrs {
            let out: BTreeMap<Sym, Slot> = b
                .iter()
                .filter(|(k, _)| a.contains_key(k))
                .map(|(k, v)| (*k, v.clone()))
                .collect();
            let mut result = Attrs::new(out);
            result.origin = b.origin.clone();
            result
        }

        fn same_origin(x: &Option<AttrOrigin>, y: &Option<AttrOrigin>) -> bool {
            match (x, y) {
                (None, None) => true,
                (Some(x), Some(y)) => {
                    Rc::ptr_eq(&x.module, &y.module) && x.unit == y.unit && x.ip == y.ip
                }
                _ => false,
            }
        }

        let mut vm = crate::vm::Vm::with_settings(crate::eval::Settings::default());
        let module = Rc::new(crate::ir::Module::default());
        let origin = vm.static_attr_origin(&module, 0, 0);

        // (a, b): overlapping, disjoint, both directions of empty, and both
        // directions of superset.
        let cases: [(&[Sym], &[Sym]); 6] = [
            (&[1, 3, 5, 9], &[2, 3, 4, 5, 8]),
            (&[1, 2], &[3, 4]),
            (&[], &[1, 2]),
            (&[1, 2], &[]),
            (&[1, 2, 3, 4], &[2, 3]),
            (&[2, 3], &[1, 2, 3, 4]),
        ];
        for (asyms, bsyms) in cases {
            let a = attrs_of(asyms, None);
            let b = attrs_of(bsyms, Some(origin.clone()));
            let expect = reference(&a, &b);
            let args = [
                Slot::value(Value::Attrs(a)),
                Slot::value(Value::Attrs(Rc::clone(&b))),
            ];
            let Ok(Value::Attrs(got)) = super::bi_intersect_attrs(&mut vm, &args) else {
                unreachable!("intersectAttrs returned an error or a non-attrs value");
            };
            // Keys in iteration order, and the exact slot each maps to:
            // `Rc::ptr_eq` proves the value is b's slot itself, position
            // and all, not a rebuilt equal.
            let got_pairs: Vec<(Sym, &Slot)> = got.iter().map(|(k, v)| (*k, v)).collect();
            let expect_pairs: Vec<(Sym, &Slot)> = expect.iter().map(|(k, v)| (*k, v)).collect();
            assert_eq!(
                got_pairs.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
                expect_pairs.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
                "keys for a={asyms:?} b={bsyms:?}"
            );
            for ((k, gv), (_, ev)) in got_pairs.iter().zip(&expect_pairs) {
                assert!(
                    Rc::ptr_eq(&gv.0, &ev.0) && b.get(k).is_some_and(|bs| Rc::ptr_eq(&gv.0, &bs.0)),
                    "slot for key {k} (a={asyms:?} b={bsyms:?}) is not b's"
                );
            }
            assert!(
                same_origin(&got.origin, &expect.origin),
                "origin for a={asyms:?} b={bsyms:?}"
            );
        }
    }
}
