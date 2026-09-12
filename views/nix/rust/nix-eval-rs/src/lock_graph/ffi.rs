//! Typed host boundary. Graph operations serialize through a mutex; snapshots
//! own their strings so callers can inspect them after releasing that mutex.

use super::{
    Edge, Graph, Json, NodeId, Path, PrefetchSchedule, Result, json, parse_input_path,
    valid_component,
};
use std::ffi::{CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Mutex, MutexGuard};

pub struct IxeLockGraph(Mutex<Graph>);
pub struct IxeLockSchedule(Mutex<PrefetchSchedule>);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeLockBytes {
    pub data: *const u8,
    pub len: usize,
}

impl IxeLockBytes {
    fn view(text: &str) -> Self {
        Self {
            data: text.as_ptr(),
            len: text.len(),
        }
    }

    unsafe fn text<'a>(self) -> Result<&'a str> {
        if self.len == 0 {
            return Ok("");
        }
        if self.data.is_null() || self.len > isize::MAX as usize {
            return Err("invalid lock graph byte view".into());
        }
        // SAFETY: the ABI caller supplies a live readable allocation.
        std::str::from_utf8(unsafe { std::slice::from_raw_parts(self.data, self.len) })
            .map_err(|error| error.to_string())
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct IxeLockPathView {
    pub data: *const IxeLockBytes,
    pub len: usize,
}

impl IxeLockPathView {
    unsafe fn owned(self) -> Result<Path> {
        if self.len == 0 {
            return Ok(Vec::new());
        }
        if self.data.is_null() || self.len > isize::MAX as usize / size_of::<IxeLockBytes>() {
            return Err("invalid lock graph path view".into());
        }
        // SAFETY: the caller supplies the indicated array and string allocations.
        unsafe { std::slice::from_raw_parts(self.data, self.len) }
            .iter()
            .map(|part| {
                // SAFETY: each component obeys the same borrowed-view contract.
                let part = unsafe { part.text() }?;
                if !valid_component(part) {
                    return Err(format!("invalid follows path component '{part}'"));
                }
                Ok(part.to_owned())
            })
            .collect()
    }
}

pub struct IxeLockPath {
    _strings: Path,
    views: Vec<IxeLockBytes>,
}

impl IxeLockPath {
    fn new(strings: Path) -> Self {
        let views = strings.iter().map(|s| IxeLockBytes::view(s)).collect();
        Self {
            _strings: strings,
            views,
        }
    }

    fn view(&self) -> IxeLockPathView {
        IxeLockPathView {
            data: self.views.as_ptr(),
            len: self.views.len(),
        }
    }
}

#[repr(C)]
pub struct IxeLockEdgeView {
    pub name: IxeLockBytes,
    pub kind: u8,
    pub target: u64,
    pub follows: IxeLockPathView,
}

struct Input {
    name: String,
    target: Option<NodeId>,
    follows: IxeLockPath,
}

pub struct IxeLockInputs(Vec<Input>);
pub struct IxeLockNodes(Vec<NodeId>);

fn boundary(action: impl FnOnce() -> Result<()>) -> *mut c_char {
    match catch_unwind(AssertUnwindSafe(action))
        .unwrap_or_else(|_| Err("panic in lock graph operation".into()))
    {
        Ok(()) => ptr::null_mut(),
        Err(error) => {
            // A dedicated plain error string, never a JSON response envelope.
            let bytes: Vec<u8> = error.bytes().filter(|byte| *byte != 0).collect();
            // SAFETY: all interior NUL bytes were removed.
            unsafe { CString::from_vec_unchecked(bytes) }.into_raw()
        }
    }
}

unsafe fn output<T>(out: *mut T, value: T) -> Result<()> {
    if out.is_null() {
        return Err("null lock graph output".into());
    }
    // SAFETY: the caller provides aligned writable storage for T.
    unsafe { out.write(value) };
    Ok(())
}

unsafe fn graph<'a>(handle: *const IxeLockGraph) -> Result<MutexGuard<'a, Graph>> {
    // SAFETY: the caller keeps the graph alive for the operation.
    let handle = unsafe { handle.as_ref() }.ok_or("null lock graph handle")?;
    handle
        .0
        .lock()
        .map_err(|_| "lock graph mutex poisoned".into())
}

fn json_string(value: Json) -> Result<*mut c_char> {
    CString::new(value.to_string())
        .map(CString::into_raw)
        .map_err(|e| e.to_string())
}

/// # Safety
/// `out` must be writable. Returned handles must be freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_new_empty(out: *mut *mut IxeLockGraph) -> *mut c_char {
    boundary(|| {
        if out.is_null() {
            return Err("null lock graph output".into());
        }
        let handle = Box::into_raw(Box::new(IxeLockGraph(Mutex::new(Graph::default()))));
        // SAFETY: output was checked and the caller provides writable storage.
        unsafe { output(out, handle) }
    })
}

/// # Safety
/// Source bytes must be readable and `out` writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_parse(
    source: IxeLockBytes,
    out: *mut *mut IxeLockGraph,
) -> *mut c_char {
    boundary(|| {
        if out.is_null() {
            return Err("null lock graph output".into());
        }
        // SAFETY: the source is borrowed according to the ABI contract.
        let parsed = Graph::parse(unsafe { source.text() }?)?;
        let handle = Box::into_raw(Box::new(IxeLockGraph(Mutex::new(parsed))));
        // SAFETY: checked writable output.
        unsafe { output(out, handle) }
    })
}

