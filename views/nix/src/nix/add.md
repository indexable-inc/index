R""(

# Description

Copy *path* to the Nix store, and print the resulting store path on
standard output.

> **Warning**
>
> Without `--out-link`, the resulting store path is not registered as a
> garbage collector root, so it could be deleted before you register it.

Use `--out-link path` to create a symlink and register a permanent root before
this command returns the store path. Relative link paths are resolved against
the current directory. Existing links into the Nix store can be replaced;
other existing files and directories are refused. This option requires a store
with local root support and cannot be combined with `--dry-run`.

# Examples

Add a directory to the store:

```console
# mkdir dir
# echo foo > dir/bar

# nix store add --out-link ./result ./dir
/nix/store/6pmjx56pm94n66n4qw1nff0y1crm8nqg-dir

# cat /nix/store/6pmjx56pm94n66n4qw1nff0y1crm8nqg-dir/bar
foo
```

)""
