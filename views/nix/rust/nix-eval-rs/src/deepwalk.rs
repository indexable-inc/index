//! The strict deep walk `builtins.toJSON` and `builtins.toXML` are both built
//! on: one driver, two renderers.
//!
//! cppnix writes each of these as a recursive function over `Value`.
//! This evaluator cannot: a walk that may force a thunk, apply a
//! `__toString`, or ask the embedder for a store copy has to be a resumable
//! machine the VM drives, and it must not put Nix-value-proportional depth on
//! the host stack. So the walk is a flat worklist and every suspension is a
//! `Yield` the driver returns and is resumed with.
//!
//! # Why a driver and not one machine, or three
//!
//! `maintainers/ix/strict-deep-walk.md` is the comparison; the short version
//! is that six of nine value arms differ in *behaviour* between the two
//! renderers rather than only in the text they emit. A path is copied into the
//! store by JSON and written verbatim by XML; the attribute arms share
//! nothing; a function is an error in one and an element in the other. So the
//! renderings are genuinely two things, and what they share is everything
//! around them: the worklist, the [`Job::Lit`] interleaving that puts closing
//! brackets and closing tags in the right place without recursion, the depth
//! and its ceiling, the accumulated string context, and the suspend-resume
//! protocol.
//!
//! `builtins.deepSeq` is deliberately **not** a client. It writes no output,
//! so it would carry [`Sink::out`] and [`Job::Lit`] and never use either.

use crate::primops_pure::want_nix_str;
use crate::task::{NeedPath, Yield};
use crate::value2::{ContextElem, NixStr, Slot, Value, type_name};
use crate::vm::{Result, Vm, VmError};
use std::collections::BTreeSet;
use std::rc::Rc;

/// cppnix's `max-call-depth`, which both walks take a slot of per level
/// (`printValueAsJSON` and `printValueAsXML` each open their recursion with
/// `state.addCallDepth`). These walkers are flat and would happily serialise a
/// value cppnix refuses, so the limit is mirrored rather than inherited:
/// eval-fail-toJSON-stack-overflow builds a 100k-deep linked list and expects
/// the refusal.
///
/// **Read from the setting, not a constant.** `addCallDepth` consults
/// `settings.maxCallDepth`, so a hard-coded ceiling here would ignore
/// `--max-call-depth` while `nix config show` reported it -- a setting that is
/// not a capability, which this repo has been bitten by before. It was a
/// `const 10_000` until `--max-call-depth 100000` was measured to change
/// nothing on either walk.
///
/// `builtins.deepSeq` reads this same function from its own flat walk rather
/// than through the driver, and `the_two_flat_walks_share_one_ceiling` holds
/// them together (ENG-12900).
#[must_use]
pub fn max_depth(vm: &crate::vm::Vm) -> usize {
    vm.max_call_depth() as usize
}

/// One queued unit: text to emit, or a value to render at a nesting depth.
///
/// `Lit` is what makes the walk flat. A renderer that has just opened a list
/// queues the children and the closing bracket together, so the bracket is
/// emitted after them without anything holding a stack frame open.
pub enum Job {
    Lit(Vec<u8>),
    /// A value to render, at a nesting depth and at whatever second number
    /// the renderer wants carried with it.
    ///
    /// The second number exists because XML's indentation is the count of
    /// *open elements*, which is not the value depth: a list contributes one
    /// element level per level and an attribute set contributes two,
    /// `<attrs>` and `<attr>`. Deriving one from the other indented
    /// `[ 1 [ 2 ] ]` two spaces too far per level, and every scalar test
    /// passed while it did, because a flat value has no second level to get
    /// wrong. The driver never reads it.
    Val(Slot, usize, usize),
}

/// What the driver is waiting for when it hands a value back to the renderer.
enum Await {
    /// A queued child's value, to be rendered at this depth and renderer
    /// depth.
    Child(usize, usize),
    /// A value the renderer asked for, under the tag it chose.
    Aux(u8),
}

/// A renderer's answer to being shown a value.
///
/// `Wait` is much larger than `Done` and is deliberately not boxed: this is
/// returned once per value on the walk, so a box would be an allocation per
/// suspend on the hot path to buy back a few words of stack.
#[allow(clippy::large_enum_variant)]
pub enum Step {
    /// Written and/or queued; the driver carries on with the worklist.
    Done,
    /// Suspend. The driver returns this `Yield` and resumes
    /// [`Renderer::aux`] with the answer under `tag`.
    Wait(Yield, u8),
}

