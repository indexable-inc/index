let
  foreign = import ./update-foreign.nix;
  merged = {
    local = 1;
    shared = 2;
  } // foreign;
in
[
  (builtins.unsafeGetAttrPos "local" merged)
  (builtins.unsafeGetAttrPos "shared" merged)
  (builtins.unsafeGetAttrPos "remote" merged)
]
