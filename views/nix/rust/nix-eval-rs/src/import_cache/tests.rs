use super::*;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT: AtomicU64 = AtomicU64::new(0);

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        Self(std::env::temp_dir().join(format!(
            "ixe-import-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed),
        )))
    }

    fn store(&self) -> Store {
        Store::open(&self.0)
            .expect("open test store")
            .with_max_bytes(8 * 1024 * 1024)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn module(text: &str) -> Module {
    crate::compile::compile_source(
        text,
        "/fixture",
        crate::compile::Origin::File("/fixture/stable.nix"),
        &Settings::default(),
    )
    .expect("compile fixture")
}

fn id(module: &Module) -> ObjId {
    ObjId::of(&crate::modcache::encode_module(module).expect("module encoding"))
}

fn miss(store: &Store, module: &Module) -> ImportKey {
    match probe(store, id(module), module, &Settings::default(), 0).expect("probe") {
        Probe::Miss(key) => key,
        _ => panic!("expected a cold eligible module"),
    }
}

fn run(module: Module) -> Value {
    let mut vm = crate::vm::Vm::with_settings(Settings::default());
    vm.start_module(&Rc::new(module));
    match vm.poll().expect("closed computation succeeds") {
        crate::vm::Step::Done(value) => value,
        other => panic!("certified module requested scheduler assistance: {other:?}"),
    }
}

#[test]
fn certificate_rejects_world_access_even_in_unforced_units() {
    for source in [
        "let unused = builtins.readFile ./secret; in 1",
        "let unused = builtins.trace \"effect\" 1; in 1",
        "let unused = derivation; in 1",
        "let unused = __nixPath; in 1",
        "let unused = ./secret; in 1",
        "builtins.currentSystem",
    ] {
        assert!(!certified(&module(source)), "{source}");
    }
    for source in [
        "42",
        "null",
        "1.25",
        "\"a\" + \"b\"",
        "let f = n: if n == 0 then 0 else 1 + f (n - 1); in f 40",
        "let a = { x = 42; unused = assert false; 0; }; in a.x",
    ] {
        let compiled = module(source);
        assert!(certified(&compiled), "{source}");
        assert!(encode_scalar(&run(compiled)).expect("encode").is_some());
    }
    for forbidden in [
        Op::Builtin { idx: 0 },
        Op::CallBuiltin { idx: 0 },
        Op::BuiltinsSet,
        Op::DerivationGlobal,
        Op::NixPathGlobal,
        Op::ConcatPath { n: 1 },
    ] {
        let mut compiled = module("42");
        compiled.units[0].ops.push(forbidden);
        assert!(!certified(&compiled), "{forbidden:?}");
    }
}

#[test]
fn codec_preserves_bits_and_bytes_and_declines_lazy_or_contextual_values() {
    let values = [
        Value::Null,
        Value::Bool(false),
        Value::Int(i64::MIN),
        Value::Int(i64::MAX),
        Value::Float(-0.0),
        Value::Float(f64::from_bits(0x7ff8000000000042)),
        Value::Str(NixStr::from(&[0, 255, 128][..])),
    ];
    for value in values {
        let encoded = encode_scalar(&value).expect("encode").expect("scalar");
        let decoded = decode_scalar(&encoded).expect("decode");
        assert_eq!(encode_scalar(&decoded).expect("re-encode"), Some(encoded));
    }
    for source in [
        "[ 1 (assert false; 0) ]",
        "{ x = assert false; 0; }",
        "x: x",
        "./a",
    ] {
        let value = run(module(source));
        assert!(
            encode_scalar(&value).expect("decline").is_none(),
            "{source}"
        );
    }
    let context = [crate::value2::ContextElem::Opaque(
        "/nix/store/context".into(),
    )]
    .into();
    let contextual = Value::Str(NixStr::with_context(b"text".to_vec(), context));
    assert!(encode_scalar(&contextual).expect("context").is_none());
    let large = Value::Str(NixStr::from(vec![b'x'; MAX_SCALAR_BYTES + 1].as_slice()));
    assert!(encode_scalar(&large).expect("large").is_none());
    assert!(decode_scalar(&[0xff]).is_err());
}

#[test]
fn identity_separates_origin_policy_host_and_remaining_depth() {
    let scratch = Scratch::new();
    let store = scratch.store();
    let compiled = module("42");
    let key = miss(&store, &compiled);
    assert!(insert(&store, &key, &Value::Int(42)).expect("publish"));
    assert!(matches!(
        probe(&store, id(&compiled), &compiled, &Settings::default(), 0).expect("hit"),
        Probe::Hit(Value::Int(42))
    ));
    assert!(matches!(
        probe(&store, id(&compiled), &compiled, &Settings::default(), 1).expect("depth"),
        Probe::Miss(_)
    ));
    for settings in [
        Settings {
            host_build_identity: Some("host-b".to_owned()),
            ..Settings::default()
        },
        Settings {
            allow_import_from_derivation: false,
            ..Settings::default()
        },
        Settings {
            max_call_depth: 20,
            ..Settings::default()
        },
        Settings {
            pure_eval: true,
            ..Settings::default()
        },
    ] {
        assert!(matches!(
            probe(&store, id(&compiled), &compiled, &settings, 0).expect("policy"),
            Probe::Miss(_)
        ));
    }
    let elsewhere = crate::compile::compile_source(
        "42",
        "/other",
        crate::compile::Origin::File("/other/stable.nix"),
        &Settings::default(),
    )
    .expect("other origin");
    assert!(matches!(
        probe(&store, id(&elsewhere), &elsewhere, &Settings::default(), 0).expect("origin"),
        Probe::Miss(_)
    ));
    let repair = Settings {
        repair: true,
        ..Settings::default()
    };
    assert!(matches!(
        probe(&store, id(&compiled), &compiled, &repair, 0).expect("repair"),
        Probe::Ineligible
    ));
}

#[test]
fn missing_corrupt_and_wrongly_typed_objects_never_become_hits() {
    let scratch = Scratch::new();
    let store = scratch.store();
    let compiled = module("42");
    let key = miss(&store, &compiled);
    insert(&store, &key, &Value::Int(42)).expect("publish");
    let Lookup::Found(output) = store.rows().get(domain(), key.key) else {
        panic!("published row");
    };
    let path = store.objects_dir().join(output.hash().to_hex());
    std::fs::write(&path, b"corrupt").expect("damage object");
    assert!(matches!(
        probe(&store, id(&compiled), &compiled, &Settings::default(), 0)
            .expect("damaged entry misses"),
        Probe::Damaged { .. }
    ));
    std::fs::remove_file(&path).expect("remove object");
    assert!(matches!(
        probe(&store, id(&compiled), &compiled, &Settings::default(), 0).expect("missing"),
        Probe::Damaged { .. }
    ));
    let wrong = store
        .cas()
        .put(&canon::encode(&CanonValue::Null).expect("wrong payload"))
        .expect("put");
    store
        .rows()
        .put(domain(), &key.request, wrong)
        .expect("row");
    assert!(matches!(
        probe(&store, id(&compiled), &compiled, &Settings::default(), 0)
            .expect("damaged entry misses"),
        Probe::Damaged { .. }
    ));
    let key_other = request(id(&compiled), &Settings::default(), 1).expect("other request");
    let rows = store.index_dir().join(domain().hash().to_hex());
    std::fs::copy(
        rows.join(key.key.hash().to_hex()),
        rows.join(key_other.key.hash().to_hex()),
    )
    .expect("swap row");
    assert!(matches!(
        probe(&store, id(&compiled), &compiled, &Settings::default(), 1)
            .expect("misfiled row misses"),
        Probe::Damaged { .. }
    ));
    insert(&store, &key, &Value::Int(42)).expect("repair");
    assert!(matches!(
        probe(&store, id(&compiled), &compiled, &Settings::default(), 0).expect("repaired entry"),
        Probe::Hit(Value::Int(42))
    ));
}

/// Invoked by the parent test as a fresh test process for every request.
#[test]
fn cold_process_fixture() {
    let Ok(root) = std::env::var("IXE_IMPORT_TEST_ROOT") else {
        return;
    };
    let source = std::env::var("IXE_IMPORT_TEST_SOURCE").expect("source");
    let expected_hit = std::env::var("IXE_IMPORT_TEST_HIT").expect("hit") == "1";
    let store = Store::open(root)
        .expect("store")
        .with_max_bytes(8 * 1024 * 1024);
    let compiled = module(&source);
    match probe(&store, id(&compiled), &compiled, &Settings::default(), 0).expect("probe") {
        Probe::Hit(value) => {
            assert!(expected_hit, "a new module unexpectedly hit");
            assert_eq!(
                encode_scalar(&value).expect("hit"),
                encode_scalar(&run(compiled)).expect("control")
            );
        }
        Probe::Miss(key) => {
            assert!(
                !expected_hit,
                "persisted scalar was not reused after restart"
            );
            assert!(insert(&store, &key, &run(compiled)).expect("publish"));
        }
        Probe::Ineligible => panic!("closed fixture rejected"),
        Probe::Damaged { warning, .. } => panic!("fresh fixture damaged: {warning}"),
    }
}

#[test]
fn a_a_b_b_a_reuses_evaluated_results_across_fresh_processes() {
    let scratch = Scratch::new();
    for (source, hit) in [
        ("6 * 7", "0"),
        ("6 * 7", "1"),
        ("6 * 8", "0"),
        ("6 * 8", "1"),
        ("6 * 7", "1"),
    ] {
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "import_cache::tests::cold_process_fixture",
                "--nocapture",
            ])
            .env("IXE_IMPORT_TEST_ROOT", &scratch.0)
            .env("IXE_IMPORT_TEST_SOURCE", source)
            .env("IXE_IMPORT_TEST_HIT", hit)
            .status()
            .expect("child test");
        assert!(
            status.success(),
            "fresh-process scenario {source} hit={hit}"
        );
    }
}

#[test]
fn common_store_sweep_bounds_scalar_objects_without_dangling_successes() {
    let scratch = Scratch::new();
    let store = scratch.store().with_max_bytes(2048);
    for i in 0..16 {
        let compiled = module(&i.to_string());
        let key = miss(&store, &compiled);
        let value = Value::Str(NixStr::from(vec![b'x'; 512].as_slice()));
        insert(&store, &key, &value).expect("publish and sweep");
        assert!(store.size().expect("size") <= 2048);
    }
}
