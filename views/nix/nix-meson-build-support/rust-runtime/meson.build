# Independent providers keep store policies below the evaluator dependency graph.
# Consumers use their provider's exact identity, never ambient runtime discovery.
if meson.project_name() in ['nix-store', 'nix-fetchers']
  rust_runtime_package = 'nix-host-rs'
  rust_runtime_provider = 'nix-store'
  rust_runtime_option = 'rust-host'
  rust_runtime_source_suffix = 'rust' / 'nix-host-rs'
  rust_runtime_metadata_prefix = 'rust_host_runtime'
else
  rust_runtime_package = 'nix-eval-rs'
  rust_runtime_provider = 'nix-flake'
  rust_runtime_option = 'rust-eval'
  rust_runtime_source_suffix = 'rust'
  rust_runtime_metadata_prefix = 'rust_runtime'
endif
rust_runtime_prefix = get_option(rust_runtime_option + '-prefix')
rust_runtime_library_stem = 'lib' + rust_runtime_package.replace('-', '_')
if host_machine.system() == 'darwin'
  rust_runtime_library_name = rust_runtime_library_stem + '.dylib'
elif host_machine.system() == 'linux'
  rust_runtime_library_name = rust_runtime_library_stem + '.so'
else
  error('The shared Rust runtime supports Linux and Darwin')
endif

if meson.project_name() == rust_runtime_provider
  if rust_runtime_prefix != ''
    rust_runtime_build_rpath = rust_runtime_prefix / 'lib'
    rust_runtime_install_rpath = rust_runtime_build_rpath
    rust_runtime_install_includedir = rust_runtime_prefix / 'include'
    rust_runtime_library = rust_runtime_build_rpath / rust_runtime_library_name
    if not import('fs').is_file(rust_runtime_library)
      error('Missing shared Rust runtime: ' + rust_runtime_library)
    endif
    rust_runtime_dependency = declare_dependency(
      link_args : [rust_runtime_library],
      compile_args : ['-I' + rust_runtime_prefix / 'include'],
      variables : {
        'build_rpath' : rust_runtime_build_rpath,
        'install_rpath' : rust_runtime_install_rpath,
      },
    )
  else
    if meson.is_cross_build()
      error('Cross builds require a Rust runtime built for the host via its explicit prefix option')
    endif
    rust_runtime_cargo = find_program('cargo', required : true)
    rust_runtime_source = meson.project_source_root() / '..' / '..' / rust_runtime_source_suffix
    rust_runtime_headers = rust_runtime_source / 'include'
    if rust_runtime_provider == 'nix-flake'
      rust_runtime_headers = rust_runtime_source / rust_runtime_package / 'include'
    endif
    rust_runtime_build_rpath = meson.current_build_dir()
    rust_runtime_install_rpath = get_option('prefix') / get_option('libdir')
    rust_runtime_install_includedir = get_option('prefix') / get_option('includedir')
    rust_runtime_cargo_flags = []
    foreach flag : get_option(rust_runtime_option + '-cargo-features').split(' ')
      if flag != ''
        rust_runtime_cargo_flags += flag
      endif
    endforeach
    rust_runtime_target = custom_target(
      rust_runtime_package,
      output : rust_runtime_library_name,
      command : [
        find_program('sh'), files('build.sh'),
        rust_runtime_cargo, rust_runtime_source,
        meson.current_build_dir() / 'cargo-target',
        '@OUTPUT@', host_machine.system(), rust_runtime_package,
      ] + rust_runtime_cargo_flags,
      build_always_stale : true,
      install : true,
      install_dir : get_option('libdir'),
    )
    rust_runtime_dependency = declare_dependency(
      sources : [rust_runtime_target],
      link_args : [rust_runtime_target.full_path()],
      compile_args : ['-I' + rust_runtime_headers],
      variables : {
        'build_rpath' : rust_runtime_build_rpath,
        'install_rpath' : rust_runtime_install_rpath,
      },
    )
    install_subdir(
      rust_runtime_headers,
      install_dir : get_option('includedir'), strip_directory : true,
    )
    import('pkgconfig').generate(
      filebase : rust_runtime_package, name : rust_runtime_package, version : '0.1.0',
      description : 'Shared Rust runtime for Nix evaluation and commands',
      libraries : ['-L${libdir}', '-l' + rust_runtime_package.replace('-', '_')],
      subdirs : '.',
    )
  endif
  meson.override_dependency(rust_runtime_package, rust_runtime_dependency, static : false)
else
  rust_runtime_owner = dependency(rust_runtime_provider)
  rust_runtime_install_rpath = rust_runtime_owner.get_variable(
    internal : rust_runtime_metadata_prefix + '_libdir', pkgconfig : rust_runtime_metadata_prefix + '_libdir',
  )
  rust_runtime_install_includedir = rust_runtime_owner.get_variable(
    internal : rust_runtime_metadata_prefix + '_includedir', pkgconfig : rust_runtime_metadata_prefix + '_includedir',
  )
  if rust_runtime_prefix != '' and (
    rust_runtime_install_rpath != rust_runtime_prefix / 'lib'
    or rust_runtime_install_includedir != rust_runtime_prefix / 'include'
  )
    error('Rust runtime prefix disagrees with the runtime recorded by ' + rust_runtime_provider)
  endif
  if rust_runtime_owner.type_name() == 'internal'
    rust_runtime_dependency = dependency(rust_runtime_package, static : false)
    if rust_runtime_dependency.type_name() != 'internal'
      error('The ' + rust_runtime_provider + ' subproject did not register its Rust runtime')
    endif
    if rust_runtime_dependency.get_variable(internal : 'install_rpath') != rust_runtime_install_rpath
      error('Registered Rust runtime disagrees with the runtime recorded by ' + rust_runtime_provider)
    endif
    rust_runtime_build_rpath = rust_runtime_dependency.get_variable(internal : 'build_rpath')
  else
    # Link the recorded file directly: a generic library lookup could select
    # a different installed runtime through an earlier -L or pkg-config path.
    rust_runtime_build_rpath = rust_runtime_install_rpath
    rust_runtime_library = rust_runtime_install_rpath / rust_runtime_library_name
    if not import('fs').is_file(rust_runtime_library)
      error('Missing Rust runtime recorded by ' + rust_runtime_provider + ': ' + rust_runtime_library)
    endif
    rust_runtime_dependency = declare_dependency(
      link_args : [rust_runtime_library],
      compile_args : ['-I' + rust_runtime_install_includedir],
    )
  endif
endif

deps_other += rust_runtime_dependency
# Static C++ consumers still link this exact shared Rust runtime.
rust_runtime_libraries_private = [rust_runtime_install_rpath / rust_runtime_library_name]
