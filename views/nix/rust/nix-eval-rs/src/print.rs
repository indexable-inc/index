//! Strict value printing in nix-instantiate's output format, and the
//! `toString` coercion, as resumable tasks. Byte compatibility with cppnix
//! here is part of the corpus contract, so every rule cites the behavior it
//! mirrors.
//!
//! Both walkers are a token worklist rather than a recursive descent: a
//! printed value can be as deep as the expression that built it, and the
//! printer is exactly the code most likely to meet the deepest one.

use crate::refusal::{Refusal, RefusalToken};
use crate::task::Yield;
use crate::value2::{Slot, Value, format_f6, format_g6, type_name};
use crate::vm::{Result, Vm, VmError};
use std::collections::BTreeSet;
use std::rc::Rc;

/// cppnix's spelling for a container it has already printed once in this
/// value: the literal in `print-ambiguous.cc` and `printRepeated` in
/// `print.cc`.
const REPEATED: &str = "«repeated»";

/// Either a literal already decided on, or a slot whose forced value gets
/// rendered when the machine hands it back.
enum Item {
    Slot(Slot),
    Lit(Vec<u8>),
}

/// A token stack and its pending values in the same push/pop order.
///
/// Literals can greatly outnumber values: a deeply nested singleton list leaves
/// one closing literal per level. Searching that token stack for the next few
/// values at every force makes printing quadratic in nesting depth. Keeping the
/// value stack alongside it bounds each fan-out offer by `FANOUT_WIDTH`, even
/// when no values remain behind a long run of literals.
#[derive(Default)]
struct Worklist {
    items: Vec<Item>,
    values: Vec<Slot>,
}

impl Worklist {
    fn push(&mut self, item: Item) {
        if let Item::Slot(slot) = &item {
            self.values.push(slot.clone());
        }
        self.items.push(item);
    }

    fn pop(&mut self) -> Option<Item> {
        let item = self.items.pop()?;
        if let Item::Slot(slot) = &item {
            let pending = self.values.pop();
            debug_assert_eq!(pending.as_ref().map(Slot::id), Some(slot.id()));
        }
        Some(item)
    }

    /// The next values in print order, without inspecting queued literals.
    fn pending_values(&self) -> Vec<Slot> {
        self.values
            .iter()
            .rev()
            .take(crate::vm::FANOUT_WIDTH)
            .cloned()
            .collect()
    }
}

/// The same, for the coercion, plus whether this position owes a separator
/// once it has been written. Separate from [`Item`] because only the coercion
/// has that obligation, and because whether it is discharged depends on what
/// the element turns out to be. See [`Coerce::render`].
enum CoerceItem {
    /// A value to coerce, whether that position owes a separator after it,
    /// and how deep it sits. The depth is per item and not per walk because
    /// cppnix's is a stack frame: `coerceToString` takes an `addCallDepth`
    /// guard whose destructor runs when that value's coercion returns
    /// (`eval-inline.hh:200`), so two elements of one list are siblings at the
    /// same depth and cost each other nothing. A counter on the walk instead
    /// of on the item counts breadth as depth, which refused
    /// `toString (genList (i: { outPath = "x"; }) 12000)` where cppnix answers
    /// 23999.
    Slot(Slot, SepAfter, u32),
    Lit(Vec<u8>),
}

/// Whether a space follows the element being coerced.
///
/// `Owed` is a *request*, not a decision: cppnix drops the separator after an
/// element that is an empty list, and that is only knowable once the element
/// is forced.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SepAfter {
    No,
    Owed,
}

/// What one container was keyed by the first time it was printed.
///
/// Two key spaces, because cppnix uses two and they answer differently. An
/// attribute set is keyed on its bindings (`seen->insert(v.attrs())`), so two
/// cells holding one map repeat. A list is keyed on the cell that holds it
/// (`seen->insert(&v)`), which `print-ambiguous.cc` explains as sidestepping
/// the question of its small-list representation, so two cells holding one
/// list are each printed in full.
///
/// Copied rather than harmonised, because the difference is visible. On nix
/// 2.34 `let l = [ 1 l ]; in l` prints `[ 1 [ 1 «repeated» ] ]`: the root is a
/// copy of the binding's cell, so the binding gets one full printing of its
/// own before the third sighting repeats. Keying a list on its elements would
/// print `[ 1 «repeated» ]` there instead.
enum Anchor {
    Attrs(Rc<crate::value2::Attrs>),
    Cell(Slot),
}

/// cppnix's `std::set<const void *>`: the second time a container is reached
/// within one print it renders as `«repeated»` instead of being walked again.
///
/// Termination depends on this, so it is not a fidelity detail. A value may
/// contain itself -- the `derivation` wrapper's result does, because its
/// `all` is a list of the outputs' values and the first of those is the
/// result -- and printing one without this never stops (ENG-12517).
struct Seen {
    marks: BTreeSet<usize>,
    /// What the marks stand for, held until the print finishes. A mark is an
    /// address, so a container dropped once its children were queued would let
    /// a later allocation reuse that address and read as a cycle that is not
    /// there. cppnix cannot meet this, because the value it is printing stays
    /// rooted for the duration; here the printer has to root it.
    anchors: Vec<Anchor>,
}

impl Seen {
    fn new() -> Self {
        Seen {
            marks: BTreeSet::new(),
            anchors: Vec::new(),
        }
    }

    /// `true` the first time this container is reached, `false` after.
    fn first_sighting(&mut self, anchor: Anchor) -> bool {
        let key = match &anchor {
            Anchor::Attrs(m) => Rc::as_ptr(m) as usize,
            Anchor::Cell(s) => s.id(),
        };
        if !self.marks.insert(key) {
            return false;
        }
        self.anchors.push(anchor);
        true
    }
}