/// The parts of the driver a renderer may write to.
///
/// Passed rather than owned so the two cannot disagree about where output
/// goes: there is one `out`, one worklist and one context set per walk.
pub struct Sink<'a> {
    pub out: &'a mut Vec<u8>,
    pub work: &'a mut Vec<Job>,
    pub context: &'a mut BTreeSet<ContextElem>,
    /// The depth of the value being rendered, for queueing its children.
    pub depth: usize,
    /// The renderer's own number for this value; see [`Job::Val`].
    pub rdepth: usize,
}

impl Sink<'_> {
    /// Queue jobs in emission order. They are pushed reversed because the
    /// worklist is a stack.
    pub fn queue(&mut self, items: Vec<Job>) {
        for it in items.into_iter().rev() {
            self.work.push(it);
        }
    }
}

pub trait Renderer {
    /// The renderer depth the root value sits at; see [`Job::Val`]. Zero for
    /// a renderer that does not use the number, one for XML, whose root value
    /// is already inside `<expr>`.
    fn root_depth(&self) -> usize {
        0
    }
    /// Anything before the value: a document prologue, a root element.
    fn open(&mut self, sink: &mut Sink);
    /// Anything after it. Fallible for the JSON renderer only: cppnix builds
    /// the whole document first and serialises second, so a non-UTF-8 string
    /// fails at the END of the walk -- after every force, `__toString`
    /// application and store copy has happened. An eval error inside the
    /// value therefore beats the serialization error, exactly as it does in
    /// cppnix, and the deferred error surfaces here.
    fn close(&mut self, sink: &mut Sink) -> Result<()>;
    /// Render one forced value.
    fn value(&mut self, vm: &mut Vm, sink: &mut Sink, v: &Value) -> Result<Step>;
    /// Resume after a [`Step::Wait`], with the value the driver got back.
    fn aux(&mut self, vm: &mut Vm, sink: &mut Sink, tag: u8, v: Value) -> Result<Step>;
}

pub struct DeepWalk {
    work: Vec<Job>,
    out: Vec<u8>,
    awaiting: Await,
    /// The depth of the value in flight, so a renderer resumed mid-value
    /// queues its children at the right one.
    depth: usize,
    rdepth: usize,
    /// Every string rendered contributes what it depended on: cppnix
    /// accumulates one `NixStringContext` across the whole value and hands it
    /// to the result, so a derivation whose attribute is a rendering of store
    /// paths still depends on them.
    context: BTreeSet<ContextElem>,
    renderer: Box<dyn Renderer>,
    /// The enclosing walk's fan-out offer, set aside at this walk's first
    /// publish and put back at `Yield::Done` ([`Vm::save_fanout_offer`]):
    /// walks nest (`builtins.toJSON` forced by the printer, say), and a
    /// nested walk that overwrote the outer offer for good left nothing to
    /// seed when a later child parked (ENG-13150).
    saved_offer: Option<std::collections::VecDeque<Slot>>,
}

impl DeepWalk {
    pub fn new(root: Slot, mut renderer: Box<dyn Renderer>) -> DeepWalk {
        let mut out = Vec::new();
        let mut work = Vec::new();
        let mut context = BTreeSet::new();
        renderer.open(&mut Sink {
            out: &mut out,
            work: &mut work,
            context: &mut context,
            depth: 0,
            rdepth: 0,
        });
        let rdepth = renderer.root_depth();
        work.push(Job::Val(root, 0, rdepth));
        DeepWalk {
            work,
            out,
            awaiting: Await::Child(0, rdepth),
            depth: 0,
            rdepth,
            context,
            renderer,
            saved_offer: None,
        }
    }