/// # Safety
/// Graph and payload must be live; `out` must be writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_add(
    handle: *mut IxeLockGraph,
    payload: IxeLockBytes,
    out: *mut u64,
) -> *mut c_char {
    boundary(|| {
        if out.is_null() {
            return Err("null lock graph output".into());
        }
        // SAFETY: live graph and readable payload are supplied by the caller.
        let value: Json =
            serde_json::from_str(unsafe { payload.text() }?).map_err(|e| e.to_string())?;
        let id = unsafe { graph(handle) }?.add(&value)?;
        // SAFETY: output was checked before mutation.
        unsafe { output(out, id.0) }
    })
}

/// # Safety
/// Graph and name bytes must be live for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_set_direct(
    handle: *mut IxeLockGraph,
    node: u64,
    name: IxeLockBytes,
    target: u64,
) -> *mut c_char {
    boundary(|| {
        if target == 0 {
            return Err("lock file contains cycle to root node".into());
        }
        // SAFETY: graph and name follow the borrowed-call contract.
        unsafe { graph(handle) }?.set_edge(
            NodeId(node),
            unsafe { name.text() }?.to_owned(),
            Edge::Node(NodeId(target)),
        )
    })
}

/// # Safety
/// Graph, name and path allocations must be live for the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_set_follows(
    handle: *mut IxeLockGraph,
    node: u64,
    name: IxeLockBytes,
    path: IxeLockPathView,
) -> *mut c_char {
    boundary(|| {
        // SAFETY: all borrowed arguments obey the ABI contract.
        unsafe { graph(handle) }?.set_edge(
            NodeId(node),
            unsafe { name.text() }?.to_owned(),
            Edge::Follows(unsafe { path.owned() }?),
        )
    })
}

/// # Safety
/// Graph must be live and `out` writable. Free the returned snapshot once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_inputs(
    handle: *const IxeLockGraph,
    node: u64,
    out: *mut *mut IxeLockInputs,
) -> *mut c_char {
    boundary(|| {
        if out.is_null() {
            return Err("null lock graph output".into());
        }
        // SAFETY: graph is live for the call.
        let locked = unsafe { graph(handle) }?;
        let inputs = locked
            .node(NodeId(node))?
            .inputs
            .iter()
            .map(|(name, edge)| {
                let (target, follows) = match edge {
                    Edge::Node(id) => (Some(*id), Vec::new()),
                    Edge::Follows(path) => (None, path.clone()),
                };
                Input {
                    name: name.clone(),
                    target,
                    follows: IxeLockPath::new(follows),
                }
            })
            .collect();
        // SAFETY: checked writable output receives a newly owned allocation.
        unsafe { output(out, Box::into_raw(Box::new(IxeLockInputs(inputs)))) }
    })
}

