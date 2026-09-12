R""(

# Description

`nix fmt` (an alias for `nix formatter run`) calls the formatter specified in the flake.

Flags can be forwarded to the formatter by using `--` followed by the flags.

Any arguments will be forwarded to the formatter. Typically these are the files to format.

The environment variable `PRJ_ROOT` (according to [prj-spec](https://github.com/numtide/prj-spec))
is set to the absolute path of the flake's local source directory. For a flake in a
subdirectory of a version controlled workspace, this is the workspace root.
The formatter runs in the current directory, with relative arguments unchanged.


# Example

To use the [official Nix formatter](https://github.com/NixOS/nixfmt):

```nix
# flake.nix
{
  outputs = { nixpkgs, self }: {
    formatter.x86_64-linux = nixpkgs.legacyPackages.${system}.nixfmt-tree;
  };
}
```

)""
