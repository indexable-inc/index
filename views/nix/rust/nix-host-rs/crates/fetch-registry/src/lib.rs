//! Registry policy and ownership, independent of the evaluator and fetch effects.
//!
//! File-backed registries are replaced at each request. Resolution holds immutable
//! snapshots, so an in-flight request sees one version of every layer. Indexes
//! preserve declaration order even when a fuzzy entry precedes an exact entry.

#![forbid(unsafe_code)]

use serde_json::{Map, Value as Json};
use std::collections::{BTreeMap, HashMap, HashSet};

pub type Result<T> = std::result::Result<T, String>;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Attr {
    String(String),
    Uint(u64),
    Bool(bool),
}

pub type Attrs = BTreeMap<String, Attr>;

fn string<'a>(attrs: &'a Attrs, name: &str) -> Result<Option<&'a str>> {
    match attrs.get(name) {
        None => Ok(None),
        Some(Attr::String(value)) => Ok(Some(value)),
        Some(_) => Err(format!("input attribute '{name}' must be a string")),
    }
}

fn input_type(attrs: &Attrs) -> Result<&str> {
    string(attrs, "type")?.ok_or_else(|| "input attribute 'type' is required".into())
}

fn validate_input(attrs: &Attrs) -> Result<()> {
    input_type(attrs)?;
    string(attrs, "ref")?;
    string(attrs, "rev")?;
    Ok(())
}

fn parse_attrs(value: &Json) -> Result<Attrs> {
    let object = value
        .as_object()
        .ok_or("registry input must be an object")?;
    object
        .iter()
        .map(|(name, value)| {
            let value = match value {
                Json::String(value) => Attr::String(value.clone()),
                Json::Bool(value) => Attr::Bool(*value),
                Json::Number(value) => Attr::Uint(value.as_u64().ok_or_else(|| {
                    format!("registry attribute '{name}' must be an unsigned integer")
                })?),
                _ => {
                    return Err(format!(
                        "registry attribute '{name}' must be a string, boolean, or unsigned integer"
                    ));
                }
            };
            Ok((name.clone(), value))
        })
        .collect()
}

