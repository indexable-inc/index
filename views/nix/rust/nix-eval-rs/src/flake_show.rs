//! Flake-show documents, their persisted representation, and presentation.
//! The host supplies classification facts; this module owns every encoded and displayed form.

use crate::terminal::terminal_text;
use serde_json::{Value as Json, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

type Result<T> = std::result::Result<T, String>;
pub(crate) type NodeId = u64;
static NEXT_NODE: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub(crate) enum Kind {
    Branch = 0,
    Derivation = 1,
    App = 2,
    Template = 3,
    NixpkgsOverlay = 4,
    NixosConfiguration = 5,
    NixosModule = 6,
    Unknown = 7,
    Empty = 8,
    NonDerivation = 9,
    OmittedSystem = 10,
    OmittedLegacy = 11,
    OmittedIfd = 12,
}

impl Kind {
    pub(crate) const ALL: [Self; 13] = [
        Self::Branch,
        Self::Derivation,
        Self::App,
        Self::Template,
        Self::NixpkgsOverlay,
        Self::NixosConfiguration,
        Self::NixosModule,
        Self::Unknown,
        Self::Empty,
        Self::NonDerivation,
        Self::OmittedSystem,
        Self::OmittedLegacy,
        Self::OmittedIfd,
    ];

    pub(crate) fn from_code(code: u32) -> Result<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| *kind as u32 == code)
            .ok_or_else(|| format!("unknown flake-show node kind {code}"))
    }
}

#[derive(Debug)]
enum Node {
    Branch(BTreeMap<String, NodeId>),
    Derivation {
        name: String,
        description: Option<String>,
    },
    App {
        description: Option<String>,
    },
    Template {
        description: String,
    },
    NixpkgsOverlay,
    NixosConfiguration,
    NixosModule,
    Unknown,
    Empty,
    NonDerivation,
    OmittedSystem,
    OmittedLegacy,
    OmittedIfd,
}

impl Node {
    fn kind(&self) -> Kind {
        match self {
            Self::Branch(_) => Kind::Branch,
            Self::Derivation { .. } => Kind::Derivation,
            Self::App { .. } => Kind::App,
            Self::Template { .. } => Kind::Template,
            Self::NixpkgsOverlay => Kind::NixpkgsOverlay,
            Self::NixosConfiguration => Kind::NixosConfiguration,
            Self::NixosModule => Kind::NixosModule,
            Self::Unknown => Kind::Unknown,
            Self::Empty => Kind::Empty,
            Self::NonDerivation => Kind::NonDerivation,
            Self::OmittedSystem => Kind::OmittedSystem,
            Self::OmittedLegacy => Kind::OmittedLegacy,
            Self::OmittedIfd => Kind::OmittedIfd,
        }
    }

    fn from_parts(
        kind: Kind,
        name: String,
        description: Option<String>,
        children: BTreeMap<String, NodeId>,
    ) -> Result<Self> {
        if kind != Kind::Branch && !children.is_empty() {
            return Err("flake-show leaf carries children".into());
        }
        if kind != Kind::Derivation && !name.is_empty() {
            return Err("only a flake-show derivation carries a name".into());
        }
        if !matches!(kind, Kind::Derivation | Kind::App | Kind::Template) && description.is_some() {
            return Err("this flake-show node cannot carry a description".into());
        }
        Ok(match kind {
            Kind::Branch => Self::Branch(children),
            Kind::Derivation if name.is_empty() => {
                return Err("flake-show derivation has no name".into());
            }
            Kind::Derivation => Self::Derivation { name, description },
            Kind::App => Self::App { description },
            Kind::Template => Self::Template {
                description: description.ok_or("flake-show template has no description")?,
            },
            Kind::NixpkgsOverlay => Self::NixpkgsOverlay,
            Kind::NixosConfiguration => Self::NixosConfiguration,
            Kind::NixosModule => Self::NixosModule,
            Kind::Unknown => Self::Unknown,
            Kind::Empty => Self::Empty,
            Kind::NonDerivation => Self::NonDerivation,
            Kind::OmittedSystem => Self::OmittedSystem,
            Kind::OmittedLegacy => Self::OmittedLegacy,
            Kind::OmittedIfd => Self::OmittedIfd,
        })
    }

