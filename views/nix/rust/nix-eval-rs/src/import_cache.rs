//! Persistent results of closed, host-free imported modules.
//!
//! The source import still happens before this boundary. Its source bytes and
//! resolved origin determine the compiled object, and remain dependencies of
//! the enclosing question. The certificate below proves that forcing the
//! imported entry cannot consume another world dependency or emit an effect.
//! No VM slots, environments, symbols, or lazy values cross this boundary.

use crate::eval::Settings;
use crate::ir::{Const, Module, Op};
use crate::store::Store;
use crate::value2::{NixStr, Value};
use ix_kernel::canon::{self, CanonValue};
use ix_kernel::cas::Cas;
use ix_kernel::rows::Lookup;
use ix_kernel::{Domain, Key, ObjId};
use std::io::Read as _;

const FORMAT: &str = "closed-import-scalar-v1";
/// Bounds both a published scalar and the allocation allowed when decoding it.
pub(crate) const MAX_SCALAR_BYTES: usize = 64 * 1024;
const MAX_OBJECT_BYTES: usize = MAX_SCALAR_BYTES + 128;

fn domain() -> Domain {
    Domain::mint("ix-eval.closed-import", FORMAT)
}

/// A certified request retained by the import continuation until WHNF returns.
/// Public because the public import continuation carries it; private fields
/// keep construction at the certificate boundary in this module.
#[derive(Clone, Debug)]
pub struct ImportKey {
    request: Vec<u8>,
    key: Key,
}

pub(crate) enum Probe {
    Ineligible,
    Miss(ImportKey),
    Damaged { key: ImportKey, warning: String },
    Hit(Value),
}

fn damaged(key: ImportKey, kind: crate::perf::CacheCorruption, detail: &str) -> Probe {
    crate::perf::note_cache_corruption(kind);
    // Preserve the certified key so normal evaluation can repair the entry.
    Probe::Damaged {
        key,
        warning: format!("import cache: {detail}"),
    }
}

/// All units are checked, including bodies that this execution may not force.
/// Exhaustive matches make a new instruction or literal require a decision.
/// No builtin whitelist is inferred from names or from an absence of recorded
/// reads: a previously forced builtin slot can otherwise hide a dependency.
pub(crate) fn certified(module: &Module) -> bool {
    module.consts.iter().all(|constant| match constant {
        Const::Int(_) | Const::Float(_) | Const::Bool(_) | Const::Null | Const::Str(_) => true,
        Const::Path(_) => false,
    }) && module.units.iter().all(|unit| {
        unit.ops.iter().all(|op| match op {
            Op::Builtin { .. }
            | Op::CallBuiltin { .. }
            | Op::BuiltinsSet
            | Op::DerivationGlobal
            | Op::NixPathGlobal
            | Op::ConcatPath { .. } => false,
            Op::Const(_)
            | Op::GetLocal { .. }
            | Op::GetLocalLazy { .. }
            | Op::Thunk { .. }
            | Op::Closure { .. }
            | Op::Apply
            | Op::PushEnv { .. }
            | Op::PopEnv
            | Op::JumpIfFalse { .. }
            | Op::Jump { .. }
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Eq
            | Op::Neq
            | Op::Lt
            | Op::Leq
            | Op::Gt
            | Op::Geq
            | Op::Not
            | Op::Negate
            | Op::ConcatStrings { .. }
            | Op::MkList { .. }
            | Op::ConcatLists
            | Op::MkAttrs { .. }
            | Op::Update
            | Op::MkAttrsOnto { .. }
            | Op::Select { .. }
            | Op::SelectSoft { .. }
            | Op::SelectSoftDyn
            | Op::OrDefault
            | Op::HasAttr { .. }
            | Op::SelectDyn
            | Op::HasAttrDyn
            | Op::PushWith
            | Op::ResolveWith { .. }
            | Op::Assert
            | Op::Ret => true,
        })
    })
}

fn request(module_id: ObjId, settings: &Settings, depth: u32) -> Result<ImportKey, String> {
    let request = canon::encode(&CanonValue::array([
        CanonValue::str(FORMAT),
        CanonValue::Bytes(module_id.hash().as_bytes().to_vec()),
        CanonValue::str(crate::modcache::compiler_fingerprint()),
        // Includes host implementation identity and every captured policy.
        CanonValue::Bytes(settings.fingerprint().as_bytes().to_vec()),
        // Root environments have no captures, but inherit the caller's budget.
        CanonValue::int(depth),
    ]))
    .map_err(|error| error.to_string())?;
    let key = Key::mint(domain(), &request);
    Ok(ImportKey { request, key })
}