/// What the machine's next value means to a `Print` in flight.
enum Await {
    /// Nothing was asked for. A value arriving against this means the machine
    /// resumed a task that did not yield, so it is named rather than rendered
    /// into the answer.
    Idle,
    /// A queued slot's value, to be rendered. The slot rides along because a
    /// list repeats on the identity of the cell holding it (see [`Anchor`]),
    /// which cannot be recovered from the value.
    Value(Slot),
    /// The `type` attribute of this attribute set, forced so the printer can
    /// tell a derivation from an ordinary set the way `EvalState::isDerivation`
    /// does. Only reached in `nix eval`'s dialect.
    DerivationCheck(Value),
}

pub struct Print {
    work: Worklist,
    /// Bytes, because a printed Nix string is written verbatim: cppnix's
    /// `printLiteralString` copies every byte it does not escape, UTF-8 or
    /// not, and `nix-instantiate --eval` output owes the corpus those bytes.
    out: Vec<u8>,
    awaiting: Await,
    seen: Seen,
    /// Print the way `nix eval` does rather than the way
    /// `nix-instantiate --eval` does.
    ///
    /// cppnix has two plain printers and they are not the same function:
    /// nix-instantiate uses `printAmbiguous`, `nix eval` uses `ValuePrinter`
    /// with `derivationPaths`. Measured over 46 expressions on one binary they
    /// agree on 44; the two they disagree on are a function (`ValuePrinter`
    /// names its source position, which this IR does not carry, ENG-12137) and
    /// a derivation-shaped attribute set (`ValuePrinter` prints
    /// `«derivation <path>»`, which needs a store, rung C). This flag makes
    /// those two refuse by name instead of printing the other dialect's
    /// answer, which is what would otherwise be a silent divergence in exactly
    /// the case nothing else checks.
    value_printer: bool,
    /// The enclosing walk's fan-out offer, set aside at this walk's first
    /// publish and put back at `Yield::Done` ([`Vm::save_fanout_offer`]):
    /// walks nest, and a nested walk that overwrote the outer offer for
    /// good left nothing to seed when a later child parked (ENG-13150).
    saved_offer: Option<std::collections::VecDeque<Slot>>,
}

impl Print {
    pub fn new(v: Value) -> Self {
        Print::with_dialect(v, false)
    }

    /// `nix eval`'s dialect. See [`Print::value_printer`].
    pub fn value_printer(v: Value) -> Self {
        Print::with_dialect(v, true)
    }

    fn with_dialect(v: Value, value_printer: bool) -> Self {
        let mut work = Worklist::default();
        work.push(Item::Slot(Slot::value(v)));
        Print {
            // A fresh cell, which is what cppnix prints from too: it hands the
            // printer a stack `Value` copied off the binding, so a list
            // reached again through the binding's own cell is a second
            // sighting rather than the first.
            work,
            out: Vec::new(),
            awaiting: Await::Idle,
            seen: Seen::new(),
            value_printer,
            saved_offer: None,
        }
    }

