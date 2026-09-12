//! Every exported leaf-runtime function must have a public header declaration.
//! Domain tests cover buffer ownership and layouts; C++ adapters compile the headers.
#![cfg(test)]

use std::collections::BTreeSet;
use std::path::Path;

const SURFACES: &[(&str, &str)] = &[
    ("include/ix-store-stream.h", "src/store_stream.rs"),
    ("include/ixe-fetch-registry.h", "src/fetch_registry/ffi.rs"),
    ("include/ixe-build-scheduler.h", "src/build_scheduler/ffi.rs"),
];

fn identifiers(text: &str) -> Vec<&str> {
    text.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .filter(|word| !word.is_empty())
        .collect()
}

fn exports(source: &str) -> Result<BTreeSet<String>, String> {
    let mut names = BTreeSet::new();
    for item in source.split("#[unsafe(no_mangle)]").skip(1) {
        let tokens = identifiers(item);
        let position = tokens.iter().position(|word| *word == "fn")
            .ok_or("export without a function declaration")?;
        let name = tokens.get(position + 1).ok_or("export without a function name")?;
        if !names.insert((*name).to_owned()) {
            return Err(format!("duplicate Rust export {name}"));
        }
    }
    Ok(names)
}

fn without_comments(source: &str) -> Result<String, String> {
    let mut result = String::new();
    let mut remaining = source;
    while let Some(start) = remaining.find("/*") {
        result.push_str(remaining.get(..start).ok_or("invalid comment boundary")?);
        remaining = remaining.get(start + 2..).ok_or("invalid comment start")?;
        let end = remaining.find("*/").ok_or("unclosed C header comment")?;
        remaining = remaining.get(end + 2..).ok_or("invalid comment end")?;
    }
    result.push_str(remaining);
    Ok(result.lines().map(|line| line.split("//").next().unwrap_or("")).collect::<Vec<_>>().join("\n"))
}

fn declarations(source: &str) -> Result<BTreeSet<String>, String> {
    let source = without_comments(source)?;
    let mut names = BTreeSet::new();
    for part in source.split('(') {
        if let Some(name) = identifiers(part).last()
            && (name.starts_with("ixs_") || name.starts_with("ixe_"))
            && !names.insert((*name).to_owned())
        {
            return Err(format!("duplicate C declaration {name}"));
        }
    }
    Ok(names)
}

#[test]
fn every_domain_exports_exactly_its_public_header() -> Result<(), String> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut all = BTreeSet::new();
    for (header, implementation) in SURFACES {
        let read = |relative| std::fs::read_to_string(root.join(relative)).map_err(|e| e.to_string());
        let declared = declarations(&read(header)?)?;
        let exported = exports(&read(implementation)?)?;
        assert!(!exported.is_empty(), "{implementation} contains no exports");
        assert_eq!(declared, exported, "ABI census for {header}");
        for name in exported {
            assert!(all.insert(name.clone()), "two domains export {name}");
        }
    }
    Ok(())
}

#[test]
fn missing_or_extra_declarations_are_detected() -> Result<(), String> {
    let implemented = exports("#[unsafe(no_mangle)] pub unsafe extern \"C\" fn ixe_example() {}")?;
    assert_eq!(declarations("void ixe_example(void);")?, implemented);
    assert_ne!(declarations("/* void ixe_example(void); */")?, implemented);
    assert_ne!(declarations("void ixe_other(void);")?, implemented);
    assert!(declarations("void ixe_example(void); void ixe_example(void);").is_err());
    Ok(())
}
