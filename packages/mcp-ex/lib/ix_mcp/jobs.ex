defmodule IxMcp.Jobs do
  @moduledoc """
  The job registry facade -- available in every cell (the workspace prelude
  aliases it), so job control needs no extra tools, exactly like the Python
  kernel's `jobs` dict:

      Jobs.tail("ab12", 20)     # last lines of a run's output
      Jobs.await("ab12")        # block this cell (only this cell) until done
      Jobs.cancel("ab12")       # kill it, and every OS process it spawned
      Jobs.history()            # recent runs, newest first
      Jobs.watch("ab12")        # announce another session's job here (#3934)

  Runs live in a durable ledger (`IxMcp.ActionLog`), not just in memory
  (#3839): `tail/head/grep/output/history/get` fall back to the SQLite tables
  when the job process is gone, so a crashed or killed run is still readable
  and still on record. `history/1` is a view over the `jobs` table scoped to
  this server instance's session.

  Live reads go through the job's Registry-value snapshot (#4082) -- an ETS
  read -- rather than GenServer calls into the control process, which under
  load can sit parked in a ledger write far past the default call timeout.
  """

  # `spawn/1` (code only) would otherwise clash with Kernel's auto-imported
  # spawn/1; cells always call this module qualified, so the exclusion is
  # invisible to them.
  import Kernel, except: [spawn: 1]

  alias IxMcp.ActionLog
  alias IxMcp.Jobs.Job
  alias IxMcp.MCP.Notifier
  alias IxMcp.Session

  @typedoc "A recorded run, as `history/1` returns it."
  @type entry :: %{
          id: String.t(),
          intent: String.t() | nil,
          session: String.t() | nil,
          topic: String.t() | nil,
          code: String.t(),
          status: Job.status(),
          started_at: DateTime.t() | nil,
          elapsed_s: float() | nil
        }

  @doc """
  Start `code` as a new job and wait up to `budget_s` seconds. Finished or
  not, returns `{summary, output}` -- when still running, the job continues
  in the background under its returned id. A bare number as the second
  argument is the budget in seconds: `Jobs.run(code, 30)` and
  `Jobs.run(code, budget: 30)` mean the same thing. `workspace: "name"`
  targets a named workspace instead of "main".
  """
  @spec run(String.t(), keyword() | number()) :: {Job.summary(), String.t()}
  def run(code, budget_s) when is_binary(code) and is_number(budget_s) do
    run(code, budget: budget_s)
  end

  def run(code, opts) when is_binary(code) and is_list(opts) do
    budget_s = Keyword.get(opts, :budget, 15)
    id = generate_id()

    {:ok, pid} =
      DynamicSupervisor.start_child(
        IxMcp.Jobs.Supervisor,
        {Job, {id, code, Keyword.take(opts, [:intent, :action_id, :watch, :workspace])}}
      )

    # Read back by id, not by 5s GenServer calls into the control process
    # (#4082): a process parked in a ledger write under load answers no
    # call inside the default timeout, and that exit killed the exec
    # handler. The id path reads the Registry-value snapshot (falling back
    # to the ledger), which answers regardless.
    case Job.await(pid, round(budget_s * 1000)) do
      {:ok, summary} -> {summary, output(id)}
      :timeout -> {get(id), output(id)}
    end
  end

  @spec run(String.t()) :: {Job.summary(), String.t()}
  def run(code), do: run(code, [])

  @doc """
  Start `code` in the background and return immediately with its running
  summary -- `Jobs.run/2` with a zero budget, under the name agents reach
  for first. `Jobs.start/2` is the same function. Read it later with
  `Jobs.tail(id)` / `Jobs.await(id)`.
  """
  @spec spawn(String.t(), keyword()) :: {Job.summary(), String.t()}
  def spawn(code, opts \\ []) when is_binary(code) and is_list(opts) do
    run(code, Keyword.put(opts, :budget, 0))
  end

  @doc "Alias of `spawn/2`: start `code` as a background job, return at once."
  @spec start(String.t(), keyword()) :: {Job.summary(), String.t()}
  def start(code, opts \\ []) when is_binary(code) and is_list(opts) do
    run(code, Keyword.put(opts, :budget, 0))
  end

  @doc "Look up a job process by id."
  @spec lookup(String.t()) :: {:ok, pid()} | {:error, :not_found}
  def lookup(id) do
    case Registry.lookup(IxMcp.Jobs.Registry, id) do
      [{pid, _}] -> {:ok, pid}
      [] -> {:error, :not_found}
    end
  end

  @doc "Summary of a job -- from its snapshot or process, or reconstructed from the ledger."
  @spec get(String.t()) :: Job.summary()
  def get(id) do
    # Snapshot first (#4082): an ETS read that stays answerable while the
    # control process sits in a ledger call. The call path only covers the
    # sliver before the job publishes its snapshot in init.
    case Job.read_summary(id) do
      nil -> live_or_ledger(id, &Job.summary/1, &summary_from_ledger/1)
      summary -> summary
    end
  end

  @doc """
  Block the calling process (a cell, never the server) until the job
  finishes. Awaiting from a cell marks that cell's own job a quiet wrapper
  (#3934): it is reading another job's terminal state, so its own clean
  finish is not news and announces nothing -- its failure or death still
  does, and the awaited job announces (or is suppressed) on its own merits.
  """
  @spec await(String.t(), timeout()) :: Job.summary() | :timeout
  def await(id, timeout_ms \\ :infinity) do
    quiet_own_job()

    with_job(id, fn pid ->
      case Job.await(pid, timeout_ms) do
        {:ok, summary} -> summary
        :timeout -> :timeout
      end
    end)
  end

  @doc """
  Subscribe this session to another job's or session's terminal
  transitions (#3934) -- how a parent follows a child agent's work without
  every session broadcasting to every other. `watch("ab12")` announces
  that job's finish once (immediately, when it is already terminal);
  `watch(session: 7)` announces every future finish of session 7 (ids via
  `Sessions.list/0`, #3881). Watches poll the shared ledger, live until
  this kernel exits, and are announced on the channel like any job finish.
  """
  @spec watch(String.t() | [session: integer()]) :: :ok
  def watch(job_id) when is_binary(job_id) do
    if ActionLog.job(job_id) == nil do
      raise ArgumentError, "no such job: #{inspect(job_id)}"
    end

    Notifier.watch(Session.ids().session_id, {:job, job_id})
  end

  def watch(session: watched) when is_integer(watched) do
    %{session_id: own} = Session.ids()

    if watched == own do
      raise ArgumentError, "session #{watched} is this session; its jobs already announce here"
    end

    Notifier.watch(own, {:session, watched})
  end

  # The cell learns its own job id from the eval process's dictionary
  # (planted at spawn); outside a cell there is nothing to mark.
  defp quiet_own_job do
    with id when is_binary(id) <- Process.get(:ix_job_id),
         {:ok, pid} <- lookup(id) do
      Job.quiet(pid)
    else
      _not_a_cell -> :ok
    end
  end

  @spec cancel(String.t()) :: :ok | {:error, :finished | Job.status()}
  def cancel(id) do
    case lookup(id) do
      {:ok, pid} ->
        try do
          Job.cancel(pid)
        catch
          # The registry unregisters dead pids asynchronously, so a lookup
          # can briefly return a process that no longer exists; that
          # :noproc means the same thing as a missed lookup (#3538).
          :exit, {:noproc, _call} -> report_dead(id)
        end

      {:error, :not_found} ->
        report_dead(id)
    end
  end

  # A job whose process is gone cannot be cancelled, but the ledger still
  # holds its terminal state (#3839): report it. Raise only for ids this
  # server never ran -- #3538 showed that raising "no such job" about an id
  # `history/1` still listed sent the operator chasing a phantom.
  defp report_dead(id) do
    case ActionLog.job(id) do
      %{status: status} -> {:error, status}
      nil -> raise ArgumentError, "no such job: #{inspect(id)}"
    end
  end

  @spec result(String.t()) :: {:ok, term()} | {:error, :running | String.t()}
  def result(id) do
    live_or_ledger(id, &Job.result/1, &result_from_ledger/1)
  end

  # The result term dies with the process; the ledger keeps only its rendered
  # form, so a gone job answers with that string, never a live term.
  defp result_from_ledger(id) do
    case ActionLog.job(id) do
      nil -> raise ArgumentError, "no such job: #{inspect(id)}"
      %{status: :running} -> {:error, :running}
      %{result: result} -> {:error, result || "job no longer resident"}
    end
  end

  @doc """
  Full captured output -- from the live buffer, or the durable table if the job
  is gone.

  When the job hit `Job.output_cap/0` this ends with a truncation notice, so
  every derived read (`tail/head/lines/slice/grep`) carries it too. Without it
  they answer confidently about output that is not there: what the buffer holds
  is the HEAD, so `tail` returns lines from the middle of the run and `grep`
  answers "" for a pattern the cell really did print (#4306).
  """
  @spec output(String.t()) :: String.t()
  def output(id) do
    # Snapshot first (#4082), same reasoning as `get/1`: the hot buffer is
    # public ETS, so reading it must not queue behind a parked control
    # process.
    body =
      case Job.read_output(id) do
        nil -> live_or_ledger(id, &Job.output/1, &output_from_ledger/1)
        output -> output
      end

    body <> truncation_notice(id, body)
  end

  # The drop count lives beside the output on both paths: counter 2 of the
  # live snapshot, `output_dropped` in the ledger once the job is gone.
  defp truncation_notice(id, body) do
    case dropped_bytes(id) do
      dropped when is_integer(dropped) and dropped > 0 ->
        "\n[output truncated at the #{Job.output_cap()}-byte per-job cap: " <>
          "#{byte_size(body)} bytes kept, #{dropped} dropped. This is the HEAD " <>
          "of the output -- the end of the run is gone, so tail/grep cannot see it.]"

      _none ->
        ""
    end
  end

  defp dropped_bytes(id) do
    case Job.read_dropped(id) do
      nil ->
        case ActionLog.job(id) do
          %{output_dropped: dropped} -> dropped
          nil -> nil
        end

      dropped ->
        dropped
    end
  end

  defp output_from_ledger(id) do
    case ActionLog.job(id) do
      nil -> raise ArgumentError, "no such job: #{inspect(id)}"
      _job -> ActionLog.job_output(id)
    end
  end

  @doc "Last `n` lines of the job's output."
  @spec tail(String.t(), pos_integer()) :: String.t()
  def tail(id, n \\ 20) do
    id |> lines() |> Enum.take(-n) |> Enum.join("\n")
  end

  @doc "First `n` lines of the job's output."
  @spec head(String.t(), pos_integer()) :: String.t()
  def head(id, n \\ 20) do
    id |> lines() |> Enum.take(n) |> Enum.join("\n")
  end

  @doc "Lines `first..last` (1-based, inclusive)."
  @spec lines(String.t(), pos_integer(), pos_integer()) :: String.t()
  def lines(id, first, last) when first >= 1 and last >= first do
    id |> lines() |> Enum.slice((first - 1)..(last - 1)) |> Enum.join("\n")
  end

  @doc "Lines `[from, to)` (0-based, exclusive), Python-slice style."
  @spec slice(String.t(), non_neg_integer(), non_neg_integer()) :: String.t()
  def slice(id, from, to) when from >= 0 and to >= from do
    id |> lines() |> Enum.slice(from, to - from) |> Enum.join("\n")
  end

  @doc "Output lines matching `pattern` (a regex or a plain substring)."
  @spec grep(String.t(), Regex.t() | String.t()) :: String.t()
  def grep(id, %Regex{} = pattern) do
    id |> lines() |> Enum.filter(&Regex.match?(pattern, &1)) |> Enum.join("\n")
  end

  def grep(id, pattern) when is_binary(pattern) do
    id |> lines() |> Enum.filter(&String.contains?(&1, pattern)) |> Enum.join("\n")
  end

  @doc "Ids and summaries of jobs that are still running."
  @spec running() :: [Job.summary()]
  def running do
    # Snapshot reads (#4082): scanning with per-child GenServer calls would
    # stall on the first control process parked in a ledger write.
    IxMcp.Jobs.Registry
    |> Registry.select([{{:"$1", :_, :_}, [], [:"$1"]}])
    |> Enum.flat_map(fn id ->
      case Job.read_summary(id) do
        nil -> []
        summary -> [summary]
      end
    end)
    |> Enum.filter(& &1.running)
  end

  @doc "Recent runs of this server instance, newest first."
  @spec history(pos_integer()) :: [entry()]
  def history(n \\ 20) do
    %{session_id: session_id} = Session.ids()

    session_id
    |> ActionLog.recent_jobs(n)
    |> Enum.map(&to_entry/1)
  end

  defp lines(id) do
    id |> output() |> String.split("\n")
  end

  defp with_job(id, fun) do
    case lookup(id) do
      {:ok, pid} -> fun.(pid)
      {:error, :not_found} -> raise ArgumentError, "no such job: #{inspect(id)}"
    end
  end

  # Read from the live process, falling back to the durable ledger when it is
  # gone -- including the registered-but-dead-pid window: the Registry
  # unregisters dead pids asynchronously (#3538), so a lookup can hand back a
  # corpse whose GenServer.call exits :noproc. That means the same as a
  # missing process (#3839).
  defp live_or_ledger(id, live_fun, dead_fun) do
    case lookup(id) do
      {:ok, pid} ->
        try do
          live_fun.(pid)
        catch
          :exit, {reason, _call} when reason in [:noproc, :normal] -> dead_fun.(id)
        end

      {:error, :not_found} ->
        dead_fun.(id)
    end
  end

  defp summary_from_ledger(id) do
    case ActionLog.job(id) do
      nil ->
        raise ArgumentError, "no such job: #{inspect(id)}"

      job ->
        %{
          id: job.id,
          status: job.status,
          running: job.status == :running,
          intent: job.intent,
          elapsed_s: (job.elapsed_ms || 0) / 1000,
          output_bytes: job.output_bytes,
          output_dropped: job.output_dropped,
          diagnostics: [],
          result: job.result
        }
    end
  end

  defp to_entry(job) do
    %{
      id: job.id,
      intent: job.intent,
      session: job.session,
      topic: job.topic,
      code: job.code,
      status: job.status,
      started_at: parse_dt(job.started_at),
      elapsed_s: if(job.elapsed_ms, do: job.elapsed_ms / 1000)
    }
  end

  defp parse_dt(nil), do: nil

  defp parse_dt(iso) do
    case DateTime.from_iso8601(iso) do
      {:ok, dt, _} -> dt
      _ -> nil
    end
  end

  defp generate_id do
    Base.encode16(:crypto.strong_rand_bytes(4), case: :lower)
  end
end
