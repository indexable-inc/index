//! The evaluator half of importing a store-validated derivation.

use crate::host::ImportedDerivation;
use crate::value2::{Attrs, ContextElem, EnvNode, NixStr, PathValue, Slot, Value};
use crate::vm::{Result, Vm};
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

pub(crate) fn argument(vm: &mut Vm, drv: &ImportedDerivation) -> Value {
    let mut attrs = BTreeMap::new();
    let path: Rc<str> = drv.path.as_str().into();
    attrs.insert(vm.intern("drvPath"), Slot::value(Value::Str(NixStr::with_context(
        Rc::clone(&path), BTreeSet::from([ContextElem::DrvDeep(Rc::clone(&path))]),
    ))));
    attrs.insert(vm.intern("name"), Slot::value(Value::Str(drv.name.as_str().into())));
    let mut names = Vec::new();
    for (name, output) in &drv.outputs {
        names.push(Slot::value(Value::Str(name.as_str().into())));
        attrs.insert(vm.intern(name), Slot::value(Value::Str(NixStr::with_context(
            output.as_bytes(), BTreeSet::from([ContextElem::Built {
                drv: Rc::clone(&path), output: name.as_str().into(),
            }]),
        ))));
    }
    attrs.insert(vm.intern("outputs"), Slot::value(Value::List(Rc::new(names))));
    Value::Attrs(Rc::new(Attrs::new(attrs)))
}

// Ordinary imported-drv-to-derivation.nix, kept inside the fingerprinted
// Rust source. Derivation data is applied as values, never interpolated.
const WRAPPER: &str = r#"attrs@{
  drvPath,
  outputs,
  name,
  ...
}:

let

  commonAttrs = (builtins.listToAttrs outputsList) // {
    all = map (x: x.value) outputsList;
    inherit drvPath name;
    type = "derivation";
  };

  outputToAttrListElement = outputName: {
    name = outputName;
    value = commonAttrs // {
      outPath = builtins.getAttr outputName attrs;
      inherit outputName;
    };
  };

  outputsList = map outputToAttrListElement outputs;

in
(builtins.head outputsList).value
"#;

pub(crate) fn wrapper(vm: &mut Vm) -> Result<Slot> {
    // The ordinary imported-derivation wrapper preserves lexical output order,
    // recursive sibling attributes and `all` without generating Nix text.
    let module = vm.import_module(
        &PathValue::ambient("/imported-drv-to-derivation.nix"),
        WRAPPER,
        "/",
    )?;
    let entry = module.entry;
    Ok(Slot::thunk(module, entry, Rc::new(EnvNode::Root)))
}