    pub fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        let DeepWalk {
            work,
            out,
            awaiting,
            depth,
            rdepth,
            context,
            renderer,
            saved_offer,
        } = self;
        if let Some(v) = incoming {
            let step = match std::mem::replace(awaiting, Await::Child(*depth, *rdepth)) {
                Await::Child(d, rd) => {
                    // Checked here rather than in each renderer, because the
                    // ceiling is the driver's and a renderer that forgot it
                    // would differ from cppnix only on inputs nobody tries.
                    if d > max_depth(vm) {
                        return Err(VmError::eval("stack overflow; max-call-depth exceeded"));
                    }
                    *depth = d;
                    *rdepth = rd;
                    let mut sink = Sink {
                        out,
                        work,
                        context,
                        depth: d,
                        rdepth: rd,
                    };
                    renderer.value(vm, &mut sink, &v)?
                }
                Await::Aux(tag) => {
                    let mut sink = Sink {
                        out,
                        work,
                        context,
                        depth: *depth,
                        rdepth: *rdepth,
                    };
                    renderer.aux(vm, &mut sink, tag, v)?
                }
            };
            if let Step::Wait(y, tag) = step {
                *awaiting = Await::Aux(tag);
                return Ok(y);
            }
        }
        while let Some(job) = work.pop() {
            match job {
                Job::Lit(s) => out.extend_from_slice(&s),
                Job::Val(slot, d, rd) => {
                    *awaiting = Await::Child(d, rd);
                    // The children after this one, in program order, offered
                    // so the scheduler can seed them as sibling strands if
                    // forcing THIS one parks on a slow question the host
                    // began (ENG-13150). An offer and not a spawn: under a
                    // host that begins nothing it is only ever replaced, and
                    // the walk stays exactly sequential. The enclosing
                    // walk's offer is set aside at the first publish and
                    // restored at `Done`.
                    if saved_offer.is_none() {
                        *saved_offer = Some(vm.save_fanout_offer());
                    }
                    vm.set_fanout_offer(pending_children(work));
                    return Ok(Yield::Force(slot));
                }
            }
        }
        // This walk is over; the enclosing one's pending children become
        // the standing offer again.
        if let Some(saved) = saved_offer.take() {
            vm.restore_fanout_offer(saved);
        }
        let mut sink = Sink {
            out,
            work,
            context,
            depth: 0,
            rdepth: 0,
        };
        renderer.close(&mut sink)?;
        Ok(Yield::Done(Value::Str(NixStr::with_context(
            std::mem::take(out),
            std::mem::take(context),
        ))))
    }
}

/// The next values the walk will force, skipping the queued literals: the
/// top of the worklist stack is the next thing emitted, so the `Val`s read
/// from the top downward are the pending children in program order. Capped
/// at [`crate::vm::FANOUT_WIDTH`] so republishing at every child force stays
/// O(1) over a large worklist.
fn pending_children(work: &[Job]) -> Vec<Slot> {
    work.iter()
        .rev()
        .filter_map(|job| match job {
            Job::Val(slot, _, _) => Some(slot.clone()),
            Job::Lit(_) => None,
        })
        .take(crate::vm::FANOUT_WIDTH)
        .collect()
}

// -- the JSON renderer -------------------------------------------------------

/// `builtins.toJSON` (`value-to-json.cc`).
#[derive(Default)]
pub struct Json {
    /// The set whose `__toString` is being applied, held across the two
    /// suspensions that takes.
    subject: Option<Value>,
    /// The first nlohmann UTF-8 rejection, in document order, held until the
    /// walk finishes; see [`Renderer::close`].
    deferred: Option<VmError>,
}

/// Resume tags. Private to the renderer, which is the point of the driver
/// carrying an opaque `u8`: two renderers cannot collide.
const J_TO_STR_FN: u8 = 1;
const J_TO_STR_RESULT: u8 = 2;
const J_TO_STR_COERCED: u8 = 3;
const J_STORE_PATH: u8 = 4;

impl Renderer for Json {
    fn open(&mut self, _sink: &mut Sink) {}
    fn close(&mut self, _sink: &mut Sink) -> Result<()> {
        match self.deferred.take() {
            Some(e) => Err(e),
            None => Ok(()),
        }
    }

