//! Evaluate and normalize flake declarations without calling outputs.
//!
//! Rust owns declaration validation and the document schema. The host only
//! constructs fetch references, prefixes follows paths with its lock root,
//! and copies configuration paths into the store. Every force shares the
//! question's JobMemo so computed metadata participates in invalidation.

use serde_json::{Map, Value as Json};
use std::collections::{BTreeMap, HashSet};
use std::rc::Rc;

use crate::eval::{EvalError, JobMemo, drive_with, map_vm_error};
use crate::host::Host;
use crate::ir::Param;
use crate::refusal::{Refusal, RefusalToken};
use crate::session::{RenderMode, render_with};
use crate::value2::{Attrs, PathValue, Slot, Value, type_name};
use crate::vm::{ErrKind, Vm};

/// Every transported leaf has an explicit tag; paths cannot masquerade as sets.
#[derive(Debug, PartialEq)]
enum Setting {
    String(String),
    Bool(bool),
    Int(i64),
    Uint(u64),
    Strings(Vec<String>),
    Path(String),
}

impl Setting {
    fn json(self) -> Json {
        let (kind, value) = match self {
            Self::String(value) => ("string", Json::String(value)),
            Self::Bool(value) => ("bool", Json::Bool(value)),
            Self::Int(value) => ("int", Json::from(value)),
            Self::Uint(value) => ("uint", Json::from(value)),
            Self::Strings(value) => ("strings", Json::from(value)),
            Self::Path(value) => ("path", Json::String(value)),
        };
        serde_json::json!({ "kind": kind, "value": value })
    }
}

#[derive(Debug, PartialEq)]
enum Reference {
    Url(String),
    Attrs(BTreeMap<String, Setting>),
    Implicit(String),
}

impl Reference {
    fn json(self) -> Json {
        match self {
            Self::Url(value) => serde_json::json!({"kind": "url", "value": value}),
            Self::Implicit(value) => serde_json::json!({"kind": "implicit", "value": value}),
            Self::Attrs(attrs) => serde_json::json!({
                "kind": "attrs",
                "value": attrs.into_iter().map(|(k, v)| (k, v.json())).collect::<Map<_, _>>()
            }),
        }
    }
}

#[derive(Debug, PartialEq)]
struct Input {
    reference: Option<Reference>,
    is_flake: bool,
    follows: Option<Vec<String>>,
    overrides: BTreeMap<String, Input>,
}

impl Default for Input {
    fn default() -> Self {
        Self {
            reference: None,
            is_flake: true,
            follows: None,
            overrides: BTreeMap::new(),
        }
    }
}

impl Input {
    fn json(self) -> Json {
        serde_json::json!({
            "reference": self.reference.map(Reference::json),
            "is_flake": self.is_flake,
            "follows": self.follows,
            "overrides": inputs_json(self.overrides),
        })
    }
}

fn inputs_json(inputs: BTreeMap<String, Input>) -> Json {
    Json::Object(inputs.into_iter().map(|(k, v)| (k, v.json())).collect())
}

fn follows_path(value: &str, at: &str) -> Result<Vec<String>, EvalError> {
    crate::lock_graph::parse_input_path(value)
        .map_err(|message| error(format!("{message} at '{at}'")))
}

fn reference(
    mut attrs: BTreeMap<String, Setting>,
    url: Option<String>,
    follows: bool,
    at: &str,
) -> Result<Option<Reference>, EvalError> {
    let reference = if attrs.contains_key("type") {
        if !matches!(attrs.get("type"), Some(Setting::String(_))) {
            return Err(error(format!("input type must be a string at '{at}'")));
        }
        if let Some(url) = url {
            attrs.insert("url".to_owned(), Setting::String(url));
        }
        Some(Reference::Attrs(attrs))
    } else {
        if let Some(name) = attrs.keys().next() {
            return Err(error(format!(
                "unexpected flake input attribute '{name}', at '{at}'"
            )));
        }
        url.map(Reference::Url)
    };
    if reference.is_some() && follows {
        return Err(error(format!(
            "flake input has both a flake reference and a follows attribute, at '{at}'"
        )));
    }
    Ok(reference)
}

