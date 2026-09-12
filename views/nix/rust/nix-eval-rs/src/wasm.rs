//! `builtins.wasm`: call a function exported by a WebAssembly module with a
//! Nix value, and get a Nix value back.
//!
//! The guest sees Nix values as opaque `u32` handles ([`ValueId`]) and reaches
//! them through the `env` host interface documented in
//! `doc/manual/source/protocols/wasm.md`: `get_int`, `make_string`,
//! `call_function`, `read_file` and the rest. Every one of those may need the
//! evaluator -- forcing a thunk, applying a function, reading a file through
//! the embedder -- and this machine has no host-stack recursion: a builtin
//! never calls back into the VM, it yields what it needs and is stepped again
//! with the answer. So the guest cannot run on the VM's stack while a host
//! call blocks on a thunk.
//!
//! # How a host call reaches the VM
//!
//! The guest runs inside a wasmtime *async* call: `call_async` executes wasm
//! on a fiber, and an async host function that returns `Pending` suspends the
//! whole fiber until the outer future is polled again. The future owning the
//! store is held here, in the [`WasmCall`] continuation, and polled by
//! [`WasmCall::step`] with a no-op waker. A host function posts a [`Request`]
//! into the shared [`Mailbox`] and awaits the [`Answer`]; the poll returns
//! `Pending`; `step` takes the request, does the Nix side of it -- yielding
//! `Force`, `Apply` or `Need` to the VM as many times as that takes -- posts
//! the answer, and polls again, which resumes the guest exactly where it
//! stopped. No thread, no channel, no re-entrancy: the guest's suspended
//! stack is the fiber, and dropping the future frees it.
//!
//! What crosses the boundary is plain data: handles, integers, byte strings.
//! The values themselves stay in [`WasmCall::values`], on this thread, in
//! `Rc`s; the store's data is `Send` because wasmtime's async surface asks
//! for it, and it holds nothing that is not.
//!
//! # What this replaces
//!
//! cppnix's `primops/wasm.cc` ran the guest synchronously and let host
//! functions call `forceValue` and `callFunction` on the C++ stack. Its WASI
//! mode (`_start` + `return_to_nix`, stdout captured as warnings) had no
//! consumer -- the one guest, `packages/ix2nix/wasm`, is a plain `env`
//! module -- and is not carried over: a module importing anything outside
//! `env` is refused by name before instantiation.
//!
//! # Determinism
//!
//! Results of `builtins.wasm` flow into derivations shared across build
//! hosts, so one divergent bit is a divergent store path. NaN payloads and
//! relaxed-SIMD lowering are the two places the Wasm spec leaves
//! implementation-defined; both are pinned deterministic here. Attribute
//! sets are handed to the guest in name order, not symbol-id order, for the
//! same reason: symbol ids depend on what the evaluation interned first.
//!
//! # Where this differs from wasm.cc on purpose
//!
//! An error raised while the VM services a host call keeps its class: a
//! `throw` inside a function the guest applied is still a throw, so
//! `tryEval` catches it exactly as it would outside the guest, and a missing
//! file or a type error stays uncatchable exactly as it would outside. wasm.cc
//! turned every one of them into a trap and rethrew the trap's text as a
//! plain `Error`, which lost the class and the original message. Names
//! crossing the boundary (`get_attr`, `make_attrset`, `make_path`) must be
//! UTF-8: the VM's symbols are `str`, and a guest handing over other bytes
//! gets a trap naming the host call, not a mangled name.

use std::collections::{BTreeMap, HashMap};
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::task::{Context, Poll, Waker};

use wasmtime::{
    Caller, Config, Engine, Extern, Func, Instance, InstancePre, Linker, Memory, Module, Store, Val,
};

use crate::primops_host::Ext;
use crate::primops_pure::{
    Begin, Cont, PathReady, PathStage, apply_rewrites, argv, coerce_for_read, want_attrs,
    want_bool, want_bytes, want_int, want_list, want_text_no_ctx,
};
use crate::task::{NeedPath, Yield};
use crate::value2::{Attrs, NixStr, PathValue, Root, Slot, Sym, Value, type_name};
use crate::vm::{Result, Vm, VmError};

/// A handle the guest holds on a Nix value: an index into
/// [`WasmCall::values`]. `0` is reserved and means "no value" (`get_attr` on
/// a missing attribute), so the table's first slot is never handed out.
pub type ValueId = u32;

/// The handle of the builtin's second argument, the value the entry point is
/// called with. The table is `[reserved, argument]` when the guest starts, so
/// it is always `1`.
const ARGUMENT: ValueId = 1;

/// Type codes of `get_type`, as the protocol document lists them.
mod type_code {
    pub const INT: u32 = 1;
    pub const FLOAT: u32 = 2;
    pub const BOOL: u32 = 3;
    pub const STRING: u32 = 4;
    pub const PATH: u32 = 5;
    pub const NULL: u32 = 6;
    pub const ATTRS: u32 = 7;
    pub const LIST: u32 = 8;
    pub const FUNCTION: u32 = 9;
}

// -- the guest side -----------------------------------------------------------

/// What a host function asks the VM for. Handles and bytes only; the guest
/// never sees a `Value`.
enum Request {
    Warn(String),
    GetType(ValueId),
    MakeInt(i64),
    GetInt(ValueId),
    MakeFloat(f64),
    GetFloat(ValueId),
    MakeString(Vec<u8>),
    CopyString(ValueId),
    MakePath { base: ValueId, relative: String },
    CopyPath(ValueId),
    MakeBool(bool),
    GetBool(ValueId),
    MakeNull,
    MakeList(Vec<ValueId>),
    /// `max_len` travels with the copy requests so the VM allocates member
    /// handles only when the guest's buffer can take them (cppnix allocates
    /// while copying, so a sizing probe leaves nothing behind there either).
    CopyList { list: ValueId, max_len: u32 },
    MakeAttrset(Vec<(String, ValueId)>),
    CopyAttrset { set: ValueId, max_len: u32 },
    CopyAttrname { set: ValueId, index: u32 },
    GetAttr { set: ValueId, name: String },
    CallFunction { function: ValueId, args: Vec<ValueId> },
    MakeApp { function: ValueId, args: Vec<ValueId> },
    ReadFile(ValueId),
}

impl Request {
    /// The handle whose value has to be forced before the request can be
    /// answered, for the requests that look at a value rather than make one.
    fn operand(&self) -> Option<ValueId> {
        match self {
            Request::GetType(id)
            | Request::GetInt(id)
            | Request::GetFloat(id)
            | Request::CopyString(id)
            | Request::CopyPath(id)
            | Request::GetBool(id)
            | Request::CopyList { list: id, .. }
            | Request::CopyAttrset { set: id, .. }
            | Request::ReadFile(id)
            | Request::MakePath { base: id, .. }
            | Request::CopyAttrname { set: id, .. }
            | Request::GetAttr { set: id, .. }
            | Request::CallFunction { function: id, .. } => Some(*id),
            Request::Warn(_)
            | Request::MakeInt(_)
            | Request::MakeFloat(_)
            | Request::MakeString(_)
            | Request::MakeBool(_)
            | Request::MakeNull
            | Request::MakeList(_)
            | Request::MakeAttrset(_)
            | Request::MakeApp { .. } => None,
        }
    }
}

/// What the VM answers. One variant per shape the protocol returns; a host
/// function that gets the wrong shape traps, which is a bug here and not in
/// the guest.
enum Answer {
    Unit,
    Id(ValueId),
    Type(u32),
    /// The size of a list or set the guest's buffer cannot hold; no handles
    /// were allocated for its members.
    Count(u32),
    Int(i64),
    Float(f64),
    Bool(bool),
    Bytes(Vec<u8>),
    Ids(Vec<ValueId>),
    /// `(value, name length)` per attribute, in name order.
    Members(Vec<(ValueId, u32)>),
}

#[derive(Default)]
struct Mailbox {
    request: Option<Request>,
    answer: Option<Answer>,
    /// The export the guest is executing, for attributing warnings and
    /// failures: `nix_wasm_init_v1` first, then the entry point.
    running: String,
}

type Shared = Arc<Mutex<Mailbox>>;

fn lock(mailbox: &Shared) -> MutexGuard<'_, Mailbox> {
    mailbox.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The store's data: the mailbox shared with the [`WasmCall`], and the guest's
/// exported memory once instantiation has found it.
pub struct Guest {
    mailbox: Shared,
    memory: Option<Memory>,
}

/// Resolves when the VM has posted an answer. Its first poll always returns
/// `Pending`, because the request it follows was posted a moment ago and the
/// VM has not run since; that `Pending` is what suspends the fiber.
struct Reply(Shared);

impl Future for Reply {
    type Output = Answer;

    fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Answer> {
        match lock(&self.0).answer.take() {
            Some(answer) => Poll::Ready(answer),
            None => Poll::Pending,
        }
    }
}

async fn ask(caller: &Caller<'_, Guest>, request: Request) -> Answer {
    let mailbox = Arc::clone(&caller.data().mailbox);
    lock(&mailbox).request = Some(request);
    Reply(mailbox).await
}