/// # Safety
/// `inputs` must be a live snapshot from this API.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_inputs_len(inputs: *const IxeLockInputs) -> usize {
    // SAFETY: live snapshot; reading Vec length cannot panic.
    unsafe { inputs.as_ref() }.map_or(0, |inputs| inputs.0.len())
}

/// # Safety
/// Snapshot must be live and `out` writable. Views expire when snapshot is freed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_inputs_get(
    inputs: *const IxeLockInputs,
    index: usize,
    out: *mut IxeLockEdgeView,
) -> *mut c_char {
    boundary(|| {
        // SAFETY: caller keeps the snapshot alive.
        let input = unsafe { inputs.as_ref() }
            .ok_or("null lock input snapshot")?
            .0
            .get(index)
            .ok_or("lock input index out of bounds")?;
        let view = IxeLockEdgeView {
            name: IxeLockBytes::view(&input.name),
            kind: u8::from(input.target.is_none()),
            target: input.target.map_or(0, |id| id.0),
            follows: input.follows.view(),
        };
        // SAFETY: caller supplies writable storage.
        unsafe { output(out, view) }
    })
}

/// # Safety
/// Graph/path must be live and both output pointers writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_find(
    handle: *const IxeLockGraph,
    path: IxeLockPathView,
    out: *mut u64,
    found: *mut u8,
) -> *mut c_char {
    boundary(|| {
        if out.is_null() || found.is_null() {
            return Err("null lock graph output".into());
        }
        // SAFETY: the caller supplies the live graph and borrowed path.
        let result = unsafe { graph(handle) }?.find(&unsafe { path.owned() }?)?;
        // SAFETY: both outputs were checked before writing.
        unsafe {
            output(out, result.map_or(0, |id| id.0))?;
            output(found, u8::from(result.is_some()))
        }
    })
}

/// # Safety
/// Graph must remain live throughout this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_check(handle: *const IxeLockGraph) -> *mut c_char {
    // SAFETY: forwarded live graph contract.
    boundary(|| unsafe { graph(handle) }?.check())
}

/// # Safety
/// Graph must be live and `out` point to 32 writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_identity(
    handle: *const IxeLockGraph,
    out: *mut u8,
) -> *mut c_char {
    boundary(|| {
        if out.is_null() {
            return Err("null lock graph output".into());
        }
        // SAFETY: the caller keeps graph live throughout the call.
        let digest = unsafe { graph(handle) }?.identity()?;
        // SAFETY: out points to 32 writable bytes, disjoint from the local digest.
        unsafe { ptr::copy_nonoverlapping(digest.as_ptr(), out, digest.len()) };
        Ok(())
    })
}

/// # Safety
/// Graph must be live and `out` writable. Free returned snapshot exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_reachable(
    handle: *const IxeLockGraph,
    out: *mut *mut IxeLockNodes,
) -> *mut c_char {
    boundary(|| {
        if out.is_null() {
            return Err("null lock graph output".into());
        }
        // SAFETY: caller supplies a live graph.
        let nodes = unsafe { graph(handle) }?.reachable()?;
        // SAFETY: checked writable output.
        unsafe { output(out, Box::into_raw(Box::new(IxeLockNodes(nodes)))) }
    })
}

/// # Safety
/// `nodes` must be a live snapshot from this API.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_nodes_len(nodes: *const IxeLockNodes) -> usize {
    // SAFETY: live snapshot; reading Vec length cannot panic.
    unsafe { nodes.as_ref() }.map_or(0, |nodes| nodes.0.len())
}