/// `module_id` must be the object's identity returned with this compiled module.
/// Cache damage is counted and re-evaluated; I/O errors are returned to the
/// caller's diagnostic sink. Neither becomes an evaluation failure.
pub(crate) fn probe(
    store: &Store,
    module_id: ObjId,
    module: &Module,
    settings: &Settings,
    depth: u32,
) -> Result<Probe, String> {
    if settings.repair || !certified(module) {
        return Ok(Probe::Ineligible);
    }
    let key = request(module_id, settings, depth)?;
    let output = match store.rows().get(domain(), key.key) {
        Lookup::Missing => return Ok(Probe::Miss(key)),
        Lookup::Refused(reason) => {
            return Ok(damaged(
                key,
                crate::perf::CacheCorruption::WitnessRefused,
                &reason.to_string(),
            ));
        }
        Lookup::Found(output) => output,
    };
    // Read through one bounded descriptor: a stat followed by CAS::get would
    // permit a concurrently replaced corrupt object to exceed the byte cap.
    let path = store.objects_dir().join(output.hash().to_hex());
    let file = match std::fs::File::open(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(damaged(
                key,
                crate::perf::CacheCorruption::ObjectMissing,
                "scalar object missing",
            ));
        }
        Err(error) => return Err(format!("import cache: {error}")),
        Ok(file) => file,
    };
    let mut bytes = Vec::new();
    file.take((MAX_OBJECT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("import cache: {error}"))?;
    if bytes.len() > MAX_OBJECT_BYTES || ObjId::of(&bytes) != output {
        return Ok(damaged(
            key,
            crate::perf::CacheCorruption::ObjectInvalid,
            "oversized or corrupt scalar object",
        ));
    }
    let value = match decode_scalar(&bytes) {
        Ok(value) => value,
        Err(detail) => {
            return Ok(damaged(
                key,
                crate::perf::CacheCorruption::ObjectInvalid,
                &detail,
            ));
        }
    };
    store.rows().touch(domain(), key.key);
    Ok(Probe::Hit(value))
}

/// Publish only the successful scalar already produced by normal evaluation.
/// The common store owns disk limits and GC; this cache retains no values in
/// memory. A configured zero disk cap has the same explicit unbounded meaning
/// as it does for compiled modules and whole-question results.
pub(crate) fn insert(store: &Store, key: &ImportKey, value: &Value) -> Result<bool, String> {
    let Some(bytes) = encode_scalar(value)? else {
        return Ok(false);
    };
    {
        let _publication = store
            .publication_guard()
            .map_err(|error| error.to_string())?;
        let output = store.cas().put(&bytes).map_err(|error| error.to_string())?;
        store
            .rows()
            .put(domain(), &key.request, output)
            .map_err(|error| error.to_string())?;
    }
    store.sweep_to_cap().map_err(|error| error.to_string())?;
    Ok(true)
}

fn encode_scalar(value: &Value) -> Result<Option<Vec<u8>>, String> {
    let scalar = match value {
        Value::Null => CanonValue::array([CanonValue::int(0)]),
        Value::Bool(value) => CanonValue::array([CanonValue::int(1), CanonValue::Bool(*value)]),
        Value::Int(value) => CanonValue::array([CanonValue::int(2), CanonValue::int(*value)]),
        Value::Float(value) => {
            CanonValue::array([CanonValue::int(3), CanonValue::int(value.to_bits())])
        }
        Value::Str(value) if !value.has_context() && value.bytes().len() <= MAX_SCALAR_BYTES => {
            CanonValue::array([
                CanonValue::int(4),
                CanonValue::Bytes(value.bytes().to_vec()),
            ])
        }
        _ => return Ok(None),
    };
    canon::encode(&CanonValue::array([CanonValue::str(FORMAT), scalar]))
        .map(Some)
        .map_err(|error| error.to_string())
}

fn decode_scalar(bytes: &[u8]) -> Result<Value, String> {
    if bytes.len() > MAX_OBJECT_BYTES {
        return Err("import cache: oversized scalar object".to_owned());
    }
    let decoded = canon::decode(bytes).map_err(|error| format!("import cache: {error}"))?;
    let CanonValue::Array(outer) = decoded else {
        return Err("import cache: scalar envelope is not an array".to_owned());
    };
    let [CanonValue::Str(format), CanonValue::Array(fields)] = outer.as_slice() else {
        return Err("import cache: malformed scalar envelope".to_owned());
    };
    if format != FORMAT {
        return Err("import cache: unknown scalar format".to_owned());
    }
    match fields.as_slice() {
        [CanonValue::Int(0)] => Ok(Value::Null),
        [CanonValue::Int(1), CanonValue::Bool(value)] => Ok(Value::Bool(*value)),
        [CanonValue::Int(2), CanonValue::Int(value)] => i64::try_from(*value)
            .map(Value::Int)
            .map_err(|error| format!("import cache: {error}")),
        [CanonValue::Int(3), CanonValue::Int(value)] => u64::try_from(*value)
            .map(|bits| Value::Float(f64::from_bits(bits)))
            .map_err(|error| format!("import cache: {error}")),
        [CanonValue::Int(4), CanonValue::Bytes(value)] if value.len() <= MAX_SCALAR_BYTES => {
            Ok(Value::Str(NixStr::from(value.as_slice())))
        }
        _ => Err("import cache: invalid scalar payload".to_owned()),
    }
}

#[cfg(test)]
mod tests;