fn trap(message: impl Into<String>) -> wasmtime::Error {
    wasmtime::Error::msg(message.into())
}

fn wrong_shape() -> wasmtime::Error {
    trap("internal: the evaluator answered a Wasm host call with the wrong shape")
}

fn id_of(answer: Answer) -> wasmtime::Result<ValueId> {
    match answer {
        Answer::Id(id) => Ok(id),
        _ => Err(wrong_shape()),
    }
}

fn bytes_of(answer: Answer) -> wasmtime::Result<Vec<u8>> {
    match answer {
        Answer::Bytes(bytes) => Ok(bytes),
        _ => Err(wrong_shape()),
    }
}

fn out_of_bounds() -> wasmtime::Error {
    trap("Wasm memory access out of bounds")
}

fn span(ptr: u32, len: u32) -> wasmtime::Result<std::ops::Range<usize>> {
    let start = usize::try_from(ptr).map_err(|_| out_of_bounds())?;
    let end = start
        .checked_add(usize::try_from(len).map_err(|_| out_of_bounds())?)
        .ok_or_else(out_of_bounds)?;
    Ok(start..end)
}

fn memory(caller: &Caller<'_, Guest>) -> wasmtime::Result<Memory> {
    caller
        .data()
        .memory
        .ok_or_else(|| trap("internal: Wasm host call before the guest's memory was found"))
}

fn read_guest(caller: &Caller<'_, Guest>, ptr: u32, len: u32) -> wasmtime::Result<Vec<u8>> {
    let range = span(ptr, len)?;
    memory(caller)?
        .data(caller)
        .get(range)
        .map(<[u8]>::to_vec)
        .ok_or_else(out_of_bounds)
}

/// A name crossing the boundary; `what` is the host call, so the trap says
/// which one got the bytes.
fn read_guest_str(
    caller: &Caller<'_, Guest>,
    ptr: u32,
    len: u32,
    what: &str,
) -> wasmtime::Result<String> {
    String::from_utf8(read_guest(caller, ptr, len)?)
        .map_err(|_| trap(format!("{what}: Wasm passed a name that is not UTF-8")))
}

/// `len` little-endian `u32`s at `ptr`: how the guest passes handle arrays.
fn read_guest_ids(caller: &Caller<'_, Guest>, ptr: u32, len: u32) -> wasmtime::Result<Vec<ValueId>> {
    let bytes = read_guest(caller, ptr, len.checked_mul(4).ok_or_else(out_of_bounds)?)?;
    bytes
        .chunks_exact(4)
        .map(|chunk| {
            <[u8; 4]>::try_from(chunk)
                .map(u32::from_le_bytes)
                .map_err(|_| out_of_bounds())
        })
        .collect()
}

fn write_guest(caller: &mut Caller<'_, Guest>, ptr: u32, bytes: &[u8]) -> wasmtime::Result<()> {
    let len = u32::try_from(bytes.len()).map_err(|_| out_of_bounds())?;
    let range = span(ptr, len)?;
    memory(caller)?
        .data_mut(caller)
        .get_mut(range)
        .ok_or_else(out_of_bounds)?
        .copy_from_slice(bytes);
    Ok(())
}

fn write_guest_u32s(caller: &mut Caller<'_, Guest>, ptr: u32, words: &[u32]) -> wasmtime::Result<()> {
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
    write_guest(caller, ptr, &bytes)
}

/// The guest's whole declared buffer must lie in memory before anything is
/// written into part of it: a `copy_*` that fits is still a bug in the guest
/// if the buffer it named runs past the end.
fn check_buffer(caller: &Caller<'_, Guest>, ptr: u32, len: u32) -> wasmtime::Result<()> {
    let range = span(ptr, len)?;
    if range.end > memory(caller)?.data(caller).len() {
        return Err(out_of_bounds());
    }
    Ok(())
}

fn guest_len(n: usize) -> wasmtime::Result<u32> {
    u32::try_from(n).map_err(|_| trap("value is too large to hand to Wasm"))
}

/// The `env` host interface. Each function reads its arguments out of guest
/// memory, asks the VM, and writes the answer back; nothing here touches a
/// `Value`. The names and signatures are the protocol document's.
fn link(linker: &mut Linker<Guest>) -> wasmtime::Result<()> {
    linker.func_wrap_async("env", "panic", |caller: Caller<'_, Guest>, (ptr, len): (u32, u32)| {
        Box::new(async move {
            let text = read_guest(&caller, ptr, len)?;
            Err::<(), _>(trap(format!("Wasm panic: {}", String::from_utf8_lossy(&text))))
        })
    })?;
    linker.func_wrap_async("env", "warn", |caller: Caller<'_, Guest>, (ptr, len): (u32, u32)| {
        Box::new(async move {
            let text = String::from_utf8_lossy(&read_guest(&caller, ptr, len)?).into_owned();
            match ask(&caller, Request::Warn(text)).await {
                Answer::Unit => Ok(()),
                _ => Err(wrong_shape()),
            }
        })
    })?;
    linker.func_wrap_async("env", "get_type", |caller: Caller<'_, Guest>, (id,): (u32,)| {
        Box::new(async move {
            match ask(&caller, Request::GetType(id)).await {
                Answer::Type(code) => Ok(code),
                _ => Err(wrong_shape()),
            }
        })
    })?;
    linker.func_wrap_async("env", "make_int", |caller: Caller<'_, Guest>, (n,): (i64,)| {
        Box::new(async move { id_of(ask(&caller, Request::MakeInt(n)).await) })
    })?;
    linker.func_wrap_async("env", "get_int", |caller: Caller<'_, Guest>, (id,): (u32,)| {
        Box::new(async move {
            match ask(&caller, Request::GetInt(id)).await {
                Answer::Int(n) => Ok(n),
                _ => Err(wrong_shape()),
            }
        })
    })?;
    linker.func_wrap_async("env", "make_float", |caller: Caller<'_, Guest>, (x,): (f64,)| {
        Box::new(async move { id_of(ask(&caller, Request::MakeFloat(x)).await) })
    })?;
    linker.func_wrap_async("env", "get_float", |caller: Caller<'_, Guest>, (id,): (u32,)| {
        Box::new(async move {
            match ask(&caller, Request::GetFloat(id)).await {
                Answer::Float(x) => Ok(x),
                _ => Err(wrong_shape()),
            }
        })
    })?;
    linker.func_wrap_async(
        "env",
        "make_string",
        |caller: Caller<'_, Guest>, (ptr, len): (u32, u32)| {
            Box::new(async move {
                let bytes = read_guest(&caller, ptr, len)?;
                id_of(ask(&caller, Request::MakeString(bytes)).await)
            })
        },
    )?;
    // The `copy_*` family shares one contract: answer the size, and write
    // only when the caller's buffer holds it. The guest asks twice, once to
    // size and once to copy; the second answer is what fills the buffer.
    linker.func_wrap_async(
        "env",
        "copy_string",
        |mut caller: Caller<'_, Guest>, (id, ptr, max_len): (u32, u32, u32)| {
            Box::new(async move {
                let bytes = bytes_of(ask(&caller, Request::CopyString(id)).await)?;
                let len = guest_len(bytes.len())?;
                if len <= max_len {
                    check_buffer(&caller, ptr, max_len)?;
                    write_guest(&mut caller, ptr, &bytes)?;
                }
                Ok(len)
            })
        },
    )?;
    linker.func_wrap_async(
        "env",
        "make_path",
        |caller: Caller<'_, Guest>, (base, ptr, len): (u32, u32, u32)| {
            Box::new(async move {
                let relative = read_guest_str(&caller, ptr, len, "make_path")?;
                id_of(ask(&caller, Request::MakePath { base, relative }).await)
            })
        },
    )?;
    linker.func_wrap_async(
        "env",
        "copy_path",
        |mut caller: Caller<'_, Guest>, (id, ptr, max_len): (u32, u32, u32)| {
            Box::new(async move {
                let bytes = bytes_of(ask(&caller, Request::CopyPath(id)).await)?;
                let len = guest_len(bytes.len())?;
                if len <= max_len {
                    check_buffer(&caller, ptr, max_len)?;
                    write_guest(&mut caller, ptr, &bytes)?;
                }
                Ok(len)
            })
        },
    )?;
    linker.func_wrap_async("env", "make_bool", |caller: Caller<'_, Guest>, (b,): (i32,)| {
        Box::new(async move { id_of(ask(&caller, Request::MakeBool(b != 0)).await) })
    })?;
    linker.func_wrap_async("env", "get_bool", |caller: Caller<'_, Guest>, (id,): (u32,)| {
        Box::new(async move {
            match ask(&caller, Request::GetBool(id)).await {
                Answer::Bool(b) => Ok(i32::from(b)),
                _ => Err(wrong_shape()),
            }
        })
    })?;
    linker.func_wrap_async("env", "make_null", |caller: Caller<'_, Guest>, (): ()| {
        Box::new(async move { id_of(ask(&caller, Request::MakeNull).await) })
    })?;
    linker.func_wrap_async(
        "env",
        "make_list",
        |caller: Caller<'_, Guest>, (ptr, len): (u32, u32)| {
            Box::new(async move {
                let ids = read_guest_ids(&caller, ptr, len)?;
                id_of(ask(&caller, Request::MakeList(ids)).await)
            })
        },
    )?;
    linker.func_wrap_async(
        "env",
        "copy_list",
        |mut caller: Caller<'_, Guest>, (id, ptr, max_len): (u32, u32, u32)| {
            Box::new(async move {
                match ask(&caller, Request::CopyList { list: id, max_len }).await {
                    Answer::Ids(ids) => {
                        check_buffer(&caller, ptr, max_len.checked_mul(4).ok_or_else(out_of_bounds)?)?;
                        write_guest_u32s(&mut caller, ptr, &ids)?;
                        guest_len(ids.len())
                    }
                    Answer::Count(total) => Ok(total),
                    _ => Err(wrong_shape()),
                }
            })
        },
    )?;
    // `len` records of `{ name_ptr: u32, name_len: u32, value: ValueId }`.
    linker.func_wrap_async(
        "env",
        "make_attrset",
        |caller: Caller<'_, Guest>, (ptr, len): (u32, u32)| {
            Box::new(async move {
                let words = read_guest_ids(&caller, ptr, len.checked_mul(3).ok_or_else(out_of_bounds)?)?;
                let mut attrs = Vec::with_capacity(words.len() / 3);
                for record in words.chunks_exact(3) {
                    let (Some(name_ptr), Some(name_len), Some(value)) =
                        (record.first(), record.get(1), record.get(2))
                    else {
                        return Err(out_of_bounds());
                    };
                    attrs.push((read_guest_str(&caller, *name_ptr, *name_len, "make_attrset")?, *value));
                }
                id_of(ask(&caller, Request::MakeAttrset(attrs)).await)
            })
        },
    )?;
    // Writes `{ value: ValueId, name_len: u32 }` per attribute, in name
    // order; the guest fetches each name with `copy_attrname` by index.
    linker.func_wrap_async(
        "env",
        "copy_attrset",
        |mut caller: Caller<'_, Guest>, (id, ptr, max_len): (u32, u32, u32)| {
            Box::new(async move {
                match ask(&caller, Request::CopyAttrset { set: id, max_len }).await {
                    Answer::Members(members) => {
                        check_buffer(&caller, ptr, max_len.checked_mul(8).ok_or_else(out_of_bounds)?)?;
                        let words: Vec<u32> = members
                            .iter()
                            .flat_map(|(value, name_len)| [*value, *name_len])
                            .collect();
                        write_guest_u32s(&mut caller, ptr, &words)?;
                        guest_len(members.len())
                    }
                    Answer::Count(total) => Ok(total),
                    _ => Err(wrong_shape()),
                }
            })
        },
    )?;
    linker.func_wrap_async(
        "env",
        "copy_attrname",
        |mut caller: Caller<'_, Guest>, (set, index, ptr, len): (u32, u32, u32, u32)| {
            Box::new(async move {
                let name = bytes_of(ask(&caller, Request::CopyAttrname { set, index }).await)?;
                if guest_len(name.len())? != len {
                    return Err(trap(
                        "copy_attrname: buffer length does not match attribute name length",
                    ));
                }
                write_guest(&mut caller, ptr, &name)
            })
        },
    )?;
    linker.func_wrap_async(
        "env",
        "get_attr",
        |caller: Caller<'_, Guest>, (set, ptr, len): (u32, u32, u32)| {
            Box::new(async move {
                let name = read_guest_str(&caller, ptr, len, "get_attr")?;
                id_of(ask(&caller, Request::GetAttr { set, name }).await)
            })
        },
    )?;
    linker.func_wrap_async(
        "env",
        "call_function",
        |caller: Caller<'_, Guest>, (function, ptr, len): (u32, u32, u32)| {
            Box::new(async move {
                let args = read_guest_ids(&caller, ptr, len)?;
                id_of(ask(&caller, Request::CallFunction { function, args }).await)
            })
        },
    )?;
    linker.func_wrap_async(
        "env",
        "make_app",
        |caller: Caller<'_, Guest>, (function, ptr, len): (u32, u32, u32)| {
            Box::new(async move {
                let args = read_guest_ids(&caller, ptr, len)?;
                id_of(ask(&caller, Request::MakeApp { function, args }).await)
            })
        },
    )?;
    linker.func_wrap_async(
        "env",
        "read_file",
        |mut caller: Caller<'_, Guest>, (path, ptr, max_len): (u32, u32, u32)| {
            Box::new(async move {
                let bytes = bytes_of(ask(&caller, Request::ReadFile(path)).await)?;
                let len = u32::try_from(bytes.len())
                    .map_err(|_| trap("file is too large to process in Wasm"))?;
                if len <= max_len {
                    check_buffer(&caller, ptr, max_len)?;
                    write_guest(&mut caller, ptr, &bytes)?;
                }
                Ok(len)
            })
        },
    )?;
    Ok(())
}