    fn value(&mut self, vm: &mut Vm, sink: &mut Sink, v: &Value) -> Result<Step> {
        match v {
            Value::Int(n) => sink.out.extend_from_slice(n.to_string().as_bytes()),
            Value::Float(x) => crate::primops_host::json_float(*x, sink.out),
            Value::Bool(b) => {
                sink.out
                    .extend_from_slice(if *b { b"true".as_slice() } else { b"false" })
            }
            Value::Null => sink.out.extend_from_slice(b"null"),
            Value::Str(s) => {
                sink.context.extend(s.context_set());
                if let Err(e) = crate::primops_host::json_string(s.bytes(), sink.out)
                    && self.deferred.is_none()
                {
                    self.deferred = Some(e);
                }
            }
            // cppnix copies a path into the store and emits the store path
            // (`value-to-json.cc:83`, with `copyToStore` on, which is what
            // `toJSON` passes). The copy is the scheduler's, as it is for a
            // path interpolated into a string. ENG-12607.
            Value::Path(p) => {
                return Ok(Step::Wait(
                    Yield::Need(NeedPath::StorePath(Rc::clone(p))),
                    J_STORE_PATH,
                ));
            }
            Value::List(items) => {
                sink.out.push(b'[');
                let d = sink.depth + 1;
                let mut queued = Vec::with_capacity(items.len() * 2 + 1);
                for (i, s) in items.iter().enumerate() {
                    if i > 0 {
                        queued.push(Job::Lit(b",".to_vec()));
                    }
                    queued.push(Job::Val(s.clone(), d, 0));
                }
                queued.push(Job::Lit(b"]".to_vec()));
                sink.queue(queued);
            }
            Value::Attrs(m) => {
                // tryAttrsToString first: a set carrying __toString becomes
                // that function's result, not an object. Then outPath, which
                // a derivation is serialised through.
                let to_string = vm.intern("__toString");
                if let Some(f) = m.get(&to_string) {
                    self.subject = Some(v.clone());
                    return Ok(Step::Wait(Yield::Force(f.clone()), J_TO_STR_FN));
                }
                let out_path = vm.intern("outPath");
                if let Some(p) = m.get(&out_path) {
                    sink.work.push(Job::Val(p.clone(), sink.depth + 1, 0));
                    return Ok(Step::Done);
                }
                let entries = sorted_entries(vm, m);
                sink.out.push(b'{');
                let d = sink.depth + 1;
                let mut queued = Vec::with_capacity(entries.len() * 2 + 1);
                for (i, (name, s)) in entries.into_iter().enumerate() {
                    let mut lead = Vec::new();
                    if i > 0 {
                        lead.push(b',');
                    }
                    // An attribute name is interned text, so this cannot
                    // fail; `?` keeps that claim checked rather than assumed.
                    crate::primops_host::json_string(name.as_bytes(), &mut lead)?;
                    lead.push(b':');
                    queued.push(Job::Lit(lead));
                    queued.push(Job::Val(s, d, 0));
                }
                queued.push(Job::Lit(b"}".to_vec()));
                sink.queue(queued);
            }
            Value::Closure(_) | Value::Builtin(_) => {
                return Err(VmError::eval(format!(
                    "cannot convert {} to JSON",
                    type_name(v)
                )));
            }
        }
        Ok(Step::Done)
    }

    fn aux(&mut self, _vm: &mut Vm, sink: &mut Sink, tag: u8, v: Value) -> Result<Step> {
        match tag {
            J_TO_STR_FN => {
                let subject = self
                    .subject
                    .take()
                    .ok_or_else(|| VmError::eval("internal: toJSON lost its set"))?;
                Ok(Step::Wait(
                    Yield::Apply(v, Slot::value(subject)),
                    J_TO_STR_RESULT,
                ))
            }
            // cppnix hands the result to `coerceToString` with `coerceMore`
            // and `copyToStore` both off (`value-to-json.cc`,
            // `tryAttrsToString(pos, v, context, false, false)`), which is a
            // walk and not a type test: a set returned from `__toString`
            // coerces on through its own `__toString` or `outPath`. ENG-12670.
            J_TO_STR_RESULT => Ok(Step::Wait(
                Yield::sub(crate::task::Task::coerce_to_json_string(Slot::value(v))),
                J_TO_STR_COERCED,
            )),
            // `copyToStore` off is what makes this differ from the
            // `Value::Path` arm: a path reached through `__toString` is
            // written as its source path and copies nothing.
            J_TO_STR_COERCED | J_STORE_PATH => {
                let s = want_nix_str(&v)?;
                sink.context.extend(s.context_set());
                if let Err(e) = crate::primops_host::json_string(s.bytes(), sink.out)
                    && self.deferred.is_none()
                {
                    self.deferred = Some(e);
                }
                Ok(Step::Done)
            }
            other => Err(VmError::eval(format!("internal: toJSON tag {other}"))),
        }
    }
}

