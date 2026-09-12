defmodule IxMcp.Forge.Verdicts do
  @moduledoc """
  The forge CI feed: a gate run that reaches a verdict becomes one channel
  line, so a session learns that main went green or red without asking.

  The ask this answers is a measured failure, not a nicety. CI verdicts were
  pull-only, so an agent that had submitted a change had two options: block
  on a waiter, or come back later and guess. Twice on 2026-08-12 a session
  guessed wrong and recorded a landing that had not happened. A verdict that
  arrives on its own removes the guess.

  ## Where the facts come from

  `IxMcp.Forge.Runs` owns reading the reconciler's per-run `progress.json`
  records and pulling detail out of a gate's `log_tail`; it also documents
  why `queue.json` and `jj forge dump` are the wrong instruments and how the
  read fails closed. This module is one of its two consumers: it sweeps by
  file mtime, because a record is rewritten when its status changes, so the
  mtime IS the moment the verdict was reached and a run that started two
  hours ago and finished just now is still inside the window. The other
  consumer, `IxMcp.Stdlib.Forge`, waits on one named change instead.

  A failed read becomes `{:error, detail}`, and `IxMcp.Inbox.Watcher` then
  backs off WITHOUT advancing its watermark, so a blinked tailnet loses no
  verdict.

  ## Configuration

  Unlike the inbox feeds this one is not on by default, because it cannot
  be: it names a specific machine, and this tree is public. The inbox feeds
  default to Beeper's documented loopback endpoint, which is "not anything
  specific to one machine"; a forge host is the opposite, so its identity
  crosses at deploy time only, the same way the fleet warning catalog's
  module name does in `IxMcp.Application`.

    * `IX_MCP_FORGE_CI` -- `[user@host:]<runs-dir>`, e.g.
      `root@some-host:/srv/ci/runs` for a remote forge or a bare absolute
      path when the records are on this machine. Unset means the feed never
      starts and says nothing.
    * `IX_MCP_FORGE_WATCH=0` turns the feed off.
    * `IX_MCP_FORGE_WATCH_INTERVAL_MS` overrides the 60s cadence.
    * `IX_MCP_FORGE_WATCH_BACKFILL_S` overrides how far back the FIRST sweep
      looks (default 600s). A gate run outlives a kernel restart, so a
      session that starts just after one would otherwise never hear the
      verdict of the run it was waiting on.

  Reads only. Nothing here submits, retries, or writes to the forge.
  """

  @behaviour IxMcp.Inbox.Source

  alias IxMcp.Forge.Runs
  alias IxMcp.Forge.VerdictAnnounce
  alias IxMcp.Inbox.Source

  require Logger

  @default_interval_ms 60_000
  @default_backfill_s 600
  # `passed` and `failed` are the two terminal members of the reconciler's
  # RunStatus; `materializing` and `building` are live and must not be
  # announced as a verdict.
  @terminal ["passed", "failed"]
  @id_chars 12

  @impl true
  def label, do: "forge"

  @impl true
  def renderer, do: VerdictAnnounce

  @impl true
  def initial_backfill_s do
    Source.interval_from_env("IX_MCP_FORGE_WATCH_BACKFILL_S", @default_backfill_s)
  end

  @impl true
  def default_interval_ms do
    Source.interval_from_env("IX_MCP_FORGE_WATCH_INTERVAL_MS", @default_interval_ms)
  end

  @impl true
  def init(opts) do
    with true <- System.get_env("IX_MCP_FORGE_WATCH") != "0",
         {:ok, target} <- target(opts),
         {:ok, read} <- reader(opts, target) do
      {:ok, %{target: target, read: read}}
    else
      _absent -> :ignore
    end
  end

  @impl true
  def fetch(state, since, limit) do
    # Per-poll heartbeat: a poller that only speaks when it has news is
    # indistinguishable from a poller that died. Debug level, because in
    # steady state this is one line a minute forever.
    Logger.debug("forge verdicts sweep since=#{DateTime.to_iso8601(since)}")

    with {:ok, output} <- state.read.(since) do
      {kept, more?} = terminal(output, limit)

      if kept != [] do
        Logger.info("forge verdicts: #{length(kept)} new terminal run(s)")
      end

      {:ok, Enum.map(kept, &item/1), more?, state}
    end
  end

  # Newest first, capped at the limit, then reversed so the announcements read
  # in the order the verdicts happened. A cap drops the OLDEST unseen runs,
  # which is why overflow is reported rather than swallowed.
  @spec terminal(String.t(), pos_integer()) :: {[map()], boolean()}
  defp terminal(output, limit) do
    records = Runs.records(output)

    runs =
      records
      |> Enum.filter(&(&1["status"] in @terminal))
      |> Enum.sort_by(&updated_at(&1), :desc)

    {kept, rest} = Enum.split(runs, limit)
    # Overflow is also true when the READ itself hit its cap, not only when
    # the terminal runs did: past the cap the reader stopped listing, so
    # older verdicts may exist that this sweep cannot see. Saying nothing
    # there would make a truncated read look like a quiet window, which is
    # the one failure this feed may not have.
    {Enum.reverse(kept), rest != [] or length(records) >= Runs.read_cap()}
  end

  @spec item(map()) :: VerdictAnnounce.item()
  defp item(record) do
    verdict = if record["status"] == "passed", do: :pass, else: :fail
    # A pass needs no garnish, and asking the log for it would be work
    # nobody reads.
    detail = if verdict == :fail, do: Runs.detail(record["log_tail"]), else: Runs.detail(nil)

    %{
      id: to_string(record["run_id"]),
      verdict: verdict,
      change_id: short(record["change_id"]),
      commit_id: short(record["commit_id"]),
      target: short_text(record["target_bookmark"]),
      duration_ms: duration_ms(record),
      failed_stages: detail.failed_stages,
      tolerated: detail.tolerated,
      log: detail.log
    }
  end

  # `started_at_ms` is when the run began, i.e. after it was dequeued: the
  # queue wait is not in this record, so this is run duration and the
  # renderer must not call it anything else. `nil` rather than 0 when either
  # end is missing, so an unknown duration cannot render as an instant one.
  defp duration_ms(record) do
    case {record["started_at_ms"], record["updated_at_ms"]} do
      {started, updated}
      when is_integer(started) and is_integer(updated) and updated >= started ->
        updated - started

      _unusable ->
        nil
    end
  end

  defp updated_at(record) do
    case record["updated_at_ms"] do
      updated when is_integer(updated) -> updated
      _absent -> 0
    end
  end

  defp short(value) when is_binary(value), do: String.slice(value, 0, @id_chars)
  defp short(_absent), do: "?"

  defp short_text(value) when is_binary(value) and value != "", do: value
  defp short_text(_absent), do: "?"

  defp target(opts) do
    case Keyword.get(opts, :target, System.get_env("IX_MCP_FORGE_CI")) do
      value when is_binary(value) -> Runs.parse_target(value)
      _unset -> :error
    end
  end

  # A test injects `:read`; otherwise the mode is decided once, here, so a
  # sweep never has to ask.
  defp reader(opts, target) do
    case Keyword.fetch(opts, :read) do
      {:ok, read} -> {:ok, read}
      :error -> Runs.reader(target)
    end
  end
end