/// Read `root`, the evaluated `flake.nix`, into the document described in the
/// module comment.
///
/// `flake_dir` is the original flake directory under its input root. Every
/// document path is relative to it, even when a symlinked source resolved
/// its literals beside a target in another directory.
pub(crate) fn flake_document(
    vm: &mut Vm,
    host: &dyn Host,
    memo: &mut JobMemo,
    root: Slot,
    flake_dir: &PathValue,
) -> Result<String, EvalError> {
    let mut reader = Reader {
        vm,
        host,
        memo,
        flake_dir,
        active_inputs: HashSet::new(),
    };
    let document = reader.document(&root)?;
    serde_json::to_string(&Json::Object(document)).map_err(|e| {
        error(format!(
            "internal: the flake document did not serialise: {e}"
        ))
    })
}

fn error(message: String) -> EvalError {
    EvalError::eval(ErrKind::Eval, message)
}

struct Reader<'a> {
    vm: &'a mut Vm,
    host: &'a dyn Host,
    memo: &'a mut JobMemo,
    flake_dir: &'a PathValue,
    active_inputs: HashSet<usize>,
}

/// `CanonPath::makeRelative`: `path` spelled from `base`, both canonical and
/// absolute. `.` for the base itself, `..` per directory left to climb.
fn relative_path(base: &str, path: &str) -> String {
    let mut base_parts = base.split('/').filter(|c| !c.is_empty()).peekable();
    let mut path_parts = path.split('/').filter(|c| !c.is_empty()).peekable();
    loop {
        match (base_parts.peek(), path_parts.peek()) {
            (Some(b), Some(p)) if b == p => {
                base_parts.next();
                path_parts.next();
            }
            _ => break,
        }
    }
    let ups = base_parts.count();
    let rest: Vec<&str> = path_parts.collect();
    if ups == 0 && rest.is_empty() {
        return ".".to_owned();
    }
    let mut out: Vec<&str> = vec![".."; ups];
    out.extend(rest);
    out.join("/")
}

