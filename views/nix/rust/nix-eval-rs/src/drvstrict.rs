//! `builtins.derivationStrict`: the part of producing a derivation that needs
//! a VM.
//!
//! [`crate::drv`] renders the ATerm and [`crate::drvpath`] computes the paths;
//! both are pure functions of strings and are measured against a real store.
//! What is left, and what lives here, is turning an attribute set into those
//! strings: forcing each attribute, coercing it with its context, reading the
//! `outputs` list, and translating the accumulated context into the input
//! sources and input derivations the builder takes.
//!
//! The oracle is cppnix's `derivationStrictInternal` (`primops.cc:1532`), and
//! the order of operations there is load-bearing in two ways this mirrors
//! exactly: which attribute is forced first decides which error a broken
//! derivation reports, and the attributes are walked in **name order**
//! (`Bindings::lexicographicOrder`), not in symbol-id order.
//!
//! Everything this does not cover refuses by name. A derivation whose path is
//! wrong is indistinguishable from one whose path is right, so a guess here
//! would be a silent divergence in the one place the whole rung is about.

use crate::drv::Derivation;
use crate::drvpath::{
    BuildError, BuiltDerivation, CaMethod, DerivationInputs, DrvSource, HashError,
    build_content_addressed, build_fixed_output, build_input_addressed, hash_derivation_modulo,
};
use crate::nixhash::{HashAlgo, HashError as HashParseError, new_hash_allow_empty, parse_algo_opt};
use crate::primops_pure::{
    Begin, Cont, text_of, want_bool, want_list, want_nix_str, want_text_no_ctx,
};
use crate::refusal::{Refusal, RefusalToken};
use crate::task::{NeedPath, Task, Yield};
use crate::value2::{Attrs, ContextElem, NixStr, Slot, Sym, Value, type_name};
use crate::vm::{Result, Vm, VmError};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

/// cppnix's `derivationStrictInternal` reads these attributes for something
/// other than "put it in the environment". Kept as one list so the walk below
/// and the refusals agree about which names are special.
mod attr {
    pub const NAME: &str = "name";
    pub const BUILDER: &str = "builder";
    pub const SYSTEM: &str = "system";
    pub const ARGS: &str = "args";
    pub const OUTPUTS: &str = "outputs";
    pub const IGNORE_NULLS: &str = "__ignoreNulls";
    pub const STRUCTURED_ATTRS: &str = "__structuredAttrs";
    pub const CONTENT_ADDRESSED: &str = "__contentAddressed";
    pub const IMPURE: &str = "__impure";
    pub const JSON: &str = "__json";
    pub const OUTPUT_HASH: &str = "outputHash";
    pub const OUTPUT_HASH_ALGO: &str = "outputHashAlgo";
    pub const OUTPUT_HASH_MODE: &str = "outputHashMode";
}

/// `StructuredAttrs::envVarName` (`parsed-derivations.hh:20`): the one
/// environment variable a structured derivation has beyond its outputs.
const STRUCTURED_ATTRS_ENV: &str = "__json";

/// Where the walk is: what the value the machine is about to hand back means.
enum Stage {
    /// The `name` attribute, forced before anything else so that a derivation
    /// with a broken name reports that rather than whatever its first
    /// attribute happens to be (`primops.cc:1443`).
    Name,
    /// `__structuredAttrs`, forced next, because it changes how every other
    /// attribute is read.
    StructuredAttrs,
    /// `__ignoreNulls`, forced last of the three, for the same reason.
    IgnoreNulls,
    /// The current attribute's own value, forced. cppnix forces it whichever
    /// branch it takes -- `forceBool`, `forceList` and `coerceToString` all
    /// do -- so forcing once up front and dispatching on the result is the
    /// same forcing order with one less state.
    AttrValue,
    /// That value, coerced to a string with its context.
    AttrCoerced,
    /// One element of the `args` list, coerced.
    ArgElement,
    /// Under `__structuredAttrs`, the current attribute rendered as JSON.
    /// cppnix renders it before reading the handful of names it also wants as
    /// strings, so a value that cannot be JSON reports that first.
    AttrJson,
    /// Under `__structuredAttrs`, one element of the `outputs` list. cppnix
    /// reads that attribute as a list of strings there, where the flat form
    /// coerces the whole thing and splits on whitespace.
    OutputName,
    /// The warning about an attribute `__structuredAttrs` disables has been
    /// reported; the value is still waiting to be read for its name.
    AttrWarned,
    /// `newHashAllowEmpty` warned about an empty `outputHash`; the finished
    /// attribute set is held until the warning is answered.
    EmptyHashWarned,
    /// The `.drv` has been handed to the embedder to write and the incoming
    /// value is the store path it landed on. Both branches pass through here,
    /// because both produce a `.drv` and cppnix writes both.
    DrvWritten,
}

/// A `derivationStrict` in flight.
pub struct DrvStrict {
    stage: Stage,
    /// Attributes still to walk, in reverse name order so `pop` yields them
    /// in cppnix's `lexicographicOrder`.
    pending: Vec<(String, Slot)>,
    /// The attribute currently being read.
    current: String,
    /// The three attributes cppnix forces before the walk, held as slots
    /// because the walk's own list is consumed as it goes and the prologue
    /// must not consume it. Absent means the attribute is not there, which is
    /// a different thing from being there and false.
    name_slot: Slot,
    structured_attrs_slot: Option<Slot>,
    ignore_nulls_slot: Option<Slot>,
    name: String,
    ignore_nulls: bool,
    /// Everything every attribute's coercion depended on, which is what
    /// becomes `inputSrcs` and `inputDrvs` (`primops.cc:1780`).
    context: BTreeSet<ContextElem>,
    env: BTreeMap<String, String>,
    args: Vec<String>,
    /// The `args` list mid-coercion, and how far through it we are.
    arg_slots: Rc<Vec<Slot>>,
    arg_index: usize,
    builder: String,
    platform: String,
    /// cppnix seeds this with `out` and `handleOutputs` replaces it wholesale
    /// (`primops.cc:1572`), so an `outputs` attribute is not additive.
    outputs: BTreeSet<String>,
    /// `__structuredAttrs = true`: every attribute becomes a member of one
    /// JSON object rather than an environment variable of its own.
    structured: bool,
    /// That object, keyed by attribute name. A `BTreeMap` because cppnix's
    /// is a `nlohmann::json::object_t`, i.e. a `std::map`, and `dump()` emits
    /// it in that order: the key order is inside the bytes the derivation
    /// hashes, so it is not a presentation detail.
    structured_attrs: BTreeMap<String, String>,
    /// The current attribute's forced value, kept across the JSON rendering
    /// because the structured branch reads some names off the value a second
    /// time after rendering it.
    current_value: Option<Value>,
    /// The `outputs` list mid-walk, under `__structuredAttrs`.
    /// The `outputHash` attribute's bytes, if it was present. Its *presence*
    /// is what switches cppnix onto the fixed-output branch, so this is an
    /// `Option` and not an empty-string sentinel: an empty `outputHash` is a
    /// real, accepted case that means something else (`newHashAllowEmpty`).
    output_hash: Option<String>,
    /// `parseHashAlgoOpt(outputHashAlgo)`. `None` covers both "absent" and
    /// "not a name cppnix knows", which behave identically downstream: the
    /// algorithm then has to come from the hash string itself.
    output_hash_algo: Option<HashAlgo>,
    /// `__contentAddressed` was true. The feature gate was already checked
    /// where the attribute was read, as cppnix does (`primops.cc:1631`).
    content_addressed: bool,
    /// `outputHashMode`, parsed. `None` means absent and defaults to `Flat`.
    ingestion_method: Option<CaMethod>,
    /// `newHashAllowEmpty`'s warning, held until it can be yielded.
    empty_hash_warning: Option<String>,
    /// The enclosing walk's fan-out offer, set aside at this walk's first
    /// publish and put back when the attribute walk finishes
    /// ([`Vm::save_fanout_offer`]). This walk runs nested inside the walks
    /// that force derivations -- a printed or `toJSON`-rendered list of
    /// imports is the common case -- and overwriting their offer for good
    /// is what serialized import-from-derivation (ENG-13150).
    saved_offer: Option<std::collections::VecDeque<Slot>>,
    /// The finished derivation, held across the write. Only the three things
    /// the answer is checked against and the result is built from, rather
    /// than the whole `BuiltDerivation`: the ATerm and the `Derivation` have
    /// already been handed over by then and keeping them would be two copies
    /// of bytes that are the largest thing here.
    built_drv_path: String,
    built_outputs: BTreeMap<String, String>,
    output_slots: Rc<Vec<Slot>>,
    output_index: usize,
    output_names: Vec<String>,
}

/// `builtins.derivationStrict`, arity 1.
pub fn bi_derivation_strict(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let attrs = crate::primops_pure::argv(args, 0)?;
    let Value::Attrs(map) = &attrs else {
        return Err(VmError::eval(format!(
            "expected a set but found {}",
            type_name(&attrs)
        )));
    };

    // cppnix's `lexicographicOrder`: by the symbol's name, not its id. The
    // walk order is observable through which attribute's error is reported
    // first, and through nothing else, because every result this builds is a
    // sorted container.
    let mut entries: Vec<(String, Slot)> = map
        .iter()
        .map(|(k, s)| (vm.sym_name(*k).to_owned(), s.clone()))
        .collect();
    entries.sort_by(|a, b| a.0.cmp(&b.0));

    let slot_named = |want: &str| {
        entries
            .iter()
            .find(|(n, _)| n == want)
            .map(|(_, s)| s.clone())
    };
    // cppnix's `getAttr` (`eval.cc:2479`), whose wording the corpus compares.
    let name_slot =
        slot_named(attr::NAME).ok_or_else(|| VmError::eval("attribute 'name' missing"))?;
    let structured_attrs_slot = slot_named(attr::STRUCTURED_ATTRS);
    let ignore_nulls_slot = slot_named(attr::IGNORE_NULLS);

    entries.reverse();
    let mut outputs = BTreeSet::new();
    outputs.insert("out".to_owned());

    Ok(Begin::Cont(Cont::Ext(crate::primops_host::Ext::DrvStrict(
        Box::new(DrvStrict {
            stage: Stage::Name,
            pending: entries,
            current: String::new(),
            name_slot,
            structured_attrs_slot,
            ignore_nulls_slot,
            name: String::new(),
            ignore_nulls: false,
            context: BTreeSet::new(),
            env: BTreeMap::new(),
            args: Vec::new(),
            arg_slots: Rc::new(Vec::new()),
            arg_index: 0,
            builder: String::new(),
            platform: String::new(),
            outputs,
            structured: false,
            structured_attrs: BTreeMap::new(),
            current_value: None,
            output_hash: None,
            output_hash_algo: None,
            content_addressed: false,
            ingestion_method: None,
            empty_hash_warning: None,
            built_drv_path: String::new(),
            built_outputs: BTreeMap::new(),
            output_slots: Rc::new(Vec::new()),
            output_index: 0,
            output_names: Vec::new(),
            saved_offer: None,
        }),
    ))))
}

impl DrvStrict {
    /// The first step has no incoming value and asks for `name`; every later
    /// one is handed exactly what the previous yield asked for.
    pub fn step(&mut self, vm: &mut Vm, incoming: Option<Value>) -> Result<Yield> {
        let Some(value) = incoming else {
            self.stage = Stage::Name;
            return Ok(Yield::Force(self.name_slot.clone()));
        };
        match self.stage {
            Stage::Name => {
                // `forceStringNoCtx`: a name carrying a context would put a
                // store path in a store path's name.
                self.name = want_text_no_ctx(&value)?;
                check_derivation_name(&self.name)?;
                self.prologue(vm)
            }
            Stage::StructuredAttrs => {
                self.structured = want_bool(&value)?;
                self.after_structured_attrs(vm)
            }
            Stage::IgnoreNulls => {
                self.ignore_nulls = want_bool(&value)?;
                self.next_attr(vm)
            }
            Stage::AttrValue => self.dispatch_attr(vm, value),
            Stage::AttrCoerced => {
                let text = self.take_string(&value)?;
                self.finish_attr(vm, text)
            }
            Stage::ArgElement => {
                let text = self.take_string(&value)?;
                self.args.push(text);
                self.arg_index += 1;
                self.next_arg(vm)
            }
            Stage::AttrJson => self.finish_json_attr(vm, &value),
            Stage::AttrWarned => {
                // The incoming value is the warning's `null`, not an
                // attribute; the attribute is the one held back.
                let Some(held) = self.current_value.take() else {
                    return Err(VmError::eval("internal: a warned attribute lost its value"));
                };
                self.read_structured_name(vm, &held)
            }
            Stage::EmptyHashWarned => {
                let Some(result) = self.current_value.take() else {
                    return Err(VmError::eval(
                        "internal: the empty-hash warning lost its result",
                    ));
                };
                Ok(Yield::Done(result))
            }
            Stage::DrvWritten => self.after_drv_written(vm, &value),
            Stage::OutputName => {
                self.output_names.push(want_text_no_ctx(&value)?);
                self.output_index += 1;
                self.next_output(vm)
            }
        }
    }

