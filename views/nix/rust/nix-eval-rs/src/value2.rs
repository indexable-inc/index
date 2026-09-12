//! Runtime values for the VM. `Value` is deliberately cheap to clone: every
//! aggregate is behind `Rc`. The two-word packed representation from the
//! architecture plan replaces this once semantics are complete; behavior
//! first, representation second, measured by the corpus differ throughout.

use crate::ir::Module;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::ops::Deref;
use std::rc::Rc;

/// Interned symbol id within one VM instance.
pub type Sym = u32;

/// The accessor a path value belongs to.
///
/// cppnix keeps this as the `SourceAccessor` inside `SourcePath`. A pointer
/// cannot cross this evaluator's ABI, so a mounted input is named by the
/// exact logical store path it was mounted at. The empty wire spelling is
/// reserved for the ambient filesystem.
#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Root {
    #[default]
    Ambient,
    Mounted(Rc<str>),
}

impl Root {
    #[must_use]
    pub fn mounted(mount_point: impl Into<Rc<str>>) -> Self {
        Root::Mounted(mount_point.into())
    }

    /// The ABI/witness spelling. Empty is ambient; a non-empty value is the
    /// exact key in cppnix's `storeFS` mount table.
    #[must_use]
    pub fn wire_name(&self) -> &str {
        match self {
            Root::Ambient => "",
            Root::Mounted(mount_point) => mount_point,
        }
    }

    pub fn from_wire_name(name: &str) -> Result<Root, String> {
        if name.is_empty() {
            return Ok(Root::Ambient);
        }
        require_canonical_absolute("mounted root", name)?;
        Ok(Root::mounted(name))
    }
}

/// `path` relative to `mount_point` when it is that root or lies below it:
/// the one test [`PathValue::try_new`], [`PathValue::normalized`] and
/// [`PathValue::accessor_path`] share.
fn below<'a>(mount_point: &str, path: &'a str) -> Option<&'a str> {
    if path == mount_point {
        return Some("/");
    }
    path.strip_prefix(mount_point)
        .filter(|rest| rest.starts_with('/'))
}

/// A Nix path's printed spelling and the accessor root that owns it.
///
/// `path` stays absolute because that is what Nix programs see. At the host
/// boundary a mounted path is encoded relative to `root`, matching the path
/// accepted by the mounted accessor and the identity cppnix fingerprints.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PathValue {
    pub root: Root,
    pub path: Rc<str>,
}

impl PathValue {
    #[must_use]
    pub fn ambient(path: impl Into<Rc<str>>) -> PathValue {
        PathValue {
            root: Root::Ambient,
            path: path.into(),
        }
    }

    #[must_use]
    #[expect(
        clippy::panic,
        reason = "the constructor for paths the evaluator derived itself (a parent, a join, a \
                  normalisation under a root it already holds): a mounted path outside its root \
                  here is a bug in the deriving code, not an input. `try_new` is the constructor \
                  for inputs (cached constants, witnesses, the wire)"
    )]
    pub fn new(root: Root, path: impl Into<Rc<str>>) -> PathValue {
        PathValue::try_new(root, path).unwrap_or_else(|why| panic!("{why}"))
    }

    /// [`PathValue::new`] for a pair that arrived from outside the evaluator
    /// -- a cached module's constant, a witness -- where a mounted path
    /// outside its root is a corrupt input to report, not an invariant to
    /// assert.
    pub fn try_new(root: Root, path: impl Into<Rc<str>>) -> Result<PathValue, String> {
        let path = path.into();
        if let Root::Mounted(mount_point) = &root
            && below(mount_point, &path).is_none()
        {
            return Err(format!(
                "mounted path '{path}' is outside its root '{mount_point}'"
            ));
        }
        Ok(PathValue { root, path })
    }

    #[must_use]
    pub fn with_path(&self, path: impl Into<Rc<str>>) -> PathValue {
        PathValue::new(self.root.clone(), path)
    }

    /// Lexically normalize `path` within `root`. A mounted accessor's `/` is
    /// a hard boundary: `..` cannot escape it into the ambient filesystem.
    #[must_use]
    pub fn normalized(root: Root, path: &str) -> PathValue {
        match &root {
            Root::Ambient => PathValue::new(root.clone(), normalize_path(path)),
            Root::Mounted(mount_point) => {
                let Some(relative) = below(mount_point, path) else {
                    // Outside its root: `new` reports that as the bug it is.
                    return PathValue::new(root.clone(), path);
                };
                let relative = normalize_path(relative);
                let absolute = if relative == "/" {
                    mount_point.to_string()
                } else {
                    format!("{mount_point}{relative}")
                };
                PathValue::new(root.clone(), absolute)
            }
        }
    }

    /// The path's parent within its accessor. The mounted root is its own
    /// parent, exactly as `/` is for the ambient accessor.
    #[must_use]
    pub fn parent(&self) -> PathValue {
        if let Root::Mounted(mount_point) = &self.root
            && self.path.as_ref() == mount_point.as_ref()
        {
            return self.clone();
        }
        let parent = match self.path.rfind('/') {
            Some(0) | None => "/",
            Some(i) => self.path.get(..i).unwrap_or("/"),
        };
        if let Root::Mounted(mount_point) = &self.root
            && parent.len() < mount_point.len()
        {
            return PathValue::new(self.root.clone(), Rc::clone(mount_point));
        }
        PathValue::new(self.root.clone(), parent)
    }

    /// The path understood by the accessor selected by [`PathValue::root`].
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "both constructors refuse a mounted path outside its root, and the fields, public \
                  so callers can read them, are written by nothing else: no struct literal and \
                  no field assignment exists outside this impl (`rg 'PathValue \\{'`, `rg \
                  '\\.(root|path) = '`), so the value below is there by construction until a \
                  writer is added, which is the change that must revisit this"
    )]
    pub fn accessor_path(&self) -> &str {
        match &self.root {
            Root::Ambient => &self.path,
            Root::Mounted(mount_point) => {
                below(mount_point, &self.path).expect("a mounted PathValue lies below its root")
            }
        }
    }

    /// Rebuild a VM path from the two fields carried over the ABI or in a
    /// witness. Reject malformed mounted pairs instead of treating them as
    /// ambient paths.
    pub fn from_wire(root: &str, accessor_path: &str) -> Result<PathValue, String> {
        if root.is_empty() {
            require_canonical_absolute("ambient path", accessor_path)?;
            return Ok(PathValue::ambient(accessor_path));
        }
        require_canonical_absolute("mounted root", root)?;
        require_canonical_absolute("accessor path", accessor_path)?;
        let absolute = if accessor_path == "/" {
            root.to_owned()
        } else {
            format!("{root}{accessor_path}")
        };
        Ok(PathValue::new(Root::mounted(root), absolute))
    }
}

fn require_canonical_absolute(kind: &str, path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("{kind} is not absolute"));
    }
    if normalize_path(path) != path {
        return Err(format!("{kind} is not canonical"));
    }
    Ok(())
}

#[must_use]
pub fn ambient_path(path: impl Into<Rc<str>>) -> Rc<PathValue> {
    Rc::new(PathValue::ambient(path))
}

impl Deref for PathValue {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        &self.path
    }
}

impl AsRef<str> for PathValue {
    fn as_ref(&self) -> &str {
        &self.path
    }
}

impl fmt::Display for PathValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.path)
    }
}

#[derive(Debug, Clone)]
pub enum Value {
    Int(i64),
    Float(f64),
    Bool(bool),
    Null,
    /// String with its (rarely present) context.
    Str(NixStr),
    Path(Rc<PathValue>),
    List(Rc<Vec<Slot>>),
    /// Sorted by symbol id at construction; iteration order for printing is
    /// name-alphabetical, resolved through the interner at print time.
    Attrs(Rc<Attrs>),
    Closure(Rc<ClosureData>),
    /// A builtin, possibly partially applied (arity > args.len()).
    Builtin(Rc<BuiltinData>),
}

impl Value {
    /// A list of these elements: the one place a list's backing `Vec` is
    /// put behind an `Rc`, so the allocation census counts every list and
    /// every element. Sharing an existing `Rc<Vec<Slot>>` (a list value
    /// passed through unchanged) is `Value::List(rc)` and is not a build.
    #[must_use]
    pub fn list(items: Vec<Slot>) -> Value {
        crate::perf::note_list(items.len());
        Value::List(Rc::new(items))
    }
}

/// An attribute set: the bindings, and where they were written.
///
/// A wrapper around the map rather than a second field on `Value::Attrs`,
/// because `Value` is stored inline in every stack entry and every slot:
/// widening the enum by two words to carry a fact only
/// `builtins.unsafeGetAttrPos` reads would cost every value in the program.
/// On the heap beside the map it costs one `Rc` bump and 16 bytes per set.
///
/// `Deref` to the map, so the hundred-odd places that only want to look
/// something up read exactly as they did.
///
/// The origin is deliberately not mutable outside this crate. Replacing it
/// would make the set's values and source provenance disagree; the constructors
/// below keep those two parts together.
///
/// ```compile_fail
/// use nix_eval_rs::value2::Attrs;
///
/// fn detach_origin(attrs: &mut Attrs) {
///     attrs.origin = None;
/// }
/// ```
#[derive(Debug)]
pub struct Attrs {
    map: AttrMap,
    /// Which `MkAttrs` built this set, or `None` when nothing in the source
    /// did.
    ///
    /// `None` is the honest answer for a set no source expression wrote, such
    /// as a set built by the bridge. `builtins.listToAttrs` carries each
    /// winning input pair's origin in the dynamic-origin slab. `//` carries a
    /// flat projection of the positions selected from both operands. When no
    /// origin exists, `unsafeGetAttrPos` answers `null`.
    pub(crate) origin: Option<AttrOrigin>,
}

