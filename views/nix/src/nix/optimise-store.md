R""(

# Examples

* Optimise the Nix store:

  ```console
  nix store optimise
  ```

* Optimise only specified outputs, without scanning the rest of the store or
  following references:

  ```console
  nix store optimise --store local /nix/store/…-program /nix/store/…-program-debug
  ```

# Description

This command deduplicates the Nix store: it scans the store for
regular files with identical contents, and replaces them with hard
links to a single instance.

Optional positional paths restrict the operation to registered top-level store
paths. This mode requires direct access to the local store and temporarily roots
every selected path against garbage collection. It does not build, delete, or
recursively select dependencies. Automatic store optimisation need not be enabled.

Note that you can also set `auto-optimise-store` to `true` in
`nix.conf` to perform this optimisation incrementally whenever a new
path is added to the Nix store. To make this efficient, Nix maintains
a content-addressed index of all the files in the Nix store in the
directory `/nix/store/.links/`.

)""