/// One engine per process. Compilation settings are part of the result's
/// identity (see the module doc on determinism), so they are fixed here and
/// nowhere else.
fn engine() -> std::result::Result<&'static Engine, String> {
    static ENGINE: OnceLock<std::result::Result<Engine, String>> = OnceLock::new();
    ENGINE
        .get_or_init(|| {
            let mut config = Config::new();
            config.cranelift_nan_canonicalization(true);
            config.relaxed_simd_deterministic(true);
            config.memory_init_cow(true);
            Engine::new(&config).map_err(|e| format!("{e:#}"))
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// The compiled, linked module for these bytes, compiled once per process
/// per distinct content: the converter is the same file for every `.ix`
/// module an evaluation imports, and compiling it is the expensive part.
///
/// Keyed by the bytes and not the path, because two paths with one content
/// are one module and one path with two contents (a rebuilt converter) is
/// two.
fn instance_pre(engine: &Engine, bytes: &[u8]) -> wasmtime::Result<InstancePre<Guest>> {
    static CACHE: OnceLock<Mutex<HashMap<[u8; 32], InstancePre<Guest>>>> = OnceLock::new();
    let cache = CACHE.get_or_init(Mutex::default);
    let key = *blake3::hash(bytes).as_bytes();
    if let Some(pre) = cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get(&key)
    {
        return Ok(pre.clone());
    }
    let module = Module::new(engine, bytes)?;
    // Fail closed on anything outside the `env` interface, by name, before
    // the linker reports it as an unresolved import: the likely case is a
    // WASI module, and "WASI is not provided" is the useful diagnosis.
    for import in module.imports() {
        if import.module() != "env" {
            return Err(trap(format!(
                "Wasm module imports '{}.{}', and builtins.wasm provides only the `env` host \
                 interface (WASI modules are not supported)",
                import.module(),
                import.name()
            )));
        }
    }
    let mut linker = Linker::new(engine);
    link(&mut linker)?;
    let pre = linker.instantiate_pre(&module)?;
    cache
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(key, pre.clone());
    Ok(pre)
}

/// `make_path`: `CanonPath(relative, base.path)` within the base's accessor.
/// Absolute stands alone, anything else hangs off the base; the result lives
/// under the same root, whose `/` for a mounted root is the mount point the
/// guest never sees, and `..` cannot climb above it (`PathValue::normalized`).
fn joined_path(base: &PathValue, relative: &str) -> Result<PathValue> {
    if relative.contains('\0') {
        return Err(VmError::eval("make_path: the path contains a NUL byte"));
    }
    let local = base.accessor_path();
    let joined = if relative.starts_with('/') {
        relative.to_owned()
    } else if local == "/" {
        format!("/{relative}")
    } else {
        format!("{local}/{relative}")
    };
    let full = match &base.root {
        Root::Ambient => joined,
        Root::Mounted(mount_point) if joined == "/" => mount_point.to_string(),
        Root::Mounted(mount_point) => format!("{mount_point}{joined}"),
    };
    Ok(PathValue::normalized(base.root.clone(), &full))
}

/// The named export as a function: "not exported" and "exported, but not a
/// function" are different guest bugs and read differently.
fn exported_function(
    instance: &Instance,
    store: &mut Store<Guest>,
    name: &str,
) -> wasmtime::Result<Func> {
    match instance.get_export(&mut *store, name) {
        Some(Extern::Func(f)) => Ok(f),
        Some(_) => Err(trap(format!("Wasm module's export '{name}' is not a function"))),
        None => Err(trap(format!("Wasm module does not export '{name}'"))),
    }
}

/// The whole guest run as one future: instantiate, `nix_wasm_init_v1`, then
/// the entry point with the argument's handle. Owns the store, so dropping
/// the future -- because the evaluation failed elsewhere -- frees the fiber
/// and the instance with it.
async fn run(
    pre: InstancePre<Guest>,
    mut store: Store<Guest>,
    function: String,
) -> wasmtime::Result<ValueId> {
    let instance = pre.instantiate_async(&mut store).await?;
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| trap("Wasm module does not export 'memory'"))?;
    store.data_mut().memory = Some(memory);
    // `running` is set before each lookup so a missing export is attributed
    // to itself, not to whatever ran before it.
    lock(&store.data().mailbox).running = "nix_wasm_init_v1".to_owned();
    let init = exported_function(&instance, &mut store, "nix_wasm_init_v1")?;
    init.call_async(&mut store, &[], &mut []).await?;
    lock(&store.data().mailbox).running.clone_from(&function);
    let entry = exported_function(&instance, &mut store, &function)?;
    let mut results = [Val::I32(0)];
    entry
        .call_async(&mut store, &[Val::I32(ARGUMENT as i32)], &mut results)
        .await?;
    match results {
        [Val::I32(id)] => Ok(id as u32),
        _ => Err(trap(format!(
            "Wasm function '{function}' did not return exactly one i32 value"
        ))),
    }
}

// -- the VM side ---------------------------------------------------------------

/// Reading a path-family value's bytes: force, coerce (which may run a
/// `__toString`), realise its context, ask for the contents. The same walk
/// `builtins.hashFile` makes, used twice here: for the module itself and for
/// the guest's `read_file`.
struct PathRead {
    slot: Slot,
    stage: ReadStage,
}

enum ReadStage {
    Force,
    Coerce(PathStage),
    Realising(Rc<PathValue>),
    Asked(Rc<PathValue>),
}

enum ReadStep {
    Yield(Yield),
    Bytes(Rc<[u8]>, Rc<PathValue>),
}

impl PathRead {
    fn forced(slot: Slot) -> Self {
        PathRead {
            slot,
            stage: ReadStage::Coerce(PathStage::Value),
        }
    }

    fn step(&mut self, incoming: Option<Value>) -> Result<ReadStep> {
        match &mut self.stage {
            ReadStage::Force => {
                self.stage = ReadStage::Coerce(PathStage::Value);
                Ok(ReadStep::Yield(Yield::Force(self.slot.clone())))
            }
            ReadStage::Coerce(stage) => {
                match coerce_for_read(std::slice::from_ref(&self.slot), 0, stage, incoming)? {
                    PathReady::Run(y) => Ok(ReadStep::Yield(y)),
                    PathReady::Realise(path, context) => {
                        self.stage = ReadStage::Realising(path);
                        Ok(ReadStep::Yield(Yield::Need(NeedPath::Realise(context))))
                    }
                    PathReady::Ready(path) => Ok(self.ask(path)),
                }
            }
            ReadStage::Realising(path) => {
                let path = apply_rewrites(Rc::clone(path), incoming)?;
                Ok(self.ask(path))
            }
            ReadStage::Asked(path) => {
                let contents =
                    incoming.ok_or_else(|| VmError::eval("internal: file contents answer lost"))?;
                Ok(ReadStep::Bytes(want_bytes(&contents)?, Rc::clone(path)))
            }
        }
    }

    fn ask(&mut self, path: Rc<PathValue>) -> ReadStep {
        self.stage = ReadStage::Asked(Rc::clone(&path));
        ReadStep::Yield(Yield::Need(NeedPath::Contents(path)))
    }
}

/// Where the continuation is between steps.
enum Phase {
    /// Reading the module bytes named by `config.path`.
    ModulePath,
    /// Module in hand; `config.function` has been yielded for forcing.
    Function,
    /// The guest is running (or suspended in a host call).
    Running,
    /// The guest returned; its result handle has been yielded for forcing.
    Result,
}

/// What the VM was asked for on the guest's behalf, so the answer that comes
/// back as `incoming` can be turned into an [`Answer`].
enum Awaiting {
    /// `Force` on the request's operand; answer the request with the value.
    Forced(Request),
    /// `Apply` in progress; `remaining` holds the arguments still to apply,
    /// last first.
    Applying { remaining: Vec<Slot> },
    /// A `Warn` line is out.
    Warned,
    /// The guest's `read_file`, mid-walk.
    Reading(PathRead),
}

enum Handled {
    Answer(Answer),
    Yield(Yield, Awaiting),
}

/// The `builtins.wasm` continuation. See the module doc for the shape.
pub struct WasmCall {
    /// The handle table. Index 0 is the reserved "no value" and holds a
    /// placeholder no guest can name.
    values: Vec<Slot>,
    /// Name order of each attribute set the guest has enumerated, by handle,
    /// so `copy_attrset` and every following `copy_attrname` agree.
    attr_orders: HashMap<ValueId, Rc<[Sym]>>,
    module_path: PathRead,
    function_slot: Slot,
    module: Option<(Rc<[u8]>, Rc<PathValue>)>,
    phase: Phase,
    mailbox: Shared,
    guest: Option<Pin<Box<dyn Future<Output = wasmtime::Result<ValueId>>>>>,
    awaiting: Option<Awaiting>,
}

/// `builtins.wasm config arg` (`primops/wasm.cc`, `prim_wasm`): `config` is
/// forced by the machine (it is in the strict list); `path` and `function`
/// are forced here, in that order, and `arg` is never forced by this side --
/// the guest decides what it looks at.
pub fn bi_wasm(vm: &mut Vm, args: &[Slot]) -> Result<Begin> {
    let config = want_attrs(&argv(args, 0)?)?;
    let path_sym = vm.intern("path");
    let function_sym = vm.intern("function");
    if let Some(unknown) = config
        .keys()
        .find(|sym| **sym != path_sym && **sym != function_sym)
    {
        return Err(VmError::eval(format!(
            "unknown attribute '{}' in first argument to `builtins.wasm`",
            vm.sym_name(*unknown)
        )));
    }
    let path = config.get(&path_sym).cloned().ok_or_else(|| {
        VmError::eval("missing required 'path' attribute in first argument to `builtins.wasm`")
    })?;
    let function = config.get(&function_sym).cloned().ok_or_else(|| {
        VmError::eval("missing required 'function' attribute in first argument to `builtins.wasm`")
    })?;
    let argument = args
        .get(1)
        .cloned()
        .ok_or_else(|| VmError::eval("internal: builtins.wasm lost its argument"))?;
    Ok(Begin::Cont(Cont::Ext(Ext::Wasm(Box::new(WasmCall {
        values: vec![Slot::value(Value::Null), argument],
        attr_orders: HashMap::new(),
        module_path: PathRead {
            slot: path,
            stage: ReadStage::Force,
        },
        function_slot: function,
        module: None,
        phase: Phase::ModulePath,
        mailbox: Shared::default(),
        guest: None,
        awaiting: None,
    })))))
}

impl WasmCall {
    pub fn step(&mut self, vm: &mut Vm, mut incoming: Option<Value>) -> Result<Yield> {
        loop {
            match self.phase {
                Phase::ModulePath => match self.module_path.step(incoming.take())? {
                    ReadStep::Yield(y) => return Ok(y),
                    ReadStep::Bytes(bytes, path) => {
                        self.module = Some((bytes, path));
                        self.phase = Phase::Function;
                        return Ok(Yield::Force(self.function_slot.clone()));
                    }
                },
                Phase::Function => {
                    let name = incoming
                        .take()
                        .ok_or_else(|| VmError::eval("internal: builtins.wasm lost 'function'"))?;
                    let function = want_text_no_ctx(&name)?;
                    let (bytes, path) = self
                        .module
                        .clone()
                        .ok_or_else(|| VmError::eval("internal: builtins.wasm lost its module"))?;
                    let engine = engine().map_err(VmError::eval)?;
                    let pre = instance_pre(engine, &bytes).map_err(|e| {
                        VmError::eval(format!("while loading the Wasm module '{path}': {e:#}"))
                    })?;
                    let store = Store::new(
                        engine,
                        Guest {
                            mailbox: Arc::clone(&self.mailbox),
                            memory: None,
                        },
                    );
                    self.guest = Some(Box::pin(run(pre, store, function)));
                    self.phase = Phase::Running;
                }
                Phase::Running => {
                    if let Some(awaiting) = self.awaiting.take() {
                        match self.resolve(vm, awaiting, incoming.take())? {
                            Handled::Answer(answer) => lock(&self.mailbox).answer = Some(answer),
                            Handled::Yield(y, next) => {
                                self.awaiting = Some(next);
                                return Ok(y);
                            }
                        }
                    }
                    let guest = self
                        .guest
                        .as_mut()
                        .ok_or_else(|| VmError::eval("internal: builtins.wasm lost its guest"))?;
                    let mut cx = Context::from_waker(Waker::noop());
                    match guest.as_mut().poll(&mut cx) {
                        Poll::Ready(Ok(id)) => {
                            self.guest = None;
                            let result = self.slot(id)?;
                            self.phase = Phase::Result;
                            return Ok(Yield::Force(result));
                        }
                        Poll::Ready(Err(e)) => {
                            self.guest = None;
                            return Err(self.failed(&e));
                        }
                        Poll::Pending => {
                            let request = lock(&self.mailbox).request.take().ok_or_else(|| {
                                VmError::eval("internal: the Wasm guest suspended without a request")
                            })?;
                            match self.handle(vm, request)? {
                                Handled::Answer(answer) => {
                                    lock(&self.mailbox).answer = Some(answer);
                                }
                                Handled::Yield(y, next) => {
                                    self.awaiting = Some(next);
                                    return Ok(y);
                                }
                            }
                        }
                    }
                }
                Phase::Result => {
                    return Ok(Yield::Done(incoming.take().ok_or_else(|| {
                        VmError::eval("internal: builtins.wasm lost its result")
                    })?));
                }
            }
        }
    }

    fn module_name(&self) -> String {
        self.module
            .as_ref()
            .map(|(_, path)| path.to_string())
            .unwrap_or_default()
    }

    fn failed(&self, e: &wasmtime::Error) -> VmError {
        let running = lock(&self.mailbox).running.clone();
        VmError::eval(format!(
            "{e:#}\n\n  … while executing the Wasm function '{running}' from '{}'",
            self.module_name()
        ))
    }

    fn slot(&self, id: ValueId) -> Result<Slot> {
        match id {
            0 => Err(VmError::eval("invalid ValueId 0")),
            _ => self
                .values
                .get(usize::try_from(id).map_err(|_| VmError::eval("invalid ValueId"))?)
                .cloned()
                .ok_or_else(|| VmError::eval(format!("invalid ValueId {id}"))),
        }
    }

    fn slots(&self, ids: &[ValueId]) -> Result<Vec<Slot>> {
        ids.iter().map(|id| self.slot(*id)).collect()
    }

    fn add_slot(&mut self, slot: Slot) -> Result<ValueId> {
        let id = u32::try_from(self.values.len())
            .map_err(|_| VmError::eval("too many values handed to Wasm"))?;
        self.values.push(slot);
        Ok(id)
    }

    fn add(&mut self, value: Value) -> Result<Answer> {
        Ok(Answer::Id(self.add_slot(Slot::value(value))?))
    }

    /// The attribute names of the set behind `id`, in name order, computed
    /// once per handle.
    fn order(&mut self, vm: &Vm, id: ValueId, attrs: &Attrs) -> Rc<[Sym]> {
        if let Some(order) = self.attr_orders.get(&id) {
            return Rc::clone(order);
        }
        let mut syms: Vec<Sym> = attrs.keys().copied().collect();
        syms.sort_by(|a, b| vm.sym_name(*a).cmp(vm.sym_name(*b)));
        let order: Rc<[Sym]> = syms.into();
        self.attr_orders.insert(id, Rc::clone(&order));
        order
    }

    /// A fresh request from the guest. Constructors are answered on the spot;
    /// inspections need their operand forced first, which is a yield unless
    /// the value is already there.
    fn handle(&mut self, vm: &mut Vm, request: Request) -> Result<Handled> {
        let Some(operand) = request.operand() else {
            return self.construct(vm, request);
        };
        let slot = self.slot(operand)?;
        match slot.peek() {
            Some(value) => self.inspect(vm, request, value),
            None => Ok(Handled::Yield(Yield::Force(slot), Awaiting::Forced(request))),
        }
    }

    fn construct(&mut self, vm: &mut Vm, request: Request) -> Result<Handled> {
        let answer = match request {
            Request::Warn(message) => {
                let running = lock(&self.mailbox).running.clone();
                let line = format!(
                    "'{}' function '{running}': {message}",
                    self.module
                        .as_ref()
                        .and_then(|(_, path)| path.path.rsplit('/').next().map(str::to_owned))
                        .unwrap_or_default()
                );
                return Ok(Handled::Yield(
                    Yield::Need(NeedPath::Warn(line)),
                    Awaiting::Warned,
                ));
            }
            Request::MakeInt(n) => self.add(Value::Int(n))?,
            Request::MakeFloat(x) => self.add(Value::Float(x))?,
            Request::MakeString(bytes) => self.add(Value::Str(NixStr::from(bytes.as_slice())))?,
            Request::MakeBool(b) => self.add(Value::Bool(b))?,
            Request::MakeNull => self.add(Value::Null)?,
            Request::MakeList(ids) => {
                let items = self.slots(&ids)?;
                self.add(Value::list(items))?
            }
            Request::MakeAttrset(attrs) => {
                let mut map = BTreeMap::new();
                for (name, id) in attrs {
                    let slot = self.slot(id)?;
                    map.insert(vm.intern(&name), slot);
                }
                self.add(Value::Attrs(Rc::new(Attrs::new(map))))?
            }
            // cppnix's `mkApp` chain, unforced on both sides: the guest is
            // building a thunk, not calling anything.
            Request::MakeApp { function, args } => {
                if args.is_empty() {
                    Answer::Id(function)
                } else {
                    let f = self.slot(function)?;
                    let args = self.slots(&args)?;
                    Answer::Id(self.add_slot(Slot::pending(f, args))?)
                }
            }
            other => {
                debug_assert!(other.operand().is_some());
                return Err(VmError::eval(
                    "internal: an inspecting Wasm request reached the constructor path",
                ));
            }
        };
        Ok(Handled::Answer(answer))
    }

    /// A request whose operand is now the forced `value`.
    fn inspect(&mut self, vm: &mut Vm, request: Request, value: Value) -> Result<Handled> {
        let answer = match request {
            Request::GetType(_) => Answer::Type(match value {
                Value::Int(_) => type_code::INT,
                Value::Float(_) => type_code::FLOAT,
                Value::Bool(_) => type_code::BOOL,
                Value::Str(_) => type_code::STRING,
                Value::Path(_) => type_code::PATH,
                Value::Null => type_code::NULL,
                Value::Attrs(_) => type_code::ATTRS,
                Value::List(_) => type_code::LIST,
                Value::Closure(_) | Value::Builtin(_) => type_code::FUNCTION,
            }),
            Request::GetInt(_) => Answer::Int(want_int(&value)?),
            Request::GetFloat(_) => Answer::Float(match value {
                Value::Float(x) => x,
                // cppnix's `forceFloat` widens an integer.
                Value::Int(n) => n as f64,
                other => {
                    return Err(VmError::eval(format!(
                        "expected a float but found {}: {other}",
                        type_name(&other)
                    )));
                }
            }),
            Request::CopyString(_) => Answer::Bytes(want_bytes(&value)?.to_vec()),
            Request::MakePath { relative, .. } => {
                let Value::Path(base) = &value else {
                    return Err(VmError::eval("make_path expects a path value"));
                };
                self.add(Value::Path(Rc::new(joined_path(base, &relative)?)))?
            }
            Request::CopyPath(_) => {
                let Value::Path(path) = &value else {
                    return Err(VmError::eval("copy_path expects a path value"));
                };
                // cppnix's `path().path.abs()`: the path within its accessor.
                Answer::Bytes(path.accessor_path().as_bytes().to_vec())
            }
            Request::GetBool(_) => Answer::Bool(want_bool(&value)?),
            Request::CopyList { max_len, .. } => {
                let items = want_list(&value)?;
                let total = guest_len(items.len()).map_err(|e| VmError::eval(format!("{e:#}")))?;
                if total > max_len {
                    Answer::Count(total)
                } else {
                    let mut ids = Vec::with_capacity(items.len());
                    for item in items.iter() {
                        ids.push(self.add_slot(item.clone())?);
                    }
                    Answer::Ids(ids)
                }
            }
            Request::CopyAttrset { set, max_len } => {
                let attrs = want_attrs(&value)?;
                let order = self.order(vm, set, &attrs);
                let total = guest_len(order.len()).map_err(|e| VmError::eval(format!("{e:#}")))?;
                if total > max_len {
                    return Ok(Handled::Answer(Answer::Count(total)));
                }
                let mut members = Vec::with_capacity(order.len());
                for sym in order.iter() {
                    let slot = attrs.get(sym).cloned().ok_or_else(|| {
                        VmError::eval("internal: attribute order names a missing attribute")
                    })?;
                    let name_len = u32::try_from(vm.sym_name(*sym).len())
                        .map_err(|_| VmError::eval("attribute name is too long for Wasm"))?;
                    members.push((self.add_slot(slot)?, name_len));
                }
                Answer::Members(members)
            }
            Request::CopyAttrname { set, index } => {
                let attrs = want_attrs(&value)?;
                let order = self.order(vm, set, &attrs);
                let sym = order
                    .get(usize::try_from(index).map_err(|_| {
                        VmError::eval("copy_attrname: attribute index out of bounds")
                    })?)
                    .ok_or_else(|| VmError::eval("copy_attrname: attribute index out of bounds"))?;
                Answer::Bytes(vm.sym_name(*sym).as_bytes().to_vec())
            }
            Request::GetAttr { name, .. } => {
                let attrs = want_attrs(&value)?;
                let sym = vm.intern(&name);
                match attrs.get(&sym).cloned() {
                    Some(slot) => Answer::Id(self.add_slot(slot)?),
                    None => Answer::Id(0),
                }
            }
            Request::CallFunction { function, args } => {
                // cppnix's `forceFunction`: a closure, a primop, or a set
                // with `__functor`.
                let callable = match &value {
                    Value::Closure(_) | Value::Builtin(_) => true,
                    Value::Attrs(attrs) => attrs.contains_key(&vm.intern("__functor")),
                    _ => false,
                };
                if !callable {
                    return Err(VmError::eval(format!(
                        "call_function expects a function but found {}: {value}",
                        type_name(&value)
                    )));
                }
                // `callFunction` with no arguments is the function itself.
                if args.is_empty() {
                    return Ok(Handled::Answer(Answer::Id(function)));
                }
                let mut remaining = self.slots(&args)?;
                remaining.reverse();
                let first = remaining
                    .pop()
                    .ok_or_else(|| VmError::eval("internal: call_function lost its arguments"))?;
                return Ok(Handled::Yield(
                    Yield::Apply(value, first),
                    Awaiting::Applying { remaining },
                ));
            }
            Request::ReadFile(path) => {
                let mut read = PathRead::forced(self.slot(path)?);
                let step = read.step(None)?;
                return Ok(Self::reading(read, step));
            }
            other => {
                debug_assert!(other.operand().is_none());
                return Err(VmError::eval(
                    "internal: a constructing Wasm request reached the inspection path",
                ));
            }
        };
        Ok(Handled::Answer(answer))
    }

    fn reading(read: PathRead, step: ReadStep) -> Handled {
        match step {
            ReadStep::Yield(y) => Handled::Yield(y, Awaiting::Reading(read)),
            ReadStep::Bytes(bytes, _) => Handled::Answer(Answer::Bytes(bytes.to_vec())),
        }
    }

    /// The VM has answered what [`Awaiting`] asked for.
    fn resolve(&mut self, vm: &mut Vm, awaiting: Awaiting, incoming: Option<Value>) -> Result<Handled> {
        match awaiting {
            Awaiting::Forced(request) => {
                let value =
                    incoming.ok_or_else(|| VmError::eval("internal: forced Wasm operand lost"))?;
                self.inspect(vm, request, value)
            }
            Awaiting::Applying { mut remaining } => {
                let result =
                    incoming.ok_or_else(|| VmError::eval("internal: Wasm call result lost"))?;
                match remaining.pop() {
                    Some(next) => Ok(Handled::Yield(
                        Yield::Apply(result, next),
                        Awaiting::Applying { remaining },
                    )),
                    None => Ok(Handled::Answer(self.add(result)?)),
                }
            }
            Awaiting::Warned => Ok(Handled::Answer(Answer::Unit)),
            Awaiting::Reading(mut read) => {
                let step = read.step(incoming)?;
                Ok(Self::reading(read, step))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    // Named imports, not `super::*`: the parent's `Result` is the VM's
    // one-parameter alias, and `host_stubs!` writes the two-parameter std one.
    use crate::compile::Origin;
    use crate::eval::{EvalError, Settings, eval_str_on};
    use crate::host::{FileType, Host};
    use crate::value2::{PathValue, Root};
    use crate::vm::Vm;
    use std::cell::RefCell;

    /// Guests as text (`wat`), served by path; the host also records what
    /// the guest warned.
    struct Guests {
        warnings: RefCell<Vec<String>>,
    }

    const DOUBLE: &str = r#"(module
      (import "env" "get_int" (func $get_int (param i32) (result i64)))
      (import "env" "make_int" (func $make_int (param i64) (result i32)))
      (import "env" "get_float" (func $get_float (param i32) (result f64)))
      (import "env" "make_float" (func $make_float (param f64) (result i32)))
      (import "env" "make_bool" (func $make_bool (param i32) (result i32)))
      (import "env" "get_bool" (func $get_bool (param i32) (result i32)))
      (import "env" "make_null" (func $make_null (result i32)))
      (import "env" "make_list" (func $make_list (param i32 i32) (result i32)))
      (memory (export "memory") 1)
      (func (export "nix_wasm_init_v1"))
      (func (export "double") (param i32) (result i32)
        (call $make_int (i64.mul (call $get_int (local.get 0)) (i64.const 2))))
      (func (export "half") (param i32) (result i32)
        (call $make_float (f64.div (call $get_float (local.get 0)) (f64.const 2))))
      (func (export "ignore") (param i32) (result i32)
        (call $make_int (i64.const 7)))
      ;; b -> [ b false null ]
      (func (export "flags") (param $arg i32) (result i32)
        (i32.store (i32.const 0) (call $make_bool (call $get_bool (local.get $arg))))
        (i32.store (i32.const 4) (call $make_bool (i32.const 0)))
        (i32.store (i32.const 8) (call $make_null))
        (call $make_list (i32.const 0) (i32.const 3))))"#;

    const APPLY: &str = r#"(module
      (import "env" "get_attr" (func $get_attr (param i32 i32 i32) (result i32)))
      (import "env" "call_function" (func $call_function (param i32 i32 i32) (result i32)))
      (import "env" "make_app" (func $make_app (param i32 i32 i32) (result i32)))
      (import "env" "make_attrset" (func $make_attrset (param i32 i32) (result i32)))
      (import "env" "warn" (func $warn (param i32 i32)))
      (memory (export "memory") 1)
      (data (i32.const 0) "f")
      (data (i32.const 8) "x")
      (data (i32.const 16) "hi")
      (data (i32.const 24) "y")
      (data (i32.const 32) "nope")
      (data (i32.const 40) "v")
      (func (export "nix_wasm_init_v1"))
      ;; { f, x } -> { y = f x }, warning "hi" on the way
      (func (export "apply") (param $arg i32) (result i32) (local $f i32) (local $x i32)
        (local.set $f (call $get_attr (local.get $arg) (i32.const 0) (i32.const 1)))
        (local.set $x (call $get_attr (local.get $arg) (i32.const 8) (i32.const 1)))
        (i32.store (i32.const 64) (local.get $x))
        (i32.store (i32.const 96) (i32.const 24))
        (i32.store (i32.const 100) (i32.const 1))
        (i32.store (i32.const 104) (call $call_function (local.get $f) (i32.const 64) (i32.const 1)))
        (call $warn (i32.const 16) (i32.const 2))
        (call $make_attrset (i32.const 96) (i32.const 1)))
      ;; { f, x } -> { v = <f x, unforced> }
      (func (export "app") (param $arg i32) (result i32) (local $f i32) (local $x i32)
        (local.set $f (call $get_attr (local.get $arg) (i32.const 0) (i32.const 1)))
        (local.set $x (call $get_attr (local.get $arg) (i32.const 8) (i32.const 1)))
        (i32.store (i32.const 64) (local.get $x))
        (i32.store (i32.const 96) (i32.const 40))
        (i32.store (i32.const 100) (i32.const 1))
        (i32.store (i32.const 104) (call $make_app (local.get $f) (i32.const 64) (i32.const 1)))
        (call $make_attrset (i32.const 96) (i32.const 1)))
      ;; hands back the "no value" handle of a missing attribute
      (func (export "missing") (param $arg i32) (result i32)
        (call $get_attr (local.get $arg) (i32.const 32) (i32.const 4)))
      ;; { f } -> f, through a zero-argument call_function
      (func (export "call0") (param $arg i32) (result i32)
        (call $call_function
          (call $get_attr (local.get $arg) (i32.const 0) (i32.const 1))
          (i32.const 64) (i32.const 0))))"#;

    const ECHO: &str = r#"(module
      (import "env" "copy_string" (func $copy_string (param i32 i32 i32) (result i32)))
      (import "env" "make_string" (func $make_string (param i32 i32) (result i32)))
      (import "env" "read_file" (func $read_file (param i32 i32 i32) (result i32)))
      (import "env" "make_path" (func $make_path (param i32 i32 i32) (result i32)))
      (import "env" "copy_path" (func $copy_path (param i32 i32 i32) (result i32)))
      (import "env" "make_int" (func $make_int (param i64) (result i32)))
      (import "env" "make_list" (func $make_list (param i32 i32) (result i32)))
      (memory (export "memory") 1)
      (data (i32.const 8192) "sub/file.nix")
      (func (export "nix_wasm_init_v1"))
      ;; s -> [ (the length copy_string reports into a 2-byte buffer) (what the
      ;; buffer holds afterwards: the "**" it was seeded with, if nothing wrote) ]
      (func (export "size") (param $arg i32) (result i32)
        (i32.store8 (i32.const 0) (i32.const 42))
        (i32.store8 (i32.const 1) (i32.const 42))
        (i32.store (i32.const 64) (call $make_int (i64.extend_i32_u
          (call $copy_string (local.get $arg) (i32.const 0) (i32.const 2)))))
        (i32.store (i32.const 68) (call $make_string (i32.const 0) (i32.const 2)))
        (call $make_list (i32.const 64) (i32.const 2)))
      ;; p -> "${p}/sub/file.nix", via make_path then copy_path
      (func (export "join") (param $arg i32) (result i32) (local $n i32)
        (local.set $n (call $copy_path
          (call $make_path (local.get $arg) (i32.const 8192) (i32.const 12))
          (i32.const 0) (i32.const 4096)))
        (call $make_string (i32.const 0) (local.get $n)))
      (func (export "echo") (param $arg i32) (result i32) (local $n i32)
        (local.set $n (call $copy_string (local.get $arg) (i32.const 0) (i32.const 4096)))
        (call $make_string (i32.const 0) (local.get $n)))
      (func (export "slurp") (param $arg i32) (result i32) (local $n i32)
        (local.set $n (call $read_file (local.get $arg) (i32.const 0) (i32.const 4096)))
        (call $make_string (i32.const 0) (local.get $n))))"#;

    const WALK: &str = r#"(module
      (import "env" "copy_list" (func $copy_list (param i32 i32 i32) (result i32)))
      (import "env" "get_type" (func $get_type (param i32) (result i32)))
      (import "env" "make_int" (func $make_int (param i64) (result i32)))
      (import "env" "make_list" (func $make_list (param i32 i32) (result i32)))
      (import "env" "copy_attrset" (func $copy_attrset (param i32 i32 i32) (result i32)))
      (import "env" "copy_attrname" (func $copy_attrname (param i32 i32 i32 i32)))
      (import "env" "make_string" (func $make_string (param i32 i32) (result i32)))
      (memory (export "memory") 1)
      (func (export "nix_wasm_init_v1"))
      ;; [ v ... ] -> [ get_type v ... ]
      (func (export "types") (param $arg i32) (result i32) (local $n i32) (local $i i32)
        (local.set $n (call $copy_list (local.get $arg) (i32.const 0) (i32.const 64)))
        (block $done (loop $next
          (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
          (i32.store (i32.add (i32.const 256) (i32.mul (local.get $i) (i32.const 4)))
            (call $make_int (i64.extend_i32_u
              (call $get_type (i32.load (i32.mul (local.get $i) (i32.const 4)))))))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $next)))
        (call $make_list (i32.const 256) (local.get $n)))
      ;; [ v ... ] -> its length, through a copy into a buffer of size 0
      (func (export "count_list") (param $arg i32) (result i32)
        (call $make_int (i64.extend_i32_u
          (call $copy_list (local.get $arg) (i32.const 0) (i32.const 0)))))
      ;; { ... } -> its size, through a copy into a buffer of size 0
      (func (export "count_attrs") (param $arg i32) (result i32)
        (call $make_int (i64.extend_i32_u
          (call $copy_attrset (local.get $arg) (i32.const 0) (i32.const 0)))))
      ;; [ v ... ] -> get_type of the first element only
      (func (export "first") (param $arg i32) (result i32)
        (drop (call $copy_list (local.get $arg) (i32.const 0) (i32.const 64)))
        (call $make_int (i64.extend_i32_u (call $get_type (i32.load (i32.const 0))))))
      ;; { ... } -> [ name ... ] in the order the host enumerates them
      (func (export "names") (param $arg i32) (result i32) (local $n i32) (local $i i32) (local $len i32)
        (local.set $n (call $copy_attrset (local.get $arg) (i32.const 0) (i32.const 32)))
        (block $done (loop $next
          (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
          (local.set $len (i32.load (i32.add (i32.const 4) (i32.mul (local.get $i) (i32.const 8)))))
          (call $copy_attrname (local.get $arg) (local.get $i) (i32.const 512) (local.get $len))
          (i32.store (i32.add (i32.const 1024) (i32.mul (local.get $i) (i32.const 4)))
            (call $make_string (i32.const 512) (local.get $len)))
          (local.set $i (i32.add (local.get $i) (i32.const 1)))
          (br $next)))
        (call $make_list (i32.const 1024) (local.get $n))))"#;

    const DIE: &str = r#"(module
      (import "env" "panic" (func $panic (param i32 i32)))
      (memory (export "memory") 1)
      (data (i32.const 0) "boom")
      (func (export "nix_wasm_init_v1"))
      (func (export "die") (param i32) (result i32)
        (call $panic (i32.const 0) (i32.const 4))
        (i32.const 0)))"#;

    const WASI: &str = r#"(module
      (import "wasi_snapshot_preview1" "proc_exit" (func $exit (param i32)))
      (memory (export "memory") 1)
      (func (export "nix_wasm_init_v1"))
      (func (export "f") (param i32) (result i32) (local.get 0)))"#;

    impl Guests {
        /// The module bytes behind a path. Guests are written as text and
        /// assembled here, so what `builtins.wasm` receives is a binary
        /// module, the only form it accepts (the `wat` feature is off in
        /// production; cppnix's wasm.cc never read text either).
        fn file(path: &PathValue) -> std::result::Result<Vec<u8>, String> {
            let text = match path.path.as_ref() {
                "/double.wasm" => DOUBLE,
                "/apply.wasm" => APPLY,
                "/echo.wasm" => ECHO,
                "/walk.wasm" => WALK,
                "/die.wasm" => DIE,
                "/wasi.wasm" => WASI,
                "/data.txt" => return Ok(b"payload".to_vec()),
                other => return Err(format!("path '{other}' does not exist")),
            };
            wat::parse_str(text).map_err(|e| format!("test fixture {path}: {e}"))
        }
    }

    impl Host for Guests {
        crate::host::host_stubs!(settle, parse_flake_ref, flake_ref_to_string);
        crate::host::host_stubs!(
            realise,
            store_text,
            write_derivation,
            store_filtered,
            fetch,
            lock_flake,
            fetch_tree,
            not_async
        );
        crate::host::host_stubs!(
            file_type_resolved,
            copy_to_store,
            ensure_path,
            find_file,
            nix_path,
            trace
        );
        fn read_file(&self, path: &PathValue) -> std::result::Result<String, String> {
            Err(format!("'{path}' is not text; nothing here imports"))
        }
        fn read_file_bytes(&self, path: &PathValue) -> std::result::Result<Vec<u8>, String> {
            Self::file(path)
        }
        fn read_dir(
            &self,
            path: &PathValue,
        ) -> std::result::Result<Vec<(String, FileType)>, String> {
            Err(format!("path '{path}' is not a directory"))
        }
        fn path_exists_checked(&self, path: &PathValue) -> std::result::Result<bool, String> {
            Ok(Self::file(path).is_ok())
        }
        fn dir_exists_checked(&self, _path: &PathValue) -> std::result::Result<bool, String> {
            Ok(false)
        }
        fn file_type(&self, path: &PathValue) -> std::result::Result<Option<FileType>, String> {
            Ok(Self::file(path).ok().map(|_| FileType::Regular))
        }
        fn get_env(&self, _name: &str) -> Option<String> {
            None
        }
        fn warn(&self, message: &str) {
            self.warnings.borrow_mut().push(message.to_owned());
        }
    }

    fn guests() -> Guests {
        Guests {
            warnings: RefCell::new(Vec::new()),
        }
    }

    fn eval(host: &Guests, src: &str) -> std::result::Result<String, EvalError> {
        let mut vm = Vm::with_settings(Settings::default());
        eval_str_on(src, "/", Origin::String, &mut vm, host)
    }

    /// The rendered value, or the error spelled out so a failing assertion
    /// shows what went wrong rather than `None`.
    fn rendered(host: &Guests, src: &str) -> String {
        match eval(host, src) {
            Ok(text) => text,
            Err(e) => format!("ERROR {e:?}"),
        }
    }

    fn failure(host: &Guests, src: &str) -> String {
        match eval(host, src) {
            Ok(text) => format!("UNEXPECTEDLY OK {text}"),
            Err(e) => format!("{e:?}"),
        }
    }

    #[test]
    fn a_guest_reads_and_makes_integers() {
        let host = guests();
        assert_eq!(
            rendered(&host, r#"builtins.wasm { path = /double.wasm; function = "double"; } 21"#),
            "42"
        );
    }

    #[test]
    fn get_float_widens_an_integer_as_cppnix_does() {
        let host = guests();
        assert_eq!(
            rendered(&host, r#"builtins.wasm { path = /double.wasm; function = "half"; } 5"#),
            "2.5"
        );
    }

    /// The argument is the guest's to force: a guest that never looks at it
    /// never evaluates it.
    #[test]
    fn the_argument_is_not_forced_unless_the_guest_asks() {
        let host = guests();
        assert_eq!(
            rendered(
                &host,
                r#"builtins.wasm { path = /double.wasm; function = "ignore"; } (throw "unforced")"#
            ),
            "7"
        );
    }

    /// `call_function` applies through the VM -- the closure runs on this
    /// machine, not on the guest's stack -- and `warn` reaches the host with
    /// the module and function named.
    #[test]
    fn a_guest_calls_back_into_nix_and_warns() {
        let host = guests();
        assert_eq!(
            rendered(
                &host,
                r#"(builtins.wasm { path = /apply.wasm; function = "apply"; } { f = n: n + 1; x = 41; }).y"#
            ),
            "42"
        );
        assert_eq!(
            *host.warnings.borrow(),
            vec!["'apply.wasm' function 'apply': hi".to_owned()]
        );
    }

    /// `make_app` builds a thunk: the function is not forced until the
    /// application is, so a set holding one is inspectable without running it.
    #[test]
    fn make_app_is_lazy_on_both_sides() {
        let host = guests();
        assert_eq!(
            rendered(
                &host,
                r#"builtins.attrNames (builtins.wasm { path = /apply.wasm; function = "app"; } { f = throw "unforced"; x = 1; })"#
            ),
            r#"[ "v" ]"#
        );
        assert_eq!(
            rendered(
                &host,
                r#"(builtins.wasm { path = /apply.wasm; function = "app"; } { f = n: n * 3; x = 5; }).v"#
            ),
            "15"
        );
    }

    #[test]
    fn the_reserved_handle_cannot_be_returned() {
        let host = guests();
        let err = failure(&host, r#"builtins.wasm { path = /apply.wasm; function = "missing"; } { }"#);
        assert!(err.contains("invalid ValueId 0"), "{err}");
    }

    #[test]
    fn strings_round_trip_through_guest_memory() {
        let host = guests();
        assert_eq!(
            rendered(&host, r#"builtins.wasm { path = /echo.wasm; function = "echo"; } "hello""#),
            r#""hello""#
        );
    }

    /// The guest's `read_file` goes through the same question `readFile`
    /// asks, so it reads what the embedder lets it read and lands in the
    /// read set like every other read.
    #[test]
    fn a_guest_reads_a_file_through_the_host() {
        let host = guests();
        assert_eq!(
            rendered(&host, r#"builtins.wasm { path = /echo.wasm; function = "slurp"; } /data.txt"#),
            r#""payload""#
        );
    }

    /// `copy_list` hands out unforced handles and `get_type` forces each,
    /// one `Force` yield at a time; the codes are the protocol document's.
    #[test]
    fn get_type_forces_and_classifies_every_kind_of_value() {
        let host = guests();
        assert_eq!(
            rendered(
                &host,
                r#"builtins.wasm { path = /walk.wasm; function = "types"; } [ 1 1.5 true "s" /p null {} [] (x: x) builtins.length ]"#
            ),
            "[ 1 2 3 4 5 6 7 8 9 9 ]"
        );
    }

    /// Attribute enumeration is in name order, whatever order the set was
    /// written in or its symbols were interned in.
    #[test]
    fn attributes_are_enumerated_in_name_order() {
        let host = guests();
        assert_eq!(
            rendered(
                &host,
                r#"builtins.wasm { path = /walk.wasm; function = "names"; } { zeta = 1; alpha = 2; mid = 3; }"#
            ),
            r#"[ "alpha" "mid" "zeta" ]"#
        );
    }

    #[test]
    fn a_guest_panic_is_an_error_with_its_text() {
        let host = guests();
        let err = failure(&host, r#"builtins.wasm { path = /die.wasm; function = "die"; } null"#);
        assert!(err.contains("Wasm panic: boom"), "{err}");
        assert!(err.contains("/die.wasm"), "{err}");
    }

    #[test]
    fn a_wasi_module_is_refused_by_name() {
        let host = guests();
        let err = failure(&host, r#"builtins.wasm { path = /wasi.wasm; function = "f"; } null"#);
        assert!(err.contains("wasi_snapshot_preview1.proc_exit"), "{err}");
        assert!(err.contains("WASI modules are not supported"), "{err}");
    }

    #[test]
    fn the_config_set_is_checked_before_anything_runs() {
        let host = guests();
        let err = failure(
            &host,
            r#"builtins.wasm { path = /double.wasm; function = "double"; bogus = 1; } 1"#,
        );
        assert!(err.contains("unknown attribute 'bogus'"), "{err}");
        let err = failure(&host, r#"builtins.wasm { path = /double.wasm; } 1"#);
        assert!(err.contains("missing required 'function' attribute"), "{err}");
        let err = failure(&host, r#"builtins.wasm { function = "double"; } 1"#);
        assert!(err.contains("missing required 'path' attribute"), "{err}");
    }

    /// `copy_list` hands out handles without forcing: an element that throws
    /// is harmless until a guest looks at it.
    #[test]
    fn copy_list_does_not_force_the_elements() {
        let host = guests();
        assert_eq!(
            rendered(
                &host,
                r#"builtins.wasm { path = /walk.wasm; function = "first"; } [ 1 (throw "unforced") ]"#
            ),
            "1"
        );
        let err = failure(
            &host,
            r#"builtins.wasm { path = /walk.wasm; function = "types"; } [ 1 (throw "forced") ]"#,
        );
        assert!(err.contains("forced"), "{err}");
    }

    #[test]
    fn booleans_and_null_round_trip() {
        let host = guests();
        assert_eq!(
            rendered(&host, r#"builtins.wasm { path = /double.wasm; function = "flags"; } true"#),
            "[ true false null ]"
        );
        // The first element is `get_bool` of the argument; a `get_bool` that
        // answered true for everything passes the line above alone.
        assert_eq!(
            rendered(&host, r#"builtins.wasm { path = /double.wasm; function = "flags"; } false"#),
            "[ false false null ]"
        );
    }

    /// A copy into a zero-sized buffer answers the size, allocates nothing
    /// and forces nothing: the `Count` path.
    #[test]
    fn a_sizing_probe_counts_without_forcing() {
        let host = guests();
        assert_eq!(
            rendered(
                &host,
                r#"builtins.wasm { path = /walk.wasm; function = "count_list"; } [ (throw "a") 1 2 ]"#
            ),
            "3"
        );
        assert_eq!(
            rendered(
                &host,
                r#"builtins.wasm { path = /walk.wasm; function = "count_attrs"; } { a = throw "a"; b = 1; }"#
            ),
            "2"
        );
    }

    #[test]
    fn a_zero_argument_call_is_the_function_itself() {
        let host = guests();
        assert_eq!(
            rendered(
                &host,
                r#"(builtins.wasm { path = /apply.wasm; function = "call0"; } { f = x: x + 1; }) 41"#
            ),
            "42"
        );
        let err = failure(&host, r#"builtins.wasm { path = /apply.wasm; function = "call0"; } { f = { }; }"#);
        assert!(err.contains("expects a function"), "{err}");
    }

    /// The copy contract: the size is answered whatever the buffer holds, and
    /// nothing is written into a buffer too small for it.
    #[test]
    fn copy_into_a_small_buffer_reports_the_size_without_writing() {
        let host = guests();
        assert_eq!(
            rendered(&host, r#"builtins.wasm { path = /echo.wasm; function = "size"; } "hello""#),
            r#"[ 5 "**" ]"#
        );
    }

    /// The other class of error keeps its class too: a missing file inside
    /// the guest's `read_file` is not caught by `tryEval`, exactly as
    /// `builtins.readFile` of a missing file is not.
    #[test]
    fn a_missing_file_inside_the_guest_is_not_catchable() {
        let host = guests();
        let err = failure(
            &host,
            r#"(builtins.tryEval (builtins.wasm { path = /echo.wasm; function = "slurp"; } /missing.txt)).success"#,
        );
        assert!(err.contains("missing.txt"), "{err}");
    }

    /// `make_path` arithmetic on a mounted root, which no guest here can reach
    /// through a Nix literal: the guest works in accessor-local paths and the
    /// result stays under the mount point.
    #[test]
    fn make_path_stays_inside_a_mounted_root() {
        let mount = Root::Mounted("/mnt".into());
        let base = PathValue::normalized(mount.clone(), "/mnt/a/b");
        let at = |relative: &str| {
            let joined = super::joined_path(&base, relative).expect("joins");
            (joined.path.to_string(), joined.accessor_path().to_string())
        };
        assert_eq!(at("c"), ("/mnt/a/b/c".to_owned(), "/a/b/c".to_owned()));
        assert_eq!(at("/x"), ("/mnt/x".to_owned(), "/x".to_owned()));
        assert_eq!(at("../../../x"), ("/mnt/x".to_owned(), "/x".to_owned()));
        assert_eq!(at("../.."), ("/mnt".to_owned(), "/".to_owned()));
        let root = PathValue::normalized(mount, "/mnt");
        assert_eq!(super::joined_path(&root, "c").expect("joins").path.as_ref(), "/mnt/c");
        let err = super::joined_path(&root, "a\0b").expect_err("NUL is refused");
        assert!(format!("{err:?}").contains("NUL"), "{err:?}");
    }

    #[test]
    fn make_path_joins_and_copy_path_reads_back() {
        let host = guests();
        assert_eq!(
            rendered(&host, r#"builtins.wasm { path = /echo.wasm; function = "join"; } /base/dir"#),
            r#""/base/dir/sub/file.nix""#
        );
    }

    /// A throw inside a function the guest applies is the throw itself, not a
    /// trap: `tryEval` sees it as it would anywhere else (module doc, "Where
    /// this differs from wasm.cc on purpose").
    #[test]
    fn an_error_inside_a_callback_is_catchable() {
        let host = guests();
        let err = failure(
            &host,
            r#"builtins.wasm { path = /apply.wasm; function = "apply"; } { f = n: throw "inner"; x = 1; }"#,
        );
        assert!(err.contains("inner"), "{err}");
        assert_eq!(
            rendered(
                &host,
                r#"(builtins.tryEval (builtins.wasm { path = /apply.wasm; function = "apply"; } { f = n: throw "inner"; x = 1; })).success"#
            ),
            "false"
        );
    }

    #[test]
    fn a_missing_export_names_itself() {
        let host = guests();
        let err = failure(&host, r#"builtins.wasm { path = /double.wasm; function = "nope"; } 1"#);
        assert!(err.contains("does not export 'nope'"), "{err}");
        // and the failure is attributed to the export that was being looked
        // up, not to the one that ran before it
        assert!(err.contains("Wasm function 'nope'"), "{err}");
    }
}
