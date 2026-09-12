let
  foreignPair = import ./list-foreign.nix;
  attrs = builtins.listToAttrs [
    { name = "local"; value = 1; }
    foreignPair
  ];
in
[
  (builtins.unsafeGetAttrPos "local" attrs)
  (builtins.unsafeGetAttrPos "foreign" attrs)
]