    pub fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        if let Some(v) = incoming {
            match std::mem::replace(&mut self.awaiting, Await::Idle) {
                Await::Idle => {
                    return Err(VmError::eval(
                        "internal: the printer was handed a value it did not ask for",
                    ));
                }
                Await::Value(cell) => {
                    if let Some(y) = self.render(vm, &v, &cell)? {
                        return Ok(y);
                    }
                }
                Await::DerivationCheck(attrs) => {
                    // cppnix's isDerivation: the `type` attribute forced, and
                    // equal to the string "derivation". Anything else is an
                    // ordinary set and prints as one.
                    if matches!(&v, Value::Str(text) if text.bytes() == b"derivation") {
                        return Err(VmError::Unimplemented(Refusal::new(
                            RefusalToken::StoreUnavailable,
                            "printing a derivation-shaped attribute set the way `nix eval` does \
                             (cppnix prints «derivation <store path>», which needs a store; rung C)",
                        )));
                    }
                    if let Some(y) = self.render_attrs(vm, &attrs)? {
                        return Ok(y);
                    }
                }
            }
        }
        while let Some(item) = self.work.pop() {
            match item {
                Item::Lit(s) => self.out.extend_from_slice(&s),
                Item::Slot(s) => {
                    self.awaiting = Await::Value(s.clone());
                    // The values after this one, in print order, offered so
                    // the scheduler can seed them as sibling strands if
                    // forcing THIS one parks on a slow question the host
                    // began (ENG-13150) -- `nix-instantiate --eval --strict`
                    // reaches import-from-derivation through this walk, not
                    // through `deepwalk`, and without the offer its builds
                    // ran strictly one after another. An offer and not a
                    // spawn: under a host that begins nothing it is only
                    // ever replaced, and the walk stays exactly sequential.
                    // The enclosing walk's offer is set aside at the first
                    // publish and restored at `Done`.
                    if self.saved_offer.is_none() {
                        self.saved_offer = Some(vm.save_fanout_offer());
                    }
                    vm.set_fanout_offer(self.work.pending_values());
                    return Ok(Yield::Force(s));
                }
            }
        }
        // This walk is over; the enclosing one's pending children become
        // the standing offer again.
        if let Some(saved) = self.saved_offer.take() {
            vm.restore_fanout_offer(saved);
        }
        // The printer renders bytes; a rendered value has no context of its
        // own, which is why `nix-instantiate --eval` output is unchanged by
        // contexts existing at all.
        Ok(Yield::Done(Value::Str(
            std::mem::take(&mut self.out).into(),
        )))
    }

    /// `Some(yield)` means the machine has to answer something before this
    /// value is written; `None` means it is written and the worklist carries
    /// on. `cell` is the slot `v` was forced out of, which is what a list
    /// repeats on.
    fn render(&mut self, vm: &mut Vm, v: &Value, cell: &Slot) -> Result<Option<Yield>> {
        match v {
            Value::Int(n) => self.out.extend_from_slice(n.to_string().as_bytes()),
            Value::Float(x) => self.out.extend_from_slice(format_g6(*x).as_bytes()),
            Value::Bool(b) => {
                self.out
                    .extend_from_slice(if *b { b"true".as_slice() } else { b"false" })
            }
            Value::Null => self.out.extend_from_slice(b"null"),
            Value::Str(s) => print_string(s.bytes(), &mut self.out),
            Value::Path(p) => self.out.extend_from_slice(p.as_bytes()),
            Value::List(items) => {
                if items.is_empty() {
                    self.out.extend_from_slice(b"[ ]");
                    return Ok(None);
                }
                // Guarded on non-empty the way cppnix guards it on
                // `v.listSize()`, so `{ a = []; b = []; }` prints two empty
                // lists rather than repeating the second.
                if !self.seen.first_sighting(Anchor::Cell(cell.clone())) {
                    self.out.extend_from_slice(REPEATED.as_bytes());
                    return Ok(None);
                }
                self.out.push(b'[');
                let mut queued = Vec::with_capacity(items.len() * 2 + 1);
                for s in items.iter() {
                    queued.push(Item::Lit(b" ".to_vec()));
                    queued.push(Item::Slot(s.clone()));
                }
                queued.push(Item::Lit(b" ]".to_vec()));
                self.queue(queued);
            }
            Value::Attrs(map) => {
                // `printAmbiguous` tests `!v.attrs()->empty()` before marking,
                // so an empty set never repeats.
                //
                // Its `nix eval` printer does mark them, and so calls the
                // second `{}` in one value repeated, because
                // `Bindings::emptyBindings` is a singleton. That is not
                // mirrored: it holds only for the constructions that reach for
                // the singleton, so on nix 2.34
                // `{ a = {}; b = removeAttrs { x = 1; } ["x"]; }` prints two
                // empty sets where `{ a = {}; b = {}; }` repeats the second.
                // An allocation artifact is not a rule worth copying, and the
                // corpus differ reads the nix-instantiate dialect (ENG-12525).
                if !map.is_empty() && !self.seen.first_sighting(Anchor::Attrs(Rc::clone(map))) {
                    self.out.extend_from_slice(REPEATED.as_bytes());
                    return Ok(None);
                }
                // In `nix eval`'s dialect a set carrying `type` might be a
                // derivation, and cppnix forces that attribute to find out.
                // Ask the same question rather than guessing from the key, so
                // `{ type = "car"; }` still prints as itself. Ordered after
                // the repeat check because cppnix orders it that way: a
                // derivation reached twice prints «repeated», not twice.
                if self.value_printer && !map.is_empty() {
                    let type_sym = vm.intern("type");
                    if let Some(slot) = map.get(&type_sym) {
                        let slot = slot.clone();
                        self.awaiting = Await::DerivationCheck(v.clone());
                        return Ok(Some(Yield::Force(slot)));
                    }
                }
                return self.render_attrs(vm, v);
            }
            Value::Closure(_) | Value::Builtin(_) if self.value_printer => {
                return Err(VmError::Unimplemented(Refusal::new(
                    RefusalToken::UnsupportedRender,
                    "printing a function the way `nix eval` does (cppnix names its \
                     source position, which this IR does not carry; ENG-12137)",
                )));
            }
            Value::Closure(_) => self.out.extend_from_slice(b"<LAMBDA>"),
            Value::Builtin(b) => {
                // cppnix: <PRIMOP> for primops, <PRIMOP-APP> once partially
                // applied.
                if b.args.is_empty() {
                    self.out.extend_from_slice(b"<PRIMOP>");
                } else {
                    self.out.extend_from_slice(b"<PRIMOP-APP>");
                }
            }
        }
        Ok(None)
    }

    /// An attribute set already known not to be a derivation, and already
    /// marked by [`Print::render`].
    fn render_attrs(&mut self, vm: &Vm, v: &Value) -> Result<Option<Yield>> {
        let Value::Attrs(map) = v else {
            return Err(VmError::eval("internal: render_attrs on a non-set"));
        };
        if map.is_empty() {
            self.out.extend_from_slice(b"{ }");
            return Ok(None);
        }
        // cppnix prints attrs sorted by name string, not symbol id.
        let mut entries: Vec<(String, Slot)> = map
            .iter()
            .map(|(k, s)| (vm.sym_name(*k).to_owned(), s.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        self.out.push(b'{');
        let mut queued = Vec::with_capacity(entries.len() * 3 + 1);
        for (name, s) in entries {
            let mut lead = b" ".to_vec();
            print_attr_name(&name, &mut lead);
            lead.extend_from_slice(b" = ");
            queued.push(Item::Lit(lead));
            queued.push(Item::Slot(s));
            queued.push(Item::Lit(b";".to_vec()));
        }
        queued.push(Item::Lit(b" }".to_vec()));
        self.queue(queued);
        Ok(None)
    }

    fn queue(&mut self, items: Vec<Item>) {
        for it in items.into_iter().rev() {
            self.work.push(it);
        }
    }
}

/// The two flags cppnix's `coerceToString` takes at a call site
/// (`eval.hh:831-838`), in the order the C++ declares them and with the
/// C++ defaults.
///
/// A named pair rather than two bare booleans at every constructor, because
/// this is also the vocabulary [`crate::builtins::TABLE`] declares a coercing
/// argument in and the vocabulary `tests/coercion_class.rs` re-derives from
/// `primops.cc`. Two of the three have to agree with the third or the gate
/// fails, which is the point: `stringLength` answered `expected a string but
/// found a path` for as long as nothing compared the two (ENG-12854).
///
/// `canonicalizePath` is the third flag and is not carried: every primop site
/// leaves it at its default of on, and the only behaviour it changes is the
/// `!canonicalizePath && !copyToStore` path arm (`eval.cc`,
/// `coerceToString`), which no caller here reaches. The gate refuses a call
/// site that passes it rather than modelling it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct CoerceFlags {
    /// cppnix's `coerceMore`: bools, null, numbers and lists coerce too.
    pub coerce_more: bool,
    /// cppnix's `copyToStore`: a path is copied into the store and coerces to
    /// the store path, with that path in the result's context.
    pub copy_to_store: bool,
}

