# Public headers directory

include_dirs = [ include_directories('../..') ]

config_pub_h = configure_file(
  configuration : configdata_pub,
  output : 'config.hh',
)

headers = [ config_pub_h ] + files(
  'attr-path.hh',
  'attr-set.hh',
  'counter.hh',
  'diagnose.hh',
  'eval-cache.hh',
  'eval-error.hh',
  'eval-gc.hh',
  'eval-inline.hh',
  'eval-perf-census.hh',
  'eval-profiler-settings.hh',
  'eval-profiler.hh',
  'eval-readset.hh',
  'eval-settings.hh',
  'eval.hh',
  'function-trace.hh',
  'gc-small-vector.hh',
  'get-drvs.hh',
  'json-to-value.hh',
  'nixexpr.hh',
  'value-wait.hh',
  'print-ambiguous.hh',
  'print-options.hh',
  'print.hh',
  'refusal-census.hh',
  'rust-eval-refusal.hh',
  'search-path.hh',
  'static-string-data.hh',
  'symbol-table.hh',
  'value-to-json.hh',
  'value-to-xml.hh',
  'value.hh',
  'value/context.hh',
)
