#!/bin/sh
set -eu

cargo=$1
source_dir=$2
target_dir=$3
output=$4
system=$5
package=$6
shift 6
library_stem=lib$(printf '%s' "$package" | tr '-' '_')

case "$system" in
    linux)
        library=$library_stem.so
        identity=-Wl,-soname,$library
        ;;
    darwin)
        library=$library_stem.dylib
        identity=-Wl,-headerpad_max_install_names,-install_name,@rpath/$library
        ;;
    *)
        echo "unsupported shared Rust runtime platform: $system" >&2
        exit 1
        ;;
esac

# rustc arguments apply only to this crate, not to host proc-macro dylibs.
"$cargo" rustc --release --locked --manifest-path "$source_dir/Cargo.toml" \
    -p "$package" --lib --target-dir "$target_dir" "$@" -- -C "link-arg=$identity"
# Ninja restat can avoid relinking C++ when Cargo found no changed Rust input.
if ! cmp -s "$target_dir/release/$library" "$output"; then
    cp "$target_dir/release/$library" "$output"
fi