    fn public_leaf(&self) -> Result<Json> {
        Ok(match self {
            Self::Branch(_) => return Err("branch requested as a leaf".into()),
            Self::Derivation { name, description } => {
                let mut fields = serde_json::Map::from_iter([
                    ("type".to_owned(), json!("derivation")),
                    ("name".to_owned(), json!(name)),
                ]);
                if let Some(description) = description {
                    fields.insert("description".to_owned(), json!(description));
                }
                Json::Object(fields)
            }
            Self::App { description } => {
                let mut fields = serde_json::Map::from_iter([("type".to_owned(), json!("app"))]);
                if let Some(description) = description {
                    fields.insert("description".to_owned(), json!(description));
                }
                Json::Object(fields)
            }
            Self::Template { description } => json!({"type":"template", "description":description}),
            Self::NixpkgsOverlay => json!({"type":"nixpkgs-overlay"}),
            Self::NixosConfiguration => json!({"type":"nixos-configuration"}),
            Self::NixosModule => json!({"type":"nixos-module"}),
            Self::Unknown => json!({"type":"unknown"}),
            Self::Empty
            | Self::NonDerivation
            | Self::OmittedSystem
            | Self::OmittedLegacy
            | Self::OmittedIfd => json!({}),
        })
    }
}

/// Flat ownership avoids recursive destruction and recursive rendering of deep output trees.
#[derive(Default)]
pub(crate) struct Document {
    nodes: BTreeMap<NodeId, Node>,
    root: Option<NodeId>,
}

pub(crate) struct Warning {
    pub kind: Kind,
    pub message: String,
}

pub(crate) struct Report {
    pub output: String,
    pub warnings: Vec<Warning>,
}

