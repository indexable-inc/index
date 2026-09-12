project(
  'nix-fetchers',
  'cpp',
  version : files('.version'),
  default_options : [
    'cpp_std=c++23',
    # TODO(Qyriad): increase the warning level
    'warning_level=1',
    'errorlogs=true', # Please print logs for tests that fail
  ],
  meson_version : '>= 1.1',
  license : 'LGPL-2.1-or-later',
)

cxx = meson.get_compiler('cpp')

subdir('nix-meson-build-support/deps-lists')

configuration_data()

deps_private_maybe_subproject = []
deps_public_maybe_subproject = [
  dependency('nix-util'),
  dependency('nix-store'),
]
subdir('nix-meson-build-support/subprojects')

nlohmann_json = dependency('nlohmann_json', version : '>= 3.9')
deps_public += nlohmann_json

libgit2 = dependency('libgit2', version : '>= 1.9')
deps_private += libgit2

# The in-process jj tree reader (see meson.options). Mirrors how src/nix links
# the prebuilt Rust evaluator: `include_directories()` rejects absolute paths
# and the prefix is outside the source tree by construction, so the include
# path travels as a compile arg and the archive as a link arg.
jj_tree_prefix = get_option('jj-tree-prefix')
if jj_tree_prefix == ''
  error('jj-tree-prefix must name a prefix holding lib/libjj_tree.a and include/jj_tree.h; the jj fetcher cannot be built without it')
endif
deps_private += declare_dependency(
  link_args : [ jj_tree_prefix / 'lib' / 'libjj_tree.a' ],
  compile_args : [ '-I' + jj_tree_prefix / 'include' ],
)

# The archive's own transitive native dependencies, which only a Darwin link
# needs naming. A static archive records no dependency of its own, so whatever
# it calls into has to appear on the link line of the library that pulls it in,
# and that library is this one. `libjj_tree.a` vendors jj-backend's RPC
# transport, which reaches rustls through `rustls-platform-verifier`; on Darwin
# that crate's backend calls Security.framework to evaluate a certificate chain
# and CoreFoundation for the CF types those APIs take and return. The symbols
# come from the OS, not from any nix package, so nothing in the build inputs can
# supply them: without this, linking libnixfetchers.dylib fails with undefined
# `_Authorization*`, `_CF*` and `_Sec*` referenced from
# `libjj_tree.a(security_framework-*.rcgu.o)` and `(rustls_platform_verifier-*.rcgu.o)`.
# The same closure carries the `whoami` crate (a dependency of jj-cli, which
# the archive vendors for the working-copy snapshot; it supplies the
# user/host defaults for commit metadata), whose macOS `devicename` calls
# `SCDynamicStoreCopyComputerName`: one more framework, SystemConfiguration,
# for the one undefined `_SCDynamicStoreCopyComputerName` referenced from
# `libjj_tree.a(whoami-*.rcgu.o)`. Each entry here names the object that
# needs it, so a framework that stops being needed can be removed by the
# same evidence that added it (a link error naming the object).
# Linux needs no counterpart, and links green today: there the same closure
# resolves against the ordinary system C library.
#
# `deps_other`, because Apple frameworks ship no pkg-config file (see
# nix-meson-build-support/deps-lists); this is the list src/libstore already
# uses for its Darwin `sandbox` and `compression` link inputs.
if host_machine.system() == 'darwin'
  deps_other += dependency('appleframeworks', modules : [ 'Security', 'CoreFoundation', 'SystemConfiguration' ])
endif

subdir('nix-meson-build-support/common')
subdir('nix-meson-build-support/rust-runtime')

sources = files(
  'attrs.cc',
  'cache.cc',
  'fetch-settings.cc',
  'fetch-to-store.cc',
  'fetchers.cc',
  'filtering-source-accessor.cc',
  'git-lfs-fetch.cc',
  'git-utils.cc',
  'git.cc',
  'github.cc',
  'indirect.cc',
  'input-cache.cc',
  'jj-tree.cc',
  'jj.cc',
  'mercurial.cc',
  'path.cc',
  'rust-registry.cc',
  'tarball.cc',
)

subdir('include/nix/fetchers')

subdir('nix-meson-build-support/export-all-symbols')
subdir('nix-meson-build-support/windows-version')

this_library = library(
  'nixfetchers',
  sources,
  soversion : nix_soversion,
  dependencies : deps_public + deps_private + deps_other,
  include_directories : include_dirs,
  link_args : linker_export_flags,
  build_rpath : rust_runtime_build_rpath,
  install_rpath : rust_runtime_install_rpath,
  prelink : true, # For C++ static initializers
  install : true,
  cpp_pch : do_pch ? [ 'pch/precompiled-headers.hh' ] : [],
)

install_headers(headers, subdir : 'nix/fetchers', preserve_path : true)

libraries_private = rust_runtime_libraries_private

subdir('nix-meson-build-support/export')
