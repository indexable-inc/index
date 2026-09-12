let
  check = d: expectedNames:
    let imported = import d.drvPath;
    in assert imported.type == "derivation";
       assert imported.name == d.name;
       assert imported.drvPath == d.drvPath;
       assert builtins.getContext imported.drvPath == builtins.getContext d.drvPath;
       assert imported.outputName == builtins.head expectedNames;
       assert map (x: x.outputName) imported.all == expectedNames;
       assert builtins.all (name:
         let output = builtins.getAttr name imported;
             original = builtins.getAttr name d;
         in output.outPath == original.outPath
            && builtins.getContext output.outPath == builtins.getContext original.outPath
            && output.outputName == name
            && (builtins.getAttr name output).outPath == output.outPath
       ) expectedNames;
       true;
  base = { name = "import-drv-regression"; system = "x86_64-linux"; builder = "/not-executed"; };
  multiple = builtins.derivation (base // { outputs = [ "out" "dev" ]; });
  fixed = builtins.derivation (base // {
    outputHashAlgo = "sha256";
    outputHashMode = "recursive";
    outputHash = "0000000000000000000000000000000000000000000000000000000000000000";
  });
  floating = builtins.derivation (base // { __contentAddressed = true; outputs = [ "out" "dev" ]; });
  deferred = builtins.derivation (base // { input = floating.out; });
in {
  inputAddressed = check multiple [ "dev" "out" ];
  fixedOutput = check fixed [ "out" ];
  floatingCA = check floating [ "dev" "out" ];
  deferred = check deferred [ "out" ];
}