impl Document {
    pub(crate) fn add(
        &mut self,
        kind: Kind,
        name: String,
        description: Option<String>,
        children: Vec<(String, NodeId)>,
    ) -> Result<NodeId> {
        if self.root.is_some() {
            return Err("flake-show document is already finished".into());
        }
        let mut named = BTreeMap::new();
        for (name, child) in children {
            if !self.nodes.contains_key(&child) {
                return Err("unknown or foreign flake-show child".into());
            }
            if named.insert(name, child).is_some() {
                return Err("duplicate flake-show child name".into());
            }
        }
        let node = Node::from_parts(kind, name, description, named)?;
        let id = NEXT_NODE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| "flake-show node identity space exhausted")?;
        self.nodes.insert(id, node);
        Ok(id)
    }

    fn reachable(&self, root: NodeId) -> Result<BTreeSet<NodeId>> {
        let mut seen = BTreeSet::new();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                return Err("flake-show nodes must form a tree".into());
            }
            if let Node::Branch(children) = self
                .nodes
                .get(&id)
                .ok_or("unknown flake-show root or child")?
            {
                pending.extend(children.values().copied());
            }
        }
        Ok(seen)
    }

    pub(crate) fn finish(&mut self, root: NodeId) -> Result<()> {
        if self.root.is_some() {
            return Err("flake-show document is already finished".into());
        }
        if !matches!(self.nodes.get(&root), Some(Node::Branch(_))) {
            return Err("flake-show document root must be a branch".into());
        }
        let reachable = self.reachable(root)?;
        // A recoverable traversal error can discard a partially constructed subtree.
        // Only the completed root becomes the document; the wire codec rejects orphans.
        self.nodes.retain(|id, _| reachable.contains(id));
        self.root = Some(root);
        Ok(())
    }

    fn root(&self) -> Result<NodeId> {
        self.root
            .ok_or_else(|| "unfinished flake-show document".into())
    }

    /// A versioned flat codec: nesting depth does not limit a legitimate flake tree.
    pub(crate) fn encode(&self) -> Result<String> {
        let root = self.root()?;
        let indices: BTreeMap<NodeId, usize> = self
            .nodes
            .keys()
            .enumerate()
            .map(|(i, id)| (*id, i))
            .collect();
        let mut rows = Vec::with_capacity(self.nodes.len());
        for node in self.nodes.values() {
            let kind = node.kind() as u32;
            rows.push(match node {
                Node::Branch(children) => {
                    let edges = children
                        .iter()
                        .map(|(name, child)| {
                            indices
                                .get(child)
                                .map(|index| json!([name, index]))
                                .ok_or_else(|| "unknown flake-show child".to_owned())
                        })
                        .collect::<Result<Vec<_>>>()?;
                    json!([kind, edges])
                }
                Node::Derivation { name, description } => json!([kind, name, description]),
                Node::App { description } => json!([kind, description]),
                Node::Template { description } => json!([kind, description]),
                _ => json!([kind]),
            });
        }
        serde_json::to_string(&json!({"version":1, "root":indices.get(&root).ok_or("unknown flake-show root")?, "nodes":rows}))
            .map_err(|error| error.to_string())
    }

    pub(crate) fn decode(encoded: &[u8]) -> Result<Self> {
        let encoded: Json = serde_json::from_slice(encoded)
            .map_err(|error| format!("invalid flake-show document: {error}"))?;
        let object = encoded
            .as_object()
            .ok_or("flake-show document is not an object")?;
        if object.len() != 3 || object.get("version").and_then(Json::as_u64) != Some(1) {
            return Err("unknown flake-show document version or fields".into());
        }
        let rows = object
            .get("nodes")
            .and_then(Json::as_array)
            .ok_or("flake-show nodes are not an array")?;
        let root_index = object
            .get("root")
            .and_then(Json::as_u64)
            .and_then(|i| usize::try_from(i).ok())
            .ok_or("invalid flake-show root index")?;
        let mut document = Self::default();
        let mut ids = Vec::with_capacity(rows.len());
        for row in rows {
            let row = row.as_array().ok_or("flake-show row is not an array")?;
            let code = row
                .first()
                .and_then(Json::as_u64)
                .and_then(|v| u32::try_from(v).ok())
                .ok_or("invalid flake-show node kind")?;
            let kind = Kind::from_code(code)?;
            let mut name = String::new();
            let mut description = None;
            let mut children = Vec::new();
            let expected = match kind {
                Kind::Branch => {
                    for edge in row
                        .get(1)
                        .and_then(Json::as_array)
                        .ok_or("branch children are not an array")?
                    {
                        let edge = edge
                            .as_array()
                            .filter(|parts| parts.len() == 2)
                            .ok_or("invalid flake-show edge")?;
                        let name = edge
                            .first()
                            .and_then(Json::as_str)
                            .ok_or("invalid flake-show edge name")?;
                        let child = edge
                            .get(1)
                            .and_then(Json::as_u64)
                            .and_then(|i| usize::try_from(i).ok())
                            .and_then(|i| ids.get(i))
                            .copied()
                            .ok_or("flake-show child must precede its parent")?;
                        children.push((name.to_owned(), child));
                    }
                    2
                }
                Kind::Derivation => {
                    name = row
                        .get(1)
                        .and_then(Json::as_str)
                        .ok_or("invalid flake-show derivation name")?
                        .to_owned();
                    description = optional_text(row.get(2))?;
                    3
                }
                Kind::App => {
                    description = optional_text(row.get(1))?;
                    2
                }
                Kind::Template => {
                    description = Some(
                        row.get(1)
                            .and_then(Json::as_str)
                            .ok_or("invalid template description")?
                            .to_owned(),
                    );
                    2
                }
                _ => 1,
            };
            if row.len() != expected {
                return Err("unexpected fields in flake-show node".into());
            }
            ids.push(document.add(kind, name, description, children)?);
        }
        let root = ids
            .get(root_index)
            .copied()
            .ok_or("unknown flake-show root index")?;
        if document.reachable(root)?.len() != rows.len() {
            return Err("orphan node in flake-show document".into());
        }
        document.finish(root)?;
        Ok(document)
    }

    fn json(&self) -> Result<String> {
        enum Part<'a> {
            Node(NodeId),
            Key(&'a str),
            Text(&'static str),
        }
        let mut pending = vec![Part::Node(self.root()?)];
        let mut output = String::new();
        while let Some(part) = pending.pop() {
            match part {
                Part::Text(text) => output.push_str(text),
                Part::Key(name) => {
                    output.push_str(&serde_json::to_string(name).map_err(|e| e.to_string())?)
                }
                Part::Node(id) => match self.nodes.get(&id).ok_or("unknown flake-show node")? {
                    Node::Branch(children) => {
                        output.push('{');
                        pending.push(Part::Text("}"));
                        for (index, (name, child)) in children.iter().enumerate().rev() {
                            if index + 1 != children.len() {
                                pending.push(Part::Text(","));
                            }
                            pending.push(Part::Node(*child));
                            pending.push(Part::Text(":"));
                            pending.push(Part::Key(name));
                        }
                    }
                    leaf => output.push_str(
                        &serde_json::to_string(&leaf.public_leaf()?).map_err(|e| e.to_string())?,
                    ),
                },
            }
        }
        Ok(output)
    }

    // One borrowed path is maintained as the traversal enters and leaves nodes.
    // Only emitted warning messages materialize a full path; JSON never builds
    // the growing prefixes and headers required by a text tree.
    fn warnings(&self, json: bool) -> Result<Vec<Warning>> {
        enum Visit<'a> {
            Enter(NodeId, Option<&'a str>),
            Exit,
        }
        let mut path = Vec::<&str>::new();
        let mut warnings = Vec::new();
        let mut pending = vec![Visit::Enter(self.root()?, None)];
        while let Some(visit) = pending.pop() {
            let (id, name) = match visit {
                Visit::Enter(id, name) => (id, name),
                Visit::Exit => {
                    path.pop();
                    continue;
                }
            };
            if let Some(name) = name {
                path.push(name);
                pending.push(Visit::Exit);
            }
            let node = self.nodes.get(&id).ok_or("unknown flake-show node")?;
            let omission = match node {
                Node::NonDerivation => {
                    Some(format!("{}.name is not a derivation", attr_path(&path)?))
                }
                Node::OmittedLegacy if json => Some(format!(
                    "{} omitted (use '--legacy' to show)",
                    attr_path(&path)?
                )),
                Node::OmittedSystem if json => Some(format!(
                    "{} omitted (use '--all-systems' to show)",
                    attr_path(&path)?
                )),
                Node::OmittedIfd if json => Some(format!(
                    "{} omitted due to use of import from derivation",
                    attr_path(&path)?
                )),
                _ => None,
            };
            if let Some(message) = omission {
                warnings.push(Warning {
                    kind: node.kind(),
                    message: terminal_text(&message).into_owned(),
                });
            }
            if let Node::Branch(children) = node {
                for (name, child) in children.iter().rev() {
                    pending.push(Visit::Enter(*child, Some(name)));
                }
            }
        }
        Ok(warnings)
    }

    pub(crate) fn render(&self, root_label: &str, json: bool, colored: bool) -> Result<Report> {
        let warnings = self.warnings(json)?;
        if json {
            return Ok(Report {
                output: self.json()?,
                warnings,
            });
        }
        let mut output = Vec::new();
        let normal = if colored { "\x1b[0m" } else { "" };
        let bold = if colored { "\x1b[1m" } else { "" };
        let green = if colored { "\x1b[32;1m" } else { "" };
        let warning = if colored { "\x1b[35;1m" } else { "" };
        let mut pending = vec![(
            self.root()?,
            Vec::<String>::new(),
            format!("{bold}{}{normal}", terminal_text(root_label)),
            String::new(),
        )];
        while let Some((id, path, header, prefix)) = pending.pop() {
            let node = self.nodes.get(&id).ok_or("unknown flake-show node")?;
            if let Node::Branch(children) = node {
                output.push(header);
                for (index, (name, child)) in children.iter().enumerate().rev() {
                    let last = index + 1 == children.len();
                    let mut child_path = path.clone();
                    child_path.push(name.clone());
                    pending.push((
                        *child,
                        child_path,
                        format!(
                            "{green}{prefix}{}{normal}{bold}{}{normal}",
                            if last { "└───" } else { "├───" },
                            terminal_text(name)
                        ),
                        format!("{prefix}{}", if last { "    " } else { "│   " }),
                    ));
                }
                continue;
            }
            let line = match node {
                Node::Branch(_) => continue,
                Node::Empty | Node::NonDerivation => continue,
                Node::Derivation { name, .. } => {
                    let label = match (path.first().map(String::as_str), path.len()) {
                        (Some("devShell"), 2) | (Some("devShells"), 2..) => {
                            "development environment"
                        }
                        (Some("checks"), 3) | (Some("hydraJobs"), _) => "derivation",
                        _ => "package",
                    };
                    format!("{header}: {label} '{}'", terminal_text(name))
                }
                Node::App { description } => format!(
                    "{header}: app: {bold}{}{normal}",
                    terminal_text(description.as_deref().unwrap_or("no description"))
                ),
                Node::Template { description } => {
                    format!(
                        "{header}: template: {bold}{}{normal}",
                        terminal_text(description)
                    )
                }
                Node::NixpkgsOverlay => format!("{header}: {warning}Nixpkgs overlay{normal}"),
                Node::NixosConfiguration => {
                    format!("{header}: {warning}NixOS configuration{normal}")
                }
                Node::NixosModule => format!("{header}: {warning}NixOS module{normal}"),
                Node::Unknown => format!("{header}: {warning}unknown{normal}"),
                Node::OmittedSystem => {
                    format!("{header} {warning}omitted{normal} (use '--all-systems' to show)")
                }
                Node::OmittedLegacy => {
                    format!("{header} {warning}omitted{normal} (use '--legacy' to show)")
                }
                Node::OmittedIfd => format!(
                    "{header} {warning}omitted due to use of import from derivation{normal}"
                ),
            };
            output.push(line);
        }
        Ok(Report {
            output: output.join("\n"),
            warnings,
        })
    }
}

