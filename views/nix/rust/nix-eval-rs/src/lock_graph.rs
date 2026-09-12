//! The lock graph owns node identity, adjacency and follows resolution.
//!
//! Fetch payloads are opaque objects here. C++ constructs its fetch handles
//! from those payloads once; it never owns a second adjacency map.

use serde_json::{Map, Value as Json, json};
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

mod ffi;

type Result<T> = std::result::Result<T, String>;
type Path = Vec<String>;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct NodeId(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
enum Edge {
    Node(NodeId),
    Follows(Path),
}

impl Edge {
    fn wire(&self) -> Json {
        match self {
            Self::Node(id) => json!(id.0),
            Self::Follows(path) => json!(path),
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Node {
    payload: Map<String, Json>,
    inputs: BTreeMap<String, Edge>,
}

#[derive(Clone, Debug)]
struct Graph {
    nodes: BTreeMap<NodeId, Node>,
    next_id: u64,
    resolved_paths: RefCell<BTreeMap<Path, Option<NodeId>>>,
    identity: RefCell<Option<[u8; 32]>>,
    #[cfg(test)]
    lookup_steps: std::cell::Cell<usize>,
    #[cfg(test)]
    dag_steps: std::cell::Cell<usize>,
}

impl Default for Graph {
    fn default() -> Self {
        Self {
            nodes: BTreeMap::from([(NodeId(0), Node::default())]),
            next_id: 1,
            resolved_paths: RefCell::new(BTreeMap::new()),
            identity: RefCell::new(None),
            #[cfg(test)]
            lookup_steps: std::cell::Cell::new(0),
            #[cfg(test)]
            dag_steps: std::cell::Cell::new(0),
        }
    }
}

/// Per-command dependency state. The immutable graph remains the sole graph
/// owner; this snapshot contains only scheduling counts and completion links.
#[derive(Debug)]
struct PrefetchSchedule {
    children: BTreeMap<NodeId, Vec<NodeId>>,
    waiting: BTreeMap<NodeId, usize>,
    ready: Vec<NodeId>,
    running: BTreeSet<NodeId>,
    completed: usize,
    failed: usize,
}

impl PrefetchSchedule {
    fn new(graph: &Graph) -> Result<Self> {
        graph.check_direct_edges()?;
        graph.check()?;
        let nodes = graph.reachable()?;
        let mut waiting: BTreeMap<NodeId, usize> = nodes.iter().map(|id| (*id, 0)).collect();
        let mut children = BTreeMap::new();
        for id in nodes {
            // Multiple names for one direct child impose one dependency.
            // Follows refer to existing nodes and never introduce fetch work.
            let direct: BTreeSet<NodeId> = graph
                .node(id)?
                .inputs
                .values()
                .filter_map(|edge| {
                    if let Edge::Node(child) = edge {
                        Some(*child)
                    } else {
                        None
                    }
                })
                .collect();
            for child in &direct {
                let count = waiting
                    .get_mut(child)
                    .ok_or_else(|| "missing scheduled child".to_owned())?;
                *count = count
                    .checked_add(1)
                    .ok_or_else(|| "prefetch dependency count overflow".to_owned())?;
            }
            children.insert(id, direct.into_iter().collect());
        }
        let ready = waiting
            .iter()
            .filter_map(|(id, count)| (*count == 0).then_some(*id))
            .collect();
        Ok(Self {
            children,
            waiting,
            ready,
            running: BTreeSet::new(),
            completed: 0,
            failed: 0,
        })
    }

    fn take_ready(&mut self) -> Vec<NodeId> {
        let ready = std::mem::take(&mut self.ready);
        self.running.extend(ready.iter().copied());
        ready
    }

    fn complete(&mut self, id: NodeId, succeeded: bool) -> Result<Vec<NodeId>> {
        if !self.running.remove(&id) {
            return Err(format!("lock prefetch node {} is not running", id.0));
        }
        self.completed += 1;
        if !succeeded {
            self.failed += 1;
            // Dependent nodes retain this unsatisfied dependency and cannot
            // race a missing parent store object or report redundant errors.
            return Ok(Vec::new());
        }
        for child in self
            .children
            .get(&id)
            .ok_or_else(|| "unknown completed prefetch node".to_owned())?
        {
            let count = self
                .waiting
                .get_mut(child)
                .ok_or_else(|| "missing prefetch dependency".to_owned())?;
            *count = count
                .checked_sub(1)
                .ok_or_else(|| "prefetch dependency already completed".to_owned())?;
            if *count == 0 {
                self.ready.push(*child);
            }
        }
        Ok(self.take_ready())
    }

    fn check_complete(&self) -> Result<()> {
        if self.failed > 0
            || self.completed != self.waiting.len()
            || !self.running.is_empty()
            || !self.ready.is_empty()
        {
            return Err(format!(
                "lock prefetch incomplete: {} of {} nodes completed, {} failed",
                self.completed,
                self.waiting.len(),
                self.failed
            ));
        }
        Ok(())
    }
}

fn object(value: &Json) -> Result<&Map<String, Json>> {
    value
        .as_object()
        .ok_or_else(|| "expected a lock graph object".to_owned())
}

fn field<'a>(value: &'a Json, name: &str) -> Result<&'a Json> {
    value
        .get(name)
        .ok_or_else(|| format!("lock graph field '{name}' is missing"))
}

fn string(value: &Json) -> Result<&str> {
    value
        .as_str()
        .ok_or_else(|| "expected a lock graph string".to_owned())
}

fn valid_component(part: &str) -> bool {
    let mut bytes = part.bytes();
    bytes.next().is_some_and(|b| b.is_ascii_alphabetic())
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(crate) fn parse_input_path(value: &str) -> Result<Path> {
    if value.is_empty() {
        return Ok(Vec::new());
    }
    value
        .split('/')
        .map(|part| {
            if !valid_component(part) {
                return Err(format!("invalid follows path component '{part}'"));
            }
            Ok(part.to_owned())
        })
        .collect()
}

fn path(value: &Json) -> Result<Path> {
    value
        .as_array()
        .ok_or_else(|| "expected a follows path".to_owned())?
        .iter()
        .map(|part| {
            let part = string(part)?;
            if !valid_component(part) {
                return Err(format!("invalid follows path component '{part}'"));
            }
            Ok(part.to_owned())
        })
        .collect()
}

fn payload(value: &Json) -> Result<Map<String, Json>> {
    let fields = object(value)?;
    for name in fields.keys() {
        if !matches!(name.as_str(), "locked" | "original" | "flake" | "parent") {
            return Err(format!("unsupported lock node field '{name}'"));
        }
    }
    object(field(value, "locked")?)?;
    object(field(value, "original")?)?;
    if let Some(value) = fields.get("flake")
        && !value.is_boolean()
    {
        return Err("lock node 'flake' must be Boolean".to_owned());
    }
    if let Some(value) = fields.get("parent") {
        path(value)?;
    }
    Ok(fields.clone())
}

impl Graph {
    fn invalidate(&mut self) {
        self.resolved_paths.get_mut().clear();
        *self.identity.get_mut() = None;
        #[cfg(test)]
        self.lookup_steps.set(0);
    }

    fn identity(&self) -> Result<[u8; 32]> {
        if let Some(identity) = *self.identity.borrow() {
            return Ok(identity);
        }
        let serialized = self.serialize()?;
        let bytes =
            serde_json::to_vec(field(&serialized, "document")?).map_err(|e| e.to_string())?;
        let identity = *blake3::hash(&bytes).as_bytes();
        *self.identity.borrow_mut() = Some(identity);
        Ok(identity)
    }

    fn node(&self, id: NodeId) -> Result<&Node> {
        self.nodes
            .get(&id)
            .ok_or_else(|| format!("unknown lock node {}", id.0))
    }

    fn add(&mut self, value: &Json) -> Result<NodeId> {
        let node = Node {
            payload: payload(value)?,
            inputs: BTreeMap::new(),
        };
        let id = NodeId(self.next_id);
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or_else(|| "lock node ID space exhausted".to_owned())?;
        self.nodes.insert(id, node);
        self.invalidate();
        Ok(id)
    }

    fn set_edge(&mut self, from: NodeId, name: String, edge: Edge) -> Result<()> {
        self.node(from)?;
        if let Edge::Follows(path) = &edge
            && path.iter().any(|part| !valid_component(part))
        {
            return Err("invalid follows path component".to_owned());
        }
        if let Edge::Node(target) = &edge {
            if *target == NodeId(0) {
                return Err("lock file contains cycle to root node".to_owned());
            }
            self.node(*target)?;
            let mut pending = vec![*target];
            let mut seen = BTreeSet::new();
            while let Some(id) = pending.pop() {
                if id == from {
                    return Err("direct cycle in lock graph".to_owned());
                }
                if !seen.insert(id) {
                    continue;
                }
                for edge in self.node(id)?.inputs.values() {
                    if let Edge::Node(id) = edge {
                        pending.push(*id);
                    }
                }
            }
        }
        self.nodes
            .get_mut(&from)
            .ok_or_else(|| "unknown source node".to_owned())?
            .inputs
            .insert(name, edge);
        self.invalidate();
        Ok(())
    }

    fn parse(contents: &str) -> Result<Self> {
        let doc: Json =
            serde_json::from_str(contents).map_err(|e| format!("invalid lock JSON: {e}"))?;
        if field(&doc, "version")?.as_u64() != Some(7) {
            return Err("unsupported lock file version; expected 7".to_owned());
        }
        for name in object(&doc)?.keys() {
            if !matches!(name.as_str(), "version" | "root" | "nodes") {
                return Err(format!("unsupported lock document field '{name}'"));
            }
        }
        let root = string(field(&doc, "root")?)?;
        let nodes = object(field(&doc, "nodes")?)?;
        if !nodes.contains_key(root) {
            return Err("lock file root node is missing".to_owned());
        }
        let mut graph = Self::default();
        let mut ids = BTreeMap::from([(root.to_owned(), NodeId(0))]);
        for (name, node) in nodes {
            let mut fields = object(node)?.clone();
            fields.remove("inputs");
            if name == root {
                if !fields.is_empty() {
                    return Err("lock root must contain only inputs".to_owned());
                }
            } else {
                ids.insert(name.clone(), graph.add(&Json::Object(fields))?);
            }
        }
        for (name, node) in nodes {
            let from = *ids
                .get(name)
                .ok_or_else(|| "missing source identity".to_owned())?;
            if let Some(inputs) = node.get("inputs") {
                for (input, value) in object(inputs)? {
                    let edge = if value.is_array() {
                        Edge::Follows(path(value)?)
                    } else {
                        let key = string(value)?;
                        let target = *ids
                            .get(key)
                            .ok_or_else(|| format!("lock file references missing node '{key}'"))?;
                        if target == NodeId(0) {
                            return Err("lock file contains cycle to root node".to_owned());
                        }
                        Edge::Node(target)
                    };
                    graph
                        .nodes
                        .get_mut(&from)
                        .ok_or_else(|| "missing source node".to_owned())?
                        .inputs
                        .insert(input.clone(), edge);
                }
            }
        }
        graph.check_direct_edges()?;
        graph.check()?;
        Ok(graph)
    }

    /// Bulk decoding checks each direct edge once, regardless of input order.
    fn check_direct_edges(&self) -> Result<()> {
        let mut colors = BTreeMap::new();
        for id in self.nodes.keys() {
            if colors.contains_key(id) {
                continue;
            }
            colors.insert(*id, 1u8);
            let mut frames = vec![(*id, self.node(*id)?.inputs.values())];
            while let Some((id, children)) = frames.last_mut() {
                let Some(edge) = children.next() else {
                    colors.insert(*id, 2);
                    frames.pop();
                    continue;
                };
                #[cfg(test)]
                self.dag_steps.set(self.dag_steps.get() + 1);
                if let Edge::Node(child) = edge {
                    match colors.get(child) {
                        Some(1) => return Err("direct cycle in lock graph".to_owned()),
                        Some(2) => {}
                        _ => {
                            colors.insert(*child, 1);
                            frames.push((*child, self.node(*child)?.inputs.values()));
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Explicit continuation frames avoid native recursion for follows chains.
    fn find(&self, target: &[String]) -> Result<Option<NodeId>> {
        if let Some(result) = self.resolved_paths.borrow().get(target) {
            return Ok(*result);
        }
        struct Frame {
            path: Path,
            index: usize,
            node: NodeId,
        }
        let mut active = BTreeSet::from([target.to_vec()]);
        let mut frames = vec![Frame {
            path: target.to_vec(),
            index: 0,
            node: NodeId(0),
        }];
        loop {
            let Some(frame) = frames.last_mut() else {
                return Ok(Some(NodeId(0)));
            };
            if frame.index == frame.path.len() {
                let result = frame.node;
                self.resolved_paths
                    .borrow_mut()
                    .insert(frame.path.clone(), Some(result));
                active.remove(&frame.path);
                frames.pop();
                if let Some(parent) = frames.last_mut() {
                    parent.node = result;
                    continue;
                }
                return Ok(Some(result));
            }
            let component = frame
                .path
                .get(frame.index)
                .ok_or_else(|| "invalid follows cursor".to_owned())?;
            frame.index += 1;
            #[cfg(test)]
            self.lookup_steps.set(self.lookup_steps.get() + 1);
            match self.node(frame.node)?.inputs.get(component) {
                None => {
                    for frame in &frames {
                        self.resolved_paths
                            .borrow_mut()
                            .insert(frame.path.clone(), None);
                    }
                    return Ok(None);
                }
                Some(Edge::Node(node)) => frame.node = *node,
                Some(Edge::Follows(path)) => {
                    let cached = self.resolved_paths.borrow().get(path).copied();
                    match cached {
                        Some(Some(node)) => {
                            frame.node = node;
                            continue;
                        }
                        Some(None) => {
                            for frame in &frames {
                                self.resolved_paths
                                    .borrow_mut()
                                    .insert(frame.path.clone(), None);
                            }
                            return Ok(None);
                        }
                        None => {}
                    }
                    if !active.insert(path.clone()) {
                        return Err(format!("follow cycle detected at '{}'", path.join("/")));
                    }
                    frames.push(Frame {
                        path: path.clone(),
                        index: 0,
                        node: NodeId(0),
                    });
                }
            }
        }
    }

    fn reachable(&self) -> Result<Vec<NodeId>> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        let mut pending = vec![NodeId(0)];
        while let Some(id) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            out.push(id);
            for edge in self.node(id)?.inputs.values().rev() {
                if let Edge::Node(child) = edge {
                    pending.push(*child);
                }
            }
        }
        Ok(out)
    }

    fn all_inputs(&self) -> Result<Json> {
        let mut seen = BTreeSet::new();
        let mut out = BTreeMap::<Path, Edge>::new();
        let mut pending = vec![(Vec::new(), NodeId(0))];
        while let Some((prefix, id)) = pending.pop() {
            if !seen.insert(id) {
                continue;
            }
            for (name, edge) in self.node(id)?.inputs.iter().rev() {
                let mut path = prefix.clone();
                path.push(name.clone());
                out.insert(path.clone(), edge.clone());
                if let Edge::Node(child) = edge {
                    pending.push((path, *child));
                }
            }
        }
        Ok(Json::Array(
            out.into_iter()
                .map(|(path, edge)| json!({"path": path, "edge": edge.wire()}))
                .collect(),
        ))
    }

    fn check(&self) -> Result<()> {
        let mut checked_parents = BTreeSet::new();
        for id in self.reachable()? {
            let node = self.node(id)?;
            for (name, edge) in &node.inputs {
                if let Edge::Follows(path) = edge
                    && self.find(path)?.is_none()
                {
                    return Err(format!(
                        "input '{name}' follows a non-existent input '{}'",
                        path.join("/")
                    ));
                }
            }
            let mut current = id;
            let mut parents = BTreeSet::new();
            while let Some(parent) = self.node(current)?.payload.get("parent") {
                if checked_parents.contains(&current) {
                    break;
                }
                if !parents.insert(current) {
                    return Err("cycle in lock node parent chain".to_owned());
                }
                current = self
                    .find(&path(parent)?)?
                    .ok_or_else(|| "lock node parent does not exist".to_owned())?;
            }
            checked_parents.extend(parents);
        }
        Ok(())
    }

    /// Canonical names depend on sorted edges, never allocation or pointer order.
    fn serialize(&self) -> Result<Json> {
        let mut keys = BTreeMap::<NodeId, String>::new();
        let mut used = BTreeSet::new();
        let mut pending = vec![("root".to_owned(), NodeId(0))];
        while let Some((hint, id)) = pending.pop() {
            if keys.contains_key(&id) {
                continue;
            }
            let mut key = hint.clone();
            let mut suffix = 2u64;
            while !used.insert(key.clone()) {
                key = format!("{hint}_{suffix}");
                suffix = suffix
                    .checked_add(1)
                    .ok_or_else(|| "node name space exhausted".to_owned())?;
            }
            keys.insert(id, key);
            for (name, edge) in self.node(id)?.inputs.iter().rev() {
                if let Edge::Node(child) = edge {
                    pending.push((name.clone(), *child));
                }
            }
        }
        let mut nodes = Map::new();
        for (id, key) in &keys {
            let node = self.node(*id)?;
            let mut out = node.payload.clone();
            if !node.inputs.is_empty() {
                let mut inputs = Map::new();
                for (name, edge) in &node.inputs {
                    let value = match edge {
                        Edge::Node(id) => Json::String(
                            keys.get(id)
                                .ok_or_else(|| "missing node key".to_owned())?
                                .clone(),
                        ),
                        Edge::Follows(path) => json!(path),
                    };
                    inputs.insert(name.clone(), value);
                }
                out.insert("inputs".to_owned(), Json::Object(inputs));
            }
            nodes.insert(key.clone(), Json::Object(out));
        }
        let document = json!({"version": 7, "root": "root", "nodes": nodes});
        let keys: Vec<Json> = keys
            .into_iter()
            .map(|(id, key)| json!({"id":id.0,"key":key}))
            .collect();
        Ok(json!({"document":document,"keys":keys}))
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;
    fn fixture() -> Json {
        json!({"locked":{"type":"git","rev":"abc"},"original":{"type":"git"}})
    }

    #[test]
    fn repeated_follows_targets_are_not_cycles() {
        let mut graph = Graph::default();
        let a = graph.add(&fixture()).expect("node");
        graph
            .set_edge(NodeId(0), "a".into(), Edge::Node(a))
            .expect("edge");
        graph
            .set_edge(a, "same".into(), Edge::Follows(vec!["a".into()]))
            .expect("follow");
        assert_eq!(
            graph
                .find(&["a".into(), "same".into(), "same".into()])
                .expect("resolve"),
            Some(a)
        );
        graph.check().expect("valid");
    }

    #[test]
    fn cycles_and_dangling_follows_fail_without_native_recursion() {
        let mut graph = Graph::default();
        let a = graph.add(&fixture()).expect("node");
        graph
            .set_edge(NodeId(0), "a".into(), Edge::Node(a))
            .expect("edge");
        assert!(graph.set_edge(a, "loop".into(), Edge::Node(a)).is_err());
        graph
            .set_edge(
                a,
                "loop".into(),
                Edge::Follows(vec!["a".into(), "loop".into()]),
            )
            .expect("follow");
        assert!(graph.check().is_err());
        graph
            .set_edge(a, "loop".into(), Edge::Follows(vec!["missing".into()]))
            .expect("follow");
        assert!(graph.check().is_err());
    }

    #[test]
    fn canonical_serialization_preserves_shared_identity() {
        let mut graph = Graph::default();
        let a = graph.add(&fixture()).expect("node");
        graph
            .set_edge(NodeId(0), "b".into(), Edge::Node(a))
            .expect("edge");
        graph
            .set_edge(NodeId(0), "a".into(), Edge::Node(a))
            .expect("edge");
        let serialized = graph.serialize().expect("serialize");
        let doc = field(&serialized, "document").expect("document");
        let parsed = Graph::parse(&doc.to_string()).expect("parse");
        assert_eq!(
            parsed.find(&["a".into()]).expect("a"),
            parsed.find(&["b".into()]).expect("b")
        );
        assert_eq!(parsed.serialize().expect("serialize"), serialized);
    }

    #[test]
    fn repeated_follows_subproblems_have_linear_work_and_mutations_invalidate() {
        let mut graph = Graph::default();
        graph
            .set_edge(NodeId(0), "a0".into(), Edge::Follows(vec![]))
            .expect("root follow");
        for depth in 1..=35 {
            let previous = format!("a{}", depth - 1);
            graph
                .set_edge(
                    NodeId(0),
                    format!("a{depth}"),
                    Edge::Follows(vec![previous.clone(), previous]),
                )
                .expect("follow");
        }
        assert_eq!(
            graph.find(&["a35".into()]).expect("resolve"),
            Some(NodeId(0))
        );
        assert!(
            graph.lookup_steps.get() <= 72,
            "{} steps",
            graph.lookup_steps.get()
        );
        graph.check().expect("all follows resolve");
        assert!(
            graph.lookup_steps.get() <= 75,
            "{} steps",
            graph.lookup_steps.get()
        );
        let before = graph.identity().expect("identity");
        graph
            .set_edge(
                NodeId(0),
                "a0".into(),
                Edge::Follows(vec!["missing".into()]),
            )
            .expect("mutation");
        assert_eq!(
            graph.find(&["a35".into()]).expect("resolve after edit"),
            None
        );
        assert_ne!(graph.identity().expect("identity"), before);
    }

    #[test]
    fn bulk_decode_checks_reverse_order_chain_edges_once() {
        let depth = 2_000;
        let mut nodes = Map::new();
        nodes.insert(
            "root".to_owned(),
            json!({"inputs":{"chain":format!("n{:04}", depth - 1)}}),
        );
        for index in 0..depth {
            let mut node = fixture();
            if index > 0 {
                node.as_object_mut().expect("object").insert(
                    "inputs".to_owned(),
                    json!({"next":format!("n{:04}", index - 1)}),
                );
            }
            nodes.insert(format!("n{index:04}"), node);
        }
        let graph = Graph::parse(&json!({"version":7,"root":"root","nodes":nodes}).to_string())
            .expect("chain");
        assert_eq!(graph.dag_steps.get(), depth);
        assert_eq!(graph.reachable().expect("reachable").len(), depth + 1);
    }

    #[test]
    fn prefetch_releases_shared_nodes_after_all_parents_without_follow_duplicates() {
        let mut graph = Graph::default();
        let a = graph.add(&fixture()).expect("a");
        let b = graph.add(&fixture()).expect("b");
        let child = graph.add(&fixture()).expect("child");
        graph
            .set_edge(NodeId(0), "a".into(), Edge::Node(a))
            .expect("edge");
        graph
            .set_edge(NodeId(0), "b".into(), Edge::Node(b))
            .expect("edge");
        graph
            .set_edge(a, "child".into(), Edge::Node(child))
            .expect("edge");
        graph
            .set_edge(a, "alias".into(), Edge::Node(child))
            .expect("edge");
        graph
            .set_edge(b, "child".into(), Edge::Node(child))
            .expect("edge");
        graph
            .set_edge(
                NodeId(0),
                "follow".into(),
                Edge::Follows(vec!["a".into(), "child".into()]),
            )
            .expect("follow");
        let mut schedule = PrefetchSchedule::new(&graph).expect("schedule");
        assert_eq!(schedule.take_ready(), vec![NodeId(0)]);
        assert!(schedule.take_ready().is_empty());
        assert_eq!(
            schedule.complete(NodeId(0), true).expect("root"),
            vec![a, b]
        );
        assert!(schedule.complete(a, true).expect("a").is_empty());
        assert_eq!(schedule.complete(b, true).expect("b"), vec![child]);
        assert!(schedule.complete(child, true).expect("child").is_empty());
        schedule.check_complete().expect("complete");
        assert!(
            schedule.complete(child, true).is_err(),
            "duplicate completion accepted"
        );
    }

    #[test]
    fn prefetch_failure_blocks_children_and_keeps_independent_work_ready() {
        let mut graph = Graph::default();
        let a = graph.add(&fixture()).expect("a");
        let b = graph.add(&fixture()).expect("b");
        let child = graph.add(&fixture()).expect("child");
        graph
            .set_edge(NodeId(0), "a".into(), Edge::Node(a))
            .expect("edge");
        graph
            .set_edge(NodeId(0), "b".into(), Edge::Node(b))
            .expect("edge");
        graph
            .set_edge(a, "child".into(), Edge::Node(child))
            .expect("edge");
        let mut schedule = PrefetchSchedule::new(&graph).expect("schedule");
        schedule.take_ready();
        assert_eq!(
            schedule.complete(NodeId(0), true).expect("root"),
            vec![a, b]
        );
        assert!(schedule.complete(a, false).expect("failure").is_empty());
        assert!(schedule.complete(b, true).expect("independent").is_empty());
        assert!(schedule.check_complete().is_err());
        assert!(
            schedule.complete(child, true).is_err(),
            "blocked child became runnable"
        );
    }

    #[test]
    fn prefetch_deep_graph_uses_iterative_dependency_release() {
        let mut graph = Graph::default();
        let depth = 10_000;
        let mut parent = NodeId(0);
        for _ in 0..depth {
            let child = graph.add(&fixture()).expect("child");
            graph
                .set_edge(parent, "child".into(), Edge::Node(child))
                .expect("edge");
            parent = child;
        }
        let mut schedule = PrefetchSchedule::new(&graph).expect("schedule");
        let mut ready = schedule.take_ready();
        let mut count = 0;
        while let Some(id) = ready.pop() {
            count += 1;
            ready.extend(schedule.complete(id, true).expect("complete"));
        }
        assert_eq!(count, depth + 1);
        schedule.check_complete().expect("all complete");
    }

    #[test]
    fn prefetch_rejects_direct_and_follow_cycles_before_releasing_work() {
        let mut graph = Graph::default();
        let a = graph.add(&fixture()).expect("a");
        graph
            .set_edge(NodeId(0), "a".into(), Edge::Node(a))
            .expect("edge");
        graph
            .set_edge(
                a,
                "loop".into(),
                Edge::Follows(vec!["a".into(), "loop".into()]),
            )
            .expect("follow");
        assert!(PrefetchSchedule::new(&graph).is_err());
        graph
            .nodes
            .get_mut(&a)
            .expect("a")
            .inputs
            .insert("loop".into(), Edge::Node(a));
        assert!(PrefetchSchedule::new(&graph).is_err());
    }

    #[test]
    fn empty_lock_document_is_corrupt() {
        assert!(Graph::parse("").is_err());
        assert!(Graph::parse("\0").is_err());
    }

    #[test]
    fn parser_rejects_obsolete_versions_missing_nodes_and_parent_cycles() {
        for doc in [
            json!({"version":6,"root":"root","nodes":{"root":{}}}),
            json!({"version":7,"root":"root","nodes":{"root":{"inputs":{"a":"absent"}}}}),
            json!({"version":7,"root":"root","nodes":{"root":{"inputs":{"a":"a"}},"a":{"locked":{},"original":{},"parent":["a"]}}}),
        ] {
            assert!(Graph::parse(&doc.to_string()).is_err());
        }
    }
}
