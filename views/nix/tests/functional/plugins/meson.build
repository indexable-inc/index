libplugintest = shared_module(
  'plugintest',
  'plugintest.cc',
  dependencies : [
    dependency('nix-util'),
  ],
  build_by_default : false,
)

libpathrootlookup = shared_module(
  'pathrootlookup',
  'pathrootlookup.cc',
  dependencies : [
    nix_cmd,
  ],
  build_by_default : false,
)