/// cppnix's `lexicographicOrder`: attribute names sorted as strings, not by
/// symbol id, which is interning order and differs between runs.
fn sorted_entries(vm: &mut Vm, m: &crate::value2::AttrMap) -> Vec<(String, Slot)> {
    let mut entries: Vec<(String, Slot)> = m
        .iter()
        .map(|(k, s)| (vm.sym_name(*k).to_owned(), s.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    entries
}

// -- the XML renderer --------------------------------------------------------

/// cppnix's `XMLWriter::writeAttrs` escaping (`xml-writer.cc:71`): five
/// characters and nothing else, with the newline escaped so attribute-value
/// normalisation cannot eat it (XML 1.0 section 3.3.3).
///
/// Notably NOT an XML-correctness routine. A tab or a carriage return inside
/// an attribute is written raw here exactly as cppnix writes it raw, and a
/// stricter escaper would produce a document cppnix does not.
fn xml_attr_escaped(value: &[u8], out: &mut Vec<u8>) {
    // Byte-wise, as cppnix's loop over `std::string` is: a non-UTF-8 byte is
    // written raw, and the resulting document is exactly as (in)valid as the
    // one cppnix writes for the same value.
    for &c in value {
        match c {
            b'"' => out.extend_from_slice(b"&quot;"),
            b'<' => out.extend_from_slice(b"&lt;"),
            b'>' => out.extend_from_slice(b"&gt;"),
            b'&' => out.extend_from_slice(b"&amp;"),
            b'\n' => out.extend_from_slice(b"&#xA;"),
            _ => out.push(c),
        }
    }
}

/// One element, with its attributes in the order cppnix emits them.
///
/// `XMLAttrs` is a `std::map<std::string, std::string>` (`xml-writer.hh:11`),
/// so cppnix writes attributes in **name order** and not in the order the code
/// sets them. `drvPath` before `outPath` is that, not a choice, and a renderer
/// emitting them in source order would differ on every derivation.
fn element(indent: usize, name: &str, attrs: &[(&str, &[u8])], empty: bool, out: &mut Vec<u8>) {
    let mut sorted: Vec<&(&str, &[u8])> = attrs.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(b.0));
    out.extend_from_slice("  ".repeat(indent).as_bytes());
    out.push(b'<');
    out.extend_from_slice(name.as_bytes());
    for (k, v) in sorted {
        out.push(b' ');
        out.extend_from_slice(k.as_bytes());
        out.extend_from_slice(b"=\"");
        xml_attr_escaped(v, out);
        out.push(b'"');
    }
    out.extend_from_slice(if empty { b" />".as_slice() } else { b">" });
    out.push(b'\n');
}

fn close_element(indent: usize, name: &str) -> Vec<u8> {
    format!("{}</{}>\n", "  ".repeat(indent), name).into_bytes()
}

/// `builtins.toXML` (`primops.cc:2620`), which is `printValueAsXML` with
/// `strict` on and `location` off, so no position attribute is ever written.
///
/// # The element depth is not the value depth
///
/// `XMLWriter` indents by `pendingElems.size()`, the number of open elements.
/// An attribute set at value depth d contributes two element levels,
/// `<attrs>` and `<attr>`, before its values. This walk is flat, so the
/// indent is computed when a job is queued rather than read off a stack.
pub struct Xml {
    /// Derivations already emitted in full, keyed on `drvPath` the way
    /// cppnix's `drvsSeen` is. Not a cycle guard: a repeat still emits a
    /// `<derivation>` element, with `<repeated />` where its children were.
    drvs_seen: BTreeSet<Vec<u8>>,
    /// The set being examined and the element depth to write it at, held
    /// across the three suspensions a derivation takes.
    pending: Option<(Value, usize)>,
    drv_path: Option<Vec<u8>>,
    out_path: Option<Vec<u8>>,
}

const X_TYPE: u8 = 1;
const X_DRV_PATH: u8 = 2;
const X_OUT_PATH: u8 = 3;

impl Default for Xml {
    fn default() -> Self {
        Self::new()
    }
}

impl Xml {
    pub fn new() -> Xml {
        Xml {
            drvs_seen: BTreeSet::new(),
            pending: None,
            drv_path: None,
            out_path: None,
        }
    }

    /// `showAttrs` (`value-to-xml.cc:35`): a wrapper element, then one
    /// `<attr name="...">` per entry in lexicographic order.
    fn attrs_body(
        &mut self,
        vm: &mut Vm,
        sink: &mut Sink,
        set: &Value,
        elem: usize,
        wrapper: &str,
        wrapper_attrs: &[(&str, &[u8])],
    ) -> Result<Step> {
        let Value::Attrs(m) = set else {
            return Err(VmError::eval("internal: toXML lost its set"));
        };
        let entries = sorted_entries(vm, m);
        element(elem, wrapper, wrapper_attrs, false, sink.out);
        let d = sink.depth + 1;
        let mut queued = Vec::with_capacity(entries.len() * 3 + 1);
        for (name, s) in entries {
            let mut open = Vec::new();
            element(
                elem + 1,
                "attr",
                &[("name", name.as_bytes())],
                false,
                &mut open,
            );
            queued.push(Job::Lit(open));
            // Two element levels deeper: the wrapper and this `<attr>`.
            queued.push(Job::Val(s, d, elem + 2));
            queued.push(Job::Lit(close_element(elem + 1, "attr")));
        }
        queued.push(Job::Lit(close_element(elem, wrapper)));
        sink.queue(queued);
        Ok(Step::Done)
    }
}

impl Renderer for Xml {
    /// The value goes inside `<expr>`, so it starts one element in.
    fn root_depth(&self) -> usize {
        1
    }

    /// `XMLWriter`'s constructor writes the declaration, and `printValueAsXML`
    /// opens `<expr>` around the whole value (`value-to-xml.cc:200`).
    fn open(&mut self, sink: &mut Sink) {
        sink.out
            .extend_from_slice(b"<?xml version='1.0' encoding='utf-8'?>\n");
        element(0, "expr", &[], false, sink.out);
    }

    fn close(&mut self, sink: &mut Sink) -> Result<()> {
        sink.out.extend_from_slice(&close_element(0, "expr"));
        Ok(())
    }

    fn value(&mut self, vm: &mut Vm, sink: &mut Sink, v: &Value) -> Result<Step> {
        // Carried rather than derived from `sink.depth`; see `Job::Val`.
        let elem = sink.rdepth;
        match v {
            Value::Int(n) => element(
                elem,
                "int",
                &[("value", n.to_string().as_bytes())],
                true,
                sink.out,
            ),
            Value::Bool(b) => element(
                elem,
                "bool",
                &[("value", if *b { b"true".as_slice() } else { b"false" })],
                true,
                sink.out,
            ),
            Value::Float(x) => element(
                elem,
                "float",
                &[("value", crate::value2::format_g6(*x).as_bytes())],
                true,
                sink.out,
            ),
            Value::Null => element(elem, "null", &[], true, sink.out),
            Value::Str(s) => {
                sink.context.extend(s.context_set());
                element(elem, "string", &[("value", s.bytes())], true, sink.out);
            }
            // No store copy, unlike the JSON renderer: cppnix writes
            // `v.path().to_string()` (`value-to-xml.cc:89`) and copies
            // nothing, so a path in a `toXML` is its own spelling.
            Value::Path(p) => element(elem, "path", &[("value", p.as_bytes())], true, sink.out),
            Value::List(items) => {
                element(elem, "list", &[], false, sink.out);
                let d = sink.depth + 1;
                let mut queued = Vec::with_capacity(items.len() + 1);
                for s in items.iter() {
                    // One element level deeper: `<list>` only. An attribute
                    // set adds two here, which is why this number is carried.
                    queued.push(Job::Val(s.clone(), d, elem + 1));
                }
                queued.push(Job::Lit(close_element(elem, "list")));
                sink.queue(queued);
            }
            Value::Attrs(m) => {
                // `isDerivation` forces `type` and compares it to
                // "derivation" (`eval.cc:2598`). An absent `type` is not a
                // derivation and costs no force.
                let type_sym = vm.intern("type");
                let Some(t) = m.get(&type_sym) else {
                    return self.attrs_body(vm, sink, v, elem, "attrs", &[]);
                };
                self.pending = Some((v.clone(), elem));
                return Ok(Step::Wait(Yield::Force(t.clone()), X_TYPE));
            }
            // cppnix renders a lambda's formals and writes `<unevaluated />`
            // for a primop, with a FIXME saying so (`value-to-xml.cc:132`).
            Value::Closure(c) => {
                element(elem, "function", &[], false, sink.out);
                let param = c
                    .module
                    .units
                    .get(c.unit as usize)
                    .and_then(|u| u.param.as_ref());
                let sym = |i: u32| -> String {
                    c.module
                        .symbols
                        .get(i as usize)
                        .cloned()
                        .unwrap_or_default()
                };
                match param {
                    Some(crate::ir::Param::Formals {
                        fields,
                        ellipsis,
                        bind,
                    }) => {
                        let bound = bind.map(sym);
                        let mut attrs: Vec<(&str, &[u8])> = Vec::new();
                        if let Some(n) = bound.as_deref() {
                            attrs.push(("name", n.as_bytes()));
                        }
                        if *ellipsis {
                            attrs.push(("ellipsis", b"1"));
                        }
                        element(elem + 1, "attrspat", &attrs, false, sink.out);
                        let mut names: Vec<String> = fields.iter().map(|f| sym(f.sym)).collect();
                        names.sort();
                        for n in names {
                            element(elem + 2, "attr", &[("name", n.as_bytes())], true, sink.out);
                        }
                        sink.out
                            .extend_from_slice(&close_element(elem + 1, "attrspat"));
                    }
                    Some(crate::ir::Param::Ident(s)) => {
                        element(
                            elem + 1,
                            "varpat",
                            &[("name", sym(*s).as_bytes())],
                            true,
                            sink.out,
                        );
                    }
                    // A unit with no parameter is a module entry, which is
                    // never a closure value. Treated as cppnix treats a
                    // primop rather than guessed at.
                    None => element(elem + 1, "unevaluated", &[], true, sink.out),
                }
                sink.out.extend_from_slice(&close_element(elem, "function"));
            }
            Value::Builtin(_) => element(elem, "unevaluated", &[], true, sink.out),
        }
        Ok(Step::Done)
    }

    fn aux(&mut self, vm: &mut Vm, sink: &mut Sink, tag: u8, v: Value) -> Result<Step> {
        let (set, elem) = self
            .pending
            .clone()
            .ok_or_else(|| VmError::eval("internal: toXML lost its set"))?;
        let Value::Attrs(m) = &set else {
            return Err(VmError::eval("internal: toXML lost its set"));
        };
        match tag {
            X_TYPE => {
                if !matches!(&v, Value::Str(s) if s.bytes() == b"derivation") {
                    self.pending = None;
                    return self.attrs_body(vm, sink, &set, elem, "attrs", &[]);
                }
                self.drv_path = None;
                self.out_path = None;
                let sym = vm.intern("drvPath");
                match m.get(&sym) {
                    Some(s) => Ok(Step::Wait(Yield::Force(s.clone()), X_DRV_PATH)),
                    None => self.aux(vm, sink, X_DRV_PATH, Value::Null),
                }
            }
            X_DRV_PATH => {
                // cppnix writes the attribute only when the forced value is a
                // string; anything else leaves it out rather than rendering
                // it (`value-to-xml.cc:101`).
                if let Value::Str(s) = &v {
                    sink.context.extend(s.context_set());
                    self.drv_path = Some(s.bytes().to_vec());
                }
                let sym = vm.intern("outPath");
                match m.get(&sym) {
                    Some(s) => Ok(Step::Wait(Yield::Force(s.clone()), X_OUT_PATH)),
                    None => self.aux(vm, sink, X_OUT_PATH, Value::Null),
                }
            }
            X_OUT_PATH => {
                if let Value::Str(s) = &v {
                    sink.context.extend(s.context_set());
                    self.out_path = Some(s.bytes().to_vec());
                }
                self.pending = None;
                let drv = self.drv_path.take();
                let out = self.out_path.take();
                let mut attrs: Vec<(&str, &[u8])> = Vec::new();
                if let Some(p) = drv.as_deref() {
                    attrs.push(("drvPath", p));
                }
                if let Some(p) = out.as_deref() {
                    attrs.push(("outPath", p));
                }
                // `drvPath != "" && drvsSeen.insert(drvPath).second`
                // (`value-to-xml.cc:119`). A derivation with no drvPath, and a
                // second sighting of one, both get `<repeated />` in place of
                // their children; the element itself is still written.
                let first = drv
                    .as_deref()
                    .is_some_and(|p| !p.is_empty() && self.drvs_seen.insert(p.to_vec()));
                if first {
                    return self.attrs_body(vm, sink, &set, elem, "derivation", &attrs);
                }
                element(elem, "derivation", &attrs, false, sink.out);
                element(elem + 1, "repeated", &[], true, sink.out);
                sink.out
                    .extend_from_slice(&close_element(elem, "derivation"));
                Ok(Step::Done)
            }
            other => Err(VmError::eval(format!("internal: toXML tag {other}"))),
        }
    }
}
