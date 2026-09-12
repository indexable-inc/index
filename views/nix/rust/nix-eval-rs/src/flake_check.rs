//! Strict flake output validation. Each policy scope has a separate VM/question;
//! all forcing, including template existence checks, shares its recording memo.
use crate::eval::{EvalError, JobMemo, drive_with, map_vm_error};
use crate::host::Host;
use crate::ir::Param;
use crate::value2::{Attrs, Slot, Value, type_name};
use crate::vm::{ErrKind, Vm};
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

type Result<T> = std::result::Result<T, EvalError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    pub hydra: bool,
    pub all_systems: bool,
    pub keep_going: bool,
    /// Read-only validation cannot answer a request that must register derivations.
    pub evaluate_only: bool,
}
impl Options {
    pub fn from_flags(flags: i32) -> std::result::Result<Self, String> {
        if !(0..=15).contains(&flags) {
            return Err("invalid flake-check options".into());
        }
        Ok(Self {
            hydra: flags & 1 != 0,
            all_systems: flags & 2 != 0,
            keep_going: flags & 4 != 0,
            evaluate_only: flags & 8 != 0,
        })
    }
    pub fn flags(self) -> i32 {
        i32::from(self.hydra)
            | (i32::from(self.all_systems) << 1)
            | (i32::from(self.keep_going) << 2)
            | (i32::from(self.evaluate_only) << 3)
    }
}