fn optional_text(value: Option<&Json>) -> Result<Option<String>> {
    match value {
        Some(Json::Null) => Ok(None),
        Some(Json::String(text)) => Ok(Some(text.clone())),
        _ => Err("invalid optional flake-show description".into()),
    }
}

fn attr_path(path: &[&str]) -> Result<String> {
    let mut bytes = Vec::new();
    for (index, name) in path.iter().enumerate() {
        if index != 0 {
            bytes.push(b'.');
        }
        crate::print::print_attr_name(name, &mut bytes);
    }
    // The input names and the printer's escapes are UTF-8.
    String::from_utf8(bytes).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(document: &mut Document, kind: Kind) -> Result<NodeId> {
        let name = if kind == Kind::Derivation {
            "package-name"
        } else {
            ""
        };
        let description = matches!(kind, Kind::Derivation | Kind::App | Kind::Template)
            .then(|| "description".to_owned());
        document.add(kind, name.to_owned(), description, Vec::new())
    }

    fn branch(document: &mut Document, children: Vec<(&str, NodeId)>) -> Result<NodeId> {
        document.add(
            Kind::Branch,
            String::new(),
            None,
            children
                .into_iter()
                .map(|(name, id)| (name.to_owned(), id))
                .collect(),
        )
    }

    #[test]
    fn all_kinds_round_trip_with_public_schema_and_omission_reasons() -> Result<()> {
        let mut document = Document::default();
        let mut children = Vec::new();
        for kind in Kind::ALL {
            let id = leaf(&mut document, kind)?;
            children.push((format!("node{}", kind as u32), id));
        }
        let root = document.add(Kind::Branch, String::new(), None, children)?;
        document.finish(root)?;
        let encoded = document.encode()?;
        let decoded = Document::decode(encoded.as_bytes())?;
        assert_eq!(encoded, decoded.encode()?);
        let json = document.render("root", true, false)?;
        let public: Json = serde_json::from_str(&json.output).map_err(|e| e.to_string())?;
        assert_eq!(
            public,
            json!({
                "node0": {}, "node1": {"type":"derivation", "name":"package-name", "description":"description"},
                "node2": {"type":"app", "description":"description"}, "node3": {"type":"template", "description":"description"},
                "node4": {"type":"nixpkgs-overlay"}, "node5": {"type":"nixos-configuration"},
                "node6": {"type":"nixos-module"}, "node7": {"type":"unknown"},
                "node8": {}, "node9": {}, "node10": {}, "node11": {}, "node12": {}
            })
        );
        let warnings: Vec<_> = json.warnings.iter().map(|warning| warning.kind).collect();
        assert_eq!(
            warnings,
            [
                Kind::OmittedSystem,
                Kind::OmittedLegacy,
                Kind::OmittedIfd,
                Kind::NonDerivation
            ]
        );
        let text = decoded.render("root", false, false)?;
        assert_eq!(text.warnings.len(), 1);
        assert_eq!(
            text.warnings.first().map(|warning| warning.kind),
            Some(Kind::NonDerivation)
        );
        for meaningful in [
            "package 'package-name'",
            "app: description",
            "template: description",
            "Nixpkgs overlay",
            "NixOS configuration",
            "NixOS module",
            "unknown",
            "omitted (use '--all-systems' to show)",
            "omitted (use '--legacy' to show)",
            "omitted due to use of import from derivation",
        ] {
            assert!(
                text.output.contains(meaningful),
                "missing {meaningful}: {}",
                text.output
            );
        }
        assert!(!text.output.contains("node8"));
        assert!(!text.output.contains("node9"));
        assert_eq!(text.output, document.render("root", false, false)?.output);
        Ok(())
    }

    #[test]
    fn optional_descriptions_and_quoted_warning_paths_keep_their_meaning() -> Result<()> {
        let mut document = Document::default();
        let app_none = document.add(Kind::App, String::new(), None, Vec::new())?;
        let app_empty = document.add(Kind::App, String::new(), Some(String::new()), Vec::new())?;
        let derivation = document.add(
            Kind::Derivation,
            "without-description".into(),
            None,
            Vec::new(),
        )?;
        let non_derivation = leaf(&mut document, Kind::NonDerivation)?;
        let root = branch(
            &mut document,
            vec![
                ("appNone", app_none),
                ("appEmpty", app_empty),
                ("package", derivation),
                ("bad.name", non_derivation),
            ],
        )?;
        document.finish(root)?;
        let json = document.render("root", true, false)?;
        let public: Json = serde_json::from_str(&json.output).map_err(|e| e.to_string())?;
        assert_eq!(public.get("appNone"), Some(&json!({"type":"app"})));
        assert_eq!(
            public.get("appEmpty"),
            Some(&json!({"type":"app", "description":""}))
        );
        assert_eq!(
            public.get("package"),
            Some(&json!({"type":"derivation", "name":"without-description"}))
        );
        assert_eq!(
            json.warnings
                .first()
                .map(|warning| warning.message.as_str()),
            Some("\"bad.name\".name is not a derivation")
        );
        let text = document.render("root", false, false)?;
        assert!(text.output.contains("appNone: app: no description"));
        assert!(text.output.contains("appEmpty: app: \n"));
        Ok(())
    }

    #[test]
    fn derivation_labels_follow_output_namespace() -> Result<()> {
        for (namespace, suffix, label) in [
            ("devShell", None, "development environment"),
            ("devShells", Some("default"), "development environment"),
            ("checks", Some("test"), "derivation"),
            ("hydraJobs", Some("nested"), "derivation"),
            ("packages", Some("default"), "package"),
        ] {
            let mut document = Document::default();
            let mut id = leaf(&mut document, Kind::Derivation)?;
            if let Some(suffix) = suffix {
                id = branch(&mut document, vec![(suffix, id)])?;
            }
            id = branch(&mut document, vec![("system", id)])?;
            let root = branch(&mut document, vec![(namespace, id)])?;
            document.finish(root)?;
            assert!(
                document
                    .render("root", false, false)?
                    .output
                    .contains(&format!(": {label} 'package-name'"))
            );
        }
        Ok(())
    }

    #[test]
    fn cache_decoder_refuses_invalid_shapes_and_graphs() {
        for encoded in [
            json!({"version":2,"root":0,"nodes":[[0,[]]]}),
            json!({"version":1,"root":0,"nodes":[[0,[]]],"extra":true}),
            json!({"version":1,"root":0,"nodes":[[0,{}]]}),
            json!({"version":1,"root":0,"nodes":[[13]]}),
            json!({"version":1,"root":0,"nodes":[[1,"",null]]}),
            json!({"version":1,"root":0,"nodes":[[3]]}),
            json!({"version":1,"root":0,"nodes":[[2,42]]}),
            json!({"version":1,"root":0,"nodes":[[0,[["self",0]]]]}),
            json!({"version":1,"root":0,"nodes":[[0,[["future",1]]],[0,[]]]}),
            json!({"version":1,"root":1,"nodes":[[8],[0,[]]]}),
            json!({"version":1,"root":1,"nodes":[[8],[0,[["x",0],["x",0]]]]}),
            json!({"version":1,"root":1,"nodes":[[8],[0,[["x",0],["y",0]]]]}),
            json!({"version":1,"root":1,"nodes":[[8],[0,[[42,0]]]]}),
            json!({"version":1,"root":0,"nodes":[[8]]}),
            json!({"version":1,"root":9,"nodes":[[0,[]]]}),
            json!({"version":1,"root":0,"nodes":[[0,[],false]]}),
        ] {
            assert!(
                Document::decode(encoded.to_string().as_bytes()).is_err(),
                "accepted {encoded}"
            );
        }
        assert!(Document::decode(b"\xff").is_err());
    }

    #[test]
    fn builder_enforces_fields_ownership_and_finished_state() -> Result<()> {
        let mut first = Document::default();
        let foreign = leaf(&mut first, Kind::Empty)?;
        let mut second = Document::default();
        assert!(branch(&mut second, vec![("foreign", foreign)]).is_err());
        assert!(
            second
                .add(Kind::Derivation, String::new(), None, Vec::new())
                .is_err()
        );
        assert!(
            second
                .add(Kind::Template, String::new(), None, Vec::new())
                .is_err()
        );
        assert!(
            second
                .add(Kind::Empty, "unexpected".into(), None, Vec::new())
                .is_err()
        );
        assert!(
            second
                .add(
                    Kind::Unknown,
                    String::new(),
                    Some("unexpected".into()),
                    Vec::new()
                )
                .is_err()
        );
        assert!(
            first
                .add(
                    Kind::Empty,
                    String::new(),
                    None,
                    vec![("child".into(), foreign)]
                )
                .is_err()
        );
        assert!(Kind::from_code(13).is_err());
        assert!(first.encode().is_err());
        // The C++ walk can recover from a legacy subtree error after building fragments.
        // They do not become part of the finalized document or its canonical bytes.
        let root = branch(&mut first, Vec::new())?;
        first.finish(root)?;
        assert_eq!(first.nodes.len(), 1);
        assert!(first.finish(root).is_err());
        assert!(leaf(&mut first, Kind::Empty).is_err());
        assert_eq!(Document::decode(first.encode()?.as_bytes())?.nodes.len(), 1);
        Ok(())
    }

    #[test]
    fn flat_codec_and_json_renderer_handle_deep_trees_without_recursion() -> Result<()> {
        let mut document = Document::default();
        let mut root = branch(&mut document, Vec::new())?;
        for _ in 0..1024 {
            root = branch(&mut document, vec![("x", root)])?;
        }
        document.finish(root)?;
        let decoded = Document::decode(document.encode()?.as_bytes())?;
        let expected = format!("{}{}{}", "{\"x\":".repeat(1024), "{}", "}".repeat(1024));
        let report = decoded.render("ignored root label", true, true)?;
        assert_eq!(report.output, expected);
        assert!(report.warnings.is_empty());
        Ok(())
    }

    #[test]
    fn json_warning_paths_leave_nested_branches_before_visiting_siblings() -> Result<()> {
        let mut document = Document::default();
        let mut nested = leaf(&mut document, Kind::NonDerivation)?;
        for _ in 0..1024 {
            nested = branch(&mut document, vec![("x", nested)])?;
        }
        let sibling = leaf(&mut document, Kind::OmittedIfd)?;
        let root = branch(&mut document, vec![("deep", nested), ("sibling", sibling)])?;
        document.finish(root)?;
        let report = document.render("ignored", true, false)?;
        let warnings: Vec<_> = report
            .warnings
            .iter()
            .map(|warning| warning.message.as_str())
            .collect();
        assert_eq!(
            warnings,
            [
                format!("deep.{}name is not a derivation", "x.".repeat(1024)),
                "sibling omitted due to use of import from derivation".to_owned(),
            ]
        );
        Ok(())
    }

    #[test]
    fn untrusted_labels_names_and_descriptions_cannot_control_the_terminal() -> Result<()> {
        let attack = "\x1b]52;c;payload\x07\x1b[2J\u{9b}31m\u{9d}title\u{9c}\r\n\t\0";
        let label = format!("root{attack}λ");
        let attribute = format!("attribute{attack}中文");
        let package = format!("package{attack}🦀");
        let description = format!("description{attack}café");
        let warning_name = format!("warning{attack}零");
        let mut document = Document::default();
        let derivation = document.add(
            Kind::Derivation,
            package.clone(),
            Some(description.clone()),
            Vec::new(),
        )?;
        let app = document.add(
            Kind::App,
            String::new(),
            Some(description.clone()),
            Vec::new(),
        )?;
        let template = document.add(
            Kind::Template,
            String::new(),
            Some(description.clone()),
            Vec::new(),
        )?;
        let omitted = document.add(Kind::NonDerivation, String::new(), None, Vec::new())?;
        let root = branch(
            &mut document,
            vec![
                (&attribute, derivation),
                ("app", app),
                ("template", template),
                (&warning_name, omitted),
            ],
        )?;
        document.finish(root)?;

        let plain = document.render(&label, false, false)?;
        assert_eq!(
            plain.output.lines().count(),
            4,
            "untrusted text forged extra lines"
        );
        assert!(
            plain
                .output
                .chars()
                .all(|ch| ch == '\n' || !ch.is_control())
        );
        for expected in [
            "root\\u{1b}]52;c;payload\\u{7}",
            "attribute\\u{1b}]52;c;payload\\u{7}",
            "package\\u{1b}]52;c;payload\\u{7}",
            "description\\u{1b}]52;c;payload\\u{7}",
            "\\u{9b}31m\\u{9d}title\\u{9c}\\r\\n\\t\\u{0}",
            "λ",
            "中文",
            "🦀",
            "café",
        ] {
            assert!(
                plain.output.contains(expected),
                "missing {expected}: {}",
                plain.output
            );
        }
        assert_eq!(plain.warnings.len(), 1);
        assert!(
            plain
                .warnings
                .iter()
                .all(|warning| !warning.message.chars().any(char::is_control))
        );

        let colored = document.render(&label, false, true)?;
        assert!(colored.output.contains("\x1b[1mroot\\u{1b}]52;"));
        let mut uncolored = colored.output;
        for trusted in ["\x1b[0m", "\x1b[1m", "\x1b[32;1m", "\x1b[35;1m"] {
            uncolored = uncolored.replace(trusted, "");
        }
        assert_eq!(uncolored, plain.output, "only generated ANSI may remain");
        assert!(
            colored
                .warnings
                .iter()
                .all(|warning| !warning.message.chars().any(char::is_control))
        );

        // Persisted and public JSON retain exact semantic strings. Escaping
        // happens only when values cross a terminal or logger boundary.
        let decoded = Document::decode(document.encode()?.as_bytes())?;
        for colored in [false, true] {
            let report = decoded.render(&label, true, colored)?;
            let value: Json = serde_json::from_str(&report.output).map_err(|e| e.to_string())?;
            assert_eq!(
                value.get(&attribute),
                Some(&json!({"type":"derivation", "name":package, "description":description}))
            );
            assert_eq!(
                value.get("app"),
                Some(&json!({"type":"app", "description":description}))
            );
            assert_eq!(
                value.get("template"),
                Some(&json!({"type":"template", "description":description}))
            );
            assert!(value.get(&warning_name).is_some());
            assert!(
                report
                    .warnings
                    .iter()
                    .all(|warning| !warning.message.chars().any(char::is_control))
            );
        }
        Ok(())
    }

    #[test]
    fn omission_warnings_escape_controls_even_when_output_is_json() -> Result<()> {
        let mut document = Document::default();
        let mut children = Vec::new();
        for kind in [
            Kind::NonDerivation,
            Kind::OmittedSystem,
            Kind::OmittedLegacy,
            Kind::OmittedIfd,
        ] {
            children.push((
                format!("unsafe{}\x1b]52;c;data\x07\u{9b}2J\r\n", kind as u32),
                leaf(&mut document, kind)?,
            ));
        }
        let root = document.add(Kind::Branch, String::new(), None, children)?;
        document.finish(root)?;
        let report = document.render("root", true, false)?;
        assert_eq!(report.warnings.len(), 4);
        for warning in report.warnings {
            assert!(
                !warning.message.chars().any(char::is_control),
                "{}",
                warning.message
            );
            assert!(warning.message.contains("\\u{1b}]52;"));
            assert!(warning.message.contains("\\u{9b}2J"));
        }
        Ok(())
    }

    #[test]
    fn omission_color_spans_are_precise() -> Result<()> {
        let mut document = Document::default();
        let system = leaf(&mut document, Kind::OmittedSystem)?;
        let ifd = leaf(&mut document, Kind::OmittedIfd)?;
        let root = branch(&mut document, vec![("system", system), ("ifd", ifd)])?;
        document.finish(root)?;
        let colored = document.render("root", false, true)?.output;
        assert!(colored.contains("\x1b[35;1momitted\x1b[0m (use '--all-systems' to show)"));
        assert!(colored.contains("\x1b[35;1momitted due to use of import from derivation\x1b[0m"));
        Ok(())
    }
}