    /// `__structuredAttrs` then `__ignoreNulls`, each forced only when
    /// present, then the walk.
    fn prologue(&mut self, vm: &mut Vm) -> Result<Yield> {
        match self.structured_attrs_slot.take() {
            Some(slot) => {
                self.stage = Stage::StructuredAttrs;
                Ok(Yield::Force(slot))
            }
            None => self.after_structured_attrs(vm),
        }
    }

    fn after_structured_attrs(&mut self, vm: &mut Vm) -> Result<Yield> {
        match self.ignore_nulls_slot.take() {
            Some(slot) => {
                self.stage = Stage::IgnoreNulls;
                Ok(Yield::Force(slot))
            }
            None => self.next_attr(vm),
        }
    }

    /// Take the next attribute off the list, or, when there is none left,
    /// finish.
    fn next_attr(&mut self, vm: &mut Vm) -> Result<Yield> {
        loop {
            let Some((name, slot)) = self.pending.pop() else {
                return self.finish(vm);
            };
            // cppnix skips this one at the top of the loop, so it never
            // becomes an environment variable (`primops.cc:1575`).
            if name == attr::IGNORE_NULLS {
                continue;
            }
            self.current = name;
            self.stage = Stage::AttrValue;
            // The attributes after this one, in cppnix's lexicographic walk
            // order, offered so the scheduler can seed them as sibling
            // strands if this one parks on a slow question the host began --
            // typically import-from-derivation realising an input
            // (ENG-13150). An offer and not a spawn: under a host that
            // begins nothing it is only ever replaced, and the walk stays
            // exactly sequential. The enclosing walk's offer is set aside
            // at the first publish and restored by `finish`.
            if self.saved_offer.is_none() {
                self.saved_offer = Some(vm.save_fanout_offer());
            }
            vm.set_fanout_offer(self.pending_attrs());
            return Ok(Yield::Force(slot));
        }
    }

    /// The attributes [`DrvStrict::next_attr`] would force next, in order,
    /// skipping the one it skips. Capped at [`crate::vm::FANOUT_WIDTH`] so
    /// republishing at every attribute force stays O(1).
    fn pending_attrs(&self) -> Vec<Slot> {
        self.pending
            .iter()
            .rev()
            .filter(|(name, _)| name != attr::IGNORE_NULLS)
            .map(|(_, slot)| slot.clone())
            .take(crate::vm::FANOUT_WIDTH)
            .collect()
    }

    /// The forced value of the current attribute, routed the way cppnix's
    /// switch routes it.
    fn dispatch_attr(&mut self, vm: &mut Vm, value: Value) -> Result<Yield> {
        // Under `__ignoreNulls` a null attribute is dropped entirely: no
        // environment variable, no context, no coercion.
        if self.ignore_nulls && matches!(value, Value::Null) {
            return self.next_attr(vm);
        }
        match self.current.as_str() {
            // Both of these are read for their boolean and never reach the
            // environment. Refused when set, because the output paths of a
            // content-addressed or impure derivation are not what
            // `build_input_addressed` computes, and answering with an
            // input-addressed path would be a wrong path rather than a
            // missing feature.
            attr::CONTENT_ADDRESSED => {
                if want_bool(&value)? {
                    // cppnix requires the feature at this exact point
                    // (`primops.cc:1632`), so the error, its wording and the
                    // moment it fires are all its
                    // (`MissingExperimentalFeature`,
                    // `experimental-features.cc:411`).
                    if !vm.settings().ca_derivations {
                        return Err(VmError::eval(
                            "experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it",
                        ));
                    }
                    self.content_addressed = true;
                }
                self.next_attr(vm)
            }
            attr::IMPURE => {
                if want_bool(&value)? {
                    return Err(unimplemented("__impure = true"));
                }
                self.next_attr(vm)
            }
            attr::ARGS => {
                self.arg_slots = want_list(&value)?;
                self.arg_index = 0;
                self.next_arg(vm)
            }
            // cppnix skips this one inside the structured branch
            // (`primops.cc:1660`), so the flag does not become a member of
            // the object it turned on. Outside that branch it is an ordinary
            // attribute and does reach the environment, which is why the skip
            // is here and not in `next_attr`.
            attr::STRUCTURED_ATTRS if self.structured => self.next_attr(vm),
            _ if self.structured => {
                // Rendered before anything else reads it, as cppnix does
                // (`primops.cc:1662`), so a value that has no JSON form
                // reports that rather than whatever the name-specific read
                // would have said.
                self.current_value = Some(value.clone());
                self.stage = Stage::AttrJson;
                let idx = crate::builtins::global_index("toJSON").ok_or_else(|| {
                    VmError::eval("internal: no toJSON builtin to render a structured attribute")
                })?;
                Ok(Yield::sub(Task::builtin(idx, vec![Slot::value(value)])))
            }
            _ => {
                self.stage = Stage::AttrCoerced;
                // cppnix coerces a derivation attribute with `coerceMore` on
                // and `copyToStore` left at its default of on
                // (`primops.cc:1728`), so `src = ./f` copies the tree in and
                // the attribute becomes the store path. `toString` is the
                // other setting of the same function and must not be
                // substituted for it.
                Ok(Yield::sub(Task::coerce_copying(Slot::value(value))))
            }
        }
    }

    /// Coerce the next `args` element, or move on.
    fn next_arg(&mut self, vm: &mut Vm) -> Result<Yield> {
        match self.arg_slots.get(self.arg_index) {
            Some(slot) => {
                self.stage = Stage::ArgElement;
                Ok(Yield::sub(Task::coerce_copying(slot.clone())))
            }
            None => {
                self.arg_slots = Rc::new(Vec::new());
                self.next_attr(vm)
            }
        }
    }

    /// A coerced attribute: its bytes become an environment variable and, for
    /// the handful of names cppnix also reads, a field of the derivation.
    fn finish_attr(&mut self, vm: &mut Vm, text: String) -> Result<Yield> {
        match self.current.as_str() {
            attr::BUILDER => self.builder.clone_from(&text),
            attr::SYSTEM => self.platform.clone_from(&text),
            // The presence of `outputHash` alone switches cppnix onto the
            // fixed-output branch (`primops.cc:1853`), which is why this is
            // recorded rather than inspected here.
            attr::OUTPUT_HASH => self.output_hash = Some(text.clone()),
            attr::OUTPUT_HASH_ALGO => {
                self.output_hash_algo = parse_algo(&text, vm.settings().blake3_hashes)?;
            }
            attr::OUTPUT_HASH_MODE => self.set_ingestion_method(&text)?,
            attr::JSON => return Err(unimplemented("__json (deprecated structured attributes)")),
            attr::OUTPUTS => self.set_outputs(&text)?,
            _ => {}
        }
        self.env.insert(self.current.clone(), text);
        self.next_attr(vm)
    }