/// Which instruction of which unit built an attribute set.
///
/// 16 bytes. Static sets copy a module refcount and two integers; their names
/// and offsets stay in the module's `attr_sites`. A set with a dynamic name
/// uses the integers as a tagged index into that module's reclaiming
/// dynamic-origin slab. `listToAttrs` retains the winning input pair's origin
/// for each result name. `Update` stores resolved per-name positions in a
/// third entry shape in the same slab. `Update` resolves byte offsets while it
/// builds that projection; line and column resolution remains confined to
/// `unsafeGetAttrPos`.
///
/// # Composite sets
///
/// `listToAttrs` already walks each pair and chooses one value for each
/// runtime name, so its slab entry records those winning pair origins.
/// `Update` projects every surviving name directly to the position from the
/// operand that supplied its value. Equality consults only the right origin;
/// a missing right position cannot expose a shadowed left position. The
/// projection is flat, so folding `//` replaces one result projection with
/// another and retains no origin chain.
///
/// The coordinates are read-only outside this crate. Mutating a cloned
/// coordinate would make its retained slab slot differ from the slot released
/// by `Drop`.
///
/// ```compile_fail
/// use nix_eval_rs::value2::AttrOrigin;
///
/// fn redirect_release(origin: &mut AttrOrigin) {
///     origin.ip = 0;
/// }
/// ```
#[derive(Debug)]
pub struct AttrOrigin {
    pub(crate) module: Rc<Module>,
    pub(crate) unit: u32,
    /// Index of the `MkAttrs` in the unit's ops, [`AttrOrigin::FORMALS`], or a
    /// slab index when `unit` is [`AttrOrigin::DYNAMIC_UNIT`],
    /// [`AttrOrigin::LIST_TO_ATTRS_UNIT`], or
    /// [`AttrOrigin::PROJECTED_UNIT`].
    pub(crate) ip: u32,
}

/// A module's symbols as one VM's global symbols: the link step.
///
/// `Module::symbols` is module-local, and every op that names an attribute
/// (`Select`, `HasAttr`, the formals of a call) used to re-intern its name on
/// each execution: a `String` clone of the module's spelling, then a hash
/// probe of the interner. On the darwin toplevel that was an allocation and
/// a probe per attribute access. This table is built once per module per
/// VM, on the first name the VM needs, and a lookup is an indexed load.
///
/// Runtime-only, like [`DynamicAttrOriginSlab`]: module encoding omits it
/// and a cloned module starts empty. It carries the id of the VM that built
/// it, and answers only that VM: two VMs number their interners
/// independently, and a module handed from one to the other (tests do this)
/// would otherwise resolve `a` to whatever symbol the first VM gave that
/// index -- a wrong attribute name, not a slow one. The table is never
/// invalidated within a VM because the interner only grows (`Vm::reset`
/// leaves it alone), so an index assigned once stays that name for the VM's
/// life.
#[derive(Debug, Default)]
pub(crate) struct LinkedSymbols(RefCell<Option<(u64, Box<[Sym]>)>>);

impl Clone for LinkedSymbols {
    fn clone(&self) -> LinkedSymbols {
        LinkedSymbols::default()
    }
}

impl LinkedSymbols {
    /// The global symbol for module-local `sym` as linked by VM `vm`, or
    /// `None` when this VM has not linked the module (or the index is out of
    /// range).
    pub(crate) fn get(&self, vm: u64, sym: u32) -> Option<Sym> {
        let linked = self.0.borrow();
        let (owner, table) = linked.as_ref()?;
        if *owner != vm {
            return None;
        }
        table.get(usize::try_from(sym).ok()?).copied()
    }

    /// Record VM `vm`'s numbering of every symbol, replacing any other VM's.
    pub(crate) fn set(&self, vm: u64, table: Box<[Sym]>) {
        *self.0.borrow_mut() = Some((vm, table));
    }
}

/// Reclaiming storage for dynamic, `listToAttrs`, and projected origins.
///
/// Entries are reused after their last `AttrOrigin` drops, so retained memory
/// is proportional to the high-water mark of simultaneously live slab
/// origins, not the number of requests a session has served. `None` until the
/// module's first origin: most modules never have one.
#[derive(Debug, Default)]
pub(crate) struct DynamicAttrOriginSlab(RefCell<Option<Box<DynamicAttrOrigins>>>);

#[derive(Debug, Default)]
struct DynamicAttrOrigins {
    dynamic: Slab<DynamicAttrOrigin>,
    list_to_attrs: Slab<ListToAttrsOrigin>,
    projected: Slab<ProjectedAttrOrigin>,
}

/// Reference-counted storage indexed by `u32`, the shape the three origin
/// kinds share: one holder at insertion, one more per [`Slab::retain`], the
/// value handed back by the [`Slab::release`] that drops the last.
///
/// A `retain` or `release` of an index that names no live entry, a free index
/// that names a live one, a count that overflows: each is a bug in the
/// holder's counting, and the holder is a `Clone` or `Drop` impl with no
/// error channel, so `slab_bug` stops the process in every build, as the
/// three slabs this one replaced did. A slab that kept going would hand a
/// position to two holders or free one still held, and a position is an
/// answer (`unsafeGetAttrPos`), so continuing is a wrong answer, not a leak.
#[derive(Debug)]
struct Slab<T> {
    entries: Vec<Option<Counted<T>>>,
    free: Vec<u32>,
}

#[derive(Debug)]
struct Counted<T> {
    refs: usize,
    value: T,
}

impl<T> Default for Slab<T> {
    fn default() -> Slab<T> {
        Slab {
            entries: Vec::new(),
            free: Vec::new(),
        }
    }
}

/// A [`Slab`] invariant did not hold (see the type's doc for why this is not
/// an error the caller could handle).
#[expect(
    clippy::panic,
    reason = "the holder is a `Clone` or `Drop` impl with no error channel, and a slab that \
              continued would hand one position to two holders; the panic is the only stop"
)]
fn slab_bug(what: &str, ip: u32) -> ! {
    panic!("slab: {what} (index {ip})")
}

impl<T> Slab<T> {
    /// The index of the new entry, or `None` when the slab is full.
    fn insert(&mut self, value: T) -> Option<u32> {
        let entry = Counted { refs: 1, value };
        if let Some(ip) = self.free.pop() {
            let Some(slot) = self.slot_mut(ip) else {
                slab_bug("a free index is outside the slab", ip)
            };
            if slot.is_some() {
                slab_bug("a free index names a live entry", ip)
            }
            *slot = Some(entry);
            return Some(ip);
        }
        let ip = u32::try_from(self.entries.len()).ok()?;
        self.entries.push(Some(entry));
        Some(ip)
    }

    fn slot_mut(&mut self, ip: u32) -> Option<&mut Option<Counted<T>>> {
        self.entries.get_mut(usize::try_from(ip).ok()?)
    }

    /// The live value at `ip`, if any.
    fn get(&self, ip: u32) -> Option<&T> {
        let entry = self.entries.get(usize::try_from(ip).ok()?)?.as_ref()?;
        Some(&entry.value)
    }

    /// One more holder of the entry at `ip`.
    fn retain(&mut self, ip: u32) {
        let Some(entry) = self.slot_mut(ip).and_then(Option::as_mut) else {
            slab_bug("retain of an entry that is not live", ip)
        };
        entry.refs = entry
            .refs
            .checked_add(1)
            .unwrap_or_else(|| slab_bug("too many holders of one entry", ip));
    }

    /// One fewer holder of the entry at `ip`; the value when that was the last.
    fn release(&mut self, ip: u32) -> Option<T> {
        let Some(slot) = self.slot_mut(ip) else {
            slab_bug("release of an index outside the slab", ip)
        };
        let Some(entry) = slot.as_mut() else {
            slab_bug("release of an entry that is not live", ip)
        };
        entry.refs = entry
            .refs
            .checked_sub(1)
            .unwrap_or_else(|| slab_bug("release of an entry with no holders", ip));
        if entry.refs != 0 {
            return None;
        }
        let Some(taken) = slot.take() else {
            slab_bug("a live entry vanished under its release", ip)
        };
        self.free.push(ip);
        Some(taken.value)
    }

    #[cfg(test)]
    fn live_len(&self) -> usize {
        self.entries.iter().flatten().count()
    }

    #[cfg(test)]
    fn slot_len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Debug)]
struct DynamicAttrOrigin {
    names: Box<[(Sym, u32)]>,
    fallback: Option<AttrOrigin>,
}

/// Origins retained only for attributes that won a `listToAttrs` insertion.
///
/// Keeping the input pair's origin defers the potentially recursive source
/// lookup until `unsafeGetAttrPos` while avoiding a reference cycle through
/// the input list and arbitrary unused attributes on its pairs.
#[derive(Debug)]
struct ListToAttrsOrigin {
    pairs: Box<[(Sym, AttrOrigin)]>,
    value_sym: Sym,
}

/// Resolved per-name positions for a set assembled from more than one origin.
///
/// `Update` pays this only for its result. Ordinary sets and their `Slot`s
/// remain unchanged. Storing resolved positions instead of child origins is
/// what makes a repeated `//` fold flat rather than a retained origin chain.
#[derive(Debug)]
struct ProjectedAttrOrigin {
    positions: Box<[ProjectedAttrPosition]>,
}

/// A resolved attribute position includes its source module. `listToAttrs`
/// may combine pairs imported from different files, so an offset alone could
/// name a real line in the wrong file.
#[derive(Debug, Clone)]
pub(crate) struct AttrPosition {
    pub module: Rc<Module>,
    pub offset: u32,
}

/// One flat `Update` projection entry. Field order keeps this at 16 bytes on
/// 64-bit targets: one module pointer and the two `u32`s `AttrOrigin` would
/// otherwise carry.
#[derive(Debug, Clone)]
pub(crate) struct ProjectedAttrPosition {
    module: Rc<Module>,
    sym: Sym,
    offset: u32,
}

impl ProjectedAttrPosition {
    #[must_use]
    pub(crate) fn new(sym: Sym, position: AttrPosition) -> ProjectedAttrPosition {
        ProjectedAttrPosition {
            module: position.module,
            sym,
            offset: position.offset,
        }
    }

    #[must_use]
    pub(crate) fn sym(&self) -> Sym {
        self.sym
    }
}