/// # Safety
/// Snapshot must be live and output writable.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_nodes_get(
    nodes: *const IxeLockNodes,
    index: usize,
    out: *mut u64,
) -> *mut c_char {
    boundary(|| {
        // SAFETY: caller keeps snapshot live.
        let id = unsafe { nodes.as_ref() }
            .ok_or("null lock node snapshot")?
            .0
            .get(index)
            .ok_or("lock node index out of bounds")?;
        // SAFETY: caller supplies writable output.
        unsafe { output(out, id.0) }
    })
}

unsafe fn schedule<'a>(handle: *const IxeLockSchedule) -> Result<MutexGuard<'a, PrefetchSchedule>> {
    // SAFETY: caller keeps the schedule alive for this operation. Shared
    // references plus the mutex permit concurrent completion notifications.
    let handle = unsafe { handle.as_ref() }.ok_or("null lock prefetch schedule")?;
    handle
        .0
        .lock()
        .map_err(|_| "lock prefetch schedule mutex poisoned".into())
}

/// # Safety
/// Graph must be live and out writable. Free the returned schedule once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_prefetch_schedule(
    handle: *const IxeLockGraph,
    out: *mut *mut IxeLockSchedule,
) -> *mut c_char {
    boundary(|| {
        if out.is_null() {
            return Err("null lock schedule output".into());
        }
        // SAFETY: the caller holds the graph alive while its dependency plan is read.
        let locked = unsafe { graph(handle) }?;
        let schedule = PrefetchSchedule::new(&locked)?;
        // SAFETY: checked writable output receives unique ownership.
        unsafe {
            output(
                out,
                Box::into_raw(Box::new(IxeLockSchedule(Mutex::new(schedule)))),
            )
        }
    })
}

/// # Safety
/// Schedule must be live and out writable. Free the returned node snapshot once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_schedule_ready(
    handle: *const IxeLockSchedule,
    out: *mut *mut IxeLockNodes,
) -> *mut c_char {
    boundary(|| {
        if out.is_null() {
            return Err("null lock schedule output".into());
        }
        // SAFETY: live shared handle, synchronized internally.
        let ready = unsafe { schedule(handle) }?.take_ready();
        unsafe { output(out, Box::into_raw(Box::new(IxeLockNodes(ready)))) }
    })
}

/// # Safety
/// Schedule must be live and out writable. Concurrent calls are synchronized.
/// succeeded is 0 or 1. Free the returned ready-node snapshot exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_schedule_complete(
    handle: *const IxeLockSchedule,
    node: u64,
    succeeded: u8,
    out: *mut *mut IxeLockNodes,
) -> *mut c_char {
    boundary(|| {
        if out.is_null() {
            return Err("null lock schedule output".into());
        }
        if succeeded > 1 {
            return Err("invalid lock prefetch success flag".into());
        }
        // SAFETY: live shared handle, synchronized internally.
        let ready = unsafe { schedule(handle) }?.complete(NodeId(node), succeeded == 1)?;
        unsafe { output(out, Box::into_raw(Box::new(IxeLockNodes(ready)))) }
    })
}

/// # Safety
/// Schedule must remain live throughout the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_schedule_check_complete(
    handle: *const IxeLockSchedule,
) -> *mut c_char {
    // SAFETY: forwarded shared live handle contract.
    boundary(|| unsafe { schedule(handle) }?.check_complete())
}

/// # Safety
/// Handle must be null or an owned API allocation, freed once after all calls finish.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_schedule_free(handle: *mut IxeLockSchedule) {
    // SAFETY: exclusive ownership returns after the last operation finishes.
    unsafe { free_handle(handle) };
}

unsafe fn read_json(
    handle: *const IxeLockGraph,
    out: *mut *mut c_char,
    read: impl FnOnce(&Graph) -> Result<Json>,
) -> *mut c_char {
    boundary(|| {
        if out.is_null() {
            return Err("null lock graph output".into());
        }
        // SAFETY: the API caller supplies a live graph and writable output.
        let locked = unsafe { graph(handle) }?;
        let value = read(&locked)?;
        unsafe { output(out, json_string(value)?) }
    })
}

