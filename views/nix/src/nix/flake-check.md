R""(

# Examples

* Validate current-system outputs and build every local check:

  ```console
  # nix flake check
  ```

* Validate without running builders or import from derivation:

  ```console
  # nix flake check --no-build
  ```

* Validate foreign-system outputs and collect independent failures:

  ```console
  # nix flake check --all-systems --keep-going
  ```

# Description

Rust validates each supported flake output and reports invalid values with
its attribute path. `checks.<system>`, `packages.<system>`, and
`devShells.<system>` must contain named derivations. `formatter.<system>`
must be a derivation. Apps, overlays, bundlers, NixOS modules and
configurations, templates, and nested `hydraJobs` jobsets receive their
corresponding shape checks. Function bodies are not invoked.

By default, foreign-system outputs are omitted and named in a warning.
`--all-systems` validates them but still builds only local-system checks.
Each check builds all of its outputs. A warm evaluation cache reuses validated
metadata; the store still checks the build targets on every invocation.

Hydra jobsets always use a separate evaluation session with import from
derivation disabled. Regular outputs use the configured IFD policy when
building; `--no-build` disables IFD for them too.

Unknown output namespaces and deprecated aliases such as `overlay`,
`defaultPackage`, and `devShell` are rejected explicitly. `legacyPackages`
and community namespaces without a validator are also rejected; they do not
silently count as checked.

)""