impl Clone for DynamicAttrOriginSlab {
    fn clone(&self) -> DynamicAttrOriginSlab {
        DynamicAttrOriginSlab::default()
    }
}

impl DynamicAttrOriginSlab {
    /// Run `f` on the origins, which exist from the first insertion on. A
    /// `retain` or `release` before that is the holder's counting bug and is
    /// treated as [`Slab`] treats one.
    fn with_live<R>(&self, f: impl FnOnce(&mut DynamicAttrOrigins) -> R) -> Option<R> {
        let mut slab = self.0.borrow_mut();
        let origins = slab.as_deref_mut();
        debug_assert!(
            origins.is_some(),
            "a slab origin outlives its module's slab"
        );
        origins.map(f)
    }

    fn insert(&self, names: Box<[(Sym, u32)]>, fallback: Option<AttrOrigin>) -> Option<u32> {
        self.0
            .borrow_mut()
            .get_or_insert_with(Box::default)
            .dynamic
            .insert(DynamicAttrOrigin { names, fallback })
    }

    fn retain(&self, ip: u32) {
        self.with_live(|origins| origins.dynamic.retain(ip));
    }

    fn release(&self, ip: u32) -> Option<DynamicAttrOrigin> {
        self.with_live(|origins| origins.dynamic.release(ip))?
    }

    fn position_of(
        &self,
        module: &Rc<Module>,
        ip: u32,
        name: &str,
        sym: Sym,
    ) -> Option<AttrPosition> {
        let slab = self.0.borrow();
        let origin = slab.as_deref()?.dynamic.get(ip)?;
        origin
            .names
            .binary_search_by_key(&sym, |(candidate, _)| *candidate)
            .ok()
            .and_then(|position| origin.names.get(position))
            .map(|(_, offset)| *offset)
            .filter(|p| *p != crate::ir::NO_POS)
            .map(|offset| AttrPosition {
                module: Rc::clone(module),
                offset,
            })
            .or_else(|| origin.fallback.as_ref()?.position_of(name, sym))
    }

    fn insert_list_to_attrs(&self, pairs: Box<[(Sym, AttrOrigin)]>, value_sym: Sym) -> Option<u32> {
        self.0
            .borrow_mut()
            .get_or_insert_with(Box::default)
            .list_to_attrs
            .insert(ListToAttrsOrigin { pairs, value_sym })
    }

    fn retain_list_to_attrs(&self, ip: u32) {
        self.with_live(|origins| origins.list_to_attrs.retain(ip));
    }

    fn release_list_to_attrs(&self, ip: u32) -> Option<ListToAttrsOrigin> {
        self.with_live(|origins| origins.list_to_attrs.release(ip))?
    }

    fn list_to_attrs_position_of(&self, ip: u32, sym: Sym) -> Option<AttrPosition> {
        let slab = self.0.borrow();
        let origin = slab.as_deref()?.list_to_attrs.get(ip)?;
        let pair_index = origin
            .pairs
            .binary_search_by_key(&sym, |(candidate, _)| *candidate)
            .ok()?;
        let (_, pair_origin) = origin.pairs.get(pair_index)?;
        pair_origin.position_of("value", origin.value_sym)
    }

    fn insert_projected(&self, positions: Box<[ProjectedAttrPosition]>) -> Option<u32> {
        self.0
            .borrow_mut()
            .get_or_insert_with(Box::default)
            .projected
            .insert(ProjectedAttrOrigin { positions })
    }

    fn retain_projected(&self, ip: u32) {
        self.with_live(|origins| origins.projected.retain(ip));
    }

    fn release_projected(&self, ip: u32) -> Option<ProjectedAttrOrigin> {
        self.with_live(|origins| origins.projected.release(ip))?
    }

    /// Every position this projected origin holds for the sorted `syms`,
    /// appended to `out` in `syms` order, in one walk under one borrow.
    ///
    /// `false` when `ip` names no live projected entry, in which case
    /// nothing was appended and the caller resolves name by name (which
    /// answers `None` for each, as [`Self::projected_position_of`] would).
    fn projected_positions_of(
        &self,
        ip: u32,
        syms: &[Sym],
        out: &mut Vec<ProjectedAttrPosition>,
    ) -> bool {
        let slab = self.0.borrow();
        let Some(origin) = slab
            .as_deref()
            .and_then(|origins| origins.projected.get(ip))
        else {
            return false;
        };
        let mut held = origin.positions.iter().peekable();
        for &sym in syms {
            while held.peek().is_some_and(|position| position.sym < sym) {
                held.next();
            }
            if let Some(position) = held.peek()
                && position.sym == sym
            {
                out.push((*position).clone());
            }
        }
        true
    }

    fn projected_position_of(&self, ip: u32, sym: Sym) -> Option<AttrPosition> {
        let slab = self.0.borrow();
        let origin = slab.as_deref()?.projected.get(ip)?;
        let position_index = origin
            .positions
            .binary_search_by_key(&sym, |position| position.sym)
            .ok()?;
        origin
            .positions
            .get(position_index)
            .map(|position| AttrPosition {
                module: Rc::clone(&position.module),
                offset: position.offset,
            })
    }

    #[cfg(test)]
    pub(crate) fn live_len(&self) -> usize {
        self.0
            .borrow()
            .as_deref()
            .map_or(0, DynamicAttrOrigins::live_len)
    }

    #[cfg(test)]
    pub(crate) fn slot_len(&self) -> usize {
        self.0
            .borrow()
            .as_deref()
            .map_or(0, DynamicAttrOrigins::slot_len)
    }
}

impl DynamicAttrOrigins {
    #[cfg(test)]
    fn live_len(&self) -> usize {
        self.dynamic.live_len() + self.list_to_attrs.live_len() + self.projected.live_len()
    }

    #[cfg(test)]
    fn slot_len(&self) -> usize {
        self.dynamic.slot_len() + self.list_to_attrs.slot_len() + self.projected.slot_len()
    }
}

impl AttrOrigin {
    /// `ip` for the set `builtins.functionArgs` builds out of a lambda's
    /// formal parameters, whose positions are on the `Param` and not on any
    /// instruction.
    pub const FORMALS: u32 = u32::MAX;
    /// `unit` tag saying `ip` indexes the module's dynamic-origin slab.
    pub const DYNAMIC_UNIT: u32 = u32::MAX;
    /// `unit` tag saying `ip` indexes the slab's `listToAttrs` origins.
    pub const LIST_TO_ATTRS_UNIT: u32 = u32::MAX - 1;
    /// `unit` tag saying `ip` indexes the slab's flat projected origins.
    pub const PROJECTED_UNIT: u32 = u32::MAX - 2;

    pub(crate) fn dynamic(
        module: Rc<Module>,
        mut names: Box<[(Sym, u32)]>,
        fallback: Option<AttrOrigin>,
    ) -> Option<AttrOrigin> {
        names.sort_unstable_by_key(|(sym, _)| *sym);
        debug_assert!(
            names.is_sorted_by(|a, b| a.0 < b.0),
            "dynamic origin names are unique"
        );
        let ip = module.dynamic_attr_origins.insert(names, fallback)?;
        Some(AttrOrigin {
            module,
            unit: AttrOrigin::DYNAMIC_UNIT,
            ip,
        })
    }

    pub(crate) fn list_to_attrs(
        mut pairs: Box<[(Sym, AttrOrigin)]>,
        value_sym: Sym,
    ) -> Option<AttrOrigin> {
        // Keep the first retained pair's module as the slab owner. Sorting is
        // a lookup detail and must not change ownership for cross-file input.
        let module = Rc::clone(&pairs.first()?.1.module);
        pairs.sort_unstable_by_key(|(sym, _)| *sym);
        debug_assert!(
            pairs.is_sorted_by(|a, b| a.0 < b.0),
            "listToAttrs origin names are unique"
        );
        let ip = module
            .dynamic_attr_origins
            .insert_list_to_attrs(pairs, value_sym)?;
        Some(AttrOrigin {
            module,
            unit: AttrOrigin::LIST_TO_ATTRS_UNIT,
            ip,
        })
    }

    pub(crate) fn projected(
        module: Rc<Module>,
        positions: Box<[ProjectedAttrPosition]>,
    ) -> Option<AttrOrigin> {
        debug_assert!(!positions.is_empty(), "a projected origin has a position");
        debug_assert!(
            positions.is_sorted_by(|a, b| a.sym < b.sym),
            "projected positions are sorted by symbol"
        );
        let ip = module.dynamic_attr_origins.insert_projected(positions)?;
        Some(AttrOrigin {
            module,
            unit: AttrOrigin::PROJECTED_UNIT,
            ip,
        })
    }