    /// cppnix warns about each of these when `__structuredAttrs` is on,
    /// because the attribute is read from the environment by the build hook
    /// and a structured derivation has no such environment variable: the
    /// setting is silently ignored (`primops.cc:1693`). Warning about it is
    /// the only thing that tells the reader their derivation does not do what
    /// it says, so it is carried rather than dropped.
    const DISABLED_BY_STRUCTURED_ATTRS: &'static [&'static str] = &[
        "allowedReferences",
        "allowedRequisites",
        "disallowedReferences",
        "disallowedRequisites",
        "maxSize",
        "maxClosureSize",
    ];

    /// The current attribute, rendered as JSON. cppnix stores it and then
    /// re-reads the same few names off the value, so this does too rather
    /// than parsing them back out of the JSON.
    fn finish_json_attr(&mut self, vm: &mut Vm, json: &Value) -> Result<Yield> {
        let rendered = want_nix_str(json)?;
        // Everything the value depended on. `printValueAsJSON` accumulates
        // into the same context the rest of the walk fills, so a structured
        // attribute holding a store path is an input like any other.
        if let Some(context) = rendered.context() {
            self.context.extend(context.iter().cloned());
        }
        // The dump went through `json_string`'s strict UTF-8 walk, so its
        // bytes are text by construction.
        self.structured_attrs
            .insert(self.current.clone(), text_of(rendered)?.to_owned());

        let Some(value) = self.current_value.take() else {
            return Err(VmError::eval(
                "internal: a structured attribute lost its value",
            ));
        };
        if Self::DISABLED_BY_STRUCTURED_ATTRS.contains(&self.current.as_str()) {
            let message = format!(
                "In a derivation named '{}', 'structuredAttrs' disables the effect of the \
                 derivation attribute '{}'; use 'outputChecks.<output>.{}' instead",
                self.name, self.current, self.current
            );
            // Yielded rather than printed: the VM performs no IO, and a
            // warning written from here would be invisible to a read set.
            // The walk continues from `Stage::AttrJson` with the value
            // already taken, so the name-specific reads below happen on the
            // way back -- which is why this comes last.
            self.current_value = Some(value);
            self.stage = Stage::AttrWarned;
            return Ok(Yield::Need(NeedPath::Warn(message)));
        }
        self.read_structured_name(vm, &value)
    }

    /// The names cppnix reads off the value again inside the structured
    /// branch (`primops.cc:1664`). Everything else is only a JSON member.
    fn read_structured_name(&mut self, vm: &mut Vm, value: &Value) -> Result<Yield> {
        match self.current.as_str() {
            // `forceString`, which keeps a context, unlike the flat branch's
            // coercion only because the coercion already happened.
            attr::BUILDER => {
                let s = want_nix_str(value)?;
                if let Some(context) = s.context() {
                    self.context.extend(context.iter().cloned());
                }
                self.builder = text_of(s)?.to_owned();
            }
            attr::SYSTEM => self.platform = want_text_no_ctx(value)?,
            attr::OUTPUT_HASH => self.output_hash = Some(want_text_no_ctx(value)?),
            attr::OUTPUT_HASH_ALGO => {
                self.output_hash_algo =
                    parse_algo(&want_text_no_ctx(value)?, vm.settings().blake3_hashes)?;
            }
            attr::OUTPUT_HASH_MODE => {
                let mode = want_text_no_ctx(value)?;
                self.set_ingestion_method(&mode)?;
            }
            attr::OUTPUTS => {
                self.output_slots = want_list(value)?;
                self.output_index = 0;
                self.output_names.clear();
                return self.next_output(vm);
            }
            _ => {}
        }
        self.next_attr(vm)
    }

    /// Force the next `outputs` element, or apply the finished list.
    fn next_output(&mut self, vm: &mut Vm) -> Result<Yield> {
        match self.output_slots.get(self.output_index) {
            Some(slot) => {
                self.stage = Stage::OutputName;
                Ok(Yield::Force(slot.clone()))
            }
            None => {
                self.output_slots = Rc::new(Vec::new());
                let names = std::mem::take(&mut self.output_names);
                self.handle_outputs(names)?;
                self.next_attr(vm)
            }
        }
    }

    /// `handleOutputs` (`primops.cc:1598`) over `tokenizeString`'s split,
    /// which is how the flat branch spells the same list.
    fn set_outputs(&mut self, text: &str) -> Result<()> {
        self.handle_outputs(text.split_whitespace().map(str::to_owned))
    }

    /// `handleOutputs` (`primops.cc:1598`).
    fn handle_outputs(&mut self, names: impl IntoIterator<Item = String>) -> Result<()> {
        let mut out = BTreeSet::new();
        for name in names {
            if name == "drvPath" {
                // The result attribute set already has one.
                return Err(VmError::eval("invalid derivation output name 'drvPath'"));
            }
            if !out.insert(name.clone()) {
                return Err(VmError::eval(format!(
                    "duplicate derivation output '{name}'"
                )));
            }
        }
        if out.is_empty() {
            return Err(VmError::eval(
                "derivation cannot have an empty set of outputs",
            ));
        }
        self.outputs = out;
        Ok(())
    }

    /// `handleHashMode` (`primops.cc:1580`). An unrecognised mode is an
    /// evaluation error, unlike an unrecognised `outputHashAlgo`, which is
    /// silently `None`; the asymmetry is cppnix's.
    fn set_ingestion_method(&mut self, mode: &str) -> Result<()> {
        let Some(method) = CaMethod::parse(mode) else {
            return Err(VmError::eval(format!(
                "invalid value '{mode}' for 'outputHashMode' attribute"
            )));
        };
        // Both of these are real cppnix modes gated behind an experimental
        // feature, and both change the store path, so a guess would be a
        // wrong path rather than a missing one.
        match method {
            CaMethod::Text => {
                return Err(unimplemented(
                    "outputHashMode = \"text\" (cppnix gates this behind the dynamic-derivations experimental feature)",
                ));
            }
            CaMethod::Git => {
                return Err(unimplemented(
                    "outputHashMode = \"git\" (cppnix gates this behind the git-hashing experimental feature)",
                ));
            }
            CaMethod::Flat | CaMethod::NixArchive => {}
        }
        self.ingestion_method = Some(method);
        Ok(())
    }

    /// The fixed-output half of `derivationStrictInternal` (`primops.cc:1853`),
    /// reached when `outputHash` was present at all.
    fn finish_fixed_output(
        &mut self,
        vm: &mut Vm,
        store_dir: &str,
        inputs: &DerivationInputs,
        hash_text: &str,
    ) -> Result<Yield> {
        // "Ignore `__contentAddressed` because fixed output derivations are
        // already content addressed."
        //
        // The lone-`out` rule is enforced by `build_fixed_output` below and
        // not repeated here. It used to be both places, with the sentence
        // written out twice, and mutation testing is what showed the copy was
        // doing nothing: inverting it changed no observable behaviour because
        // the other one refuses the same inputs with the same words
        // (ENG-13020).
        let (hash, warning) = new_hash_allow_empty(
            hash_text,
            self.output_hash_algo,
            vm.settings().blake3_hashes,
        )
        .map_err(hash_parse_error)?;
        let method = self.ingestion_method.unwrap_or(CaMethod::Flat);

        let built = build_fixed_output(store_dir, inputs, method, &hash).map_err(build_error)?;

        // `hashDerivationModulo` reads the three fields this just wrote, so
        // the memo entry is what lets a derivation naming this one as an
        // input find its hash without reading a `.drv` back off a store that
        // read-only mode never wrote.
        let modulo = hash_derivation_modulo(
            store_dir,
            &built.drv,
            &self.name,
            false,
            &InProcess,
            vm.drv_hashes_mut(),
        )
        .map_err(hash_error)?;
        vm.drv_hashes_mut().insert(built.drv_path.clone(), modulo);

        // cppnix warns from inside `newHashAllowEmpty`, before the path is
        // computed. Yielded rather than printed for the reason the
        // structuredAttrs warnings are, and held past the write because the
        // answer has to be ready to hand back when the warning returns.
        self.empty_hash_warning = warning;
        self.write_drv(vm, built)
    }

    /// Everything after the walk: the context becomes inputs, the required
    /// attributes are checked, the derivation is built, and the result is the
    /// attribute set cppnix returns.
    fn finish(&mut self, vm: &mut Vm) -> Result<Yield> {
        // The attribute walk is over: nothing below publishes again, so the
        // enclosing walk's pending children become the standing offer once
        // more. Restored here rather than at the `Yield::Done` sites so the
        // window where this walk's (long-forced) attributes shadowed them
        // ends with the forcing, not with the derivation write.
        if let Some(saved) = self.saved_offer.take() {
            vm.restore_fanout_offer(saved);
        }
        let mut input_srcs: BTreeSet<String> = BTreeSet::new();
        let mut input_drvs: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
        for element in &self.context {
            match element {
                ContextElem::Opaque(path) => {
                    input_srcs.insert(path.to_string());
                }
                ContextElem::Built { drv, output } => {
                    input_drvs
                        .entry(drv.to_string())
                        .or_default()
                        .insert(output.to_string());
                }
                // cppnix answers this one with `computeFSClosure`, a store
                // read of the whole graph below the derivation, and its own
                // comment says it does not work in read-only mode
                // (`primops.cc:1789`). There is no closure to walk in this
                // process, so the honest answer is that the derivation cannot
                // be built rather than one missing some of its inputs.
                ContextElem::DrvDeep(drv) => {
                    return Err(unimplemented(&format!(
                        "a derivation attribute depending on every output of '{drv}' \
                         (cppnix walks the store closure)"
                    )));
                }
            }
        }

        // Checked in cppnix's order (`primops.cc:1832`), after the context is
        // translated, so a derivation missing both reports `builder` first.
        if self.builder.is_empty() {
            return Err(VmError::eval("required attribute 'builder' missing"));
        }
        if self.platform.is_empty() {
            return Err(VmError::eval("required attribute 'system' missing"));
        }
        if self.name.ends_with(".drv") {
            return Err(VmError::eval(
                "derivation names are allowed to end in '.drv' only if they produce a single derivation file",
            ));
        }

        let store_dir = vm.settings().store_dir.clone().ok_or_else(|| {
            VmError::Unimplemented(Refusal::new(
                RefusalToken::StoreUnavailable,
                "builtins.derivationStrict without a store directory (the embedder never \
                 called ixe_set_store_dir, and a guessed store directory is a wrong \
                 output path rather than a missing one)",
            ))
        })?;

        let mut env = std::mem::take(&mut self.env);
        if self.structured {
            // `StructuredAttrs::unparse` (`parsed-derivations.cc:34`): one
            // env var named `__json` holding the whole object, dumped by
            // nlohmann with no spaces and its `std::map` key order, which is
            // this `BTreeMap`'s. The bytes go straight into the ATerm the
            // derivation path hashes, so both of those are load-bearing.
            let mut json_bytes: Vec<u8> = b"{".to_vec();
            for (i, (key, value)) in self.structured_attrs.iter().enumerate() {
                if i > 0 {
                    json_bytes.push(b',');
                }
                crate::primops_host::json_string(key.as_bytes(), &mut json_bytes)?;
                json_bytes.push(b':');
                json_bytes.extend_from_slice(value.as_bytes());
            }
            json_bytes.push(b'}');
            // Every piece was strictly validated when it was rendered, so
            // this error path is unreachable; named rather than panicked on.
            let json = String::from_utf8(json_bytes)
                .map_err(|_| VmError::eval("internal: a structured attr rendered non-UTF-8"))?;
            // `StructuredAttrs::checkKeyNotInUse` (`parsed-derivations.cc:40`).
            // cppnix runs it at unparse time, by which point every output
            // name is an environment variable, so an output named `__json`
            // is the way to reach it: nothing else writes to `env` in this
            // branch. Checking `env` alone answered such a derivation with a
            // well-formed path where cppnix raises, which is what the
            // drv-parity case for it caught.
            if env.contains_key(STRUCTURED_ATTRS_ENV) || self.outputs.contains(STRUCTURED_ATTRS_ENV)
            {
                return Err(VmError::eval(
                    "Cannot have an environment variable named '__json'. This key is reserved for encoding structured attrs",
                ));
            }
            env.insert(STRUCTURED_ATTRS_ENV.to_owned(), json);
        }
        // cppnix writes each attribute with `emplace` and then assigns the
        // output variables with `operator[]` (`primops.cc:1913`), so an
        // attribute sharing a name with an output is overwritten and its
        // value never reaches the builder. Its *context* still counts, which
        // is why this drops the entry here rather than skipping the coercion
        // above. `build_input_addressed` refuses the collision, which is what
        // makes this line load-bearing rather than defensive.
        for output in &self.outputs {
            env.remove(output);
        }

        let inputs = DerivationInputs {
            name: self.name.clone(),
            platform: std::mem::take(&mut self.platform),
            builder: std::mem::take(&mut self.builder),
            args: std::mem::take(&mut self.args),
            output_names: self.outputs.iter().cloned().collect(),
            env,
            input_srcs,
            input_drvs,
        };

        // The presence of `outputHash` decides the branch, exactly as it does
        // in cppnix, and it is checked before `__contentAddressed`.
        if let Some(hash_text) = self.output_hash.take() {
            return self.finish_fixed_output(vm, &store_dir, &inputs, &hash_text);
        }

        // cppnix's `else if (contentAddressed || isImpure)` (`primops.cc:1878`).
        // `__impure` was refused when it was read, so only the CA half is
        // reachable, and the `contentAddressed && isImpure` error with it.
        // The defaults are cppnix's own: SHA-256, NAR ingestion.
        if self.content_addressed {
            let algo = self.output_hash_algo.unwrap_or(HashAlgo::Sha256);
            let method = self.ingestion_method.unwrap_or(CaMethod::NixArchive);
            let built =
                build_content_addressed(&store_dir, &inputs, method, algo).map_err(build_error)?;
            // The same memo write as the other two branches, for the same
            // reason: a downstream derivation naming this one as an input
            // finds its (deferred) hash here rather than reading the `.drv`
            // back. `mask_outputs = false`, as at cppnix's call.
            let modulo = hash_derivation_modulo(
                &store_dir,
                &built.drv,
                &self.name,
                false,
                &InProcess,
                vm.drv_hashes_mut(),
            )
            .map_err(hash_error)?;
            vm.drv_hashes_mut().insert(built.drv_path.clone(), modulo);
            return self.write_drv(vm, built);
        }

        let built = build_input_addressed(&store_dir, &inputs, &InProcess, vm.drv_hashes_mut())
            .map_err(build_error)?;

        // "Optimisation, but required in read-only mode!" (`primops.cc:1937`):
        // nothing writes the `.drv`, so a later derivation naming this one as
        // an input cannot read it back and has to find its hash here. Note
        // `mask_outputs = false`, which is what cppnix passes at this call
        // (`hashDerivationModulo(*state.store, drv, false)`) and is not what
        // the build above used.
        let modulo = hash_derivation_modulo(
            &store_dir,
            &built.drv,
            &self.name,
            false,
            &InProcess,
            vm.drv_hashes_mut(),
        )
        .map_err(hash_error)?;
        vm.drv_hashes_mut().insert(built.drv_path.clone(), modulo);

        self.write_drv(vm, built)
    }

    /// Hand the finished `.drv` to the embedder to put in the store.
    ///
    /// cppnix writes it here, inside `derivationStrictInternal`
    /// (`primops.cc:1937`), and it has to be here rather than anywhere later:
    /// `nix build` needs every `.drv` in the input closure present, not only
    /// the one the installable named, and this is the only place that sees
    /// them all. Under `readOnlyMode` -- and under no store at all -- nothing
    /// is written and the path this already computed stands, which is the
    /// same branch cppnix takes. ENG-12799.
    ///
    /// Once per derivation per evaluation, where cppnix asks its store once
    /// per `derivationStrict` call and nixpkgs makes the same call for the
    /// same derivation several times over: the repeat has the same bytes and
    /// so the same path, and the first ask is what wrote it
    /// ([`Vm::note_drv_written`]). The repeat takes the `Null` branch of
    /// [`Self::after_drv_written`], which is the "nothing to compare" case
    /// it already is. Under `--repair` every ask reaches the store: repair
    /// rewrites a damaged object, and the repeat could be the ask that
    /// would have rewritten it.
    fn write_drv(&mut self, vm: &mut Vm, built: BuiltDerivation) -> Result<Yield> {
        self.built_drv_path = built.drv_path;
        self.built_outputs = built.outputs;
        if !vm.settings().repair && !vm.note_drv_written(&self.built_drv_path) {
            crate::perf::note_drv_write_skipped();
            return self.after_drv_written(vm, &Value::Null);
        }
        self.stage = Stage::DrvWritten;
        Ok(Yield::Need(NeedPath::WriteDrv {
            name: self.name.clone(),
            aterm: built.aterm,
            expected: self.built_drv_path.clone(),
        }))
    }

    /// The embedder has answered the write; build the result it was holding.
    ///
    /// A `Null` answer means no store was behind this evaluation, so nothing
    /// was written and there is nothing to check -- see
    /// [`NeedPath::WriteDrv`]. Any other answer is the path the store used,
    /// and it must be the one computed here: two names for one derivation is
    /// not a divergence to average out, it is the failure the whole rung is
    /// about, and it is caught at the derivation that caused it rather than
    /// as a missing path in some consumer much later.
    fn after_drv_written(&mut self, vm: &mut Vm, value: &Value) -> Result<Yield> {
        if !matches!(value, Value::Null) {
            let written = text_of(want_nix_str(value)?)?.to_owned();
            if written != self.built_drv_path {
                return Err(VmError::eval(format!(
                    "this evaluation computed the derivation path '{}', and the store wrote                      it to '{written}'",
                    self.built_drv_path
                )));
            }
        }
        let drv_path = std::mem::take(&mut self.built_drv_path);
        let outputs = std::mem::take(&mut self.built_outputs);
        let result = result_attrs(vm, &drv_path, &outputs);
        if let Some(message) = self.empty_hash_warning.take() {
            self.current_value = Some(result);
            self.stage = Stage::EmptyHashWarned;
            return Ok(Yield::Need(NeedPath::Warn(message)));
        }
        Ok(Yield::Done(result))
    }

    /// A coercion's answer: its bytes, with its context folded into the
    /// derivation's.
    fn take_string(&mut self, value: &Value) -> Result<String> {
        // The `.drv` leg of this backend is text over the store ABI, so a
        // coercion answer that is not UTF-8 refuses here (a named boundary)
        // rather than being repaired into a diverging derivation.
        let text = want_nix_str(value)?;
        self.context.extend(text.context_set());
        Ok(text_of(text)?.to_owned())
    }
}

/// The result of `derivationStrict`: `drvPath`, plus one attribute per output
/// (`primops.cc:1943`). `type`, `name` and `outputs` are **not** here -- the
/// `derivation` wrapper in `primops/derivation.nix` adds those, and putting
/// them here would make this primop disagree with cppnix's for any caller
/// that uses it directly.
fn result_attrs(vm: &mut Vm, drv_path: &str, outputs: &BTreeMap<String, String>) -> Value {
    let path: Rc<str> = drv_path.into();
    let mut map: BTreeMap<Sym, Slot> = BTreeMap::new();

    let drv_path_sym = vm.intern("drvPath");
    let mut deep = BTreeSet::new();
    deep.insert(ContextElem::DrvDeep(Rc::clone(&path)));
    map.insert(
        drv_path_sym,
        Slot::value(Value::Str(NixStr::with_context(Rc::clone(&path), deep))),
    );

    for (name, out_path) in outputs {
        let sym = vm.intern(name);
        let mut built = BTreeSet::new();
        built.insert(ContextElem::Built {
            drv: Rc::clone(&path),
            output: name.as_str().into(),
        });
        map.insert(
            sym,
            Slot::value(Value::Str(NixStr::with_context(out_path.as_bytes(), built))),
        );
    }
    Value::Attrs(Rc::new(Attrs::new(map)))
}