impl Reader<'_> {
    fn force(&mut self, slot: &Slot) -> Result<Value, EvalError> {
        if let Some(value) = slot.peek() {
            return Ok(value);
        }
        self.vm.start_force(slot.clone());
        drive_with(self.vm, self.host, self.memo).map_err(map_vm_error)
    }

    fn expect_attrs(&mut self, slot: &Slot, at: &str) -> Result<std::rc::Rc<Attrs>, EvalError> {
        match self.force(slot)? {
            Value::Attrs(attrs) => Ok(attrs),
            other => Err(error(format!(
                "expected a set but got {} at {at}",
                type_name(&other)
            ))),
        }
    }

    /// A set's bindings by name, in name order. Cloned out, so the reader can
    /// force while the set is not borrowed.
    fn entries(&self, attrs: &Attrs) -> Vec<(String, Slot)> {
        let mut entries: Vec<(String, Slot)> = attrs
            .iter()
            .map(|(sym, slot)| (self.vm.sym_name(*sym).to_owned(), slot.clone()))
            .collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    }

    fn document(&mut self, root: &Slot) -> Result<Map<String, Json>, EvalError> {
        let top = match self.force(root)? {
            Value::Attrs(attrs) => attrs,
            other => {
                return Err(error(format!(
                    "expected a set but got {} at the top of the file",
                    type_name(&other)
                )));
            }
        };
        let mut description = Json::Null;
        let mut inputs = BTreeMap::new();
        let mut self_attrs = Map::new();
        let mut formals = None;
        let mut config = Json::Object(Map::new());
        for (name, slot) in self.entries(&top) {
            let at = format!("'{name}'");
            match name.as_str() {
                "description" => {
                    let value = self.force(&slot)?;
                    description = Json::String(self.string_text(value, &at)?);
                }
                "inputs" => inputs = self.inputs(&slot, &name, Some(&mut self_attrs))?,
                "outputs" => formals = Some(self.outputs(&slot, &at)?),
                "nixConfig" => config = self.nix_config(&slot, &at)?,
                other => {
                    return Err(error(format!(
                        "flake has an unsupported attribute '{other}'"
                    )));
                }
            }
        }
        let formals = formals.ok_or_else(|| error("flake lacks attribute 'outputs'".to_owned()))?;
        for name in formals {
            if name != "self" {
                inputs.entry(name.clone()).or_insert_with(|| Input {
                    reference: Some(Reference::Implicit(name)),
                    ..Input::default()
                });
            }
        }
        Ok(Map::from_iter([
            ("description".to_owned(), description),
            ("inputs".to_owned(), inputs_json(inputs)),
            ("self_attrs".to_owned(), Json::Object(self_attrs)),
            ("config".to_owned(), config),
        ]))
    }

    /// Read nested input overrides. Bound native recursion and reject active
    /// cycles; shared sibling sets are valid and leave the active set on return.
    fn inputs(
        &mut self,
        slot: &Slot,
        path: &str,
        mut self_attrs: Option<&mut Map<String, Json>>,
    ) -> Result<BTreeMap<String, Input>, EvalError> {
        const MAX_INPUT_DEPTH: usize = 128;
        let inputs = self.expect_attrs(slot, &format!("'{path}'"))?;
        let identity = Rc::as_ptr(&inputs) as usize;
        if self.active_inputs.contains(&identity) {
            return Err(error(format!("cyclic flake inputs at '{path}'")));
        }
        if self.active_inputs.len() >= MAX_INPUT_DEPTH {
            return Err(error(format!(
                "flake input nesting exceeds {MAX_INPUT_DEPTH} at '{path}'"
            )));
        }
        self.active_inputs.insert(identity);
        let answer = (|| {
            let mut out = BTreeMap::new();
            for (name, slot) in self.entries(&inputs) {
                let path = format!("{path}.{name}");
                let input = self.expect_attrs(&slot, &format!("'{path}'"))?;
                if name == "self" {
                    let target = self_attrs.as_deref_mut().ok_or_else(|| {
                        error(format!("'self' input attribute not allowed at '{path}'"))
                    })?;
                    for (name, slot) in self.entries(&input) {
                        if name != "submodules" && name != "lfs" {
                            return Err(error(format!(
                                "flake 'self' attribute '{name}' is not supported"
                            )));
                        }
                        let value = self.force(&slot)?;
                        let Value::Bool(value) = value else {
                            return Err(error(format!(
                                "flake 'self' attribute '{name}' must be a Boolean"
                            )));
                        };
                        target.insert(name, Setting::Bool(value).json());
                    }
                } else {
                    out.insert(name, self.input(&input, &path)?);
                }
            }
            Ok(out)
        })();
        self.active_inputs.remove(&identity);
        answer
    }

    fn input(&mut self, input: &Attrs, path: &str) -> Result<Input, EvalError> {
        let mut result = Input::default();
        let mut attrs = BTreeMap::new();
        let mut url = None;
        for (name, slot) in self.entries(input) {
            let at = format!("{path}.{name}");
            match name.as_str() {
                "inputs" => result.overrides = self.inputs(&slot, &at, None)?,
                "publicKeys" => {
                    let json = self.deep_json(&slot, &at)?;
                    attrs.insert(name, Setting::String(json.to_string()));
                }
                "url" => {
                    let value = self.force(&slot)?;
                    url = Some(match value {
                        Value::Path(_) => format!("path:{}", self.path(&value, &at)?),
                        value => self.string_text(value, &at)?,
                    });
                }
                "flake" => {
                    let value = self.force(&slot)?;
                    let Value::Bool(value) = value else {
                        return Err(error(format!("expected a Boolean at '{at}'")));
                    };
                    result.is_flake = value;
                }
                "follows" => {
                    let value = self.force(&slot)?;
                    result.follows = Some(follows_path(&self.string_text(value, &at)?, &at)?);
                }
                _ => {
                    let value = self.force(&slot)?;
                    let value = match value {
                        Value::Str(_) => Setting::String(self.string_text(value, &at)?),
                        Value::Bool(value) => Setting::Bool(value),
                        Value::Int(value) => Setting::Uint(u64::try_from(value).map_err(|_| {
                            error(format!(
                                "negative value given for flake input attribute {name}: {value}"
                            ))
                        })?),
                        value => {
                            return Err(error(format!(
                                "unsupported input attribute type {} at '{at}'",
                                type_name(&value)
                            )));
                        }
                    };
                    attrs.insert(name, value);
                }
            }
        }
        result.reference = reference(attrs, url, result.follows.is_some(), path)?;
        Ok(result)
    }

    /// Render public keys strictly through the same memo as every other force.
    fn deep_json(&mut self, slot: &Slot, path: &str) -> Result<Json, EvalError> {
        let value = match slot.peek() {
            Some(value) => value,
            None => {
                self.vm.start_force(slot.clone());
                drive_with(self.vm, self.host, self.memo).map_err(map_vm_error)?
            }
        };
        let bytes = render_with(self.vm, self.host, value, RenderMode::Json, self.memo)?;
        serde_json::from_slice(&bytes).map_err(|e| {
            error(format!(
                "internal: toJSON produced text that is not JSON at '{path}': {e}"
            ))
        })
    }

    /// Read the lambda's parameter shape without invoking its body.
    fn outputs(&mut self, slot: &Slot, at: &str) -> Result<Vec<String>, EvalError> {
        let mut formals: Vec<String> = match self.force(slot)? {
            Value::Closure(closure) => match closure
                .module
                .units
                .get(closure.unit as usize)
                .and_then(|unit| unit.param.as_ref())
            {
                Some(Param::Formals { fields, .. }) => fields
                    .iter()
                    .map(|formal| {
                        closure
                            .module
                            .symbols
                            .get(formal.sym as usize)
                            .cloned()
                            .ok_or_else(|| {
                                error(format!(
                                    "internal: formal symbol {} is not in its module at {at}",
                                    formal.sym
                                ))
                            })
                    })
                    .collect::<Result<_, _>>()?,
                Some(Param::Ident(_)) | None => Vec::new(),
            },
            // Builtins expose no named parameters.
            Value::Builtin(_) => Vec::new(),
            other => {
                return Err(error(format!(
                    "expected a function but got {} at {at}",
                    type_name(&other)
                )));
            }
        };
        formals.sort();
        Ok(formals)
    }

    /// Read supported configuration values, recording any evaluation reads.
    fn nix_config(&mut self, slot: &Slot, at: &str) -> Result<Json, EvalError> {
        let settings = self.expect_attrs(slot, at)?;
        let mut out = Map::new();
        for (name, slot) in self.entries(&settings) {
            let at = format!("'nixConfig.{name}'");
            let value = match self.force(&slot)? {
                value @ Value::Str(_) => Setting::String(self.string_text(value, &at)?),
                value @ Value::Path(_) => Setting::Path(self.path(&value, &at)?),
                Value::Int(value) => Setting::Int(value),
                Value::Bool(value) => Setting::Bool(value),
                Value::List(items) => {
                    let mut strings = Vec::with_capacity(items.len());
                    for item in items.iter() {
                        let value = self.force(item)?;
                        strings.push(self.string_text(value, &at)?);
                    }
                    Setting::Strings(strings)
                }
                other => {
                    return Err(error(format!(
                        "flake configuration setting '{name}' is {}",
                        type_name(&other)
                    )));
                }
            };
            out.insert(name, value.json());
        }
        Ok(Json::Object(out))
    }

    /// `forceStringNoCtx`: the text, refusing a string that names a store
    /// path (a `flake.nix` has no derivations to name one) and one this
    /// text-only boundary cannot carry.
    fn string_text(&self, value: Value, at: &str) -> Result<String, EvalError> {
        match value {
            Value::Str(text) => {
                if text.has_context() {
                    return Err(error(format!(
                        "the string at {at} is not allowed to refer to a store path"
                    )));
                }
                match text.as_str() {
                    Some(text) => Ok(text.to_owned()),
                    None => Err(EvalError::Unimplemented(Refusal::new(
                        RefusalToken::NonUtf8Boundary,
                        format!("a non-UTF-8 string at {at} in a flake document"),
                    ))),
                }
            }
            other => Err(error(format!(
                "expected a string but got {} at {at}",
                type_name(&other)
            ))),
        }
    }

    /// The path spelled relative to the flake's directory. A path under
    /// another root cannot be resolved through this document's mounted tree.
    fn path(&self, value: &Value, at: &str) -> Result<String, EvalError> {
        let Value::Path(path) = value else {
            return Err(error(format!(
                "internal: {} is not a path",
                type_name(value)
            )));
        };
        if path.root != self.flake_dir.root {
            return Err(error(format!(
                "path '{}' at {at} is outside the flake's source tree",
                path.path
            )));
        }
        Ok(relative_path(&self.flake_dir.path, &path.path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(source: &str) -> Result<Json, EvalError> {
        let mut vm = Vm::with_settings(crate::eval::Settings::default());
        let host = crate::host::FnHost::default();
        let mut memo = JobMemo::default();
        let compiled = vm
            .compile(source, "/flake", crate::compile::Origin::String)
            .map_err(|e| error(e.to_string()))?;
        let root = crate::session::run_to_value_with(&mut vm, &compiled.module, &host, &mut memo)?;
        let json = flake_document(
            &mut vm,
            &host,
            &mut memo,
            Slot::value(root),
            &PathValue::ambient("/flake"),
        )?;
        serde_json::from_str(&json).map_err(|e| error(e.to_string()))
    }

    #[test]
    fn declarations_reject_ambiguous_references_and_invalid_shapes() {
        for source in [
            "{ inputs.a = { url = \"github:a/b\"; follows = \"b\"; }; outputs = x: {}; }",
            "{ inputs.a = { type = \"git\"; follows = \"b\"; }; outputs = x: {}; }",
            "{ inputs.a.revCount = -1; outputs = x: {}; }",
            "{ inputs.a.rev = \"x\"; outputs = x: {}; }",
            "{ inputs.a.type = true; outputs = x: {}; }",
            "{ inputs.a.flake = 1; outputs = x: {}; }",
            "{ inputs.a.inputs.self.lfs = true; outputs = x: {}; }",
            "{ inputs.self.unknown = true; outputs = x: {}; }",
            "{ inputs.self.lfs = \"yes\"; outputs = x: {}; }",
            "{ nixConfig.jobs = [ 1 ]; outputs = x: {}; }",
            "{ inputs.a = {}; }",
        ] {
            assert!(document(source).is_err(), "accepted {source}");
        }
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn normalization_preserves_explicit_inputs_and_adds_only_missing_formals() {
        let result = document(
            r#"{
            inputs.a = { flake = false; follows = "root/dep"; };
            inputs.self = { submodules = true; lfs = false; };
            outputs = { self, a, missing }: throw "must remain lazy";
            nixConfig = { jobs = -1; names = [ "one" "two" ]; registry = ./registry; };
        }"#,
        )
        .expect("valid document");
        assert_eq!(result["inputs"]["a"]["reference"], Json::Null);
        assert_eq!(result["inputs"]["a"]["is_flake"], false);
        assert_eq!(
            result["inputs"]["a"]["follows"],
            serde_json::json!(["root", "dep"])
        );
        assert_eq!(
            result["inputs"]["missing"]["reference"],
            serde_json::json!({"kind":"implicit", "value":"missing"})
        );
        assert!(result["inputs"].get("self").is_none());
        assert_eq!(
            result["self_attrs"]["lfs"],
            serde_json::json!({"kind":"bool", "value":false})
        );
        assert_eq!(
            result["config"]["jobs"],
            serde_json::json!({"kind":"int", "value":-1})
        );
        assert_eq!(
            result["config"]["names"],
            serde_json::json!({"kind":"strings", "value":["one", "two"]})
        );
        assert_eq!(
            result["config"]["registry"],
            serde_json::json!({"kind":"path", "value":"registry"})
        );
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn explicit_fetch_attributes_and_relative_urls_are_normalized() {
        let result = document(
            r#"{
            inputs.a = { type = "git"; url = "https://example.invalid/repo";
                revCount = 3; publicKeys = [ { type = "ssh-ed25519"; key = "k"; } ]; };
            inputs.b.url = ./sub;
            inputs.c.follows = "";
            outputs = x: {};
        }"#,
        )
        .expect("valid document");
        assert_eq!(
            result["inputs"]["a"]["reference"]["value"]["revCount"],
            serde_json::json!({"kind":"uint", "value":3})
        );
        assert_eq!(
            result["inputs"]["a"]["reference"]["value"]["publicKeys"],
            serde_json::json!({"kind":"string", "value":r#"[{"key":"k","type":"ssh-ed25519"}]"#})
        );
        assert_eq!(
            result["inputs"]["b"]["reference"],
            serde_json::json!({"kind":"url", "value":"path:sub"})
        );
        assert_eq!(result["inputs"]["c"]["follows"], serde_json::json!([]));
    }

    #[test]
    fn follows_paths_have_unambiguous_components() {
        for value in ["", "a", "a/b_c-D2"] {
            assert!(follows_path(value, "test").is_ok());
        }
        for value in ["/a", "a/", "a//b", "a/..", "1a", "a.b", "a/é"] {
            assert!(follows_path(value, "test").is_err(), "accepted {value}");
        }
    }

    /// The shapes `CanonPath::makeRelative` produces (canon-path.cc).
    #[test]
    fn relative_path_spells_like_make_relative() {
        assert_eq!(relative_path("/a/b", "/a/b/c"), "c");
        assert_eq!(relative_path("/a/b", "/a/b/c/d"), "c/d");
        assert_eq!(relative_path("/a/b", "/a/b"), ".");
        assert_eq!(relative_path("/a/b", "/a/c"), "../c");
        assert_eq!(relative_path("/a/b", "/"), "../..");
        assert_eq!(relative_path("/", "/x"), "x");
        assert_eq!(relative_path("/", "/"), ".");
    }
}