impl CoerceFlags {
    /// What a primop that passes neither flag gets: `coerceMore` off,
    /// `copyToStore` on. `stringLength`, `substring`, `throw`, `abort`,
    /// `concatStringsSep` and the three context rewrites all call
    /// `coerceToString` this way.
    pub const DEFAULTS: CoerceFlags = CoerceFlags {
        coerce_more: false,
        copy_to_store: true,
    };
    /// `false, false`: coerces no further than a string or a path and copies
    /// nothing. `baseNameOf`, `dirOf`, `findFile` and `addErrorContext` pass
    /// this.
    pub const NEITHER: CoerceFlags = CoerceFlags {
        coerce_more: false,
        copy_to_store: false,
    };
    /// `true, false`, which is `builtins.toString` and only it among the
    /// primops: everything coerces and a path stays a source path.
    pub const TO_STRING: CoerceFlags = CoerceFlags {
        coerce_more: true,
        copy_to_store: false,
    };
    /// `true` and the default: a derivation attribute, which coerces
    /// everything and copies paths into the store.
    pub const DERIVATION_ATTR: CoerceFlags = CoerceFlags {
        coerce_more: true,
        copy_to_store: true,
    };
}

/// cppnix's `coerceToString`, as a resumable task.
///
/// Three flags in cppnix, two of which matter here and are carried as fields.
///
/// `copyToStore` decides what a path becomes: `builtins.toString` passes it
/// off and gets the source path back (`primops.cc`, `prim_toString`), while a
/// derivation attribute leaves it at its default of on (`primops.cc:1728`) and
/// gets the store path the tree was copied to, with that path in the result's
/// context.
///
/// `coerceMore` decides how much is allowed to coerce at all: `toString` sets
/// it, so bools, null, numbers and lists all coerce; string interpolation does
/// not, so those are errors there. An attribute set is *not* gated on it.
/// cppnix tests the `nAttrs` case before the `coerceMore` block
/// (`eval.cc`, `coerceToString`), which is why `"${pkg}/bin/sh"` works in an
/// interpolation that would reject a bare integer.
///
/// One structure with flags rather than one coercion per caller, because the
/// alternative is several implementations to keep in step and the difference
/// between them is invisible in every expression that has no path, no list and
/// no set in it.
pub struct Coerce {
    work: Vec<CoerceItem>,
    /// Bytes: the coercion concatenates whatever bytes the parts hold, as
    /// cppnix's `coerceToString` does over `std::string`.
    out: Vec<u8>,
    /// The union of every element's context. `toString` of a list is a string
    /// built out of the elements, so it depends on whatever they did.
    context: std::collections::BTreeSet<crate::value2::ContextElem>,
    /// cppnix's `copyToStore`. See the type comment.
    copy_to_store: bool,
    /// cppnix's `coerceMore`. See the type comment.
    coerce_more: bool,
    awaiting: CoerceAwait,
}

/// What the machine's next value means to a `Coerce` in flight.
enum CoerceAwait {
    /// A queued slot's value, to be coerced, whether that position owes a
    /// separator after it, and its depth.
    Value(SepAfter, u32),
    /// The `__toString` attribute of this set, about to be applied to it, and
    /// the depth the set sits at.
    ToStrFn(Value, u32),
    /// What `__toString` returned. cppnix coerces that result with the same
    /// two flags rather than demanding a string, so it goes back through
    /// `render` and a set returned from `__toString` is stepped through again,
    /// one level down.
    ToStrResult(u32),
}

impl Coerce {
    /// A derivation attribute: a path is copied into the store first and
    /// coerces to the store path.
    pub fn copying(slot: Slot) -> Self {
        Coerce::with_flags(slot, true, true)
    }

    /// A set inside a string literal. `copyToStore` is on, because
    /// `ExprConcatStrings` passes it whenever the concatenation began with a
    /// string, and `coerceMore` is off, because interpolation refuses the
    /// things `toString` accepts.
    pub fn interpolating(slot: Slot) -> Self {
        Coerce::with_flags(slot, true, false)
    }

    /// `builtins.concatStringsSep`: every element coerced with cppnix's
    /// default flags (`primops.cc:5127`) and `sep` written between them.
    ///
    /// The elements are seeded as siblings at the top level rather than as one
    /// list value, because that is what they are in cppnix: the primop makes a
    /// separate `coerceToString` call per element (`primops.cc:5122`) instead
    /// of coercing the list. It matters for the depth bound -- a list would
    /// put every element one level down -- and for the separator, which is
    /// this one and not the space a list coercion writes.
    pub fn joining(items: &[Slot], sep: &crate::value2::NixStr) -> Self {
        let mut c = Coerce::with_flags(Slot::value(Value::Str(sep.clone())), true, false);
        // The separator's context goes in whether or not any element is
        // reached, which is what makes an empty list still depend on what the
        // separator depended on.
        c.context = sep.context_set();
        c.work.clear();
        for (i, s) in items.iter().enumerate().rev() {
            c.work.push(CoerceItem::Slot(s.clone(), SepAfter::No, 0));
            if i > 0 {
                c.work.push(CoerceItem::Lit(sep.bytes().to_vec()));
            }
        }
        c
    }

    /// An operand of `+`. `coerceMore` is off for the reason interpolation has
    /// it off, but `copyToStore` cannot be fixed here: cppnix reads it off the
    /// first part of the concatenation, so `"a" + ./f` copies `./f` and
    /// `{ outPath = "a"; } + ./f` does not.
    pub fn concatenating(slot: Slot, copy_to_store: bool) -> Self {
        Coerce::with_flags(slot, copy_to_store, false)
    }

    /// The tail of cppnix's `EvalState::coerceToPath` (`eval.cc`,
    /// `coerceToPath`), which every builtin taking a path argument reaches
    /// through `realisePath`: `coerceToString` with both flags off, then a
    /// check that what came out is absolute. The check is the caller's; this
    /// is the coercion.
    ///
    /// `copyToStore` off is the load-bearing half. `builtins.readFile
    /// { outPath = ./f; }` reads the source tree, not a store copy of it, and
    /// a coercion that copied would both write to the store and answer about
    /// a different file.
    pub fn to_path(slot: Slot) -> Self {
        Coerce::with_flags(slot, false, false)
    }