    /// [`Self::position_of`] for a sorted run of names, appended to `out` in
    /// that order, skipping the names this origin does not hold.
    ///
    /// A projected origin (the accumulator of a `//` fold) answers all of
    /// them in one linear walk of its flat positions beside `syms`, under
    /// one slab borrow. Before this the fold resolved every accumulated name
    /// with its own borrow and binary search at every step, and that was
    /// 5-8% of the darwin toplevel's main thread (round 8 profile,
    /// goals/rust-eval.md). Any other origin resolves name by name.
    pub(crate) fn positions_of<'a>(
        &self,
        syms: &[Sym],
        name_of: impl Fn(Sym) -> &'a str,
        out: &mut Vec<ProjectedAttrPosition>,
    ) {
        if self.unit == AttrOrigin::PROJECTED_UNIT
            && self
                .module
                .dynamic_attr_origins
                .projected_positions_of(self.ip, syms, out)
        {
            return;
        }
        for &sym in syms {
            if let Some(position) = self.position_of(name_of(sym), sym) {
                out.push(ProjectedAttrPosition::new(sym, position));
            }
        }
    }

    /// Where the attribute named `name` was written, or `None` when this
    /// origin does not name it because it came from somewhere else.
    #[must_use]
    pub(crate) fn position_of(&self, name: &str, sym: Sym) -> Option<AttrPosition> {
        if self.unit == AttrOrigin::DYNAMIC_UNIT {
            return self
                .module
                .dynamic_attr_origins
                .position_of(&self.module, self.ip, name, sym);
        }
        if self.unit == AttrOrigin::LIST_TO_ATTRS_UNIT {
            return self
                .module
                .dynamic_attr_origins
                .list_to_attrs_position_of(self.ip, sym);
        }
        if self.unit == AttrOrigin::PROJECTED_UNIT {
            return self
                .module
                .dynamic_attr_origins
                .projected_position_of(self.ip, sym);
        }
        let unit = self.module.units.get(self.unit as usize)?;
        // A lookup arrives with the name's text. Formals are scanned (nothing
        // bounds them, but a lambda's are a handful in practice). An attr
        // site's static half is in emission
        // order, the VM zips it with the values it pops, so the lookup goes
        // through the site's text-sorted index instead: `//` folds and
        // position tracking resolve names by the million, and a linear scan
        // here was 6.4% of the hil-compute-1 toplevel (2026-09-04 profile,
        // goals/rust-eval.md).
        let offset = if self.ip == AttrOrigin::FORMALS {
            let Some(crate::ir::Param::Formals { fields, .. }) = &unit.param else {
                return None;
            };
            fields
                .iter()
                .find(|f| self.module.symbol_is(f.sym, name))
                .map(|f| f.pos)
                .filter(|p| *p != crate::ir::NO_POS)?
        } else {
            let site = unit
                .attr_sites
                .binary_search_by_key(&self.ip, |s| s.ip)
                .ok()
                .and_then(|i| unit.attr_sites.get(i))?;
            site.static_offset(name, &self.module.symbols)
                .filter(|p| *p != crate::ir::NO_POS)?
        };
        Some(AttrPosition {
            module: Rc::clone(&self.module),
            offset,
        })
    }
}

impl Clone for AttrOrigin {
    fn clone(&self) -> AttrOrigin {
        if self.unit == AttrOrigin::DYNAMIC_UNIT {
            self.module.dynamic_attr_origins.retain(self.ip);
        } else if self.unit == AttrOrigin::LIST_TO_ATTRS_UNIT {
            self.module
                .dynamic_attr_origins
                .retain_list_to_attrs(self.ip);
        } else if self.unit == AttrOrigin::PROJECTED_UNIT {
            self.module.dynamic_attr_origins.retain_projected(self.ip);
        }
        AttrOrigin {
            module: Rc::clone(&self.module),
            unit: self.unit,
            ip: self.ip,
        }
    }
}

impl Drop for AttrOrigin {
    fn drop(&mut self) {
        if self.unit == AttrOrigin::DYNAMIC_UNIT {
            let released = self.module.dynamic_attr_origins.release(self.ip);
            // A fallback can belong to this module's slab too. Drop it after
            // the mutable borrow ends so its own `Drop` can enter the slab.
            drop(released);
            return;
        }
        if self.unit == AttrOrigin::LIST_TO_ATTRS_UNIT {
            let released = self
                .module
                .dynamic_attr_origins
                .release_list_to_attrs(self.ip);
            // A retained pair origin may use this slab. Release it after the
            // list slab's mutable borrow ends.
            drop(released);
            return;
        }
        if self.unit == AttrOrigin::PROJECTED_UNIT {
            let released = self.module.dynamic_attr_origins.release_projected(self.ip);
            // Positions can retain this slot's owner module. Drop them after
            // the mutable borrow ends.
            drop(released);
        }
    }
}

/// The bindings of an attribute set: `(Sym, Slot)` pairs in strictly
/// ascending `Sym` order, in one allocation.
///
/// cppnix's `Bindings` (a sorted array of 16-byte `Attr`s), chosen over a
/// `BTreeMap` for the reasons cppnix chose it: a set is read far more often
/// than it is built, most sets are small, and the two operations Nix code
/// performs on sets in bulk, lookup and `//`, are a binary search and a
/// merge over contiguous memory. Measured on the hil-compute-1 toplevel
/// (goals/rust-eval.md, footprint ledger): B-tree node clones and inserts
/// were 19% of every page the evaluation touched, because a leaf node holds
/// up to eleven entries in 160 bytes whether the set has one attribute or
/// eleven, and cloning a set for `//` rebuilt every node.
///
/// Iteration is in `Sym` order, the order the `BTreeMap` gave, and what the
/// merges rely on. Inserting into the middle moves the tail; the evaluator
/// builds sets from a stream it sorts once (`FromIterator`) or already has
/// sorted (`from_sorted`), or by merging (`update`), and reaches for `insert`
/// only for dynamic names, which are few.
#[derive(Debug, Clone, Default)]
pub struct AttrMap {
    entries: Vec<(Sym, Slot)>,
}

/// Borrowing iteration over an [`AttrMap`], `(&Sym, &Slot)` in `Sym` order.
#[derive(Debug, Clone)]
pub struct AttrIter<'a>(std::slice::Iter<'a, (Sym, Slot)>);

impl<'a> Iterator for AttrIter<'a> {
    type Item = (&'a Sym, &'a Slot);

    fn next(&mut self) -> Option<Self::Item> {
        self.0.next().map(|(k, v)| (k, v))
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl DoubleEndedIterator for AttrIter<'_> {
    fn next_back(&mut self) -> Option<Self::Item> {
        self.0.next_back().map(|(k, v)| (k, v))
    }
}

impl ExactSizeIterator for AttrIter<'_> {}

impl AttrMap {
    #[must_use]
    pub fn new() -> AttrMap {
        AttrMap {
            entries: Vec::new(),
        }
    }

    /// Pairs already in strictly ascending `Sym` order: what a merge over
    /// sorted inputs, or iteration over another map, produces. Sortedness is
    /// the caller's contract, checked in debug builds, which is why this is
    /// crate-private: every caller is in view.
    #[must_use]
    pub(crate) fn from_sorted(entries: Vec<(Sym, Slot)>) -> AttrMap {
        debug_assert!(
            entries
                .windows(2)
                .all(|w| matches!(w, [(a, _), (b, _)] if a < b)),
            "AttrMap::from_sorted: keys not in strictly ascending order"
        );
        AttrMap { entries }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn position(&self, key: Sym) -> std::result::Result<usize, usize> {
        self.entries.binary_search_by_key(&key, |(k, _)| *k)
    }

    #[must_use]
    pub fn get(&self, key: &Sym) -> Option<&Slot> {
        let i = self.position(*key).ok()?;
        self.entries.get(i).map(|(_, v)| v)
    }

    pub fn get_mut(&mut self, key: &Sym) -> Option<&mut Slot> {
        let i = self.position(*key).ok()?;
        self.entries.get_mut(i).map(|(_, v)| v)
    }

    #[must_use]
    pub fn contains_key(&self, key: &Sym) -> bool {
        self.position(*key).is_ok()
    }

    /// Bind `key`, returning the slot it replaced. Moves the tail when the
    /// name is new, so bulk construction goes through `FromIterator` or
    /// `from_sorted`.
    pub fn insert(&mut self, key: Sym, slot: Slot) -> Option<Slot> {
        match self.position(key) {
            Ok(i) => self
                .entries
                .get_mut(i)
                .map(|entry| std::mem::replace(&mut entry.1, slot)),
            Err(i) => {
                self.entries.insert(i, (key, slot));
                None
            }
        }
    }

    pub fn remove(&mut self, key: &Sym) -> Option<Slot> {
        let i = self.position(*key).ok()?;
        Some(self.entries.remove(i).1)
    }

    /// Keep the bindings `keep` accepts, in order.
    pub fn retain(&mut self, mut keep: impl FnMut(&Sym, &mut Slot) -> bool) {
        self.entries.retain_mut(|(k, v)| keep(k, v));
    }

    pub fn iter(&self) -> AttrIter<'_> {
        AttrIter(self.entries.iter())
    }

    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &Sym> + ExactSizeIterator + Clone {
        self.entries.iter().map(|(k, _)| k)
    }

    pub fn values(&self) -> impl DoubleEndedIterator<Item = &Slot> + ExactSizeIterator + Clone {
        self.entries.iter().map(|(_, v)| v)
    }

    /// `self // right`: every binding of `right`, plus the bindings of `self`
    /// whose names `right` lacks. One pass over each side; the result is born
    /// sorted.
    #[must_use]
    pub fn update(&self, right: &AttrMap) -> AttrMap {
        let mut out = Vec::with_capacity(self.len() + right.len());
        let mut lefts = self.entries.iter().peekable();
        let mut rights = right.entries.iter().peekable();
        while let (Some((lk, _)), Some((rk, _))) = (lefts.peek(), rights.peek()) {
            match lk.cmp(rk) {
                std::cmp::Ordering::Less => out.extend(lefts.next().cloned()),
                std::cmp::Ordering::Greater => out.extend(rights.next().cloned()),
                std::cmp::Ordering::Equal => {
                    lefts.next();
                    out.extend(rights.next().cloned());
                }
            }
        }
        out.extend(lefts.cloned());
        out.extend(rights.cloned());
        AttrMap { entries: out }
    }
}

/// Later pairs win, as `BTreeMap::from_iter` had it.
impl FromIterator<(Sym, Slot)> for AttrMap {
    fn from_iter<I: IntoIterator<Item = (Sym, Slot)>>(iter: I) -> AttrMap {
        let mut entries: Vec<(Sym, Slot)> = iter.into_iter().collect();
        // Stable, so equal keys keep their arrival order and the last one
        // of each run is the one that arrived last.
        entries.sort_by_key(|(k, _)| *k);
        let mut out: Vec<(Sym, Slot)> = Vec::with_capacity(entries.len());
        for entry in entries {
            match out.last_mut() {
                Some(last) if last.0 == entry.0 => *last = entry,
                _ => out.push(entry),
            }
        }
        AttrMap { entries: out }
    }
}

impl From<BTreeMap<Sym, Slot>> for AttrMap {
    /// A `BTreeMap` iterates sorted and unique, so this is the sorted-stream
    /// constructor with no check to fail.
    fn from(map: BTreeMap<Sym, Slot>) -> AttrMap {
        AttrMap {
            entries: map.into_iter().collect(),
        }
    }
}

impl IntoIterator for AttrMap {
    type Item = (Sym, Slot);
    type IntoIter = std::vec::IntoIter<(Sym, Slot)>;