#[derive(Default, Debug)]
pub struct Report {
    pub derivations: Vec<Derivation>,
    pub errors: Vec<String>,
    pub omitted_systems: BTreeSet<String>,
}
#[derive(Debug)]
pub struct Derivation {
    pub path: Vec<String>,
    pub drv_path: String,
    pub build: bool,
}
impl Report {
    pub fn encode(&self) -> String {
        json!({"derivations": self.derivations.iter().map(|d| json!({"path":d.path,"drvPath":d.drv_path,"build":d.build})).collect::<Vec<_>>(),
            "errors":self.errors,"omittedSystems":self.omitted_systems}).to_string()
    }
    pub fn decode(
        text: &[u8],
        store_dir: &str,
        options: Options,
        local_system: &str,
    ) -> std::result::Result<Self, String> {
        let value: Json = serde_json::from_slice(text).map_err(|e| e.to_string())?;
        let object = value
            .as_object()
            .ok_or("flake-check report must be an object")?;
        if object.len() != 3 {
            return Err("invalid flake-check report fields".into());
        }
        let strings = |name: &str| -> std::result::Result<Vec<String>, String> {
            object
                .get(name)
                .and_then(Json::as_array)
                .ok_or_else(|| format!("missing {name}"))?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or_else(|| format!("invalid {name}"))
                })
                .collect()
        };
        let errors = strings("errors")?;
        if !options.keep_going && !errors.is_empty() {
            return Err("unexpected keep-going diagnostics".into());
        }
        let omitted_systems: BTreeSet<_> = strings("omittedSystems")?.into_iter().collect();
        if omitted_systems.iter().any(|system| {
            options.hydra || options.all_systems || system == local_system || !system.contains('-')
        }) {
            return Err("omitted system contradicts check scope".into());
        }
        let mut derivations = Vec::new();
        for drv in object
            .get("derivations")
            .and_then(Json::as_array)
            .ok_or("missing derivations")?
        {
            let drv = drv.as_object().ok_or("invalid derivation record")?;
            if drv.len() != 3 {
                return Err("invalid derivation record fields".into());
            }
            let path: Vec<String> = drv
                .get("path")
                .and_then(Json::as_array)
                .ok_or("missing attribute path")?
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .map(str::to_owned)
                        .ok_or("invalid attribute path")
                })
                .collect::<std::result::Result<_, _>>()?;
            let namespace = path
                .first()
                .map(String::as_str)
                .ok_or("empty attribute path")?;
            let system_scoped = match namespace {
                "hydraJobs" if options.hydra && path.len() >= 2 => false,
                "checks" | "packages" | "devShells" if !options.hydra && path.len() == 3 => true,
                "formatter" if !options.hydra && path.len() == 2 => true,
                "nixosConfigurations" if !options.hydra && path.len() == 2 => false,
                _ => return Err("derivation attribute path contradicts check scope".into()),
            };
            let system = path.get(1).ok_or("missing derivation path component")?;
            if system_scoped
                && (!system.contains('-') || (!options.all_systems && system != local_system))
            {
                return Err("derivation system contradicts check scope".into());
            }
            let drv_path = drv
                .get("drvPath")
                .and_then(Json::as_str)
                .ok_or("missing drvPath")?;
            if !valid_drv_path(store_dir, drv_path) {
                return Err("invalid derivation store path".into());
            }
            let build = drv
                .get("build")
                .and_then(Json::as_bool)
                .ok_or("missing build decision")?;
            // The retained question determines build eligibility, never the cached bit alone.
            let expected_build = !options.hydra && namespace == "checks" && system == local_system;
            if build != expected_build {
                return Err("build decision contradicts check scope".into());
            }
            derivations.push(Derivation {
                path,
                drv_path: drv_path.to_owned(),
                build: expected_build,
            });
        }
        Ok(Self {
            derivations,
            errors,
            omitted_systems,
        })
    }
}
fn valid_drv_path(store_dir: &str, path: &str) -> bool {
    crate::storepath::parse_store_path(store_dir, path).is_some_and(|canonical| {
        format!("{store_dir}/{canonical}") == path && crate::storepath::is_derivation(path)
    })
}
fn error(message: impl Into<String>) -> EvalError {
    EvalError::eval(ErrKind::Eval, message)
}
pub(crate) fn path_text(path: &[String]) -> String {
    let mut result = Vec::new();
    for (index, part) in path.iter().enumerate() {
        if index != 0 {
            result.push(b'.');
        }
        crate::print::print_attr_name(part, &mut result);
    }
    String::from_utf8_lossy(&result).into_owned()
}
#[derive(Clone, Copy)]
enum Schema {
    Root,
    Systems(Leaf),
    Members(Leaf),
    Leaf(Leaf),
    Hydra(bool),
}
#[derive(Clone, Copy)]
enum Leaf {
    Derivation(bool),
    App,
    Overlay,
    Module,
    Configuration,
    Template,
    Bundler,
}
enum Work {
    Check {
        slot: Slot,
        path: Vec<String>,
        schema: Schema,
    },
    Leave {
        attrs: Rc<Attrs>,
        derivations: usize,
        errors: usize,
    },
}
struct Checker<'a> {
    vm: &'a mut Vm,
    host: &'a dyn Host,
    memo: &'a mut JobMemo,
    options: Options,
    system: String,
    store_dir: String,
    report: Report,
    pending: Vec<Work>,
    active: BTreeSet<usize>,
    completed_empty: BTreeMap<usize, Rc<Attrs>>,
}
impl Checker<'_> {
    fn force(&mut self, slot: Slot) -> Result<Value> {
        if let Some(value) = slot.peek() {
            return Ok(value);
        }
        self.vm.start_force(slot);
        drive_with(self.vm, self.host, self.memo).map_err(map_vm_error)
    }
    fn attrs(&mut self, slot: Slot) -> Result<Rc<Attrs>> {
        match self.force(slot)? {
            Value::Attrs(attrs) => Ok(attrs),
            other => Err(error(format!(
                "expected an attribute set, got {}",
                type_name(&other)
            ))),
        }
    }
    fn field(&mut self, attrs: &Attrs, name: &str) -> Result<Slot> {
        let sym = self.vm.intern(name);
        attrs
            .get(&sym)
            .cloned()
            .ok_or_else(|| error(format!("missing attribute '{name}'")))
    }
    fn optional(&mut self, attrs: &Attrs, name: &str) -> Option<Slot> {
        let sym = self.vm.intern(name);
        attrs.get(&sym).cloned()
    }
    fn string(&mut self, slot: Slot, context: bool) -> Result<String> {
        match self.force(slot)? {
            Value::Str(text) if context || !text.has_context() => text
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| error("expected UTF-8 string")),
            Value::Str(_) => Err(error("string must not carry a store context")),
            other => Err(error(format!(
                "expected a string, got {}",
                type_name(&other)
            ))),
        }
    }
    fn entries(&self, attrs: &Attrs) -> Vec<(String, Slot)> {
        let mut values: Vec<_> = attrs
            .iter()
            .map(|(name, slot)| (self.vm.sym_name(*name).to_owned(), slot.clone()))
            .collect();
        values.sort_by(|a, b| a.0.cmp(&b.0));
        values
    }
    fn push(&mut self, slot: Slot, path: &[String], name: String, schema: Schema) {
        let mut path = path.to_vec();
        path.push(name);
        self.pending.push(Work::Check { slot, path, schema });
    }
    fn allowed(&self, attrs: &Attrs, names: &[&str]) -> Result<()> {
        for (name, _) in self.entries(attrs) {
            if !names.contains(&name.as_str()) {
                return Err(error(format!("unsupported attribute '{name}'")));
            }
        }
        Ok(())
    }
    fn is_derivation(&mut self, attrs: &Attrs) -> Result<bool> {
        match self.optional(attrs, "type") {
            Some(slot) => Ok(
                matches!(self.force(slot)?,Value::Str(text) if text.as_str()==Some("derivation")),
            ),
            None => Ok(false),
        }
    }
    fn derivation(&mut self, slot: Slot, path: &[String], build: bool) -> Result<()> {
        let attrs = self.attrs(slot)?;
        if !self.is_derivation(&attrs)? {
            return Err(error("value is not a derivation"));
        }
        let name = self.field(&attrs, "name")?;
        self.string(name, false)?;
        let drv = self.field(&attrs, "drvPath")?;
        let drv_path = self.string(drv, true)?;
        if !valid_drv_path(&self.store_dir, &drv_path) {
            return Err(error("derivation has an invalid drvPath"));
        }
        self.report.derivations.push(Derivation {
            path: path.to_vec(),
            drv_path,
            build,
        });
        Ok(())
    }
    fn existing_path(&mut self, path: Slot) -> Result<()> {
        let value = self.force(path.clone())?;
        if !matches!(value, Value::Path(_) | Value::Str(_)) {
            return Err(error("expected a path or string"));
        }
        let builtins = self.vm.builtins_value().map_err(map_vm_error)?;
        let Value::Attrs(builtins) = builtins else {
            return Err(error("builtins are not an attribute set"));
        };
        let exists = self.field(&builtins, "pathExists")?;
        if !matches!(
            self.force(Slot::pending(exists, vec![path]))?,
            Value::Bool(true)
        ) {
            return Err(error("path does not exist"));
        }
        Ok(())
    }
    fn leaf(&mut self, slot: Slot, path: &[String], leaf: Leaf) -> Result<()> {
        match leaf {
            Leaf::Derivation(build) => {
                self.derivation(slot, path, build && path.get(1) == Some(&self.system))
            }
            Leaf::Configuration => {
                let mut current = slot;
                for name in ["config", "system", "build", "toplevel"] {
                    let attrs = self.attrs(current)?;
                    current = self.field(&attrs, name)?;
                }
                self.derivation(current, path, false)
            }
            Leaf::Module => match self.force(slot.clone())? {
                Value::Attrs(_) | Value::Closure(_) => Ok(()),
                Value::Path(_) | Value::Str(_) => self.existing_path(slot),
                _ => Err(error(
                    "module must be an attribute set, function, or existing path",
                )),
            },
            Leaf::Overlay | Leaf::Bundler => {
                let Value::Closure(closure) = self.force(slot)? else {
                    return Err(error("expected a function"));
                };
                if matches!(leaf, Leaf::Overlay) {
                    let param = closure
                        .module
                        .units
                        .get(closure.unit as usize)
                        .and_then(|unit| unit.param.as_ref());
                    let Some(Param::Ident(sym)) = param else {
                        return Err(error("overlay must take a positional 'final' argument"));
                    };
                    let name = closure
                        .module
                        .symbols
                        .get(*sym as usize)
                        .ok_or_else(|| error("invalid overlay parameter"))?;
                    if name != "final" && name != "_final" && name != "_" {
                        return Err(error("overlay must take an argument named 'final'"));
                    }
                }
                Ok(())
            }
            Leaf::App => {
                let attrs = self.attrs(slot)?;
                self.allowed(&attrs, &["type", "program", "meta"])?;
                let kind = self.field(&attrs, "type")?;
                if self.string(kind, false)? != "app" {
                    return Err(error("app type must be 'app'"));
                }
                let program = self.field(&attrs, "program")?;
                if self.string(program, true)?.is_empty() {
                    return Err(error("app program is empty"));
                }
                if let Some(meta) = self.optional(&attrs, "meta") {
                    let meta = self.attrs(meta)?;
                    if let Some(description) = self.optional(&meta, "description") {
                        self.string(description, false)?;
                    }
                }
                Ok(())
            }
            Leaf::Template => {
                let attrs = self.attrs(slot)?;
                self.allowed(&attrs, &["path", "description", "welcomeText"])?;
                let description = self.field(&attrs, "description")?;
                self.string(description, false)?;
                if let Some(welcome) = self.optional(&attrs, "welcomeText") {
                    self.string(welcome, false)?;
                }
                let path = self.field(&attrs, "path")?;
                self.existing_path(path)
            }
        }
    }
    fn check(&mut self, slot: Slot, path: &[String], schema: Schema) -> Result<()> {
        if path.len() > 1024 {
            return Err(error("flake-check nesting exceeds 1024"));
        }
        if let Schema::Leaf(leaf) = schema {
            return self.leaf(slot, path, leaf);
        }
        let attrs = self.attrs(slot)?;
        match schema {
            Schema::Root => {
                let outputs = self.field(&attrs, "outputs")?;
                let attrs = self.attrs(outputs)?;
                for (name, slot) in self.entries(&attrs).into_iter().rev() {
                    if self.options.hydra {
                        if name == "hydraJobs" {
                            self.push(slot, path, name, Schema::Hydra(true));
                        }
                        continue;
                    }
                    let schema = match name.as_str() {
                        "hydraJobs" => continue,
                        "checks" => Schema::Systems(Leaf::Derivation(true)),
                        "packages" | "devShells" => Schema::Systems(Leaf::Derivation(false)),
                        "formatter" => Schema::Systems(Leaf::Derivation(false)),
                        "apps" => Schema::Systems(Leaf::App),
                        "bundlers" => Schema::Systems(Leaf::Bundler),
                        "overlays" => Schema::Members(Leaf::Overlay),
                        "nixosModules" => Schema::Members(Leaf::Module),
                        "nixosConfigurations" => Schema::Members(Leaf::Configuration),
                        "templates" => Schema::Members(Leaf::Template),
                        _ => {
                            self.report
                                .errors
                                .push(format!("unsupported flake output '{name}'"));
                            if !self.options.keep_going {
                                return Err(error(format!("unsupported flake output '{name}'")));
                            }
                            continue;
                        }
                    };
                    self.push(slot, path, name, schema);
                }
                Ok(())
            }
            Schema::Systems(leaf) => {
                for (name, slot) in self.entries(&attrs).into_iter().rev() {
                    if !name.contains('-') {
                        return Err(error(format!("'{name}' is not a valid system name")));
                    }
                    if !self.options.all_systems && name != self.system {
                        self.report.omitted_systems.insert(name);
                        continue;
                    }
                    let schema = if path.first().is_some_and(|name| name == "formatter") {
                        Schema::Leaf(leaf)
                    } else {
                        Schema::Members(leaf)
                    };
                    self.push(slot, path, name, schema);
                }
                Ok(())
            }
            Schema::Members(leaf) => {
                for (name, slot) in self.entries(&attrs).into_iter().rev() {
                    self.push(slot, path, name, Schema::Leaf(leaf));
                }
                Ok(())
            }
            Schema::Hydra(root) => {
                let identity = Rc::as_ptr(&attrs) as usize;
                if self.completed_empty.contains_key(&identity) {
                    return Ok(());
                }
                if self.is_derivation(&attrs)? {
                    if root {
                        return Err(error("hydraJobs root must be a jobset, not a derivation"));
                    }
                    return self.derivation(Slot::value(Value::Attrs(attrs)), path, false);
                }
                if !self.active.insert(identity) {
                    return Err(error("cycle in Hydra jobset"));
                }
                self.pending.push(Work::Leave {
                    attrs: attrs.clone(),
                    derivations: self.report.derivations.len(),
                    errors: self.report.errors.len(),
                });
                for (name, slot) in self.entries(&attrs).into_iter().rev() {
                    self.push(slot, path, name, Schema::Hydra(false));
                }
                Ok(())
            }
            Schema::Leaf(_) => Err(error("invalid flake-check traversal state")),
        }
    }
}

