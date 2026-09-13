import Config

# Stdout belongs to the MCP JSON-RPC wire; every log line must go to stderr
# or it would corrupt the protocol stream.
config :logger, :default_handler, config: [type: :standard_error]

# Build exqlite's NIF from its vendored sqlite3 source instead of downloading
# a precompiled artefact: the sandboxed nix builds have no network, and a cc
# build from the hex tarball is reproducible everywhere else too.
config :elixir_make, :force_build, exqlite: true

# The fleet warning policy: which module supplies FleetMesh.Engine's
# conditions. Empty is the honest public default (this tree carries no
# catalog); the deployed kernel overrides it at boot from
# IX_MCP_FLEET_POLICY (see IxMcp.Application), pointing at the private
# fleet-policy app.
config :fleet_mesh, policy: FleetMesh.Policy.Empty

# Tests keep the action log in memory: the sandboxed check has no writable
# HOME, and no test should touch the operator's real log file.
if config_env() == :test do
  config :ix_mcp, actions_db: ":memory:"
  # Fast stack samples so the live-row tests observe one without real waits.
  config :ix_mcp, stack_sample_interval_ms: 25
  # Flush output to the durable table quickly so tests can read a dead job's
  # output without long waits (#3839).
  config :ix_mcp, output_flush_interval_ms: 20
  # A tiny output cap so the over-cap truncation-notice test does not have to
  # produce 8 MiB to reach it.
  config :ix_mcp, output_cap: 2_048
  # Short coalesce/poll windows so the notification tests observe scoped
  # delivery, suppression, and digests without real waits (#3934). The
  # coalesce window still has to outlast the exec reply path's ack under CI
  # load, so it is not arbitrarily small.
  config :ix_mcp, notify_coalesce_ms: 150
  config :ix_mcp, watch_poll_ms: 100
  # The fleet watcher polls ClickHouse over ssh (ENG-11209). Tests drive
  # Watch.run_poll/2, run_heartbeat/2 and run_anomaly/2 directly with stubs; the
  # timers stay disarmed so no test reaches a production host, and so the
  # sandboxed nix check does not read a missing network as a fleet outage.
  config :ix_mcp, fleet_watch_enabled: false
end