    fn into_iter(self) -> Self::IntoIter {
        self.entries.into_iter()
    }
}

impl<'a> IntoIterator for &'a AttrMap {
    type Item = (&'a Sym, &'a Slot);
    type IntoIter = AttrIter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl Attrs {
    /// The one place an `Attrs` comes into existence, so the allocation
    /// census (`perf::note_attrs`) sees every set: the public constructors,
    /// `Default`, and `Clone` all come through here.
    fn built(map: AttrMap, origin: Option<AttrOrigin>) -> Attrs {
        crate::perf::note_attrs(map.len());
        Attrs { map, origin }
    }

    /// A set with no source behind it.
    #[must_use]
    pub fn new(map: impl Into<AttrMap>) -> Attrs {
        Attrs::built(map.into(), None)
    }

    /// A set with no source behind it, built from pairs already in strictly
    /// ascending `Sym` order: what a merge over two sorted sets produces, like
    /// `intersectAttrs`. States the order guarantee at the construction site;
    /// `AttrMap::from_sorted` checks it in debug builds.
    #[must_use]
    pub(crate) fn from_sorted_iter(iter: impl IntoIterator<Item = (Sym, Slot)>) -> Attrs {
        Attrs::built(AttrMap::from_sorted(iter.into_iter().collect()), None)
    }

    /// A set the given instruction built.
    #[must_use]
    pub fn at(map: impl Into<AttrMap>, origin: AttrOrigin) -> Attrs {
        Attrs::built(map.into(), Some(origin))
    }
}

impl Default for Attrs {
    fn default() -> Attrs {
        Attrs::built(AttrMap::new(), None)
    }
}

/// A clone is a second set on the heap, so it is counted as one, at the
/// width it has when cloned.
impl Clone for Attrs {
    fn clone(&self) -> Attrs {
        Attrs::built(self.map.clone(), self.origin.clone())
    }
}
impl From<BTreeMap<Sym, Slot>> for Attrs {
    fn from(map: BTreeMap<Sym, Slot>) -> Attrs {
        Attrs::new(map)
    }
}

impl std::ops::Deref for Attrs {
    type Target = AttrMap;

    fn deref(&self) -> &AttrMap {
        &self.map
    }
}

impl std::ops::DerefMut for Attrs {
    /// Mutating the bindings does NOT clear the origin, and the callers that
    /// rely on that are the derived sets: `//` and `removeAttrs` derive from
    /// a set whose origin is deliberately the one whose values survive. See
    /// [`AttrOrigin`] for why that is safe, and `Attrs::new` for a set with
    /// no origin at all.
    fn deref_mut(&mut self) -> &mut AttrMap {
        &mut self.map
    }
}

/// One element of a string's context: what the string depends on.
///
/// cppnix's `NixStringContextElem`, with its three cases and their rendered
/// spellings, which are corpus-visible through `builtins.getContext` and are
/// what `derivationStrict` reads `inputSrcs` and `inputDrvs` out of.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum ContextElem {
    /// A plain store path the string depends on: `/nix/store/...`.
    Opaque(Rc<str>),
    /// Every output of a derivation: `=/nix/store/....drv`.
    DrvDeep(Rc<str>),
    /// One named output of a derivation: `!out!/nix/store/....drv`.
    Built { drv: Rc<str>, output: Rc<str> },
}

impl ContextElem {
    /// How cppnix renders one element in an error, from
    /// `NixStringContextElem::display`: a bare store path, `=drv` for every
    /// output of a derivation, `!output!drv` for one of them.
    #[must_use]
    pub fn display(&self) -> String {
        match self {
            ContextElem::Opaque(p) => p.to_string(),
            ContextElem::DrvDeep(p) => format!("={p}"),
            ContextElem::Built { drv, output } => format!("!{output}!{drv}"),
        }
    }

    /// The same shapes, with each store path reduced to its name part.
    ///
    /// **Not a tidier [`ContextElem::display`], a different cppnix
    /// rendering.** `NixStringContextElem::to_string` (`value/context.cc:65`)
    /// writes `o.path.to_string()`, and a `StorePath`'s own `to_string` is
    /// `<hash>-<name>` with no store directory, so any message built from it
    /// names the base name. `forceStringNoCtx`'s failure is the other one and
    /// prints the whole path, which is why both exist rather than one being
    /// folded into the other. Quoted from nix 2.34.7+ix.g69e4d9e9db39:
    ///
    /// ```text
    /// toFile:            ... references !out!nrb4avj6...-d.drv
    /// forceStringNoCtx:  ... (such as '/nix/store/g76zcpqc...-a')
    /// ```
    #[must_use]
    pub fn display_base_name(&self) -> String {
        let base = |p: &str| p.rsplit('/').next().unwrap_or(p).to_owned();
        match self {
            ContextElem::Opaque(p) => base(p),
            ContextElem::DrvDeep(p) => format!("={}", base(p)),
            ContextElem::Built { drv, output } => format!("!{output}!{}", base(drv)),
        }
    }

    /// The inverse of [`ContextElem::display`], which is cppnix's
    /// `NixStringContextElem::parse` (`value/context.cc:9`).
    ///
    /// It exists because [`ContextElem::display`] is a wire format and not
    /// only an error rendering: a [`crate::task::NeedPath::Realise`] question
    /// travels to the embedder as these strings, and a witness naming one has
    /// to decode back into the same element or the question replayed is a
    /// different question. Recording and replaying through one pair of
    /// functions is what makes that impossible rather than merely unlikely.
    ///
    /// `None` for anything the renderer could not have produced. cppnix
    /// throws `BadNixStringContextElem` in the same three places -- an empty
    /// string, a leading `!` with no second `!`, and a `!` in a string that
    /// does not start with one -- and this crate has no error to raise here,
    /// because every caller of this is a decoder for which a malformed input
    /// is a miss rather than a program's fault.
    #[must_use]
    pub fn parse(s: &str) -> Option<ContextElem> {
        match s.as_bytes().first()? {
            b'!' => {
                let rest = s.get(1..)?;
                let (output, drv) = rest.split_once('!')?;
                // cppnix recurses here and rejects a nested `Built`, which
                // needs `dynamic-derivations`; this crate has no
                // representation for one, so a third `!` is malformed.
                if drv.contains('!') {
                    return None;
                }
                Some(ContextElem::Built {
                    drv: drv.into(),
                    output: output.into(),
                })
            }
            b'=' => Some(ContextElem::DrvDeep(s.get(1..)?.into())),
            _ => {
                if s.contains('!') {
                    return None;
                }
                Some(ContextElem::Opaque(s.into()))
            }
        }
    }
}

/// The context a value contributes to a string built out of it: a string's
/// own, and nothing for anything else. A path contributes nothing because a
/// coercion that does not copy creates no dependency, and one that does copy
/// answers with a string that already carries the element.
#[must_use]
pub fn context_of(v: &Value) -> BTreeSet<ContextElem> {
    match v {
        Value::Str(s) => s.context_set(),
        _ => BTreeSet::new(),
    }
}

/// A Nix string: the bytes, and the store paths the value depends on.
///
/// **Bytes, not text.** cppnix's `nString` is an arbitrary byte sequence --
/// `builtins.readFile` of a binary, `substring` sliced mid-codepoint,
/// `getEnv` of a variable that was never UTF-8 -- and every string operation
/// there is byte-oriented. Holding a Rust `str` here meant every such value
/// was either refused or repaired to U+FFFD before the program saw it
/// (ENG-13147, ENG-13146), which is a divergence no call site could observe
/// locally. So the representation is `Rc<[u8]>` and the places that
/// genuinely need text -- an attribute name for the interner, a path for the
/// host, a URL -- say so through [`NixStr::as_str`] and refuse loudly when
/// the bytes are not text, rather than every string paying a validation it
/// does not want.
///
/// The context is `Option<Rc<...>>` and not a plain set because the
/// overwhelming majority of strings in any evaluation have none, and a string
/// is the most frequently allocated value there is; `None` costs one null
/// pointer and no allocation.
///
/// Equality ignores the context, as cppnix's does: `"a" == "a"` whatever
/// either side depends on. That is also why the context is not part of `Ord`.
#[derive(Clone)]
pub struct NixStr {
    bytes: Rc<[u8]>,
    context: Option<Rc<BTreeSet<ContextElem>>>,
}

/// Text when the bytes are text, the raw byte array only when they are not:
/// almost every string a debugger meets is text, and a page of ASCII codes
/// hides the one byte that matters.
impl std::fmt::Debug for NixStr {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut d = f.debug_struct("NixStr");
        match self.as_str() {
            Some(text) => d.field("text", &text),
            None => d.field("bytes", &self.bytes),
        };
        d.field("context", &self.context).finish()
    }
}

impl NixStr {
    /// The bytes, with no claim about the context. Named rather than reached
    /// through `Deref` at the sites that matter, so a builtin dropping a
    /// context is visible in the source rather than only at runtime.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// The bytes, sharing the allocation.
    #[must_use]
    pub fn bytes_rc(&self) -> Rc<[u8]> {
        Rc::clone(&self.bytes)
    }

    /// The bytes as text, for the boundaries that are genuinely text-only:
    /// an attribute name headed for the interner, a path or URL headed for
    /// the host, a hash-algorithm name. `None` when the bytes are not UTF-8,
    /// and the caller decides what that means there -- usually a refusal,
    /// never a silent repair.
    #[must_use]
    pub fn as_str(&self) -> Option<&str> {
        std::str::from_utf8(&self.bytes).ok()
    }