/// # Safety
/// Graph must be live and output writable. Free the resulting string once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_payloads(
    handle: *const IxeLockGraph,
    out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: forwarded live graph and writable output contract.
    unsafe {
        read_json(handle, out, |graph| {
            Ok(Json::Array(
                graph
                    .nodes
                    .iter()
                    .filter(|(id, _)| **id != NodeId(0))
                    .map(|(id, node)| json!({"id": id.0, "payload": node.payload}))
                    .collect(),
            ))
        })
    }
}

/// # Safety
/// Graph must be live and output writable. Free the resulting string once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_serialize(
    handle: *const IxeLockGraph,
    out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: forwarded live graph and writable output contract.
    unsafe { read_json(handle, out, Graph::serialize) }
}

/// # Safety
/// Graph must be live and output writable. Free the resulting string once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_all_inputs(
    handle: *const IxeLockGraph,
    out: *mut *mut c_char,
) -> *mut c_char {
    // SAFETY: forwarded live graph and writable output contract.
    unsafe { read_json(handle, out, Graph::all_inputs) }
}

/// # Safety
/// Source must be readable and output writable. Free returned path exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_parse_path(
    source: IxeLockBytes,
    out: *mut *mut IxeLockPath,
) -> *mut c_char {
    boundary(|| {
        if out.is_null() {
            return Err("null lock graph output".into());
        }
        // SAFETY: caller supplies source bytes and writable output.
        let path = parse_input_path(unsafe { source.text() }?)?;
        unsafe { output(out, Box::into_raw(Box::new(IxeLockPath::new(path)))) }
    })
}

/// # Safety
/// Path must remain live while the returned view is used.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_path_view(path: *const IxeLockPath) -> IxeLockPathView {
    // SAFETY: caller supplies a live path; accessing its vectors cannot panic.
    unsafe { path.as_ref() }.map_or(
        IxeLockPathView {
            data: ptr::null(),
            len: 0,
        },
        IxeLockPath::view,
    )
}

unsafe fn free_handle<T>(handle: *mut T) {
    if !handle.is_null() {
        // SAFETY: API caller transfers ownership back exactly once.
        unsafe { drop(Box::from_raw(handle)) };
    }
}

/// # Safety
/// Handle must be null or an owned API allocation, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_free(handle: *mut IxeLockGraph) {
    // SAFETY: forwarded ownership contract; these destructors cannot panic.
    unsafe { free_handle(handle) };
}

/// # Safety
/// Handle must be null or an owned API allocation, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_inputs_free(handle: *mut IxeLockInputs) {
    // SAFETY: forwarded ownership contract; these destructors cannot panic.
    unsafe { free_handle(handle) };
}

/// # Safety
/// Handle must be null or an owned API allocation, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_nodes_free(handle: *mut IxeLockNodes) {
    // SAFETY: forwarded ownership contract; these destructors cannot panic.
    unsafe { free_handle(handle) };
}

/// # Safety
/// Handle must be null or an owned API allocation, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_path_free(handle: *mut IxeLockPath) {
    // SAFETY: forwarded ownership contract; these destructors cannot panic.
    unsafe { free_handle(handle) };
}