/// The only source of input-derivation hashes during an evaluation: the map
/// on the VM, which [`hash_derivation_modulo`] consults before it asks this.
///
/// So every call that reaches here is a miss, and a miss means a `.drv` that
/// this evaluation did not produce -- `builtins.storePath` of one, or an
/// imported derivation. cppnix reads it off the store; this refuses by name,
/// because a store read is the thing rung C's design deliberately keeps off
/// the critical path.
struct InProcess;

impl DrvSource for InProcess {
    fn read_drv(&self, drv_path: &str) -> std::result::Result<(Derivation, String), String> {
        Err(format!(
            "'{drv_path}' was not produced by this evaluation, so its hash is not in \
             the in-process table and reading it would need a store"
        ))
    }
}

/// A hash failure whose cause is a missing input is this evaluator's gap and
/// not the expression's, so it is reported as unimplemented; everything else
/// is an error cppnix raises too.
fn hash_error(e: HashError) -> VmError {
    match e {
        HashError::UnreadableInput { .. } => {
            VmError::Unimplemented(Refusal::new(RefusalToken::UnreadableInput, e.to_string()))
        }
        other => VmError::eval(other.to_string()),
    }
}

fn build_error(e: BuildError) -> VmError {
    match e {
        BuildError::Hash(h) => hash_error(h),
        other => VmError::eval(other.to_string()),
    }
}

/// `parseHashAlgoOpt` for the `outputHashAlgo` attribute. Blake3 is recognised
/// before its feature is required; every other unrecognised name is `None`,
/// which is cppnix's behaviour and not a shrug (`libutil/hash.cc:468-481`).
fn parse_algo(s: &str, blake3_hashes: bool) -> Result<Option<HashAlgo>> {
    parse_algo_opt(s)
        .and_then(|algo| {
            algo.map(|parsed| parsed.require_enabled(blake3_hashes))
                .transpose()
        })
        .map_err(hash_parse_error)
}

/// A hash the derivation declared could not be read. An experimentally-gated
/// algorithm is a refusal, because cppnix with the feature on answers fine;
/// everything else is a real evaluation error that cppnix also raises.
fn hash_parse_error(e: HashParseError) -> VmError {
    match e {
        HashParseError::Unsupported(_) => unimplemented(&e.to_string()),
        other => VmError::eval(other.to_string()),
    }
}

fn unimplemented(what: &str) -> VmError {
    VmError::Unimplemented(Refusal::new(
        RefusalToken::DerivationStrict,
        format!("builtins.derivationStrict with {what}"),
    ))
}