    /// The bytes as text when they are text, a loud marker around the lossy
    /// rendering when they are not. For tests and examples: an assertion
    /// comparing against real text fails visibly on the marker, with no
    /// panic path in the library. Production code goes through
    /// `primops_pure::text_of`, which refuses by name.
    #[must_use]
    pub fn expect_text(&self) -> String {
        match self.as_str() {
            Some(text) => text.to_owned(),
            None => format!("<non-UTF-8 NixStr: {}>", self.lossy()),
        }
    }

    /// The bytes rendered for a human, U+FFFD for what is not text. **Error
    /// messages and logs only**: anything a program or a hash can observe
    /// must take [`NixStr::bytes`], because this rendering is exactly the
    /// repair the byte representation exists to avoid.
    #[must_use]
    pub fn lossy(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.bytes)
    }

    #[must_use]
    pub fn context(&self) -> Option<&Rc<BTreeSet<ContextElem>>> {
        self.context.as_ref()
    }

    pub fn has_context(&self) -> bool {
        self.context.as_ref().is_some_and(|c| !c.is_empty())
    }

    /// An owned copy of the context, for a builtin building a new string out
    /// of this one. Empty when there is none, so a caller can extend it
    /// without caring which case it started from.
    #[must_use]
    pub fn context_set(&self) -> BTreeSet<ContextElem> {
        self.context
            .as_ref()
            .map(|c| (**c).clone())
            .unwrap_or_default()
    }

    /// The same bytes with no context, which is `unsafeDiscardStringContext`.
    #[must_use]
    pub fn without_context(&self) -> NixStr {
        NixStr {
            bytes: Rc::clone(&self.bytes),
            context: None,
        }
    }

    /// The same bytes carrying `context`. `None` and an empty set are the same
    /// string, and are normalised to `None` so two equal strings cannot hash
    /// or print differently for a reason nobody can see.
    #[must_use]
    pub fn with_context(bytes: impl Into<Rc<[u8]>>, context: BTreeSet<ContextElem>) -> NixStr {
        NixStr {
            bytes: bytes.into(),
            context: if context.is_empty() {
                None
            } else {
                Some(Rc::new(context))
            },
        }
    }

    /// The same bytes carrying `context` in place of whatever this string
    /// had, sharing the byte allocation. What the context-rewriting builtins
    /// (`addDrvOutputDependencies`, `unsafeDiscardOutputDependency`,
    /// `appendContext`) hand back: all three keep `s` and replace the set.
    #[must_use]
    pub fn replacing_context(&self, context: BTreeSet<ContextElem>) -> NixStr {
        NixStr::with_context(Rc::clone(&self.bytes), context)
    }

    /// The union of several strings' contexts, for concatenation. cppnix's
    /// `copyContext` per part, which is what makes `"${a}${b}"` depend on
    /// everything `a` and `b` did.
    #[must_use]
    pub fn union_context<'a>(parts: impl Iterator<Item = &'a NixStr>) -> BTreeSet<ContextElem> {
        let mut out = BTreeSet::new();
        for p in parts {
            if let Some(c) = &p.context {
                out.extend(c.iter().cloned());
            }
        }
        out
    }
}

/// Bytes only, as cppnix's `eqValues` does for `nString`.
impl PartialEq for NixStr {
    fn eq(&self, other: &Self) -> bool {
        self.bytes == other.bytes
    }
}

impl Eq for NixStr {}

impl From<String> for NixStr {
    fn from(text: String) -> Self {
        NixStr {
            bytes: text.into_bytes().into(),
            context: None,
        }
    }
}

impl From<&str> for NixStr {
    fn from(text: &str) -> Self {
        NixStr {
            bytes: text.as_bytes().into(),
            context: None,
        }
    }
}

impl From<Vec<u8>> for NixStr {
    fn from(bytes: Vec<u8>) -> Self {
        NixStr {
            bytes: bytes.into(),
            context: None,
        }
    }
}

impl From<&[u8]> for NixStr {
    fn from(bytes: &[u8]) -> Self {
        NixStr {
            bytes: bytes.into(),
            context: None,
        }
    }
}

impl From<Rc<[u8]>> for NixStr {
    fn from(bytes: Rc<[u8]>) -> Self {
        NixStr {
            bytes,
            context: None,
        }
    }
}

impl From<Rc<str>> for NixStr {
    fn from(text: Rc<str>) -> Self {
        NixStr {
            // Allocation-free: std converts the `Rc` in place.
            bytes: text.into(),
            context: None,
        }
    }
}

#[derive(Debug)]
pub struct ClosureData {
    pub module: Rc<Module>,
    pub unit: u32,
    pub env: Env,
}

#[derive(Debug)]
pub struct BuiltinData {
    pub idx: u16,
    pub args: Vec<Slot>,
}

/// A lazily-evaluated cell: thunk until forced, then value forever.
#[derive(Debug, Clone)]
pub struct Slot(pub Rc<RefCell<SlotState>>);

#[derive(Debug)]
pub enum SlotState {
    Value(Value),
    Thunk {
        module: Rc<Module>,
        unit: u32,
        env: Env,
    },
    /// Under evaluation: hitting this is infinite recursion.
    Blackhole,
    /// A forced thunk whose evaluation threw. Re-forcing rethrows the same
    /// error (cppnix memoizes failures the same way).
    Failed(Rc<crate::vm::Catchable>),
    /// `f a b` not yet performed. cppnix's `mkApp` stores an **unforced**
    /// left-hand side and forces it at the application, so the callee here is
    /// a `Slot` and not a `Value`.
    ///
    /// The distinction is observable, not an internal detail:
    /// `builtins.mapAttrs` never forces its function (`primops.cc`,
    /// `prim_mapAttrs` forces only the set), so `mapAttrs throw attrs` is a
    /// set of unexploded thunks. Holding a forced `Value` here made the
    /// builtin strict in its function, which is what turned nixpkgs'
    /// `idrisPackages` self-reference -- `{ ... } // mapAttrs
    /// self.build-builtin-package { ... }` -- into `infinite recursion
    /// encountered` where cppnix succeeds (ENG-13124).
    PendingApply {
        f: Slot,
        args: Vec<Slot>,
    },
    /// A builtin (or other feature) the evaluator does not implement yet;
    /// forcing reports it as unimplemented, which the harnesses count
    /// separately from mismatches. The whole [`crate::refusal::Refusal`]
    /// is kept, not just the prose: a refusal memoized here by a spawned
    /// strand (ENG-13150) must re-raise with its original token, because
    /// the census groups by token and the sequential evaluation would
    /// have raised the original.
    Unimplemented(Rc<crate::refusal::Refusal>),
}

impl Slot {
    pub fn value(v: Value) -> Self {
        crate::perf::note_slot_value();
        Slot(Rc::new(RefCell::new(SlotState::Value(v))))
    }

    pub fn thunk(module: Rc<Module>, unit: u32, env: Env) -> Self {
        crate::perf::note_slot_thunk();
        Slot(Rc::new(RefCell::new(SlotState::Thunk {
            module,
            unit,
            env,
        })))
    }

    pub fn pending(f: Slot, args: Vec<Slot>) -> Self {
        crate::perf::note_slot_pending();
        Slot(Rc::new(RefCell::new(SlotState::PendingApply { f, args })))
    }

    pub fn unimplemented(what: &str) -> Self {
        crate::perf::note_slot_refusal();
        Slot(Rc::new(RefCell::new(SlotState::Unimplemented(Rc::new(
            crate::refusal::Refusal::new(crate::refusal::RefusalToken::UnimplementedBuiltin, what),
        )))))
    }

    /// The memoized value, or `None` when this slot has not been forced.
    /// The machine forces every value a builtin is allowed to look at, so a
    /// builtin reading `None` is an interpreter bug, never a Nix-level one.
    pub fn peek(&self) -> Option<Value> {
        match &*self.0.borrow() {
            SlotState::Value(v) => Some(v.clone()),
            _ => None,
        }
    }

    /// Identity of the cell, for cycle detection in deep traversals.
    pub fn id(&self) -> usize {
        Rc::as_ptr(&self.0) as usize
    }
}

/// One Nix expression routinely builds a value nested tens of thousands of
/// levels deep, and the derived drop glue recurses once per level, so a
/// teardown would blow the host stack right after an evaluation that never
/// touched it. Dismantle iteratively instead: take each cell's state out
/// behind a leaf and push its children onto a worklist.
impl Drop for Slot {
    fn drop(&mut self) {
        if Rc::strong_count(&self.0) != 1 {
            return;
        }
        let Ok(mut here) = self.0.try_borrow_mut() else {
            return;
        };
        let mut work = vec![Junk::State(std::mem::replace(
            &mut *here,
            SlotState::Blackhole,
        ))];
        drop(here);
        while let Some(j) = work.pop() {
            match j {
                Junk::State(SlotState::Value(v)) => dismantle_value(v, &mut work),
                Junk::State(SlotState::Thunk { env, .. }) => work.push(Junk::Env(env)),
                Junk::State(SlotState::PendingApply { f, args }) => {
                    for s in &args {
                        drain_slot(s, &mut work);
                    }
                    drain_slot(&f, &mut work);
                }
                Junk::State(_) => {}
                Junk::Env(e) => dismantle_env(e, &mut work),
            }
        }
    }
}

enum Junk {
    State(SlotState),
    Env(Env),
}

/// Empty a slot we are the last owner of, queueing whatever it held. The
/// slot itself is left holding a leaf, so its own `Drop` is then trivial.
fn drain_slot(s: &Slot, work: &mut Vec<Junk>) {
    if Rc::strong_count(&s.0) != 1 {
        return;
    }
    if let Ok(mut st) = s.0.try_borrow_mut() {
        work.push(Junk::State(std::mem::replace(
            &mut *st,
            SlotState::Blackhole,
        )));
    }
}