    /// What `builtins.toJSON` does with a `__toString` result:
    /// `tryAttrsToString(pos, v, context, false, false)`
    /// (`value-to-json.cc`, `printValueAsJSON`'s `nAttrs` case). Both flags
    /// off, so a set returned from `__toString` coerces on through its own
    /// `__toString` or `outPath`, and a path returned from it renders as the
    /// source path -- unlike the `nPath` case one arm up, which `toJSON`
    /// reaches with `copyToStore` on and which does copy.
    pub fn to_json_string(slot: Slot) -> Self {
        Coerce::with_flags(slot, false, false)
    }

    /// The coercion a primop performs on an argument, with the flags read
    /// off that primop's own `coerceToString` call. The constructor the
    /// builtin driver uses, so a builtin declares its coercion in
    /// [`crate::builtins::TABLE`] and has it run for it rather than
    /// hand-rolling one; see `ArgType::Coerce`.
    ///
    /// The named constructors above stay for the callers that are not
    /// primops. Picking one of them for a primop is how `findFile` came to
    /// coerce its search-path entries with `builtins.toString`'s flags, which
    /// accepts a list where cppnix raises a type error.
    pub fn as_primop(slot: Slot, flags: CoerceFlags) -> Self {
        Coerce::with_flags(slot, flags.copy_to_store, flags.coerce_more)
    }

    fn with_flags(slot: Slot, copy_to_store: bool, coerce_more: bool) -> Self {
        Coerce {
            work: vec![CoerceItem::Slot(slot, SepAfter::No, 0)],
            out: Vec::new(),
            context: std::collections::BTreeSet::new(),
            copy_to_store,
            coerce_more,
            awaiting: CoerceAwait::Value(SepAfter::No, 0),
        }
    }

    pub fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        if let Some(v) = incoming {
            match std::mem::replace(&mut self.awaiting, CoerceAwait::Value(SepAfter::No, 0)) {
                CoerceAwait::Value(sep, depth) => {
                    if let Some(y) = self.render(vm, &v, sep, depth)? {
                        return Ok(y);
                    }
                }
                // What `__toString` returned is coerced in the position the
                // set occupied, but the separator that position owed was
                // already queued against the set itself, which is the value
                // cppnix tests.
                CoerceAwait::ToStrResult(depth) => {
                    if let Some(y) = self.render(vm, &v, SepAfter::No, depth)? {
                        return Ok(y);
                    }
                }
                CoerceAwait::ToStrFn(attrs, depth) => {
                    self.awaiting = CoerceAwait::ToStrResult(depth);
                    return Ok(Yield::Apply(v, Slot::value(attrs)));
                }
            }
        }
        while let Some(item) = self.work.pop() {
            match item {
                CoerceItem::Lit(s) => self.out.extend_from_slice(&s),
                CoerceItem::Slot(s, sep, depth) => {
                    self.awaiting = CoerceAwait::Value(sep, depth);
                    return Ok(Yield::Force(s));
                }
            }
        }
        Ok(Yield::Done(Value::Str(
            crate::value2::NixStr::with_context(
                std::mem::take(&mut self.out),
                std::mem::take(&mut self.context),
            ),
        )))
    }

    /// `Some(yield)` means the machine has to answer something before this
    /// value can be written; `None` means it is written.
    fn render(
        &mut self,
        vm: &mut Vm,
        v: &Value,
        sep: SepAfter,
        depth: u32,
    ) -> Result<Option<Yield>> {
        // cppnix takes the guard on entry to `coerceToString`, before it looks
        // at the value, so the bound applies to every value it coerces and not
        // only to the ones that recurse.
        if depth > vm.max_call_depth() {
            // cppnix's wording, from EvalErrorStackOverflow.
            return Err(VmError::eval("stack overflow; max-call-depth exceeded"));
        }
        // Queued before the element's own work, so it pops after it, and
        // decided here because it depends on what the element turned out to
        // be: cppnix appends the space unless the element is an empty list
        // (`eval.cc`, `!v2->isList() || v2->listSize() != 0`).
        //
        // A *non-empty* list still gets its separator even when it coerces to
        // nothing, which is why the test is on the element and not on the
        // bytes it produced: `toString [ [[]] "b" ]` is `" b"`, with the
        // space, while `toString [ [] "b" ]` is `"b"` without it.
        if sep == SepAfter::Owed && !is_empty_list(v) {
            self.work.push(CoerceItem::Lit(b" ".to_vec()));
        }
        match v {
            // cppnix reaches a set before it consults `coerceMore`, so this
            // arm is not gated on the flag. `tryAttrsToString` first, which
            // applies `__toString` and coerces what it returns; then
            // `outPath`, which is how a derivation coerces and therefore how
            // `buildInputs = [ pkg ]` and `"${pkg}/bin/sh"` work.
            Value::Attrs(map) => {
                let to_string = vm.intern("__toString");
                if let Some(f) = map.get(&to_string) {
                    let f = f.clone();
                    self.awaiting = CoerceAwait::ToStrFn(v.clone(), depth + 1);
                    return Ok(Some(Yield::Force(f)));
                }
                let out_path = vm.intern("outPath");
                let Some(p) = map.get(&out_path) else {
                    return Err(VmError::eval("cannot coerce a set to a string"));
                };
                // Takes this position in the walk, so the coercion carries on
                // with whatever `outPath` turns out to be, under the same
                // flags. That is cppnix's tail call, and it is a call, so it
                // is one level down.
                self.work
                    .push(CoerceItem::Slot(p.clone(), SepAfter::No, depth + 1));
            }
            Value::List(_) if !self.coerce_more => {
                return Err(VmError::eval("cannot coerce a list to a string"));
            }
            Value::List(items) => {
                // Every element but the last owes a separator; whether it is
                // written is settled when that element is forced.
                // One level down and siblings of each other: cppnix coerces
                // each element with its own recursive call (`eval.cc:2686`),
                // so a long list is wide rather than deep.
                let last = items.len().saturating_sub(1);
                for (i, s) in items.iter().enumerate().rev() {
                    let sep = if i < last {
                        SepAfter::Owed
                    } else {
                        SepAfter::No
                    };
                    self.work.push(CoerceItem::Slot(s.clone(), sep, depth + 1));
                }
            }
            // The copy leaves through the scheduler, like every other
            // question this crate asks the world, and its answer is already
            // a string carrying the Opaque element for the path it produced
            // (`eval::answer_path`). So the answer arrives here as an
            // ordinary incoming value and needs no state of its own.
            Value::Path(p) if self.copy_to_store => {
                return Ok(Some(Yield::Need(crate::task::NeedPath::StorePath(
                    Rc::clone(p),
                ))));
            }
            other => {
                self.context.extend(crate::value2::context_of(other));
                let text = if self.coerce_more {
                    coerce_scalar(other)?
                } else {
                    // Without `coerceMore` only a string and a path coerce.
                    // Same wording as the permissive arm, because the differ
                    // reads an error's class off its text and both are
                    // cppnix's `cannot coerce ...` type error.
                    match other {
                        Value::Str(s) => s.bytes().to_vec(),
                        Value::Path(p) => p.as_bytes().to_vec(),
                        other => {
                            return Err(VmError::eval(format!(
                                "cannot coerce {} to a string",
                                type_name(other)
                            )));
                        }
                    }
                };
                self.out.extend_from_slice(&text);
            }
        }
        Ok(None)
    }
}