/// # Safety
/// Text must be null or an owned API string, freed exactly once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn ixe_lock_graph_string_free(text: *mut c_char) {
    if !text.is_null() {
        // SAFETY: caller transfers ownership back exactly once; destruction cannot panic.
        unsafe { drop(CString::from_raw(text)) };
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    unsafe fn success(error: *mut c_char) {
        if !error.is_null() {
            // SAFETY: the test owns the API error until freeing it below.
            let message = unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned();
            unsafe { ixe_lock_graph_string_free(error) };
            panic!("{message}");
        }
    }

    #[test]
    fn schedule_and_ready_snapshots_outlive_graph_ownership() {
        // SAFETY: API allocations are freed once; each borrowed handle remains
        // live until its last use, including snapshots after owner destruction.
        unsafe {
            let mut graph = ptr::null_mut();
            success(ixe_lock_graph_new_empty(&mut graph));
            let mut schedule = ptr::null_mut();
            success(ixe_lock_graph_prefetch_schedule(graph, &mut schedule));
            ixe_lock_graph_free(graph);
            let mut ready = ptr::null_mut();
            success(ixe_lock_schedule_ready(schedule, &mut ready));
            let mut root = u64::MAX;
            success(ixe_lock_nodes_get(ready, 0, &mut root));
            assert_eq!(root, 0);
            let mut next = ptr::null_mut();
            success(ixe_lock_schedule_complete(schedule, root, 1, &mut next));
            success(ixe_lock_schedule_check_complete(schedule));
            let mut duplicate = ptr::null_mut();
            let error = ixe_lock_schedule_complete(schedule, root, 1, &mut duplicate);
            assert!(!error.is_null());
            assert!(duplicate.is_null());
            ixe_lock_graph_string_free(error);
            ixe_lock_schedule_free(schedule);
            assert_eq!(ixe_lock_nodes_len(ready), 1);
            assert_eq!(ixe_lock_nodes_len(next), 0);
            ixe_lock_nodes_free(ready);
            ixe_lock_nodes_free(next);
        }
    }

    #[test]
    fn concurrent_parent_completion_releases_shared_child_once() {
        let mut graph = Graph::default();
        let payload = json!({"locked":{},"original":{}});
        let a = graph.add(&payload).expect("a");
        let b = graph.add(&payload).expect("b");
        let child = graph.add(&payload).expect("child");
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
            .set_edge(b, "child".into(), Edge::Node(child))
            .expect("edge");
        let mut plan = PrefetchSchedule::new(&graph).expect("plan");
        plan.take_ready();
        assert_eq!(plan.complete(NodeId(0), true).expect("root"), vec![a, b]);
        let plan = std::sync::Arc::new(IxeLockSchedule(Mutex::new(plan)));
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let workers: Vec<_> = [a, b]
            .into_iter()
            .map(|id| {
                let plan = plan.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    // SAFETY: Arc keeps the shared handle alive; API internally
                    // locks the mutex and returns a separately owned snapshot.
                    unsafe {
                        let mut ready = ptr::null_mut();
                        success(ixe_lock_schedule_complete(
                            std::sync::Arc::as_ptr(&plan),
                            id.0,
                            1,
                            &mut ready,
                        ));
                        let count = ixe_lock_nodes_len(ready);
                        if count == 1 {
                            let mut value = 0;
                            success(ixe_lock_nodes_get(ready, 0, &mut value));
                            assert_eq!(value, child.0);
                        }
                        ixe_lock_nodes_free(ready);
                        count
                    }
                })
            })
            .collect();
        assert_eq!(
            workers
                .into_iter()
                .map(|worker| worker.join().expect("worker"))
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn empty_parse_is_an_error_but_new_empty_is_valid() {
        // SAFETY: local outputs are writable; returned allocations are freed once.
        unsafe {
            let mut graph = ptr::null_mut();
            let error = ixe_lock_graph_parse(IxeLockBytes::view(""), &mut graph);
            assert!(!error.is_null());
            assert!(graph.is_null());
            ixe_lock_graph_string_free(error);
            success(ixe_lock_graph_new_empty(&mut graph));
            success(ixe_lock_graph_check(graph));
            ixe_lock_graph_free(graph);
        }
    }

    #[test]
    fn snapshots_own_paths_and_names_after_graph_mutation_and_free() {
        // SAFETY: every view refers to live local bytes or an owned snapshot;
        // the snapshot deliberately outlives the graph, as guaranteed by the ABI.
        unsafe {
            let mut graph = ptr::null_mut();
            success(ixe_lock_graph_new_empty(&mut graph));
            let mut node = 0;
            success(ixe_lock_graph_add(
                graph,
                IxeLockBytes::view(r#"{"locked":{},"original":{}}"#),
                &mut node,
            ));
            success(ixe_lock_graph_set_direct(
                graph,
                0,
                IxeLockBytes::view("a"),
                node,
            ));
            let path = [IxeLockBytes::view("a")];
            success(ixe_lock_graph_set_follows(
                graph,
                0,
                IxeLockBytes::view("b"),
                IxeLockPathView {
                    data: path.as_ptr(),
                    len: path.len(),
                },
            ));
            let mut snapshot = ptr::null_mut();
            success(ixe_lock_graph_inputs(graph, 0, &mut snapshot));
            success(ixe_lock_graph_set_direct(
                graph,
                0,
                IxeLockBytes::view("b"),
                node,
            ));
            ixe_lock_graph_free(graph);

            assert_eq!(ixe_lock_inputs_len(snapshot), 2);
            let mut view = std::mem::MaybeUninit::uninit();
            success(ixe_lock_inputs_get(snapshot, 1, view.as_mut_ptr()));
            let view = view.assume_init();
            assert_eq!(view.name.text().expect("name"), "b");
            assert_eq!(view.kind, 1);
            assert_eq!(view.follows.owned().expect("path"), vec!["a"]);
            let error = ixe_lock_inputs_get(snapshot, 2, ptr::null_mut());
            assert!(!error.is_null());
            ixe_lock_graph_string_free(error);
            ixe_lock_inputs_free(snapshot);
        }
    }

    #[test]
    fn find_distinguishes_missing_from_root_and_identity_tracks_mutation() {
        // SAFETY: graph, paths and output storage stay live for each ABI call.
        unsafe {
            let mut graph = ptr::null_mut();
            success(ixe_lock_graph_new_empty(&mut graph));
            let mut before = [0; 32];
            success(ixe_lock_graph_identity(graph, before.as_mut_ptr()));
            let mut node = 99;
            let mut found = 99;
            success(ixe_lock_graph_find(
                graph,
                IxeLockPathView {
                    data: ptr::null(),
                    len: 0,
                },
                &mut node,
                &mut found,
            ));
            assert_eq!((node, found), (0, 1));
            let path = [IxeLockBytes::view("missing")];
            success(ixe_lock_graph_find(
                graph,
                IxeLockPathView {
                    data: path.as_ptr(),
                    len: path.len(),
                },
                &mut node,
                &mut found,
            ));
            assert_eq!(found, 0);
            success(ixe_lock_graph_add(
                graph,
                IxeLockBytes::view(r#"{"locked":{},"original":{}}"#),
                &mut node,
            ));
            success(ixe_lock_graph_set_direct(
                graph,
                0,
                IxeLockBytes::view("missing"),
                node,
            ));
            let mut after = [0; 32];
            success(ixe_lock_graph_identity(graph, after.as_mut_ptr()));
            assert_ne!(before, after);
            let mut resolved = 0;
            success(ixe_lock_graph_find(
                graph,
                IxeLockPathView {
                    data: path.as_ptr(),
                    len: path.len(),
                },
                &mut resolved,
                &mut found,
            ));
            assert_eq!(found, 1);
            assert_eq!(resolved, node);
            ixe_lock_graph_free(graph);
        }
    }

    #[test]
    fn malformed_views_and_embedded_nul_do_not_truncate() {
        // SAFETY: null arguments deliberately exercise validated boundary cases;
        // all nonnull pointers reference local live allocations.
        unsafe {
            let mut path = ptr::null_mut();
            let error = ixe_lock_graph_parse_path(IxeLockBytes::view("a\0/b"), &mut path);
            assert!(!error.is_null());
            assert!(path.is_null());
            ixe_lock_graph_string_free(error);
            let mut graph = ptr::null_mut();
            success(ixe_lock_graph_new_empty(&mut graph));
            let error = ixe_lock_graph_set_follows(
                graph,
                0,
                IxeLockBytes::view("a"),
                IxeLockPathView {
                    data: ptr::null(),
                    len: 1,
                },
            );
            assert!(!error.is_null());
            ixe_lock_graph_string_free(error);
            ixe_lock_graph_free(graph);
        }
    }
}
