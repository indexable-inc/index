# The two runtimes share no mutable source boundary.
{
  lib,
  nixSrc,
}: {
  evaluator = builtins.path {
    path = nixSrc;
    name = "nix-eval-source";
    filter = path: _type: let
      relative = lib.removePrefix (toString nixSrc + "/") path;
    in
      path
      == toString nixSrc
      || (relative == "rust" || lib.hasPrefix "rust/" relative)
      && relative != "rust/nix-host-rs"
      && !lib.hasPrefix "rust/nix-host-rs/" relative
      || builtins.elem relative [
        "src"
        "src/libexpr"
        "src/libexpr/primops"
        "src/libexpr/primops/derivation.nix"
        "src/libexpr/fetchurl.nix"
      ];
  };
  host = builtins.path {
    path = nixSrc + "/rust/nix-host-rs";
    name = "nix-host-source";
    filter = path: _type: baseNameOf path != "target";
  };
}
