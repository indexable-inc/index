//! Capability checks use implemented bodies and typed evaluator settings.
use nix_eval_rs::{
    builtins,
    eval::{BuiltinFeatures, Settings},
};

fn members(features: BuiltinFeatures) -> Vec<&'static str> {
    builtins::set_member_names(&Settings {
        builtin_features: features,
        ..Settings::default()
    })
    .collect()
}

#[test]
fn gates_only_enable_implemented_builtins() {
    for bits in 0..8 {
        let Some(features) = BuiltinFeatures::from_bits(bits) else {
            unreachable!()
        };
        let names = members(features);
        for unsupported in [
            "exec",
            "fetchClosure",
            "importNative",
            "outputOf",
            "parallel",
            "fetchMercurial",
            "currentTime",
            "fetchFinalTree",
            "recordedTreeAttr",
        ] {
            assert!(
                !names.contains(&unsupported),
                "{unsupported} advertised under flags {bits}"
            );
        }
        assert_eq!(
            names.contains(&"fetchTree"),
            features.flakes || features.fetch_tree
        );
        assert_eq!(names.contains(&"wasm"), features.wasm);
        for name in ["getFlake", "parseFlakeRef", "flakeRefToString"] {
            assert_eq!(names.contains(&name), features.flakes);
        }
        assert!(names.contains(&"add"));
        let settings = Settings {
            builtin_features: features,
            ..Settings::default()
        };
        assert_eq!(
            builtins::is_global(&settings, "fetchTree"),
            features.flakes || features.fetch_tree
        );
        assert_eq!(builtins::is_global(&settings, "__wasm"), features.wasm);
        assert!(!builtins::is_global(&settings, "__fetchClosure"));
    }
}

#[test]
fn every_feature_configuration_has_its_own_key() {
    let mut keys = std::collections::BTreeSet::new();
    for bits in 0..8 {
        let Some(features) = BuiltinFeatures::from_bits(bits) else {
            unreachable!()
        };
        let key = Settings {
            builtin_features: features,
            ..Settings::default()
        }
        .fingerprint();
        assert!(
            keys.insert(format!("{key:?}")),
            "configuration {bits} shares a memo key"
        );
    }
    assert!(BuiltinFeatures::from_bits(8).is_none());
}
