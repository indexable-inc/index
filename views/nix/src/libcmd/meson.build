project(
  'nix-cmd',
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

configdata = configuration_data()

deps_private_maybe_subproject = []
deps_public_maybe_subproject = [
  dependency('nix-util'),
  dependency('nix-store'),
  dependency('nix-fetchers'),
  dependency('nix-expr'),
  dependency('nix-flake'),
  dependency('nix-main'),
]
subdir('nix-meson-build-support/subprojects')

nlohmann_json = dependency('nlohmann_json', version : '>= 3.9')
deps_public += nlohmann_json

lowdown = dependency(
  'lowdown',
  version : '>= 0.9.0',
  required : get_option('markdown'),
)
deps_private += lowdown
configdata.set('HAVE_LOWDOWN', lowdown.found().to_int())
# The API changed slightly around terminal initialization.
configdata.set(
  'HAVE_LOWDOWN_1_4',
  lowdown.version().version_compare('>= 1.4.0').to_int(),
)
configdata.set(
  'HAVE_LOWDOWN_3',
  lowdown.version().version_compare('>= 3.0.0').to_int(),
)

config_priv_h = configure_file(
  configuration : configdata,
  output : 'cmd-config-private.hh',
)

subdir('nix-meson-build-support/common')

sources = files(
  'app.cc',
  'built-path.cc',
  'command-installable-value.cc',
  'command.cc',
  'common-eval-args.cc',
  'editor-for.cc',
  'get-build-log.cc',
  'installable-derived-path.cc',
  'installable-flake.cc',
  'installable-value.cc',
  'installables.cc',
  'markdown.cc',
  'misc-store-flags.cc',
  'network-proxy.cc',
  'rust-closure-diff.cc',
  'rust-persistent.cc',
  'rust-eval-session.cc',
  'rust-search.cc',
)

if host_machine.system() != 'windows'
  sources += files(
    'unix/unix-socket-server.cc',
  )
endif

subdir('nix-meson-build-support/rust-runtime')

subdir('include/nix/cmd')

subdir('nix-meson-build-support/export-all-symbols')
subdir('nix-meson-build-support/windows-version')

this_library = library(
  'nixcmd',
  sources,
  config_priv_h,
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

install_headers(headers, subdir : 'nix/cmd', preserve_path : true)

# The phantom-token guard. Its own executable because it needs both the
# command layer's constants and the evaluator's ABI vocabulary linked, and
# this component links the shared Rust runtime.
refusal_vocabulary_test = executable(
  'nix-refusal-vocabulary-test',
  files('rust-eval-refusal-vocabulary-test.cc'),
  dependencies : deps_public + deps_private + deps_other,
  include_directories : include_dirs,
  build_rpath : rust_runtime_build_rpath,
)
test(
  'refusal-vocabulary',
  refusal_vocabulary_test,
  suite : 'nix-cmd',
  protocol : 'exitcode',
)

perf_scope_test = executable(
  'nix-rust-eval-perf-scope-test',
  files('rust-eval-perf-scope-test.cc'),
  include_directories : include_dirs,
)
test('rust-eval-perf-scope', perf_scope_test, suite : 'nix-cmd')

libraries_private = rust_runtime_libraries_private

subdir('nix-meson-build-support/export')