fn attrs_json(attrs: &Attrs) -> Json {
    Json::Object(
        attrs
            .iter()
            .map(|(name, value)| {
                let value = match value {
                    Attr::String(value) => Json::String(value.clone()),
                    Attr::Uint(value) => Json::from(*value),
                    Attr::Bool(value) => Json::Bool(*value),
                };
                (name.clone(), value)
            })
            .collect(),
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub from: Attrs,
    pub to: Attrs,
    pub extra: Attrs,
    pub exact: bool,
}

impl Entry {
    fn validate(&self) -> Result<()> {
        validate_input(&self.from)?;
        validate_input(&self.to)?;
        for (name, value) in &self.extra {
            if name != "dir" || !matches!(value, Attr::String(_)) {
                return Err("registry extra attributes may contain only a string 'dir'".into());
            }
        }
        Ok(())
    }
}

#[derive(Clone, Default)]
pub struct Registry {
    entries: Vec<Entry>,
    full: HashMap<Attrs, usize>,
    fuzzy: HashMap<Attrs, usize>,
}

impl Registry {
    /// Entries in declaration order; mutations are validated by this owner.
    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn from_entries(entries: Vec<Entry>) -> Result<Self> {
        let mut registry = Self::default();
        for entry in entries {
            registry.add(entry)?;
        }
        Ok(registry)
    }

    pub fn parse(source: &str) -> Result<Self> {
        let document: Json = serde_json::from_str(source).map_err(|error| error.to_string())?;
        let document = document.as_object().ok_or("registry must be an object")?;
        if document.get("version").and_then(Json::as_u64) != Some(2) {
            return Err("registry requires schema version 2".into());
        }
        let entries = document
            .get("flakes")
            .and_then(Json::as_array)
            .ok_or("registry 'flakes' must be an array")?;
        let mut parsed = Vec::with_capacity(entries.len());
        for entry in entries {
            let entry = entry
                .as_object()
                .ok_or("registry entry must be an object")?;
            let from = parse_attrs(entry.get("from").ok_or("registry entry requires 'from'")?)?;
            let mut to = parse_attrs(entry.get("to").ok_or("registry entry requires 'to'")?)?;
            let mut extra = Attrs::new();
            if let Some(dir) = to.remove("dir") {
                extra.insert("dir".into(), dir);
            }
            let exact = match entry.get("exact") {
                None => false,
                Some(Json::Bool(value)) => *value,
                Some(_) => return Err("registry 'exact' must be a boolean".into()),
            };
            parsed.push(Entry {
                from,
                to,
                extra,
                exact,
            });
        }
        Self::from_entries(parsed)
    }

    pub fn serialize(&self) -> Result<String> {
        let entries: Vec<Json> = self
            .entries
            .iter()
            .map(|entry| {
                let mut to = entry.to.clone();
                to.extend(entry.extra.clone());
                let mut object = Map::new();
                object.insert("from".into(), attrs_json(&entry.from));
                object.insert("to".into(), attrs_json(&to));
                if entry.exact {
                    object.insert("exact".into(), Json::Bool(true));
                }
                Json::Object(object)
            })
            .collect();
        serde_json::to_string_pretty(&serde_json::json!({"version": 2, "flakes": entries}))
            .map_err(|error| error.to_string())
    }

    pub fn add(&mut self, entry: Entry) -> Result<()> {
        entry.validate()?;
        let index = self.entries.len();
        self.full.entry(entry.from.clone()).or_insert(index);
        if !entry.exact {
            self.fuzzy.entry(entry.from.clone()).or_insert(index);
        }
        self.entries.push(entry);
        Ok(())
    }

    pub fn remove(&mut self, input: &Attrs) -> Result<()> {
        validate_input(input)?;
        self.entries.retain(|entry| &entry.from != input);
        self.full.clear();
        self.fuzzy.clear();
        for (index, entry) in self.entries.iter().enumerate() {
            self.full.entry(entry.from.clone()).or_insert(index);
            if !entry.exact {
                self.fuzzy.entry(entry.from.clone()).or_insert(index);
            }
        }
        Ok(())
    }

    fn find(&self, input: &Attrs, unqualified: &Attrs) -> Option<&Entry> {
        let full = self.full.get(input).copied();
        let fuzzy = self.fuzzy.get(unqualified).copied();
        let index = match (full, fuzzy) {
            (Some(full), Some(fuzzy)) => full.min(fuzzy),
            (Some(index), None) | (None, Some(index)) => index,
            (None, None) => return None,
        };
        self.entries.get(index)
    }
}

/// Scheme-specific overrides have one owner, including calls from forge fetchers.
pub fn apply_overrides(
    input: &Attrs,
    reference: Option<&str>,
    revision: Option<&str>,
) -> Result<Attrs> {
    validate_input(input)?;
    let scheme = input_type(input)?;
    let mut result = input.clone();
    match scheme {
        "git" | "jj" | "hg" | "indirect" => {
            if let Some(revision) = revision {
                result.insert("rev".into(), Attr::String(revision.into()));
            }
            if let Some(reference) = reference {
                result.insert("ref".into(), Attr::String(reference.into()));
            }
            if scheme == "git"
                && string(&result, "rev")?.is_some()
                && string(&result, "ref")?.is_none()
            {
                return Err("Git input has a commit hash but no branch/tag name".into());
            }
        }
        "github" | "gitlab" | "sourcehut" => {
            if reference.is_some() && revision.is_some() {
                return Err(
                    "cannot apply both a commit hash and a branch/tag name to a forge input".into(),
                );
            }
            if let Some(revision) = revision {
                result.insert("rev".into(), Attr::String(revision.into()));
                result.remove("ref");
            }
            if let Some(reference) = reference {
                result.insert("ref".into(), Attr::String(reference.into()));
                result.remove("rev");
            }
        }
        _ if reference.is_some() || revision.is_some() => {
            return Err(format!(
                "input scheme '{scheme}' does not support reference overrides"
            ));
        }
        _ => {}
    }
    Ok(result)
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LayerKind {
    Flag,
    User,
    System,
    Global,
    Custom,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum UseRegistries {
    No,
    All,
    Limited,
}

pub struct Resolution {
    pub input: Attrs,
    pub extra: Attrs,
    #[cfg(test)]
    probes: usize,
}

pub fn resolve<'a>(
    layers: impl IntoIterator<Item = (LayerKind, &'a Registry)>,
    mut input: Attrs,
    mode: UseRegistries,
) -> Result<Resolution> {
    validate_input(&input)?;
    let mut extra = Attrs::new();
    #[cfg(test)]
    let mut probes = 0;
    if mode != UseRegistries::No {
        let mut layers: Vec<(LayerKind, &Registry)> = layers
            .into_iter()
            .filter_map(|(kind, registry)| {
                (mode != UseRegistries::Limited
                    || matches!(kind, LayerKind::Flag | LayerKind::Global))
                .then_some((kind, registry))
            })
            .collect();
        layers.sort_by_key(|(kind, _)| *kind);
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(input.clone()) {
                return Err("cycle detected in flake registry".into());
            }
            let mut unqualified = input.clone();
            unqualified.remove("ref");
            unqualified.remove("rev");
            let mut matched = None;
            for (_, registry) in &layers {
                #[cfg(test)]
                {
                    probes += 2;
                }
                if let Some(entry) = registry.find(&input, &unqualified) {
                    matched = Some(entry);
                    break;
                }
            }
            let Some(entry) = matched else {
                break;
            };
            let next = if entry.exact {
                entry.to.clone()
            } else {
                apply_overrides(
                    &entry.to,
                    if string(&entry.from, "ref")?.is_none() {
                        string(&input, "ref")?
                    } else {
                        None
                    },
                    if string(&entry.from, "rev")?.is_none() {
                        string(&input, "rev")?
                    } else {
                        None
                    },
                )?
            };
            extra = entry.extra.clone();
            input = next;
        }
        if input_type(&input)? == "indirect" {
            return Err(format!(
                "cannot find flake 'flake:{}' in the flake registries",
                string(&input, "id")?.unwrap_or("<missing id>")
            ));
        }
    }
    Ok(Resolution {
        input,
        extra,
        #[cfg(test)]
        probes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attrs(value: Json) -> Attrs {
        parse_attrs(&value).unwrap()
    }
    fn alias(name: &str) -> Attrs {
        attrs(serde_json::json!({"type":"indirect","id":name}))
    }
    fn target(name: &str) -> Attrs {
        attrs(serde_json::json!({"type":"jj","url":name}))
    }
    fn entry(from: Attrs, to: Attrs) -> Entry {
        Entry {
            from,
            to,
            extra: Attrs::new(),
            exact: false,
        }
    }
    fn resolve_one(registry: &Registry, input: Attrs) -> Result<Resolution> {
        resolve([(LayerKind::Global, registry)], input, UseRegistries::All)
    }

    #[test]
    fn declaration_order_wins_over_exactness_and_indexes_follow_mutation() {
        let base = alias("x");
        let mut qualified = base.clone();
        qualified.insert("ref".into(), Attr::String("main".into()));
        let first = entry(base.clone(), target("first"));
        let mut exact = entry(qualified.clone(), target("exact"));
        exact.exact = true;
        let mut registry = Registry::from_entries(vec![first, exact.clone()]).unwrap();
        let answer = resolve_one(&registry, qualified.clone()).unwrap();
        assert_eq!(string(&answer.input, "url").unwrap(), Some("first"));
        assert_eq!(string(&answer.input, "ref").unwrap(), Some("main"));
        registry.remove(&base).unwrap();
        assert_eq!(
            resolve_one(&registry, qualified.clone()).unwrap().input,
            exact.to
        );
        registry.remove(&qualified).unwrap();
        assert!(resolve_one(&registry, qualified.clone()).is_err());
        registry.add(entry(base, target("new"))).unwrap();
        assert_eq!(
            string(&resolve_one(&registry, qualified).unwrap().input, "url").unwrap(),
            Some("new")
        );
    }

    #[test]
    fn layer_modes_and_final_directory_follow_the_selected_resolution() {
        let flag = Registry::from_entries(vec![entry(alias("x"), target("flag"))]).unwrap();
        let user = Registry::from_entries(vec![entry(alias("x"), target("user"))]).unwrap();
        let mut first = entry(alias("x"), alias("y"));
        first.extra.insert("dir".into(), Attr::String("old".into()));
        let mut second = entry(alias("y"), target("global"));
        second
            .extra
            .insert("dir".into(), Attr::String("new".into()));
        let global = Registry::from_entries(vec![first, second]).unwrap();
        assert_eq!(
            resolve(
                [(LayerKind::Global, &global), (LayerKind::User, &user)],
                alias("x"),
                UseRegistries::All
            )
            .unwrap()
            .input,
            target("user")
        );
        let limited = resolve(
            [(LayerKind::User, &user), (LayerKind::Global, &global)],
            alias("x"),
            UseRegistries::Limited,
        )
        .unwrap();
        assert_eq!(limited.input, target("global"));
        assert_eq!(string(&limited.extra, "dir").unwrap(), Some("new"));
        assert_eq!(
            resolve(
                [(LayerKind::Global, &global), (LayerKind::Flag, &flag)],
                alias("x"),
                UseRegistries::Limited
            )
            .unwrap()
            .input,
            target("flag")
        );
        assert_eq!(
            resolve_one(&global, alias("x")).unwrap().input,
            target("global")
        );
        assert_eq!(
            resolve(
                [(LayerKind::Global, &global)],
                alias("x"),
                UseRegistries::No
            )
            .unwrap()
            .input,
            alias("x")
        );
    }

    #[test]
    fn long_chains_are_indexed_and_real_cycles_are_rejected() {
        let count = 10_000;
        let mut entries: Vec<Entry> = (0..count)
            .map(|i| entry(alias(&i.to_string()), alias(&(i + 1).to_string())))
            .collect();
        entries.push(entry(alias(&count.to_string()), target("end")));
        let mut registry = Registry::from_entries(entries).unwrap();
        let result = resolve_one(&registry, alias("0")).unwrap();
        assert_eq!(result.input, target("end"));
        assert_eq!(result.probes, 2 * (count + 2));
        registry.add(entry(target("end"), alias("0"))).unwrap();
        assert!(
            resolve_one(&registry, alias("0"))
                .err()
                .unwrap()
                .contains("cycle")
        );
        let self_cycle = Registry::from_entries(vec![entry(alias("self"), alias("self"))]).unwrap();
        assert!(resolve_one(&self_cycle, alias("self")).is_err());
    }

    #[test]
    fn overrides_preserve_backend_invariants() {
        let git = attrs(serde_json::json!({"type":"git","url":"file:///repo"}));
        assert!(apply_overrides(&git, None, Some("rev")).is_err());
        assert_eq!(
            string(
                &apply_overrides(&git, Some("main"), Some("rev")).unwrap(),
                "rev"
            )
            .unwrap(),
            Some("rev")
        );
        for scheme in ["github", "gitlab", "sourcehut"] {
            let input = attrs(serde_json::json!({"type":scheme,"ref":"old"}));
            let revised = apply_overrides(&input, None, Some("rev")).unwrap();
            assert!(!revised.contains_key("ref"));
            assert_eq!(string(&revised, "rev").unwrap(), Some("rev"));
            let referenced = apply_overrides(&revised, Some("main"), None).unwrap();
            assert!(!referenced.contains_key("rev"));
            assert!(apply_overrides(&input, Some("main"), Some("rev")).is_err());
        }
        for scheme in ["jj", "hg", "indirect"] {
            let input = attrs(serde_json::json!({"type":scheme}));
            assert!(apply_overrides(&input, Some("main"), Some("rev")).is_ok());
        }
        let path = attrs(serde_json::json!({"type":"path","path":"/store/path"}));
        assert_eq!(apply_overrides(&path, None, None).unwrap(), path);
        assert!(apply_overrides(&path, Some("main"), None).is_err());
    }

    #[test]
    fn document_roundtrip_and_malformed_documents_are_atomic() {
        let source = r#"{"version":2,"flakes":[{"from":{"type":"indirect","id":"x"},"to":{"type":"jj","url":"file:///r","dir":"sub","submodules":true,"revCount":18446744073709551615},"exact":true}]}"#;
        let registry = Registry::parse(source).unwrap();
        let decoded = Registry::parse(&registry.serialize().unwrap()).unwrap();
        assert_eq!(decoded.entries, registry.entries);
        assert!(!decoded.entries[0].to.contains_key("dir"));
        assert_eq!(
            string(&decoded.entries[0].extra, "dir").unwrap(),
            Some("sub")
        );
        assert_eq!(
            Registry::parse(&Registry::default().serialize().unwrap())
                .unwrap()
                .entries
                .len(),
            0
        );
        for source in [
            "",
            "{}",
            r#"{"version":1,"flakes":[]}"#,
            r#"{"version":2,"flakes":null}"#,
            r#"{"version":2,"flakes":[{"from":{"type":"indirect"},"to":{"type":"jj","revCount":-1}}]}"#,
            r#"{"version":2,"flakes":[{"from":{"type":"indirect"},"to":{"type":"jj","revCount":1.5}}]}"#,
            r#"{"version":2,"flakes":[{"from":{"type":"indirect"},"to":{"type":"jj","dir":false}}]}"#,
            r#"{"version":2,"flakes":[{"from":{"type":"indirect"},"to":{"type":"jj"},"exact":1}]}"#,
        ] {
            assert!(Registry::parse(source).is_err(), "accepted {source}");
        }
    }
}