fn is_empty_list(v: &Value) -> bool {
    matches!(v, Value::List(items) if items.is_empty())
}

pub fn coerce_scalar(v: &Value) -> Result<Vec<u8>> {
    Ok(match v {
        Value::Str(s) => s.bytes().to_vec(),
        Value::Path(p) => p.as_bytes().to_vec(),
        Value::Int(n) => n.to_string().into_bytes(),
        // The coercion's rendering, not the printer's. cppnix uses
        // `std::to_string` here (`eval.cc:2657`) and `%.6g` when printing,
        // and the two agree on almost nothing.
        Value::Float(x) => format_f6(*x).into_bytes(),
        Value::Bool(true) => b"1".to_vec(),
        Value::Bool(false) => Vec::new(),
        Value::Null => Vec::new(),
        other => {
            return Err(VmError::eval(format!(
                "cannot coerce {} to a string",
                type_name(other)
            )));
        }
    })
}

/// Quoted-string escaping per cppnix printLiteralString: backslash, quote,
/// newline as \n, CR as \r, tab as \t, and `${` escaped as \${. Byte-wise,
/// as cppnix's loop is -- everything not escaped is copied verbatim, UTF-8
/// or not.
fn print_string(s: &[u8], out: &mut Vec<u8>) {
    out.push(b'"');
    let mut i = 0;
    while let Some(&c) = s.get(i) {
        match c {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            b'$' => {
                if s.get(i + 1) == Some(&b'{') {
                    out.extend_from_slice(b"\\$");
                } else {
                    out.push(b'$');
                }
            }
            c => out.push(c),
        }
        i += 1;
    }
    out.push(b'"');
}

/// Attr names print bare when they are valid identifiers, quoted otherwise.
pub(crate) fn print_attr_name(name: &str, out: &mut Vec<u8>) {
    let ident = !name.is_empty()
        && name
            .chars()
            .next()
            .map(|c| c.is_ascii_alphabetic() || c == '_')
            .unwrap_or(false)
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '\'' || c == '-')
        && !matches!(
            name,
            "if" | "then" | "else" | "assert" | "with" | "let" | "in" | "rec" | "inherit" | "or"
        );
    if ident {
        out.extend_from_slice(name.as_bytes());
    } else {
        print_string(name.as_bytes(), out);
    }
}

#[cfg(test)]
mod tests {
    use super::coerce_scalar;
    use crate::host::RealFs;
    use crate::session::{RenderMode, render};
    use crate::value2::Value;
    use crate::vm::Vm;

    #[test]
    fn fanout_offer_preserves_order_across_literals_and_nested_pushes() {
        use super::{Item, Worklist};
        use crate::value2::Slot;

        let mut work = Worklist::default();
        for _ in 0..(crate::vm::FANOUT_WIDTH + 3) {
            work.push(Item::Slot(Slot::value(Value::Null)));
            work.push(Item::Lit(b" ]".to_vec()));
        }
        // A nested container queues its children above the existing siblings.
        work.push(Item::Slot(Slot::value(Value::Int(100))));
        work.push(Item::Lit(b" ".to_vec()));
        while !work.items.is_empty() {
            let original = work
                .items
                .iter()
                .rev()
                .filter_map(|item| match item {
                    Item::Slot(slot) => Some(slot.id()),
                    Item::Lit(_) => None,
                })
                .take(crate::vm::FANOUT_WIDTH)
                .collect::<Vec<_>>();
            assert_eq!(
                work.pending_values()
                    .iter()
                    .map(Slot::id)
                    .collect::<Vec<_>>(),
                original
            );
            let _ = work.pop();
        }
        assert!(work.pending_values().is_empty());
    }

    #[test]
    fn literal_depth_does_not_hide_pending_siblings() {
        use super::{Item, Worklist};
        use crate::value2::Slot;

        let sibling = Slot::value(Value::Int(7));
        let mut work = Worklist::default();
        work.push(Item::Slot(sibling.clone()));
        for _ in 0..100_000 {
            work.push(Item::Lit(b" ]".to_vec()));
        }
        assert_eq!(
            work.pending_values()
                .iter()
                .map(Slot::id)
                .collect::<Vec<_>>(),
            vec![sibling.id()]
        );
        assert_eq!(work.values.len(), 1);
        while work.pop().is_some() {}
        assert!(work.pending_values().is_empty());
    }