/// `checkName` (`libstore/path.cc:8`), reached through `checkDerivationName`
/// (`primops.cc:1512`). The name goes into the store path's fingerprint, so a
/// name cppnix refuses must be refused here rather than hashed.
fn check_derivation_name(name: &str) -> Result<()> {
    let invalid = |why: String| {
        VmError::eval(format!(
            "invalid derivation name: {why}. Please pass a different 'name'."
        ))
    };
    if name.is_empty() {
        return Err(invalid("name must not be empty".to_owned()));
    }
    // `StorePath::MaxPathLen`.
    if name.len() > 211 {
        return Err(invalid(format!(
            "name '{name}' must be no longer than 211 characters"
        )));
    }
    let bytes = name.as_bytes();
    if bytes.first() == Some(&b'.') {
        let not_valid = || invalid(format!("name '{name}' is not valid"));
        let component = |c: &str| {
            invalid(format!(
                "name '{name}' is not valid: first dash-separated component must not be '{c}'"
            ))
        };
        match (bytes.len(), bytes.get(1), bytes.get(2)) {
            (1, _, _) => return Err(not_valid()),
            (_, Some(&b'-'), _) => return Err(component(".")),
            (2, Some(&b'.'), _) => return Err(not_valid()),
            (_, Some(&b'.'), Some(&b'-')) => return Err(component("..")),
            _ => {}
        }
    }
    for c in name.chars() {
        if !(c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.' | '_' | '?' | '=')) {
            return Err(invalid(format!(
                "name '{name}' contains illegal character '{c}'"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{InProcess, check_derivation_name, parse_algo};
    use crate::drvpath::DrvSource;
    use crate::nixhash::HashAlgo;
    use crate::refusal::{Refusal, RefusalToken};
    use crate::vm::VmError;

    /// Evaluate under `/nix/store`, which is the value cppnix used on the
    /// machines the golden paths below were recorded on.
    ///
    /// Stated per evaluation rather than set once for the process. The
    /// `OnceLock` this used to call configured the whole binary, so whether a
    /// test saw a store directory depended on whether it happened to run
    /// after this one (ENG-12939).
    fn render(src: &str) -> String {
        crate::eval::render_str_with(&crate::eval::settings_with_store(), src)
    }

    /// A fixed-output derivation's path is a function of the declared hash
    /// alone, so these bytes are checkable without a store and without a
    /// build. Every one is what nix 2.34.7 printed on the cpp backend for the
    /// same expression, taken rather than reasoned out.
    ///
    /// The rows cover the branches that decide the path, and each pair differs
    /// in exactly one thing:
    ///
    /// * SRI, bare base16, bare nix32 and prefixed `sha256:` all name the same
    ///   32 bytes, so all four must land on the same path -- that is what says
    ///   the *decoders* agree rather than merely that one of them works;
    /// * `flat` and `recursive` differ, because sha256 + NAR is the one
    ///   combination that takes `makeStorePath("source", ...)` instead of the
    ///   `fixed:out:` double hash;
    /// * sha1 + recursive is *not* that combination, which is the case a
    ///   "recursive means source" reading gets wrong.
    #[test]
    fn a_fixed_output_derivation_lands_where_cpp_puts_it() {
        let fo = |attrs: &str| {
            render(&format!(
                r#"(derivation {{ name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; {attrs} }}).outPath"#
            ))
        };
        // The same sha256, four spellings, one path.
        let flat = r#""/nix/store/9zicq1m4wy4bv50xa4l1jk4kpyy41kih-x""#;
        assert_eq!(
            fo(
                r#"outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="; outputHashAlgo = "sha256"; outputHashMode = "flat";"#
            ),
            flat
        );
        assert_eq!(
            fo(
                r#"outputHash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"; outputHashAlgo = "sha256";"#
            ),
            flat
        );
        assert_eq!(
            fo(
                r#"outputHash = "0mdqa9w1p6cmli6976v4wi0sw9r4p5prkj7lzfd1877wk11c9c73"; outputHashAlgo = "sha256";"#
            ),
            flat
        );
        assert_eq!(
            fo(
                r#"outputHash = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";"#
            ),
            flat
        );
        // sha256 + NAR is the `source` branch, and lands elsewhere.
        assert_eq!(
            fo(
                r#"outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="; outputHashMode = "recursive";"#
            ),
            r#""/nix/store/d6q30mlzfljh7ha1b1m6fnifi34qr99p-x""#
        );
        // sha1 + NAR is not, so it stays on the `fixed:out:` branch with an
        // `r:` prefix in the payload.
        assert_eq!(
            fo(
                r#"outputHash = "da39a3ee5e6b4b0d3255bfef95601890afd80709"; outputHashAlgo = "sha1"; outputHashMode = "recursive";"#
            ),
            r#""/nix/store/dii4qkmn9kdb6ziy6psgldkwa3k3gyia-x""#
        );
        assert_eq!(
            fo(r#"outputHash = "d41d8cd98f00b204e9800998ecf8427e"; outputHashAlgo = "md5";"#),
            r#""/nix/store/v4hm49i7f7fmjvrmdcda7qrcizb2ckkn-x""#
        );
        // `newHashAllowEmpty`: an empty hash is *accepted*, becomes the
        // all-zero hash of the declared algorithm, and warns. cpp prints this
        // same path with the warning on stderr.
        assert_eq!(
            fo(r#"outputHash = ""; outputHashAlgo = "sha256";"#),
            r#""/nix/store/yplc6kklcg6k8aln037jq8jxq5v92wln-x""#
        );
        // And it really warns. cpp prints this exact line on stderr for the
        // expression above; a path with no warning would be a silent
        // substitution of a hash the caller did not write.
        assert_eq!(
            warnings_of(
                r#"(builtins.derivationStrict { name = "x"; system = "x86_64-linux";
                     builder = "/bin/sh"; outputHash = ""; outputHashAlgo = "sha256"; }).out"#
            ),
            vec![
                "found empty hash, assuming \
                 'sha256-AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA='"
                    .to_owned()
            ]
        );
    }

    /// The corner cases of hash parsing, which are the part of this that is
    /// easy to get subtly wrong. All seven are errors cpp also raises, in
    /// cpp's wording, and none of them may become a path: a fixed-output path
    /// that is wrong is indistinguishable from one that is right.
    /// The `.drv` path of a fixed-output derivation, and of a derivation that
    /// consumes one.
    ///
    /// Separate from the `outPath` cases above because the two are computed
    /// from different things and fail independently. `outPath` comes from
    /// [`make_fixed_output_path`], which reads the ingestion method out of
    /// `render_prefix`. The `.drv` path comes from the ATerm, whose output
    /// tuple carries `print_method_algo` -- a **different call site for the
    /// same prefix** -- and that same string is the `<methodAlgo>` field of
    /// `hashDerivationModulo`'s `fixed:out:` payload, which every downstream
    /// output path is built on.
    ///
    /// So dropping the `r:` from the ATerm's `hashAlgo` leaves every
    /// `outPath` in this file correct and silently moves the `.drv` path and
    /// every dependent's output path. Measured, on this code with that one
    /// edit applied:
    ///
    /// ```text
    ///                       correct                         with r: dropped
    ///  source .drv   7nw8jhad9wcsrki6m9gapv8hxcy8vhpx  1iy3iqa8bybc3mdhx05lyppzcvd19cak
    ///  source out    7q2y7crif31i14hipkifg4w8n05zahdd  7q2y7crif31i14hipkifg4w8n05zahdd
    ///  dependent out rzl2ij7libwahjhay8wnqq5vkca2cm2i  3akdijx07x8is1sw5972qgrvj0arbx9q
    /// ```
    ///
    /// The middle row is why an `outPath`-only test set cannot see this, and
    /// the bottom row is what it costs: a wrong store path for everything
    /// below any recursive fixed-output source, which in nixpkgs is every
    /// `fetchzip` and `fetchFromGitHub`.
    ///
    /// Every golden here was printed by cpp nix 2.34.7+ix.g69e4d9e9db39 with
    /// store directory `/nix/store`, from `nix-instantiate --eval --strict
    /// --json` over these exact expressions. They come from the binary's
    /// output, not from this code: if one of them ever fails, the code is
    /// wrong until proven otherwise, and re-recording them to match the code
    /// defeats the entire point of having them.
    #[test]
    fn fixed_output_drv_paths_and_their_dependents_match_cppnix() {
        const SRI: &str = "sha256-jZkUKv2SV28wsM18tCqNxoCZmLxdYH2Idh9RLibH2yA=";
        let src = |name: &str, mode: &str| {
            format!(
                r#"builtins.derivationStrict {{
                     name = "{name}"; system = "x86_64-linux"; builder = "/bin/sh";
                     outputHash = "{SRI}"; outputHashMode = "{mode}";
                   }}"#
            )
        };

        // Recursive: the one mode whose prefix is not the empty string, so the
        // only mode in which a missing `r:` is observable at all.
        assert_eq!(
            render(&src("source", "recursive")),
            "{ drvPath = \"/nix/store/7nw8jhad9wcsrki6m9gapv8hxcy8vhpx-source.drv\"; \
             out = \"/nix/store/7q2y7crif31i14hipkifg4w8n05zahdd-source\"; }"
        );

        // `nar` is `recursive` under a newer name. Same ingestion method, so
        // the same `out`; different attribute bytes, so a different `.drv`.
        // Asserting both is how the alias is told from a typo: a backend that
        // silently ignored an unrecognised mode would agree on `out` here and
        // still be wrong.
        assert_eq!(
            render(&src("source", "nar")),
            "{ drvPath = \"/nix/store/r6qy1k2bryzviza5bh8m2k03f0x3nz0k-source.drv\"; \
             out = \"/nix/store/7q2y7crif31i14hipkifg4w8n05zahdd-source\"; }"
        );

        // Flat, for the contrast: the prefix is empty here, which is why the
        // rest of the suite cannot see the bug this test exists for.
        assert_eq!(
            render(&src("hello-2.12.3.tar.gz", "flat")),
            "{ drvPath = \"/nix/store/c72hz6pi619xqmq7jlhxfh61arpc2jfy-hello-2.12.3.tar.gz.drv\"; \
             out = \"/nix/store/99xrss47wb4qxgxvzm6f6535b2iv5ach-hello-2.12.3.tar.gz\"; }"
        );

        // The propagation. `dep = a.out` makes the fixed-output derivation an
        // input, so this derivation's own output path runs through
        // `hashDerivationModulo`'s `fixed:out:<methodAlgo>:<hash>:<path>`.
        // This is the row that turns a cosmetic ATerm field into a wrong path
        // for real packages.
        let dependent = |mode: &str| {
            format!(
                r#"let a = {}; in builtins.derivationStrict {{
                     name = "b"; system = "x86_64-linux"; builder = "/bin/sh"; dep = a.out;
                   }}"#,
                src("source", mode)
            )
        };
        assert_eq!(
            render(&dependent("recursive")),
            "{ drvPath = \"/nix/store/9z7q0lcdhcs8nw4fmg57ssxlz36955am-b.drv\"; \
             out = \"/nix/store/rzl2ij7libwahjhay8wnqq5vkca2cm2i-b\"; }"
        );
        assert_eq!(
            render(&dependent("flat")),
            "{ drvPath = \"/nix/store/3n1xhxp38l3rjn8j1ra3ddjr62ycd1q5-b.drv\"; \
             out = \"/nix/store/jpd4xmammqrsjp5zpnjq45wbwf3pb1vn-b\"; }"
        );
    }

    /// One digest, five spellings, one answer.
    ///
    /// The encoding is chosen by string length (`baseFromSize`), never by
    /// inspection, so a decoder wired to the wrong branch still returns 32
    /// plausible bytes for most of these. Uppercase base-16 is here because
    /// cppnix's `parseHexDigit` takes both cases and a decoder built on a
    /// lowercase-only table rejects a hash cppnix accepts.
    #[test]
    fn every_spelling_of_one_hash_reaches_the_same_output_path() {
        const B16: &str = "8d99142afd92576f30b0cd7cb42a8dc6809998bc5d607d88761f512e26c7db20";
        const WANT: &str = "\"/nix/store/99xrss47wb4qxgxvzm6f6535b2iv5ach-hello-2.12.3.tar.gz\"";
        let out = |hash: &str, algo: &str| {
            render(&format!(
                r#"(builtins.derivationStrict {{
                     name = "hello-2.12.3.tar.gz"; system = "x86_64-linux";
                     builder = "/bin/sh"; outputHash = "{hash}"; {algo}
                   }}).out"#
            ))
        };
        let with_algo = r#"outputHashAlgo = "sha256";"#;
        assert_eq!(out(B16, with_algo), WANT, "base16");
        assert_eq!(
            out(&B16.to_uppercase(), with_algo),
            WANT,
            "base16 uppercase"
        );
        assert_eq!(
            out(
                "086vqwk2wl8zfs47sq2xpjc9k066ilmb8z6dn0q6ymwjzlm196cd",
                with_algo
            ),
            WANT,
            "nix32"
        );
        assert_eq!(
            out("jZkUKv2SV28wsM18tCqNxoCZmLxdYH2Idh9RLibH2yA=", with_algo),
            WANT,
            "base64"
        );
        assert_eq!(
            out("sha256-jZkUKv2SV28wsM18tCqNxoCZmLxdYH2Idh9RLibH2yA=", ""),
            WANT,
            "SRI"
        );
        assert_eq!(out(&format!("sha256:{B16}"), ""), WANT, "prefixed base16");
    }

    /// `__structuredAttrs` reads `outputHash`, `outputHashAlgo` and
    /// `outputHashMode` off the value with `forceStringNoCtx` rather than off
    /// the coerced string (`primops.cc:1671`), and the `.drv` carries one JSON
    /// object instead of the flat variables. Both halves of that reach the
    /// bytes, so the pair is worth one golden.
    #[test]
    fn a_structured_fixed_output_derivation_matches_cppnix() {
        assert_eq!(
            render(
                r#"builtins.derivationStrict {
                     name = "hello-2.12.3.tar.gz"; system = "x86_64-linux"; builder = "/bin/sh";
                     __structuredAttrs = true;
                     outputHash = "sha256-jZkUKv2SV28wsM18tCqNxoCZmLxdYH2Idh9RLibH2yA=";
                     outputHashMode = "flat"; outputHashAlgo = "sha256";
                   }"#
            ),
            "{ drvPath = \"/nix/store/bgg1fgl7parqc818pf0vqapmb1nh7lcy-hello-2.12.3.tar.gz.drv\"; \
             out = \"/nix/store/99xrss47wb4qxgxvzm6f6535b2iv5ach-hello-2.12.3.tar.gz\"; }"
        );
    }

    #[test]
    fn a_malformed_fixed_output_hash_is_refused_the_way_cpp_refuses_it() {
        let fo = |attrs: &str| {
            render(&format!(
                r#"(derivation {{ name = "x"; system = "x86_64-linux"; builder = "/bin/sh"; {attrs} }}).outPath"#
            ))
        };
        for (attrs, want) in [
            // Right prefix, wrong length. Length is what picks the encoding,
            // so this is a length error and not a bad-character one.
            (
                r#"outputHash = "sha256-tooshort";"#,
                "length 6 != expected length 32",
            ),
            // A prefix inside the hash string *is* checked, unlike the
            // `outputHashAlgo` attribute, which is silently ignored when it is
            // not a name cpp knows.
            (
                r#"outputHash = "bogus-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";"#,
                "unknown hash algorithm 'bogus'",
            ),
            // No prefix and no attribute: nothing says which algorithm.
            (
                r#"outputHash = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";"#,
                "does not include a type, nor is the type otherwise known from context",
            ),
            (
                r#"outputHash = "";"#,
                "empty hash requires explicit hash algorithm",
            ),
            // The string and the attribute both name one, and they disagree.
            (
                r#"outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="; outputHashAlgo = "sha1";"#,
                "should have type 'sha1'",
            ),
            (
                r#"outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="; outputHashMode = "bogus";"#,
                "invalid value 'bogus' for 'outputHashMode' attribute",
            ),
            (
                r#"outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="; outputs = [ "out" "dev" ];"#,
                "multiple outputs are not supported in fixed-output derivations",
            ),
        ] {
            let got = fo(attrs);
            assert!(
                got.contains(want),
                "`{attrs}` should fail with {want:?}; got: {got}"
            );
        }
    }

    /// An unrecognised `outputHashAlgo` is **not** an error, unlike an
    /// unrecognised mode: cpp's `parseHashAlgoOpt` returns nothing and the
    /// algorithm then has to come from the hash string. Worth its own case
    /// because the obvious implementation raises here and cpp does not.
    #[test]
    fn an_unknown_output_hash_algo_falls_back_to_the_hash_string() {
        assert_eq!(
            render(
                r#"(derivation { name = "x"; system = "x86_64-linux"; builder = "/bin/sh";
                    outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU=";
                    outputHashAlgo = "not-an-algorithm"; }).outPath"#
            ),
            r#""/nix/store/9zicq1m4wy4bv50xa4l1jk4kpyy41kih-x""#
        );
    }

    /// A host that records what the evaluator warned about. The VM performs
    /// no IO, so a warning leaves as a `Yield::Need` and arrives here; a test
    /// that asserts on warnings has to drive with a host rather than call
    /// `eval_str`.
    #[derive(Default)]
    struct Warns(std::cell::RefCell<Vec<String>>);

    impl crate::host::Host for Warns {
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
            find_file,
            nix_path,
            trace
        );
        fn read_file(&self, _p: &crate::value2::PathValue) -> std::result::Result<String, String> {
            Ok(String::new())
        }
        fn read_dir(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, crate::host::FileType)>, String> {
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
        ) -> std::result::Result<Option<crate::host::FileType>, String> {
            Ok(Some(crate::host::FileType::Regular))
        }
        fn warn(&self, message: &str) {
            self.0.borrow_mut().push(message.to_owned());
        }
    }

    /// A host that stands in for a store: it remembers every `.drv` it was
    /// asked to write and answers with the path it was told to expect, or
    /// with a path of its own when the test wants the guard to fire.
    struct WritesDrvs {
        written: std::cell::RefCell<Vec<(String, String)>>,
        /// What to answer instead of the store path. `None` answers
        /// correctly, by recomputing the same text path the evaluator did.
        lie: Option<String>,
    }

    impl WritesDrvs {
        fn honest() -> Self {
            WritesDrvs {
                written: std::cell::RefCell::new(Vec::new()),
                lie: None,
            }
        }
    }

    impl crate::host::Host for WritesDrvs {
        crate::host::host_stubs!(settle);
        crate::host::host_stubs!(parse_flake_ref, flake_ref_to_string);
        fn read_file_bytes(&self, path: &crate::value2::PathValue) -> Result<Vec<u8>, String> {
            self.read_file(path).map(String::into_bytes)
        }
        crate::host::host_stubs!(
            realise,
            store_text,
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
        fn read_file(&self, _p: &crate::value2::PathValue) -> std::result::Result<String, String> {
            Ok(String::new())
        }
        fn read_dir(
            &self,
            _p: &crate::value2::PathValue,
        ) -> std::result::Result<Vec<(String, crate::host::FileType)>, String> {
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
        ) -> std::result::Result<Option<crate::host::FileType>, String> {
            Ok(Some(crate::host::FileType::Regular))
        }
        fn warn(&self, _message: &str) {}
        fn write_derivation(
            &self,
            name: &str,
            aterm: &str,
        ) -> std::result::Result<String, crate::host::StoreError> {
            self.written
                .borrow_mut()
                .push((name.to_owned(), aterm.to_owned()));
            if let Some(lie) = &self.lie {
                return Ok(lie.clone());
            }
            // The same rule cppnix's `writeDerivation` uses under
            // `readOnlyMode`: the text path of these bytes under this name,
            // with the reference set derived from the parsed derivation, as
            // that writer derives it. Computed with the crate's own hashing
            // rather than echoed back from the request, so the assertion in
            // the test below is a real comparison of two computations.
            let drv = crate::drv::parse(aterm).map_err(|e| {
                crate::host::StoreError::Failed(format!("unparseable ATerm: {e:?}"))
            })?;
            Ok(crate::drvpath::text_store_path(
                "/nix/store",
                &format!("{name}.drv"),
                aterm,
                &drv.references(),
            ))
        }
    }

    fn drive_with(host: &dyn crate::host::Host, src: &str) -> std::result::Result<String, String> {
        drive_with_settings(host, src, crate::eval::settings_with_store())
    }

    fn drive_with_settings(
        host: &dyn crate::host::Host,
        src: &str,
        settings: crate::eval::Settings,
    ) -> std::result::Result<String, String> {
        let module =
            crate::compile::compile_source(src, "/m", crate::compile::Origin::String, &settings)
                .map_err(|e| format!("{e:?}"))?;
        let mut vm = crate::vm::Vm::with_settings(settings);
        vm.start_module(&std::rc::Rc::new(module));
        let value = crate::eval::drive(&mut vm, host).map_err(|e| format!("{e:?}"))?;
        vm.start_print(value);
        match crate::eval::drive(&mut vm, host).map_err(|e| format!("{e:?}"))? {
            crate::value2::Value::Str(s) => Ok(s.expect_text()),
            other => Err(format!("printer produced {other:?}")),
        }
    }

    /// Every derivation an evaluation produces is handed to the store, not
    /// only the one the expression selected.
    ///
    /// This is what `nix build` needs and `nix eval` never did: a build reads
    /// the whole input closure back out of the store, so a `.drv` that was
    /// computed and not written is a path that does not exist. Before this,
    /// `nix eval -f x.nix drvPath` on the Rust backend printed a path the
    /// store had never heard of, while the cpp backend left a 272-byte file
    /// there (measured on dev-compute-6).
    #[test]
    fn every_derivation_is_handed_to_the_store_to_write() {
        let host = WritesDrvs::honest();
        let out = drive_with(
            &host,
            r#"let a = derivation { name = "a"; system = "x86_64-linux"; builder = "/bin/sh"; };
               in (derivation { name = "b"; system = "x86_64-linux"; builder = "/bin/sh";
                                dep = a.out; }).drvPath"#,
        )
        .unwrap_or_else(|e| unreachable!("evaluation: {e}"));
        let written = host.written.borrow();
        let names: Vec<&str> = written.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["a", "b"],
            "both derivations, in dependency order"
        );

        // Destructured rather than indexed, both because `indexing_slicing`
        // is denied and because it says the count once instead of at every
        // use.
        let [_a, b] = written.as_slice() else {
            unreachable!("the name check above already required exactly two: {names:?}")
        };
        // The name arrives without the suffix, the way `writeDerivation`
        // takes it, and the ATerm is the real thing rather than a placeholder.
        assert!(b.1.starts_with("Derive(["), "b's ATerm: {}", b.1);
        // `b` names `a`'s `.drv` as a reference, which is what makes the
        // store keep the input closure alive.
        let b_refs = crate::drv::parse(&b.1)
            .unwrap_or_else(|e| unreachable!("b's ATerm parses: {e:?}"))
            .references();
        assert!(
            b_refs.iter().any(|r| r.ends_with("-a.drv")),
            "b's references: {b_refs:?}"
        );
        // And the path the evaluation reports is the one the store answered,
        // recomputed from the bytes rather than echoed back.
        let expected = crate::drvpath::text_store_path("/nix/store", "b.drv", &b.1, &b_refs);
        assert_eq!(out.trim_matches('"'), expected);
    }

    /// The same derivation built twice in one evaluation is handed to the
    /// store once: the second `derivationStrict` has the same bytes, so the
    /// same path, and the first ask wrote it. nixpkgs does this constantly
    /// (73k asks for 23k distinct derivations on one home-manager closure),
    /// and each repeat used to be a store round trip answered "already
    /// valid". Both values still carry the path.
    #[test]
    fn a_repeated_derivation_is_handed_to_the_store_once() {
        let host = WritesDrvs::honest();
        let out = drive_with(
            &host,
            r#"let mk = derivation { name = "a"; system = "x86_64-linux"; builder = "/bin/sh"; };
                   x = mk.drvPath; y = (derivation { name = "a"; system = "x86_64-linux"; builder = "/bin/sh"; }).drvPath;
               in if x == y then x else throw "two paths for one derivation""#,
        )
        .unwrap_or_else(|e| unreachable!("evaluation: {e}"));
        let written = host.written.borrow();
        let names: Vec<&str> = written.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["a"],
            "one write for two builds of one derivation"
        );
        assert!(out.trim_matches('"').ends_with("-a.drv"), "{out}");
    }

    /// Under `--repair` the repeat is asked again: repair rewrites a damaged
    /// object, and a skipped repeat could be the ask that would have
    /// rewritten it (cppnix calls `writeDerivation(.., Repair)` per
    /// `derivationStrict`).
    #[test]
    fn under_repair_every_build_of_a_derivation_reaches_the_store() {
        let host = WritesDrvs::honest();
        let settings = crate::eval::Settings {
            repair: true,
            ..crate::eval::settings_with_store()
        };
        let out = drive_with_settings(
            &host,
            r#"let mk = derivation { name = "a"; system = "x86_64-linux"; builder = "/bin/sh"; };
                   x = mk.drvPath; y = (derivation { name = "a"; system = "x86_64-linux"; builder = "/bin/sh"; }).drvPath;
               in if x == y then x else throw "two paths for one derivation""#,
            settings,
        )
        .unwrap_or_else(|e| unreachable!("evaluation: {e}"));
        let written = host.written.borrow();
        let names: Vec<&str> = written.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(
            names,
            vec!["a", "a"],
            "repair hands every build to the store"
        );
        assert!(out.trim_matches('"').ends_with("-a.drv"), "{out}");
    }

    /// A store that answers with a different path is a hard failure, not a
    /// value to accept.
    ///
    /// Watched failing by removing the comparison in `after_drv_written`, at
    /// which point this test evaluates cleanly and returns the evaluator's
    /// own path, which is exactly the silent divergence the check exists to
    /// stop: two names for one derivation, discovered later as a missing
    /// store path in whatever tried to build it.
    #[test]
    fn a_store_that_names_the_derivation_differently_is_a_failure() {
        let host = WritesDrvs {
            written: std::cell::RefCell::new(Vec::new()),
            lie: Some("/nix/store/0000000000000000000000000000000-not-the-one.drv".to_owned()),
        };
        let Err(error) = drive_with(
            &host,
            r#"(derivation { name = "a"; system = "x86_64-linux"; builder = "/bin/sh"; }).drvPath"#,
        ) else {
            unreachable!("a disagreeing store must fail the evaluation")
        };
        assert!(
            error.contains("not-the-one.drv") && error.contains("-a.drv"),
            "the message must name both paths: {error}"
        );
    }

    /// A host with no store leaves the `.drv` unwritten and changes nothing
    /// about the value, which is cppnix under `readOnlyMode` and is what
    /// keeps `cargo test` and `examples/nixpkgs-probe.rs` able to answer
    /// `hello.outPath` without one.
    #[test]
    fn no_store_means_no_write_and_the_same_path() {
        let with = drive_with(
            &WritesDrvs::honest(),
            r#"(derivation { name = "a"; system = "x86_64-linux"; builder = "/bin/sh"; }).drvPath"#,
        )
        .unwrap_or_else(|e| unreachable!("evaluation with a store: {e}"));
        let without = render(
            r#"(derivation { name = "a"; system = "x86_64-linux"; builder = "/bin/sh"; }).drvPath"#,
        );
        assert_eq!(with, without);
    }

    fn warnings_of(src: &str) -> Vec<String> {
        let host = Warns::default();
        let Ok(module) = crate::compile::compile_source(
            src,
            "/m",
            crate::compile::Origin::String,
            &crate::eval::settings_with_store(),
        ) else {
            return vec!["compile failed".to_owned()];
        };
        let mut vm = crate::vm::Vm::with_settings(crate::eval::settings_with_store());
        vm.start_module(&std::rc::Rc::new(module));
        let _ = crate::eval::drive(&mut vm, &host);
        host.0.take()
    }

    /// The `derivation` global is not a primop: cppnix evaluates
    /// `derivation-internal.nix` into a value at startup and binds that. Here
    /// the same source is compiled on first use, so what the name resolves to
    /// is a lambda and applying it runs the wrapper around `derivationStrict`.
    ///
    /// Golden, for the reason the sibling test gives. All bytes are what nix
    /// 2.34.7 printed for the same expression on the cpp backend, on
    /// dev-compute-4, store directory `/nix/store`.
    #[test]
    fn the_derivation_global_is_the_wrapper_around_derivation_strict() {
        assert_eq!(render("builtins.typeOf derivation"), r#""lambda""#);
        assert_eq!(
            render(
                r#"(derivation { name = "w"; system = "x86_64-linux"; builder = "/bin/sh"; }).outPath"#
            ),
            r#""/nix/store/zqaf7spll6pc558li7qm0vp5gjfy8ikc-w""#
        );
        assert_eq!(
            render(
                r#"(derivation { name = "w"; system = "x86_64-linux"; builder = "/bin/sh"; }).type"#
            ),
            r#""derivation""#
        );
        // cppnix's `addConstant` puts one value in both the global scope and
        // the `builtins` set, so these are the same wrapper and not two.
        assert_eq!(
            render(
                r#"(builtins.derivation { name = "w"; system = "x86_64-linux"; builder = "/bin/sh"; }).outPath"#
            ),
            r#""/nix/store/zqaf7spll6pc558li7qm0vp5gjfy8ikc-w""#
        );
        // It is a global and not a keyword, so a binding shadows it.
        assert_eq!(render("let derivation = 1; in derivation"), "1");
    }

    /// Printing one whole, strictly. The wrapper's result contains itself
    /// (`all` is a list of the outputs' values, and the first of those is the
    /// result), so this is the expression that did not terminate before the
    /// printer learned `«repeated»`, and the two changes have to compose for
    /// these bytes to exist at all.
    #[test]
    fn a_whole_derivation_prints_the_way_cppnix_prints_it() {
        assert_eq!(
            render(r#"derivation { name = "w"; system = "x86_64-linux"; builder = "/bin/sh"; }"#),
            concat!(
                r#"{ all = [ «repeated» ]; builder = "/bin/sh"; "#,
                r#"drvAttrs = { builder = "/bin/sh"; name = "w"; system = "x86_64-linux"; }; "#,
                r#"drvPath = "/nix/store/0i079dr3v4wak7rxxzn9bv9xz78lm2d6-w.drv"; name = "w"; "#,
                r#"out = «repeated»; "#,
                r#"outPath = "/nix/store/zqaf7spll6pc558li7qm0vp5gjfy8ikc-w"; "#,
                r#"outputName = "out"; system = "x86_64-linux"; type = "derivation"; }"#
            )
        );
    }

    /// Two derivations compare by `outPath` and nothing else, which is
    /// cppnix's `eqValues` rule and not an optimisation standing in for the
    /// structural answer. `eval-okay-eq-derivations` is the case that proves
    /// the difference: `d == d // { dummy = 1; }` is true even though those
    /// two sets differ in size.
    ///
    /// It is also what makes the comparison terminate, for the same reason
    /// the printer needed `«repeated»`.
    #[test]
    fn derivations_compare_by_out_path_alone() {
        assert_eq!(
            render(
                r#"let d = derivation { name = "a"; system = "i686-linux"; builder = "/foo"; };
                       e = derivation { name = "a"; system = "i686-linux"; builder = "/foo"; };
                       f = derivation { name = "c"; system = "i686-linux"; builder = "/bar"; };
                   in [ (d == d) (d == e) (d == d // { dummy = 1; }) (d == f) ]"#
            ),
            "[ true true true false ]"
        );
    }

    /// A literal derivation, with the bytes cppnix produced for the same
    /// expression: `nix-instantiate --eval --strict -E` on nix
    /// 2.34.7+ix.g69e4d9e9db39, store directory `/nix/store`.
    ///
    /// Golden rather than structural. A structural assertion ("32 base-32
    /// characters, ends in `-hello`") passes for every wrong answer this
    /// module could produce, and which 32 characters is the entire question.
    #[test]
    fn a_literal_derivation_has_cppnix_s_paths() {
        assert_eq!(
            render(
                r#"builtins.derivationStrict {
                     name = "hello";
                     system = "x86_64-linux";
                     builder = "/bin/sh";
                   }"#
            ),
            "{ drvPath = \"/nix/store/x0sj6ynccvc1a8kxr8fifnlf7qlxw6hd-hello.drv\"; \
             out = \"/nix/store/pnwh4xsfs4j508bs9iw6bpkyc4zw6ryx-hello\"; }"
        );
    }

    /// `outputs` replaces the default rather than adding to it, and each name
    /// becomes an attribute of the result.
    #[test]
    fn declared_outputs_replace_the_default() {
        let rendered = render(
            r#"builtins.attrNames (builtins.derivationStrict {
                 name = "multi";
                 system = "x86_64-linux";
                 builder = "/bin/sh";
                 outputs = [ "dev" "lib" ];
               })"#,
        );
        assert_eq!(rendered, "[ \"dev\" \"drvPath\" \"lib\" ]");
    }

    /// A multi-output derivation, with cppnix's bytes. Each output has its
    /// own path (`makeOutputPath`'s `output:<name>` type string) and the set
    /// of output *names* is inside the hash even though none of their paths
    /// is, which is what the masked pass buys.
    #[test]
    fn a_multi_output_derivation_matches_cppnix() {
        assert_eq!(
            render(
                r#"builtins.derivationStrict {
                     name = "multi";
                     system = "x86_64-linux";
                     builder = "/bin/sh";
                     outputs = [ "dev" "lib" ];
                   }"#
            ),
            "{ dev = \"/nix/store/vvn2fvqkaxxglm77kwgzj0brhc4qsmva-multi-dev\"; \
             drvPath = \"/nix/store/vp4yhflnpb33asgsyxpxgai5qhrap4qk-multi.drv\"; \
             lib = \"/nix/store/gzx81a8pd54hrhh67r1j0b4lfqlxisq5-multi-lib\"; }"
        );
    }

    /// A derivation consuming another's output, with cppnix's bytes.
    ///
    /// This is the case the in-process hash table exists for, and the only
    /// one that can catch a wrong `mask_outputs` on the memo insert: the leaf
    /// derivation's own path does not depend on it, so
    /// [`a_literal_derivation_has_cppnix_s_paths`] passes either way. A test
    /// asserting only that the consumer's path *moved* is no better -- a
    /// wrong input hash also moves it.
    #[test]
    fn a_consumed_output_is_hashed_the_way_cppnix_hashes_it() {
        assert_eq!(
            render(
                r#"let a = builtins.derivationStrict {
                     name = "a"; system = "x86_64-linux"; builder = "/bin/sh";
                   };
                   in builtins.derivationStrict {
                     name = "b"; system = "x86_64-linux"; builder = "/bin/sh";
                     dep = a.out;
                   }"#
            ),
            "{ drvPath = \"/nix/store/g2wnwbbdkb6ww7124j7y0a2zhrfxs714-b.drv\"; \
             out = \"/nix/store/y5rzqdqhkh912mnfjdwsl96x6c2mq8hg-b\"; }"
        );
    }

    /// Three levels, where the third consumes both the second's output and
    /// the first's -- through `args`, which is the one attribute that is not
    /// an environment variable and whose context is accumulated on a
    /// different path through the walk.
    #[test]
    fn a_two_deep_chain_and_an_args_dependency_match_cppnix() {
        assert_eq!(
            render(
                r#"let a = builtins.derivationStrict {
                     name = "a"; system = "x86_64-linux"; builder = "/bin/sh";
                   };
                   in let b = builtins.derivationStrict {
                     name = "b"; system = "x86_64-linux"; builder = "/bin/sh";
                     dep = a.out;
                   };
                   in builtins.derivationStrict {
                     name = "c"; system = "x86_64-linux"; builder = "/bin/sh";
                     dep = b.out; args = [ "-x" a.out ];
                   }"#
            ),
            "{ drvPath = \"/nix/store/v15m7i45c5ihs7x3637463dfl8xmpk8r-c.drv\"; \
             out = \"/nix/store/3llw12slqpb3bvrr386p9l45z08r1rl7-c\"; }"
        );
    }

    /// The subtle one. cppnix coerces an attribute sharing a name with an
    /// output like any other -- so its context still becomes an input -- and
    /// then overwrites its environment variable with the output path, so its
    /// bytes never reach the builder. Two derivations differing only in that
    /// attribute's *context* must therefore differ, while `out`'s own value
    /// is not what either of them says.
    #[test]
    fn an_attribute_named_after_an_output_keeps_its_context_and_loses_its_value() {
        let plain = render(
            r#"(builtins.derivationStrict {
                 name = "c"; system = "x86_64-linux"; builder = "/bin/sh";
                 out = "ignored";
               }).out"#,
        );
        let with_context = render(
            r#"let a = builtins.derivationStrict {
                 name = "a"; system = "x86_64-linux"; builder = "/bin/sh";
               };
               in (builtins.derivationStrict {
                 name = "c"; system = "x86_64-linux"; builder = "/bin/sh";
                 out = a.out;
               }).out"#,
        );
        assert!(plain.starts_with("\"/nix/store/"), "{plain}");
        assert!(with_context.starts_with("\"/nix/store/"), "{with_context}");
        // Same name, same builder, same system; the only difference is what
        // the discarded attribute depended on.
        assert_ne!(plain, with_context);
    }

    /// Scalars of every coercible type in one derivation, with cppnix's
    /// bytes. The float is the reason this test exists: `coerceToString`
    /// renders it with `std::to_string` and the printer renders it with
    /// `%.6g`, and using the printer's rendering here produced a `.drv` that
    /// was well-formed, reproducible and not cppnix's -- caught by the
    /// cross-backend run on dev-compute-4 and by nothing before it.
    #[test]
    fn coerced_scalars_match_cppnix() {
        assert_eq!(
            render(
                r#"builtins.derivationStrict {
                     name = "scalars"; system = "x86_64-linux"; builder = "/bin/sh";
                     n = 42; f = 1.5; big = 1.0e10; t = true; e = false;
                     l = [ "a" 1 2.5 ];
                   }"#
            ),
            "{ drvPath = \"/nix/store/045wyza478dcp14d4wyd0w3mm7wgkk6h-scalars.drv\"; \
             out = \"/nix/store/rg0pjwrn5y3zgw9czz5fddid07ybpczj-scalars\"; }"
        );
    }

    /// `__ignoreNulls` drops a null attribute entirely: no environment
    /// variable, so the derivation is the one that never mentioned it.
    #[test]
    fn ignore_nulls_drops_the_attribute_rather_than_emptying_it() {
        let ignored = render(
            r#"(builtins.derivationStrict {
                 name = "n"; system = "x86_64-linux"; builder = "/bin/sh";
                 __ignoreNulls = true; extra = null;
               }).out"#,
        );
        let absent = render(
            r#"(builtins.derivationStrict {
                 name = "n"; system = "x86_64-linux"; builder = "/bin/sh";
                 __ignoreNulls = true;
               }).out"#,
        );
        let kept = render(
            r#"(builtins.derivationStrict {
                 name = "n"; system = "x86_64-linux"; builder = "/bin/sh";
                 extra = null;
               }).out"#,
        );
        assert_eq!(ignored, absent);
        // Without the flag the null coerces to the empty string and is a
        // variable like any other, so the path moves.
        assert_ne!(ignored, kept);
    }

    /// `__ignoreNulls` drops nulls and *only* nulls.
    ///
    /// The companion to `ignore_nulls_drops_the_attribute_rather_than_emptying_it`,
    /// which pins the null half. The flag is one `&&` away from dropping every
    /// attribute of a derivation that sets it, and nixpkgs sets it widely, so
    /// that mutation is a silently wrong `.drv` for a large part of the tree.
    /// Nothing was watching the other half until mutation testing looked
    /// (ENG-13020).
    ///
    /// Both paths are cppnix's, from `nix-instantiate --eval --strict` on
    /// 2.34.7.
    #[test]
    fn ignore_nulls_keeps_every_attribute_that_is_not_null() {
        let with_extra = render(
            r#"(builtins.derivationStrict {
                 name = "n"; system = "x86_64-linux"; builder = "/bin/sh";
                 __ignoreNulls = true; extra = "v";
               }).out"#,
        );
        let without = render(
            r#"(builtins.derivationStrict {
                 name = "n"; system = "x86_64-linux"; builder = "/bin/sh";
                 __ignoreNulls = true;
               }).out"#,
        );
        assert_eq!(
            with_extra,
            "\"/nix/store/2n3f3c4b1mvdm4gk41swq0zkcgrj961f-n\""
        );
        assert_eq!(without, "\"/nix/store/dgd4d4979i4g3fc25y4f3067gzhfaa03-n\"");
        assert_ne!(
            with_extra, without,
            "a non-null attribute was dropped by __ignoreNulls"
        );
    }

    /// `__structuredAttrs = false` is an ordinary attribute and reaches the
    /// environment.
    ///
    /// Only the *enabled* flag is swallowed, because cppnix skips it inside
    /// the branch it turns on (`primops.cc:1660`) and nowhere else. A guard
    /// that swallowed it unconditionally would drop a variable every
    /// `__structuredAttrs = false` derivation carries, which is a different
    /// output path for each of them.
    #[test]
    fn a_false_structured_attrs_flag_is_an_ordinary_variable() {
        let declared = render(
            r#"(builtins.derivationStrict {
                 name = "n"; system = "x86_64-linux"; builder = "/bin/sh";
                 __structuredAttrs = false;
               }).out"#,
        );
        let absent = render(
            r#"(builtins.derivationStrict {
                 name = "n"; system = "x86_64-linux"; builder = "/bin/sh";
               }).out"#,
        );
        assert_eq!(
            declared,
            "\"/nix/store/3fxw12dcd601qq0vrn8rnh6c55jvcpg7-n\""
        );
        assert_eq!(absent, "\"/nix/store/dgd4d4979i4g3fc25y4f3067gzhfaa03-n\"");
        assert_ne!(
            declared, absent,
            "__structuredAttrs = false was swallowed instead of becoming a variable"
        );
    }

    /// Under `__structuredAttrs`, `outputHashAlgo` and `outputHashMode` are
    /// still read for their meaning rather than only serialised into the JSON.
    ///
    /// `a_structured_fixed_output_derivation_matches_cppnix` declares both and
    /// does not test them: its hash is SRI, so the algorithm comes from the
    /// string, and its mode is `flat`, which is the default. Both attributes
    /// are therefore redundant in that fixture and deleting the code that
    /// reads them changes nothing about it.
    ///
    /// Here the hash is bare hex, so the algorithm is available *only* from
    /// `outputHashAlgo`, and the two modes are compared against each other so
    /// `outputHashMode` cannot be ignored either. Both paths are cppnix's.
    #[test]
    fn structured_attrs_still_reads_the_output_hash_algo_and_mode() {
        let fixed = |mode: &str| {
            render(&format!(
                r#"(builtins.derivationStrict {{
                     name = "f"; system = "x86_64-linux"; builder = "/bin/sh";
                     __structuredAttrs = true;
                     outputHash = "0000000000000000000000000000000000000000000000000000000000000000";
                     outputHashAlgo = "sha256";
                     outputHashMode = "{mode}";
                   }}).out"#
            ))
        };
        assert_eq!(
            fixed("flat"),
            "\"/nix/store/zpncywkm9d9isz2ld7nirj16v8ads0a2-f\"",
            "the flat path moved, so outputHashAlgo or outputHashMode is not being read"
        );
        assert_eq!(
            fixed("recursive"),
            "\"/nix/store/23qvwckf0gjik4ws6ybmq4hjk0z6c3zc-f\"",
            "the recursive path moved, so outputHashMode is not being read"
        );
        assert_ne!(fixed("flat"), fixed("recursive"));
    }

    /// A fixed-output derivation must have exactly one output and it must be
    /// named `out` -- both halves, not either.
    ///
    /// cppnix refuses this with the same sentence
    /// (`multiple outputs are not supported in fixed-output derivations`),
    /// which reads oddly for a single output named something else and is
    /// nonetheless the message it produces. Accepting it instead would compute
    /// a fixed-output path for an output cppnix never builds.
    #[test]
    fn a_fixed_output_derivation_must_use_the_output_named_out() {
        let renamed = render(
            r#"(builtins.derivationStrict {
                 name = "n"; system = "x86_64-linux"; builder = "/bin/sh";
                 outputs = [ "dev" ];
                 outputHash = "sha256-jZkUKv2SV28wsM18tCqNxoCZmLxdYH2Idh9RLibH2yA=";
               }).dev"#,
        );
        assert!(
            renamed.contains("multiple outputs are not supported in fixed-output derivations"),
            "a fixed-output derivation whose output is not 'out' was accepted: {renamed}"
        );
    }

    #[test]
    fn required_attributes_are_reported_in_cppnix_s_order() {
        assert!(
            render(r#"builtins.derivationStrict { name = "x"; }"#)
                .contains("required attribute 'builder' missing"),
        );
        assert!(
            render(r#"builtins.derivationStrict { name = "x"; builder = "/bin/sh"; }"#)
                .contains("required attribute 'system' missing"),
        );
        assert!(render("builtins.derivationStrict { }").contains("attribute 'name' missing"));
    }

    /// `__structuredAttrs = true` puts every attribute into one `__json`
    /// environment variable instead of one variable each, so the derivation
    /// hashes different bytes and lands on a different path.
    ///
    /// Golden, and the golden is cppnix's own: this is
    /// `tests/functional/lang/eval-okay-derivation-legacy.nix` verbatim, and
    /// the expected path is the one in its `.exp` file. Matching it proves
    /// the `__json` bytes exactly, since the path is a hash of the ATerm they
    /// sit in, and nothing weaker would: an object with the right members in
    /// the wrong order, or with a space after a colon, hashes differently.
    #[test]
    fn structured_attrs_hash_cppnix_s_json_bytes() {
        assert_eq!(
            render(
                r#"(builtins.derivationStrict {
                     name = "eval-okay-derivation-legacy";
                     system = "x86_64-linux";
                     builder = "/dontcare";
                     __structuredAttrs = true;
                     allowedReferences = [ ];
                     disallowedReferences = [ ];
                     allowedRequisites = [ ];
                     disallowedRequisites = [ ];
                     maxSize = 1234;
                     maxClosureSize = 12345;
                   }).out"#
            ),
            r#""/nix/store/mzgwvrjjir216ra58mwwizi8wj6y9ddr-eval-okay-derivation-legacy""#
        );
        // The flag alone moves the path, which is what makes the golden a
        // test of the structured branch rather than of the flat one.
        let base = r#"name = "sa"; system = "x86_64-linux"; builder = "/bin/sh";
                      n = 1234; l = [ "a" "b" ]; b = true; s = "str";"#;
        let flat = render(&format!("(builtins.derivationStrict {{ {base} }}).out"));
        let structured = render(&format!(
            "(builtins.derivationStrict {{ {base} __structuredAttrs = true; }}).out"
        ));
        // Both measured on dev-compute-4 under `eval-backend = cpp`.
        assert_eq!(flat, r#""/nix/store/y4a4hs37kdqzmg32wcislgyvx64crwr7-sa""#);
        assert_eq!(
            structured,
            r#""/nix/store/ishqfc03fgvg0viq939ib2anfkyc0nmh-sa""#
        );
    }

    /// An output named `__json` collides with the variable the structured
    /// object is encoded in. cppnix checks this at unparse time, when the
    /// output names are already environment variables, so checking only the
    /// attributes answered with a well-formed path where cppnix raises. Found
    /// by the drv-parity case for it and not by anything else here.
    #[test]
    fn an_output_named_json_collides_with_the_structured_object() {
        let refused = render(
            r#"builtins.derivationStrict {
                 name = "g"; system = "x86_64-linux"; builder = "/bin/sh";
                 __structuredAttrs = true; outputs = [ "__json" ];
               }"#,
        );
        assert!(
            refused.contains("reserved for encoding structured attrs"),
            "{refused}"
        );
        // Without the flag it is an output name like any other.
        let fine = render(
            r#"(builtins.derivationStrict {
                 name = "g"; system = "x86_64-linux"; builder = "/bin/sh";
                 outputs = [ "__json" ];
               }).__json"#,
        );
        assert!(fine.starts_with("\"/nix/store/"), "{fine}");
    }

    /// The six attributes `__structuredAttrs` silently disables are warned
    /// about, in the walk's name order, and only under the flag.
    ///
    /// The warning is the only thing telling the reader their derivation does
    /// not do what it says, so losing it is a divergence even though the
    /// value is right. Watched failing by removing the `Yield::Need`, which
    /// empties the list.
    #[test]
    fn the_attributes_structured_attrs_disables_are_warned_about() {
        let attrs = r#"allowedReferences = [ ]; maxSize = 1234;"#;
        let got = warnings_of(&format!(
            r#"(builtins.derivationStrict {{
                 name = "w"; system = "x86_64-linux"; builder = "/bin/sh";
                 __structuredAttrs = true; {attrs}
               }}).out"#
        ));
        assert_eq!(
            got,
            vec![
                "In a derivation named 'w', 'structuredAttrs' disables the effect of the \
                 derivation attribute 'allowedReferences'; use \
                 'outputChecks.<output>.allowedReferences' instead"
                    .to_owned(),
                "In a derivation named 'w', 'structuredAttrs' disables the effect of the \
                 derivation attribute 'maxSize'; use 'outputChecks.<output>.maxSize' instead"
                    .to_owned(),
            ]
        );
        // Without the flag these are ordinary environment variables and
        // cppnix says nothing.
        assert!(
            warnings_of(&format!(
                r#"(builtins.derivationStrict {{
                     name = "w"; system = "x86_64-linux"; builder = "/bin/sh"; {attrs}
                   }}).out"#
            ))
            .is_empty()
        );
    }

    /// Every shape this does not build refuses by name and is counted
    /// `unimplemented`, never answered with an input-addressed path that
    /// would be wrong.
    #[test]
    fn unsupported_shapes_refuse_by_name() {
        let base = r#"name = "x"; system = "x86_64-linux"; builder = "/bin/sh";"#;
        for (attrs, want) in [
            ("__impure = true;", "__impure"),
            (r#"__json = "{}";"#, "__json"),
            // `outputHash` itself builds now. The two ingestion methods that
            // still refuse are the ones cpp puts behind an experimental
            // feature, and both change the store path, so guessing either
            // would be a wrong path rather than a missing one.
            (
                r#"outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="; outputHashMode = "text";"#,
                "dynamic-derivations",
            ),
            (
                r#"outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="; outputHashMode = "git";"#,
                "git-hashing",
            ),
            (
                r#"outputHash = "sha256-47DEQpj8HBSa+/TImW+5JCeuQeRkm5NMpJWZG3hSuFU="; outputHashAlgo = "blake3";"#,
                "blake3",
            ),
        ] {
            let rendered = render(&format!("builtins.derivationStrict {{ {base} {attrs} }}"));
            assert!(
                rendered.contains("Unimplemented") && rendered.contains(want),
                "{attrs} should refuse by name, got {rendered}"
            );
        }
        // Every output of a derivation is cppnix's `computeFSClosure`, a
        // store read of the whole graph below it.
        let deep = render(&format!(
            r#"let a = builtins.derivationStrict {{ {base} }};
               in builtins.derivationStrict {{ {base} dep = a.drvPath; }}"#
        ));
        assert!(
            deep.contains("Unimplemented") && deep.contains("every output"),
            "a drvPath dependency should refuse by name, got {deep}"
        );
    }

    /// cppnix recognises Blake3 in `parseHashAlgoOpt`, then requires
    /// `blake3-hashes` (`libutil/hash.cc:468-473`). The disabled case keeps
    /// the refusal token and prose emitted before Blake3 support; the enabled
    /// case must reach fixed-output path construction. The exact recursive
    /// path is pinned because it fails before the Blake3 `source`-scheme fix
    /// and passes after it; a shape-only assertion cannot distinguish that
    /// branch from the well-formed but wrong `output:out` branch.
    #[test]
    fn blake3_hashes_off_keeps_the_refusal_and_on_builds_the_derivation() {
        let refusal = match parse_algo("blake3", false) {
            Err(VmError::Unimplemented(refusal)) => Some(refusal),
            _ => None,
        };
        assert_eq!(
            refusal,
            Some(Refusal::new(
                RefusalToken::DerivationStrict,
                "builtins.derivationStrict with blake3 hashes (cppnix gates these behind the blake3-hashes experimental feature)",
            ))
        );

        assert!(matches!(
            parse_algo("blake3", true),
            Ok(Some(HashAlgo::Blake3))
        ));
        let settings = crate::eval::Settings {
            blake3_hashes: true,
            ..crate::eval::settings_with_store()
        };
        let path = crate::eval::render_str_with(
            &settings,
            r#"(builtins.derivationStrict {
                name = "x"; system = "x86_64-linux"; builder = "/bin/sh";
                outputHash = "6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85";
                outputHashAlgo = "blake3"; outputHashMode = "recursive";
            }).out"#,
        );
        assert_eq!(
            path, "\"/nix/store/yn6wzl5kf2iajdq4m3srq4hgy77i14d5-x\"",
            "enabled blake3-hashes should use cppnix's recursive source scheme"
        );
    }

    /// The content-addressed branch, against cppnix goldens: every string
    /// below is what this fork's `nix-instantiate` printed on the cpp
    /// backend with `extra-experimental-features = ca-derivations` and store
    /// `/nix/store`, taken rather than reasoned out (ENG-13140).
    ///
    /// The rows cover what decides the bytes: the `.drv` path (method and
    /// algo defaults, and both overridden), the output value (a downstream
    /// placeholder, not a path), a non-default output name (the placeholder
    /// preimage runs through `outputPathName`), and a derivation DOWNSTREAM
    /// of a CA one, whose own outputs go `Deferred` -- empty in the ATerm,
    /// placeholder in the value -- because no input-addressed path can exist
    /// over a floating input.
    #[test]
    fn content_addressed_matches_cppnix() {
        let mut settings = crate::eval::settings_with_store();
        settings.ca_derivations = true;
        let ca = r#"builtins.derivationStrict { name = "g"; system = "x86_64-linux"; builder = "/bin/sh"; __contentAddressed = true; }"#;
        let downstream = format!(
            r#"let a = {ca}; in builtins.derivationStrict {{ name = "h"; system = "x86_64-linux"; builder = "/bin/sh"; dep = a.out; }}"#
        );
        for (label, expr, want) in [
            (
                "drvPath, defaults (sha256 + NAR)",
                format!("({ca}).drvPath"),
                "/nix/store/x0s0ij87c3vk8n3zg8kd6yk8x3jydb76-g.drv",
            ),
            (
                "out is a downstream placeholder",
                format!("({ca}).out"),
                "/1c0q8mq6g6msknscl8sb5xpm44s662n7a09p9l13rgpafrprzxjl",
            ),
            (
                "non-default output name",
                r#"(builtins.derivationStrict { name = "g"; system = "x86_64-linux"; builder = "/bin/sh"; __contentAddressed = true; outputs = [ "dev" "out" ]; }).dev"#.to_owned(),
                "/0zmhz7rlr4mn3iz94244waas1zajpgxdnaxcmb774m0p7ix45z91",
            ),
            (
                "method and algo overridden",
                r#"(builtins.derivationStrict { name = "g"; system = "x86_64-linux"; builder = "/bin/sh"; __contentAddressed = true; outputHashMode = "flat"; outputHashAlgo = "sha1"; }).drvPath"#.to_owned(),
                "/nix/store/cw8fb430pxicyyy6nbx2c55zq5zjs863-g.drv",
            ),
            (
                "downstream of CA: deferred drvPath",
                format!("({downstream}).drvPath"),
                "/nix/store/h67izriairn23n32j5hs1v9qp8p41n31-h.drv",
            ),
            (
                "downstream of CA: deferred out placeholder",
                format!("({downstream}).out"),
                "/0mmf4mjzmvlnmm23p1spx9cpqs8kngpq2d3qq3rh5dxyqh88vblf",
            ),
        ] {
            assert_eq!(
                crate::eval::render_str_with(&settings, &expr),
                format!("\"{want}\""),
                "{label}"
            );
        }
    }

    /// With the feature off the attribute is cppnix's feature-is-disabled
    /// error, wording and all (`MissingExperimentalFeature`,
    /// `experimental-features.cc:411`) -- an evaluation error, not a named
    /// refusal, because cppnix fails the same way.
    #[test]
    fn content_addressed_without_the_feature_is_cppnixs_error() {
        let rendered = render(
            r#"(builtins.derivationStrict { name = "g"; system = "x86_64-linux"; builder = "/bin/sh"; __contentAddressed = true; }).drvPath"#,
        );
        assert!(
            rendered.contains(
                "experimental Nix feature 'ca-derivations' is disabled; add '--extra-experimental-features ca-derivations' to enable it"
            ),
            "got {rendered}"
        );
    }

    #[test]
    fn bad_output_names_are_refused() {
        let base = r#"name = "x"; system = "x86_64-linux"; builder = "/bin/sh";"#;
        assert!(
            render(&format!(
                r#"builtins.derivationStrict {{ {base} outputs = [ "drvPath" ]; }}"#
            ))
            .contains("invalid derivation output name 'drvPath'")
        );
        assert!(
            render(&format!(
                r#"builtins.derivationStrict {{ {base} outputs = [ "out" "out" ]; }}"#
            ))
            .contains("duplicate derivation output 'out'")
        );
        assert!(
            render(&format!(
                r#"builtins.derivationStrict {{ {base} outputs = [ ]; }}"#
            ))
            .contains("empty set of outputs")
        );
    }

    #[test]
    fn checked_names_are_cppnix_s() {
        assert!(check_derivation_name("hello-1.0").is_ok());
        assert!(check_derivation_name("+f?x=1_a.b").is_ok());
        for bad in ["", ".", "..", ".-x", "..-x", "a/b", "a b", "a:b"] {
            assert!(
                check_derivation_name(bad).is_err(),
                "'{bad}' should be refused"
            );
        }
        assert!(check_derivation_name(&"a".repeat(211)).is_ok());
        assert!(check_derivation_name(&"a".repeat(212)).is_err());
    }

    /// A derivation whose name ends in `.drv` would produce a store path a
    /// reader cannot tell from a derivation file.
    #[test]
    fn a_name_ending_in_drv_is_refused() {
        assert!(
            render(
                r#"builtins.derivationStrict {
                     name = "x.drv"; system = "x86_64-linux"; builder = "/bin/sh";
                   }"#
            )
            .contains("allowed to end in '.drv'")
        );
    }

    /// A path attribute is copied into the store and becomes the store path,
    /// with that path as an input source. cppnix leaves `copyToStore` at its
    /// default of on here (`primops.cc:1728`), which is the opposite of what
    /// `builtins.toString` passes, so reusing the wrong one of the two would
    /// produce a derivation that both names the source path and depends on
    /// nothing -- wrong twice, and identical in shape to a right answer.
    ///
    /// Pinned from both sides: against the same derivation written with the
    /// store path as a plain string (same bytes, no context, so a different
    /// `inputSrcs`), and against one written with the source path as a plain
    /// string (different bytes). Either comparison alone passes for a
    /// coercion that got one of the two halves wrong.
    #[test]
    fn a_path_attribute_is_copied_into_the_store() {
        use crate::compile;
        use crate::eval::drive;
        use crate::host::{FileType, Host, StoreError};
        use crate::value2::Value;
        use crate::vm::Vm;

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
                    p if p.path.as_ref() == "/m/f" => Ok(STORE_PATH.to_owned()),
                    p => Err(StoreError::Failed(format!("path '{p}' does not exist"))),
                }
            }
        }
        const STORE_PATH: &str = "/nix/store/00000000000000000000000000000000-f";

        fn run(src: &str) -> String {
            let Ok(module) = compile::compile_source(
                src,
                "/m",
                crate::compile::Origin::String,
                &crate::eval::settings_with_store(),
            ) else {
                return "compile failed".to_owned();
            };
            let module = std::rc::Rc::new(module);
            let mut vm = Vm::with_settings(crate::eval::settings_with_store());
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

        let drv = |attr: &str| {
            run(&format!(
                r#"(builtins.derivationStrict {{
                     name = "p"; system = "x86_64-linux"; builder = "/bin/sh";
                     src = {attr};
                   }}).out"#
            ))
        };
        let copied = drv("/m/f");
        assert!(
            copied.starts_with("\"/nix/store/"),
            "a path attribute should copy, not refuse: {copied}"
        );
        // Same bytes in the environment, no input source.
        assert_ne!(copied, drv(&format!(r#""{STORE_PATH}""#)));
        // Different bytes in the environment.
        assert_ne!(copied, drv(r#""/m/f""#));

        // `args` is the other way into the coercion and does not go through
        // the environment at all, so the assertions above say nothing about
        // it. Without this the two calls could disagree about `copyToStore`
        // and every test still passed -- which is what happened when this
        // guard was first written with only the attribute case.
        let with_args = |arg: &str| {
            run(&format!(
                r#"(builtins.derivationStrict {{
                     name = "p"; system = "x86_64-linux"; builder = "/bin/sh";
                     args = [ {arg} ];
                   }}).out"#
            ))
        };
        let arg_copied = with_args("/m/f");
        assert!(
            arg_copied.starts_with("\"/nix/store/"),
            "a path in args should copy, not refuse: {arg_copied}"
        );
        assert_ne!(arg_copied, with_args(&format!(r#""{STORE_PATH}""#)));
        assert_ne!(arg_copied, with_args(r#""/m/f""#));
    }

    /// The in-process table is the only source of an input's modulo hash, and
    /// a miss names the file rather than reaching for a store.
    ///
    /// Exercised directly because no expression can reach it yet: a `.drv`
    /// from outside the evaluation arrives through `builtins.storePath` or
    /// `builtins.appendContext`, and neither is implemented (ENG-12479).
    #[test]
    fn a_drv_from_outside_the_evaluation_is_refused_by_name() {
        let message = InProcess
            .read_drv("/nix/store/aaaa-foo.drv")
            .err()
            .unwrap_or_default();
        assert!(message.contains("/nix/store/aaaa-foo.drv"), "{message}");
        assert!(
            message.contains("not produced by this evaluation"),
            "{message}"
        );
    }
}