fn dismantle_value(v: Value, work: &mut Vec<Junk>) {
    match v {
        Value::List(rc) => {
            if let Ok(items) = Rc::try_unwrap(rc) {
                for s in &items {
                    drain_slot(s, work);
                }
            }
        }
        Value::Attrs(rc) => {
            if let Ok(map) = Rc::try_unwrap(rc) {
                for s in map.values() {
                    drain_slot(s, work);
                }
            }
        }
        Value::Closure(rc) => {
            if let Ok(c) = Rc::try_unwrap(rc) {
                work.push(Junk::Env(c.env));
            }
        }
        Value::Builtin(rc) => {
            if let Ok(b) = Rc::try_unwrap(rc) {
                for s in &b.args {
                    drain_slot(s, work);
                }
            }
        }
        Value::Int(_)
        | Value::Float(_)
        | Value::Bool(_)
        | Value::Null
        | Value::Str(_)
        | Value::Path(_) => {}
    }
}

fn dismantle_env(e: Env, work: &mut Vec<Junk>) {
    let mut cur = e;
    loop {
        match Rc::try_unwrap(cur) {
            Ok(EnvNode::Frame { up, slots }) => {
                for s in slots.borrow().iter() {
                    drain_slot(s, work);
                }
                cur = up;
            }
            Ok(EnvNode::With { up, subject }) => {
                drain_slot(&subject, work);
                cur = up;
            }
            Ok(EnvNode::Root) | Err(_) => return,
        }
    }
}

/// Environment: a chain of frames, innermost last. Frames are shared, not
/// copied, when captured by thunks and closures.
pub type Env = Rc<EnvNode>;

#[derive(Debug)]
pub enum EnvNode {
    Root,
    Frame {
        up: Env,
        slots: RefCell<Vec<Slot>>,
    },
    /// A `with` scope. Kept in the same chain so PopEnv/PopWith stay
    /// balanced under laziness (thunks capture whatever chain existed).
    With {
        up: Env,
        /// The with subject, lazily forced on first dynamic resolve.
        subject: Slot,
    },
}

impl EnvNode {
    /// A frame over `up` holding `slots`: the one place a frame is
    /// allocated, so the allocation census counts every frame and slot.
    #[must_use]
    pub fn frame(up: Env, slots: Vec<Slot>) -> Env {
        crate::perf::note_frame(slots.len());
        Rc::new(EnvNode::Frame {
            up,
            slots: RefCell::new(slots),
        })
    }

    /// Give a frame built empty its slots. A `let`, a `rec`, or a call with
    /// defaulted formals builds the frame first because its thunks capture
    /// it, then fills it once they exist; this is that second step, and the
    /// census counts the slots here rather than at the empty build.
    ///
    /// Only a frame has slots; a `with` scope or the root here is an
    /// interpreter bug, reported as one rather than dropped (the `if let`
    /// this replaces did nothing on that path).
    pub(crate) fn fill(frame: &Env, slots: Vec<Slot>) -> Result<(), crate::vm::VmError> {
        let EnvNode::Frame { slots: cell, .. } = &**frame else {
            return Err(crate::vm::VmError::eval(
                "internal: filling slots into an env node that is not a frame",
            ));
        };
        crate::perf::note_frame_filled(slots.len());
        *cell.borrow_mut() = slots;
        Ok(())
    }
}

pub fn type_name(v: &Value) -> &'static str {
    match v {
        Value::Int(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Bool(_) => "a Boolean",
        Value::Null => "null",
        Value::Str(_) => "a string",
        Value::Path(_) => "a path",
        Value::List(_) => "a list",
        Value::Attrs(_) => "a set",
        Value::Closure(_) | Value::Builtin(_) => "a function",
    }
}

/// Lexical path normalization ("." and ".." segments); never touches the
/// filesystem, same as cppnix's path handling. Applied both to path literals
/// at compile time and to the result of `path + string`, which is why
/// `/foo/bar + "/../xyzzy/." + "/foo.txt"` is `/foo/xyzzy/foo.txt`.
pub fn normalize_path(p: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for seg in p.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    format!("/{}", out.join("/"))
}

/// printf `%.6g`: how cppnix **prints** a float, in `printValue` and
/// everything built on it.
///
/// Not how it **coerces** one. cppnix has two float renderings and they
/// disagree on almost every value -- `coerceToString` uses
/// `std::to_string(double)`, which is `%f` (see [`format_f6`]). Store paths
/// depend on the coercion and never on this, so a comment here once claiming
/// that "drv hashes depend on it" was pointing at the wrong function; the
/// hashes depend on `format_f6`.
pub fn format_g6(x: f64) -> String {
    if x == 0.0 {
        return "0".to_owned();
    }
    let exp = x.abs().log10().floor() as i32;
    if (-5..6).contains(&exp) {
        let prec = usize::try_from(5i64 - i64::from(exp)).unwrap_or(0);
        let mut out = format!("{x:.prec$}");
        if out.contains('.') {
            while out.ends_with('0') {
                out.pop();
            }
            if out.ends_with('.') {
                out.pop();
            }
        }
        out
    } else {
        let s = format!("{x:.5e}");
        let Some((mantissa, e)) = s.split_once('e') else {
            return s;
        };
        let mantissa = mantissa.trim_end_matches('0').trim_end_matches('.');
        let e: i32 = e.parse().unwrap_or(0);
        format!("{mantissa}e{e:+03}")
    }
}

/// `std::to_string(double)`, which is how cppnix **coerces** a float to a
/// string (`eval.cc:2657`): `%f`, so exactly six digits after the point, never
/// an exponent, and no round trip -- `1.0` coerces to `"1.000000"` and `1e10`
/// to `"10000000000.000000"`.
///
/// This one is hashed. A float attribute of a derivation goes through
/// `coerceToString` into an environment variable and from there into the
/// `.drv` and every store path below it, so rendering it as [`format_g6`]
/// does (`"1"`, `"1e+10"`) produced a derivation that was well-formed, stable,
/// and not cppnix's -- measured as a wrong `outPath` against the cpp backend
/// on dev-compute-4.
pub fn format_f6(x: f64) -> String {
    format!("{x:.6}")
}

#[cfg(test)]
mod rooted_path_tests {
    use super::{PathValue, Root};

    #[test]
    fn accessor_path_borrows_each_wire_spelling() {
        let mount = "/nix/store/00000000000000000000000000000000-source";
        let ambient = PathValue::ambient("/tmp/a");
        let mounted_root = PathValue::new(Root::mounted(mount), mount);
        let mounted_child = PathValue::new(Root::mounted(mount), format!("{mount}/dir/a"));

        assert_eq!(ambient.accessor_path(), "/tmp/a");
        assert_eq!(mounted_root.accessor_path(), "/");
        assert_eq!(mounted_child.accessor_path(), "/dir/a");
    }

    #[test]
    fn wire_paths_reject_aliasing_spellings() {
        let mount = "/nix/store/00000000000000000000000000000000-source";
        for (root, path) in [
            (String::new(), "/a/../b"),
            (String::new(), "/a//b"),
            (String::new(), "/a/"),
            (format!("{mount}/"), "/a"),
            (format!("{mount}/../other"), "/a"),
            (mount.to_owned(), "/a/./b"),
        ] {
            assert!(
                PathValue::from_wire(&root, path).is_err(),
                "accepted root={root:?}, path={path:?}"
            );
        }
    }
}

impl fmt::Display for Value {
    /// Debug-ish display for errors; the real corpus printer lives in
    /// `print` (it needs the interner for attr names and forces lazily).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(n) => write!(f, "{n}"),
            Value::Float(x) => write!(f, "{}", format_g6(*x)),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Null => write!(f, "null"),
            // Debug rendering for errors, so lossy is honest here: the raw
            // bytes go through `print`, never through `Display`.
            Value::Str(s) => write!(f, "\"{}\"", s.lossy()),
            Value::Path(p) => write!(f, "{p}"),
            Value::List(_) => write!(f, "[ ... ]"),
            Value::Attrs(_) => write!(f, "{{ ... }}"),
            Value::Closure(_) | Value::Builtin(_) => write!(f, "<LAMBDA>"),
        }
    }
}