    /// One expression through both plain printers: the dialect
    /// `nix-instantiate --eval --strict` uses and the one `nix eval` uses.
    /// Both are pinned because both hung on a self-referential value -- the
    /// `nix eval` side refuses derivation-shaped sets by name, which is not
    /// the same shield as terminating.
    ///
    /// A failure renders as its debug form rather than panicking, the way
    /// `eval::tests::render` does: the workspace denies `panic`, tests
    /// included.
    fn print_both(src: &str) -> (String, String) {
        (
            print_with(src, RenderMode::Plain),
            print_with(src, RenderMode::ValuePrinter),
        )
    }

    /// `nix-instantiate --eval` without `--strict`: a value with no children
    /// prints as it does strictly, a value with children is refused by name
    /// rather than printed with `<CODE>` markers this side cannot place
    /// where cppnix does.
    #[test]
    fn lazy_plain_printing_serves_scalars_and_refuses_the_rest() {
        assert_eq!(print_with("1 + 1", RenderMode::PlainLazy), "2");
        assert_eq!(print_with(r#""s""#, RenderMode::PlainLazy), r#""s""#);
        assert_eq!(print_with("true", RenderMode::PlainLazy), "true");
        assert_eq!(print_with("null", RenderMode::PlainLazy), "null");
        let list = print_with("[ (1 + 1) ]", RenderMode::PlainLazy);
        assert!(
            list.contains("lazy top-level printing of a list"),
            "a list is refused by name, not printed: {list}"
        );
        let set = print_with("{ a = 1; }", RenderMode::PlainLazy);
        assert!(
            set.contains("lazy top-level printing of a set"),
            "a set is refused by name, not printed: {set}"
        );
    }

    fn print_with(src: &str, mode: RenderMode) -> String {
        let module = match crate::compile::compile_source(
            src,
            ".",
            crate::compile::Origin::String,
            &crate::eval::Settings::default(),
        ) {
            Ok(m) => std::rc::Rc::new(m),
            Err(e) => return format!("compile: {e:?}"),
        };
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let host = RealFs;
        vm.start_module(&module);
        let value = match crate::eval::drive(&mut vm, &host) {
            Ok(v) => v,
            Err(e) => return format!("eval: {e:?}"),
        };
        match render(&mut vm, &host, value, mode) {
            Ok(s) => String::from_utf8_lossy(&s).into_owned(),
            Err(e) => format!("render: {e:?}"),
        }
    }

    /// A value that contains itself. Both dialects walked one forever before
    /// this (ENG-12517), which is what binding the `derivation` global
    /// produces: the wrapper's result has `all = [ <the result> ]`.
    ///
    /// Every expected string here is what nix 2.34.7+ix printed for the same
    /// expression on the cpp backend.
    #[test]
    fn a_self_referential_value_prints_repeated_in_both_dialects() {
        let want = "{ a = 1; self = «repeated»; }".to_owned();
        assert_eq!(
            print_both("let x = { a = 1; self = x; }; in x"),
            (want.clone(), want)
        );
    }

    /// The two container kinds are keyed differently, so they do not repeat
    /// in the same place, and getting this wrong is invisible until a value
    /// holds one twice. An attribute set is keyed on its bindings, so the
    /// second cell holding one map repeats. A list is keyed on the cell, so
    /// the binding's own cell prints in full before its self-reference
    /// repeats -- keying lists on their elements would collapse that to
    /// `[ 1 «repeated» ]`.
    #[test]
    fn attrsets_repeat_on_their_bindings_and_lists_on_their_cell() {
        assert_eq!(
            print_both("let s = { a = 1; }; in { p = s; q = s; }"),
            (
                "{ p = { a = 1; }; q = «repeated»; }".to_owned(),
                "{ p = { a = 1; }; q = «repeated»; }".to_owned()
            )
        );
        assert_eq!(
            print_both("let l = [ 1 l ]; in l"),
            (
                "[ 1 [ 1 «repeated» ] ]".to_owned(),
                "[ 1 [ 1 «repeated» ] ]".to_owned()
            )
        );
        // Two attribute-set entries reading one binding share the cell, the
        // way cppnix's `ExprVar::maybeThunk` hands back the same `Value *`
        // rather than a fresh thunk, so the list repeats here too.
        assert_eq!(
            print_both("let l = [ 1 2 ]; in { p = l; q = l; }").0,
            "{ p = [ 1 2 ]; q = «repeated»; }"
        );
    }

    /// An empty container is never marked: `printAmbiguous` guards on
    /// `!v.attrs()->empty()` and on `v.listSize()`.
    ///
    /// The container has to be *shared* to test this. Two `{}` literals are
    /// two allocations here, so they cannot collide whether they are marked
    /// or not, and an earlier version of this test asserted on those and
    /// passed with the guard deleted. `let e = {}; in ...` is one allocation
    /// reached twice, which is the case the guard decides.
    #[test]
    fn a_shared_empty_container_never_repeats() {
        assert_eq!(
            print_both("let e = {}; in { a = e; b = e; }").0,
            "{ a = { }; b = { }; }"
        );
        assert_eq!(
            print_both("let e = []; in { a = e; b = e; }").0,
            "{ a = [ ]; b = [ ]; }"
        );
        assert_eq!(print_both("let e = {}; in [ e e ]").0, "[ { } { } ]");
    }

    /// Sharing is not a cycle. A value reached twice down separate branches
    /// still prints its bytes once and repeats after, and a value merely
    /// equal to another does not repeat at all -- the key is identity, not
    /// structure.
    #[test]
    fn equal_but_distinct_values_do_not_repeat() {
        assert_eq!(
            print_both("{ p = { a = 1; }; q = { a = 1; }; }").0,
            "{ p = { a = 1; }; q = { a = 1; }; }"
        );
    }

    /// An attribute set coerces through `__toString` and then `outPath`, which
    /// is the mechanism `buildInputs = [ pkg ]` and `"${pkg}/bin/sh"` run on.
    ///
    /// Every expected value is what nix 2.34.7 answered for the same
    /// expression on the cpp backend, on dev-compute-4.
    #[test]
    fn a_set_coerces_through_to_string_then_out_path() {
        for (src, want) in [
            (r#"builtins.toString { outPath = "x"; }"#, r#""x""#),
            (r#"builtins.toString { __toString = self: "y"; }"#, r#""y""#),
            // `__toString` wins over `outPath`: cppnix tries
            // `tryAttrsToString` before it looks for `outPath`.
            (
                r#"builtins.toString { __toString = self: "fn"; outPath = "op"; }"#,
                r#""fn""#,
            ),
            // The result of `__toString` is coerced with the same flags, not
            // required to be a string already.
            (r#"builtins.toString { __toString = self: 42; }"#, r#""42""#),
            // `outPath` is a tail call, so a set behind a set resolves.
            (
                r#"builtins.toString { outPath = { outPath = "deep"; }; }"#,
                r#""deep""#,
            ),
        ] {
            assert_eq!(print_both(src).0, want, "coercing {src}");
        }
    }

    /// The set case sits before cppnix's `coerceMore` test, so interpolation
    /// coerces a set even though it refuses the scalars `toString` accepts.
    /// Getting that ordering wrong is not visible in `toString`, which sets
    /// the flag, and is exactly what `"${pkg}"` depends on.
    #[test]
    fn interpolation_coerces_a_set_but_still_refuses_a_number() {
        assert_eq!(
            print_both(r#""pre-${{ outPath = "x"; }}-post""#).0,
            r#""pre-x-post""#
        );
        assert_eq!(
            print_both(r#""x${{ __toString = self: "t"; }}""#).0,
            r#""xt""#
        );
        // cppnix: `cannot coerce an integer to a string`.
        assert!(
            print_both(r#""n${1}""#)
                .0
                .contains("cannot coerce an integer"),
            "interpolating an integer must still be refused"
        );
        // A set with neither attribute is a type error on both arms, not a
        // silent empty string.
        assert!(
            print_both(r#""x${{ a = 1; }}""#)
                .0
                .contains("cannot coerce a set"),
            "a set with no outPath must still be refused"
        );
    }

    /// cppnix drops the separator after a list element that is an empty list
    /// (`eval.cc`: `n < size - 1 && (!v2->isList() || v2->listSize() != 0)`),
    /// and this crate joined with a plain separator-before-every-element, so
    /// it emitted a space cppnix does not (ENG-12527).
    ///
    /// The test is on the *element*, not on the bytes it produced, and these
    /// two cases are what force that distinction: a non-empty list that
    /// coerces to nothing still gets its separator, while an empty one does
    /// not. A single "did the last thing write anything" flag passes the easy
    /// cases and gets both of these wrong.
    ///
    /// Every expected value is what nix 2.34.7 answered on the cpp backend,
    /// dev-compute-4.
    #[test]
    fn a_coerced_list_drops_the_separator_after_an_empty_list() {
        for (src, want) in [
            (r#"builtins.toString [ [] "b" ]"#, r#""b""#),
            (r#"builtins.toString [ "a" [] "b" ]"#, r#""a b""#),
            (r#"builtins.toString [ "a" "b" ]"#, r#""a b""#),
            // Non-empty list, coerces to nothing, separator kept.
            (r#"builtins.toString [ [[]] "b" ]"#, r#"" b""#),
            (r#"builtins.toString [ [ [] ] [] "b" ]"#, r#"" b""#),
            // A trailing empty element is last, so it owed no separator and
            // the space before it stays.
            (r#"builtins.toString [ "a" [] ]"#, r#""a ""#),
            (r#"builtins.toString [ [] [] "b" ]"#, r#""b""#),
            (r#"builtins.toString [ "a" [] [] "b" ]"#, r#""a b""#),
            (r#"builtins.toString [ [1 2] "b" ]"#, r#""1 2 b""#),
            (r#"builtins.toString [ [] ]"#, r#""""#),
            (r#"builtins.toString []"#, r#""""#),
            // The separator is decided on the element, so a set that coerces
            // through outPath still gets one.
            (
                r#"builtins.toString [ "a" { outPath = "p"; } "b" ]"#,
                r#""a p b""#,
            ),
        ] {
            assert_eq!(print_both(src).0, want, "coercing {src}");
        }
    }

    /// A set whose `outPath` is itself is a walk with no bottom. cppnix bounds
    /// the recursion with `addCallDepth`; without a matching bound here the
    /// coercion would hang, which is the failure this file just finished
    /// removing from the printer and not one to reintroduce next door.
    #[test]
    fn a_self_referential_out_path_is_refused_rather_than_walked() {
        let out = print_both("let a = { outPath = a; }; in builtins.toString a").0;
        assert!(
            out.contains("stack overflow"),
            "expected a max-call-depth refusal, got {out}"
        );
    }

    /// cppnix renders a float one way when it prints it and another when it
    /// coerces it, and only the coercion is hashed into a store path. Every
    /// value here is what `nix-instantiate --eval --strict -E 'builtins.toString
    /// (x)'` printed on nix 2.34.7+ix.g69e4d9e9db39.
    ///
    /// The two renderings agree on no value in this list, which is why the
    /// wrong one survived: nothing in the corpus coerces a float, so the
    /// divergence was invisible until a float became a derivation attribute
    /// and moved its `outPath`.
    #[test]
    fn a_coerced_float_is_cppnix_s_std_to_string() {
        for (value, want) in [
            (1.5, "1.500000"),
            (1.0, "1.000000"),
            (0.1, "0.100000"),
            (0.0, "0.000000"),
            (1.0e10, "10000000000.000000"),
            (123_456_789.5, "123456789.500000"),
        ] {
            assert_eq!(
                coerce_scalar(&Value::Float(value)).ok(),
                Some(want.as_bytes().to_vec()),
                "coercing {value}"
            );
            // The printer's rendering is the other one, and swapping them is
            // the mistake this pins.
            assert_ne!(crate::value2::format_g6(value), want, "printing {value}");
        }
    }
}