pub(crate) fn execute(
    vm: &mut Vm,
    host: &dyn Host,
    memo: &mut JobMemo,
    root: Slot,
    options: Options,
    interrupted: Option<&dyn Fn() -> bool>,
) -> Result<Report> {
    let system = vm
        .settings()
        .current_system
        .clone()
        .ok_or_else(|| error("flake check needs the current system"))?;
    let store_dir = vm
        .settings()
        .store_dir
        .clone()
        .ok_or_else(|| error("flake check needs the store directory"))?;
    if (options.hydra || options.evaluate_only) && vm.settings().allow_import_from_derivation {
        return Err(error(
            "Hydra and evaluate-only check questions require IFD disabled before session creation",
        ));
    }
    let mut checker = Checker {
        vm,
        host,
        memo,
        options,
        system,
        store_dir,
        report: Report::default(),
        pending: vec![Work::Check {
            slot: root,
            path: vec![],
            schema: Schema::Root,
        }],
        active: BTreeSet::new(),
        completed_empty: BTreeMap::new(),
    };
    while let Some(work) = checker.pending.pop() {
        if interrupted.is_some_and(|hook| hook()) {
            return Err(error("interrupted by the user"));
        }
        match work {
            Work::Leave {
                attrs,
                derivations,
                errors,
            } => {
                let identity = Rc::as_ptr(&attrs) as usize;
                checker.active.remove(&identity);
                // Reuse only successful empty subtrees. Keep the allocation alive:
                // pointer reuse must never turn a different jobset into a hit.
                if checker.report.derivations.len() == derivations
                    && checker.report.errors.len() == errors
                {
                    checker.completed_empty.insert(identity, attrs);
                }
            }
            Work::Check { slot, path, schema } => {
                if let Err(failure) = checker.check(slot, &path, schema) {
                    if checker.vm.interrupted() || matches!(failure, EvalError::Unimplemented(_)) {
                        return Err(failure);
                    }
                    if !options.keep_going {
                        return Err(match failure {
                            EvalError::Eval(kind, message, pos) => EvalError::Eval(
                                kind,
                                format!("{}: {message}", path_text(&path)),
                                pos,
                            ),
                            EvalError::Parse(message) => {
                                EvalError::Parse(format!("{}: {message}", path_text(&path)))
                            }
                            other => other,
                        });
                    }
                    let message = match failure {
                        EvalError::Eval(_, message, _) | EvalError::Parse(message) => message,
                        EvalError::Unimplemented(_) => unreachable!("refusals return above"),
                    };
                    checker
                        .report
                        .errors
                        .push(format!("{}: {message}", path_text(&path)));
                }
            }
        }
    }
    Ok(checker.report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::Origin;
    use crate::eval::Settings;
    use crate::host::RealFs;
    const DRV: &str = "/nix/store/00000000000000000000000000000000-test.drv";
    fn check(source: &str, flags: i32) -> std::result::Result<Report, String> {
        check_bounded(source, flags, usize::MAX)
    }
    fn check_bounded(
        source: &str,
        flags: i32,
        limit: usize,
    ) -> std::result::Result<Report, String> {
        let _guard = crate::eval::globals_shared();
        let options = Options::from_flags(flags)?;
        let mut vm = Vm::with_settings(Settings {
            current_system: Some("test-system".into()),
            store_dir: Some("/nix/store".into()),
            allow_import_from_derivation: !(options.hydra || options.evaluate_only),
            ..Settings::default()
        });
        let module = vm
            .compile(source, "/", Origin::String)
            .map_err(|e| format!("{e:?}"))?
            .module;
        let mut memo = JobMemo::default();
        let root = crate::session::run_to_value_with(&mut vm, &module, &RealFs, &mut memo)
            .map_err(|e| format!("{e:?}"))?;
        let visits = std::cell::Cell::new(0_usize);
        let interrupted = || {
            visits.set(visits.get() + 1);
            visits.get() > limit
        };
        execute(
            &mut vm,
            &RealFs,
            &mut memo,
            Slot::value(root),
            options,
            Some(&interrupted),
        )
        .map_err(|e| format!("{e:?}"))
    }
    #[test]
    fn strict_checks_reject_non_derivations_and_keep_every_alias() -> std::result::Result<(), String>
    {
        let source = format!(
            r#"let d={{type="derivation";name="test";drvPath="{DRV}";}};in {{outputs.checks.test-system={{a=d;b=d;}};}}"#
        );
        let report = check(&source, 0)?;
        assert_eq!(report.derivations.len(), 2);
        assert!(report.derivations.iter().all(|d| d.build));
        assert!(
            Report::decode(
                report.encode().as_bytes(),
                "/nix/store",
                Options::from_flags(0)?,
                "test-system"
            )?
            .errors
            .is_empty()
        );
        let error = check(r#"{outputs.checks.test-system.bad=123;}"#, 0)
            .err()
            .ok_or("non-derivation accepted")?;
        assert!(error.contains("checks.test-system.bad"));
        Ok(())
    }
    #[test]
    fn incompatible_systems_are_lazy_but_all_systems_checks_them() -> std::result::Result<(), String>
    {
        let source = r#"{outputs.packages.other-system.bad=throw "foreign output forced";}"#;
        assert!(check(source, 0)?.omitted_systems.contains("other-system"));
        assert!(check(source, 2).is_err());
        Ok(())
    }
    #[test]
    fn keep_going_reports_independent_errors_and_does_not_claim_success()
    -> std::result::Result<(), String> {
        let report = check(
            r#"{outputs.checks.test-system={a=throw "one";b=throw "two";};}"#,
            4,
        )?;
        assert_eq!(report.errors.len(), 2);
        assert!(
            !Report::decode(
                report.encode().as_bytes(),
                "/nix/store",
                Options::from_flags(4)?,
                "test-system"
            )?
            .errors
            .is_empty()
        );
        Ok(())
    }
    #[test]
    fn hydra_is_separate_strict_and_cycle_bounded() -> std::result::Result<(), String> {
        assert!(
            check(r#"{outputs.hydraJobs=throw "hydra";}"#, 0)?
                .errors
                .is_empty()
        );
        assert!(check(r#"{outputs.hydraJobs=throw "hydra";}"#, 1).is_err());
        assert!(check(r#"{outputs.hydraJobs={bad=1;};}"#, 1).is_err());
        let error = check(r#"let jobs={again=jobs;};in {outputs.hydraJobs=jobs;}"#, 1)
            .err()
            .ok_or("cycle accepted")?;
        assert!(error.contains("cycle"));
        Ok(())
    }
    #[test]
    fn schemas_validate_functions_apps_and_reject_deprecated_outputs()
    -> std::result::Result<(), String> {
        let source = r#"{outputs={overlays.default=final: throw "body";nixosModules.default={};bundlers.test-system.default=x: x;apps.test-system.default={type="app";program="/bin/tool";};};}"#;
        assert!(check(source, 0)?.errors.is_empty());
        assert!(check(r#"{outputs.overlay=final:prev:{};}"#, 0).is_err());
        assert!(
            check(
                r#"{outputs.apps.test-system.bad={type="wrong";program="/bin/tool";};}"#,
                0
            )
            .is_err()
        );
        assert!(check(r#"{outputs.overlays.bad={};}"#, 0).is_err());
        Ok(())
    }
    #[test]
    fn scope_and_flags_have_distinct_question_keys() -> std::result::Result<(), String> {
        let mut keys = BTreeSet::new();
        for flags in 0..=15 {
            let options = Options::from_flags(flags)?;
            assert_eq!(options.flags(), flags);
            let question = crate::session::Question::FlakeCheck {
                selection: crate::session::Selection::one(""),
                options,
            };
            assert!(keys.insert(question.fingerprint()));
        }
        assert!(Options::from_flags(16).is_err());
        Ok(())
    }
    #[test]
    fn report_rejects_invalid_store_paths_and_noncheck_build_targets() {
        for record in [
            json!({"path":["checks","test-system","bad"],"drvPath":"/etc/passwd","build":true}),
            json!({"path":["packages","test-system","bad"],"drvPath":DRV,"build":true}),
        ] {
            let report = json!({"derivations":[record],"errors":[],"omittedSystems":[]});
            assert!(
                Report::decode(
                    report.to_string().as_bytes(),
                    "/nix/store",
                    Options::from_flags(0).expect("valid flags"),
                    "test-system"
                )
                .is_err()
            );
        }
    }
    #[test]
    fn empty_hydra_dag_is_linear_but_aliases_and_errors_are_retained()
    -> std::result::Result<(), String> {
        let mut source = "let n0={};".to_owned();
        for depth in 1..=30 {
            source.push_str(&format!("n{depth}={{a=n{};b=n{};}};", depth - 1, depth - 1));
        }
        source.push_str("in {outputs.hydraJobs=n30;}");
        assert!(check_bounded(&source, 1, 100)?.errors.is_empty());
        let aliases = format!(
            r#"let d={{type="derivation";name="test";drvPath="{DRV}";}};jobs={{job=d;}};in {{outputs.hydraJobs={{a=jobs;b=jobs;}};}}"#
        );
        let report = check_bounded(&aliases, 1, 20)?;
        assert_eq!(report.derivations.len(), 2);
        assert_ne!(report.derivations[0].path, report.derivations[1].path);
        let report = check_bounded(
            r#"let jobs={bad=123;};in {outputs.hydraJobs={a=jobs;b=jobs;};}"#,
            5,
            20,
        )?;
        assert_eq!(report.errors.len(), 2);
        assert!(
            report
                .errors
                .iter()
                .any(|message| message.contains("hydraJobs.a.bad"))
        );
        assert!(
            report
                .errors
                .iter()
                .any(|message| message.contains("hydraJobs.b.bad"))
        );
        Ok(())
    }
    #[test]
    fn cached_build_decisions_cannot_escape_scope_or_skip_local_checks()
    -> std::result::Result<(), String> {
        for (flags, path, build) in [
            (2, vec!["checks", "foreign-system", "job"], true),
            (1, vec!["hydraJobs", "job"], true),
            (1, vec!["checks", "test-system", "job"], true),
            (0, vec!["checks", "test-system", "job"], false),
        ] {
            let report = json!({"derivations":[{"path":path,"drvPath":DRV,"build":build}],"errors":[],"omittedSystems":[]});
            assert!(
                Report::decode(
                    report.to_string().as_bytes(),
                    "/nix/store",
                    Options::from_flags(flags)?,
                    "test-system"
                )
                .is_err()
            );
        }
        Ok(())
    }
}