#[cfg(test)]
mod attr_origin_tests {
    use super::AttrOrigin;
    use crate::ir::Module;
    use std::rc::Rc;

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn attr_origin_remains_16_bytes() {
        assert_eq!(std::mem::size_of::<AttrOrigin>(), 16);
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn dynamic_origin_slab_layout_matches_the_documented_cost() {
        assert_eq!(std::mem::size_of::<super::DynamicAttrOriginSlab>(), 16);
        assert_eq!(std::mem::size_of::<super::Slab<()>>(), 48);
        assert_eq!(std::mem::size_of::<super::DynamicAttrOrigins>(), 144);
        assert_eq!(std::mem::size_of::<super::DynamicAttrOrigin>(), 32);
        assert_eq!(
            std::mem::size_of::<super::Counted<super::DynamicAttrOrigin>>(),
            40
        );
        assert_eq!(
            std::mem::size_of::<Option<super::Counted<super::DynamicAttrOrigin>>>(),
            40
        );
        assert_eq!(std::mem::size_of::<super::ListToAttrsOrigin>(), 24);
        assert_eq!(std::mem::size_of::<(super::Sym, AttrOrigin)>(), 24);
        assert_eq!(
            std::mem::size_of::<Option<super::Counted<super::ListToAttrsOrigin>>>(),
            32
        );
        assert_eq!(std::mem::size_of::<super::AttrPosition>(), 16);
        assert_eq!(std::mem::size_of::<super::ProjectedAttrPosition>(), 16);
        assert_eq!(std::mem::size_of::<super::ProjectedAttrOrigin>(), 16);
        assert_eq!(
            std::mem::size_of::<Option<super::Counted<super::ProjectedAttrOrigin>>>(),
            24
        );
    }

    #[test]
    fn the_last_origin_clone_reclaims_dynamic_position_storage() {
        let module = Rc::new(Module::default());
        let fallback = AttrOrigin {
            module: Rc::clone(&module),
            unit: 0,
            ip: 0,
        };
        let origin = AttrOrigin::dynamic(
            Rc::clone(&module),
            vec![(1, 2)].into_boxed_slice(),
            Some(fallback),
        )
        .expect("the slab has room");
        let first_ip = origin.ip;
        let clone = origin.clone();

        drop(origin);
        assert_eq!(module.dynamic_attr_origins.live_len(), 1);
        drop(clone);
        assert_eq!(module.dynamic_attr_origins.live_len(), 0);

        let reused = AttrOrigin::dynamic(Rc::clone(&module), vec![(3, 4)].into_boxed_slice(), None)
            .expect("the freed slot is reusable");
        assert_eq!(reused.ip, first_ip);
    }

    /// `//` resolves each winning position. These two origin kinds must make
    /// that lookup logarithmic, so their constructor boundary owns the sorted
    /// storage invariant even when evaluation encountered names out of order.
    #[test]
    fn lookup_origins_store_names_in_symbol_order() {
        let module = Rc::new(Module::default());
        let dynamic = AttrOrigin::dynamic(
            Rc::clone(&module),
            vec![(9, 90), (2, 20), (5, 50)].into_boxed_slice(),
            None,
        )
        .expect("the dynamic slab has room");
        {
            let slab = module.dynamic_attr_origins.0.borrow();
            let origins = slab.as_deref().expect("the slab was allocated");
            let stored = origins
                .dynamic
                .get(dynamic.ip)
                .expect("the dynamic origin is live");
            assert_eq!(
                stored.names.iter().map(|(sym, _)| *sym).collect::<Vec<_>>(),
                [2, 5, 9]
            );
        }

        let pairs = [8, 1, 4]
            .into_iter()
            .map(|sym| {
                (
                    sym,
                    AttrOrigin {
                        module: Rc::clone(&module),
                        unit: 0,
                        ip: 0,
                    },
                )
            })
            .collect::<Vec<_>>()
            .into_boxed_slice();
        let list = AttrOrigin::list_to_attrs(pairs, 0).expect("the list slab has room");
        let slab = module.dynamic_attr_origins.0.borrow();
        let origins = slab.as_deref().expect("the slab was allocated");
        let stored = origins
            .list_to_attrs
            .get(list.ip)
            .expect("the list origin is live");
        assert_eq!(
            stored.pairs.iter().map(|(sym, _)| *sym).collect::<Vec<_>>(),
            [1, 4, 8]
        );
    }

    #[test]
    fn the_last_list_to_attrs_origin_clone_reclaims_pair_origins() {
        let module = Rc::new(Module::default());
        let pair_origin = AttrOrigin {
            module: Rc::clone(&module),
            unit: 0,
            ip: 0,
        };
        let origin = AttrOrigin::list_to_attrs(vec![(1, pair_origin)].into_boxed_slice(), 2)
            .expect("the slab has room");
        let clone = origin.clone();

        drop(origin);
        assert_eq!(module.dynamic_attr_origins.live_len(), 1);
        drop(clone);
        assert_eq!(module.dynamic_attr_origins.live_len(), 0);
    }

    #[test]
    fn the_last_projected_origin_clone_reclaims_flat_positions() {
        let module = Rc::new(Module::default());
        let position = super::AttrPosition {
            module: Rc::clone(&module),
            offset: 7,
        };
        let origin = AttrOrigin::projected(
            Rc::clone(&module),
            vec![super::ProjectedAttrPosition::new(1, position)].into_boxed_slice(),
        )
        .expect("the slab has room");
        let first_ip = origin.ip;
        let clone = origin.clone();
        assert_eq!(
            origin.position_of("a", 1).map(|position| position.offset),
            Some(7)
        );

        drop(origin);
        assert_eq!(module.dynamic_attr_origins.live_len(), 1);
        drop(clone);
        assert_eq!(module.dynamic_attr_origins.live_len(), 0);

        let reused = AttrOrigin::projected(
            Rc::clone(&module),
            vec![super::ProjectedAttrPosition::new(
                2,
                super::AttrPosition {
                    module: Rc::clone(&module),
                    offset: 9,
                },
            )]
            .into_boxed_slice(),
        )
        .expect("the freed slot is reusable");
        assert_eq!(reused.ip, first_ip);
    }

    /// The counting stops the process instead of continuing: a `release` of
    /// an entry with no holders left is the slab's own bug, and the control
    /// for the three tests above (a slab that answered `None` here would pass
    /// every `live_len` assertion they make).
    #[test]
    #[should_panic(expected = "release of an entry that is not live (index 0)")]
    fn slab_release_of_a_freed_entry_stops() {
        let mut slab = super::Slab::default();
        let ip = slab.insert(7_u8).expect("the slab has room");
        assert_eq!(slab.release(ip), Some(7));
        slab.release(ip);
    }

    #[test]
    #[should_panic(expected = "retain of an entry that is not live (index 3)")]
    fn slab_retain_outside_the_slab_stops() {
        let mut slab: super::Slab<u8> = super::Slab::default();
        slab.retain(3);
    }
}

#[cfg(test)]
mod attr_map_tests {
    use super::{AttrMap, Attrs, Slot, Sym, Value};
    use std::collections::BTreeMap;

    fn slot(n: i64) -> Slot {
        Slot::value(Value::Int(n))
    }

    fn int_of(slot: &Slot) -> i64 {
        match &*slot.0.borrow() {
            super::SlotState::Value(Value::Int(n)) => *n,
            other => unreachable!("expected an int slot, found {other:?}"),
        }
    }

    fn pairs(map: &AttrMap) -> Vec<(Sym, i64)> {
        map.iter().map(|(k, v)| (*k, int_of(v))).collect()
    }

    /// The reference is the map this type replaced: whatever a `BTreeMap`
    /// built from the same pairs would hold, in the same order.
    fn reference(items: &[(Sym, i64)]) -> Vec<(Sym, i64)> {
        items
            .iter()
            .copied()
            .collect::<BTreeMap<Sym, i64>>()
            .into_iter()
            .collect()
    }

    #[cfg(target_pointer_width = "64")]
    #[test]
    fn one_binding_is_sixteen_bytes_and_the_map_is_one_vec() {
        assert_eq!(std::mem::size_of::<(Sym, Slot)>(), 16);
        assert_eq!(std::mem::size_of::<AttrMap>(), 24);
        assert_eq!(std::mem::size_of::<Attrs>(), 40);
    }

    #[test]
    fn from_iter_sorts_and_the_last_duplicate_wins() {
        let items = [(7, 1), (3, 2), (7, 3), (1, 4), (3, 5)];
        let map: AttrMap = items.iter().map(|(k, v)| (*k, slot(*v))).collect();
        assert_eq!(pairs(&map), reference(&items));
        assert_eq!(pairs(&map), vec![(1, 4), (3, 5), (7, 3)]);
    }

    #[test]
    fn insert_remove_and_lookup_match_the_reference() {
        let mut map = AttrMap::new();
        let mut model = BTreeMap::new();
        for (k, v) in [(5, 1), (2, 2), (9, 3), (5, 4), (1, 5)] {
            let replaced = map.insert(k, slot(v)).map(|s| int_of(&s));
            assert_eq!(replaced, model.insert(k, v));
        }
        assert_eq!(
            pairs(&map),
            model.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>()
        );
        assert_eq!(map.get(&5).map(int_of), Some(4));
        assert_eq!(map.get(&4).map(int_of), None);
        assert!(map.contains_key(&9));
        assert!(!map.contains_key(&0));
        assert_eq!(map.remove(&2).map(|s| int_of(&s)), Some(2));
        assert_eq!(map.remove(&2).map(|s| int_of(&s)), None);
        model.remove(&2);
        assert_eq!(
            pairs(&map),
            model.iter().map(|(k, v)| (*k, *v)).collect::<Vec<_>>()
        );
        assert_eq!(map.len(), 3);
        assert!(!map.is_empty());
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), vec![1, 5, 9]);
        assert_eq!(map.values().map(int_of).collect::<Vec<_>>(), vec![5, 4, 3]);
    }

    #[test]
    fn update_is_right_biased_and_sorted() {
        let left: AttrMap = [(1, 10), (3, 30), (5, 50), (8, 80)]
            .into_iter()
            .map(|(k, v)| (k, slot(v)))
            .collect();
        let right: AttrMap = [(0, 0), (3, 33), (6, 66), (8, 88), (9, 99)]
            .into_iter()
            .map(|(k, v)| (k, slot(v)))
            .collect();
        let merged = left.update(&right);
        assert_eq!(
            pairs(&merged),
            vec![(0, 0), (1, 10), (3, 33), (5, 50), (6, 66), (8, 88), (9, 99)]
        );
        // Either side empty is the other side.
        assert_eq!(pairs(&left.update(&AttrMap::new())), pairs(&left));
        assert_eq!(pairs(&AttrMap::new().update(&right)), pairs(&right));
        // Neither input moved.
        assert_eq!(left.len(), 4);
        assert_eq!(right.len(), 5);
    }

    #[test]
    fn retain_keeps_order_and_reports_each_key_once() {
        let mut map: AttrMap = (0..10).map(|k| (k, slot(i64::from(k)))).collect();
        let mut seen = Vec::new();
        map.retain(|k, _| {
            seen.push(*k);
            k % 3 == 0
        });
        assert_eq!(seen, (0..10).collect::<Vec<_>>());
        assert_eq!(pairs(&map), vec![(0, 0), (3, 3), (6, 6), (9, 9)]);
    }

    #[test]
    fn a_btreemap_converts_without_reordering() {
        let model: BTreeMap<Sym, Slot> = [(4, slot(4)), (2, slot(2)), (8, slot(8))]
            .into_iter()
            .collect();
        let keys: Vec<Sym> = model.keys().copied().collect();
        let map = AttrMap::from(model);
        assert_eq!(map.keys().copied().collect::<Vec<_>>(), keys);
        let attrs = Attrs::new(map);
        assert_eq!(attrs.len(), 3);
        assert_eq!(attrs.get(&8).map(int_of), Some(8));
        assert!(attrs.origin.is_none());
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "AttrMap::from_sorted: keys not in strictly ascending order")]
    fn from_sorted_refuses_an_unsorted_stream_in_debug_builds() {
        assert_eq!(
            AttrMap::from_sorted(vec![(2, slot(2)), (2, slot(3))]).len(),
            2
        );
    }
}
