defmodule IxMcp.Jobs.Job do
  @moduledoc """
  One cell run. The GenServer is the job's control point; the evaluation
  itself happens in a separate spawned process so that a blocking cell blocks
  only itself -- the scheduler preempts it, other jobs and the server keep
  running, and cancellation is `Process.exit(pid, :kill)` plus OS
  subprocess-tree cleanup rather than a signal hack against a wedged loop.

  Processes compute; data remembers (#3839). Every terminal transition
  (`done`/`failed`/`cancelled`/`killed`) is one atomic write to the durable
  ledger (`IxMcp.ActionLog`): the `jobs` row and its `actions` row go terminal
  together and a notification lands in the outbox, so no death is silent and
  the two logs can never disagree. Output streams into the `job_output` table
  as it is produced, so `Jobs.tail/output/result` keep working after the
  process is gone. A hot ETS buffer stays for fast live reads.

  The control process is deliberately survivable. It traps exits and only
  *links* its IOProxy, so an IOProxy crash -- the shared-component failure
  that in #3839 killed every linked job brutally, skipping finish and
  notification -- arrives as a trappable EXIT and becomes a recorded `killed`
  transition with the output captured so far. A hard external kill of the
  control process itself (which trapping cannot catch) is caught by the
  single reaper (`IxMcp.Jobs.Reaper`), which writes the `killed` transition
  from outside.

  The durable ledger must never take a job down with it (#3874): every
  `ActionLog` call from this process goes through `safe_log/1`, which
  absorbs the exit a caller inherits when the log dies mid-request (the
  log's own client API already retries across the restart blip). A failed
  output flush leaves the hot buffer unflushed for the next tick; a failed
  terminal transition re-arms itself (`:record_terminal`) until the ledger
  takes it, so the notification is delayed, never lost.

  Reads never queue behind those ledger calls (#4082): the job publishes a
  snapshot (status, rendered result, buffer/counter handles) as its Registry
  value at init and again at finish, so `Jobs.get`/`Jobs.output` are plain
  ETS reads while this process is parked in a 30s `ActionLog` call -- under
  concurrent load the old call path timed out at the default 5s and killed
  the exec handler. The same load can lose the `job_started` write itself
  (`safe_log` absorbs it), so the start metadata rides with the reaper and
  with the terminal transition, either of which reconstructs the jobs row;
  before that, a lost start row made `finish_job` no-op as `:already_final`
  and the whole run vanished from the record.
  """

  use GenServer, restart: :temporary

  alias IxMcp.ActionLog
  alias IxMcp.Evaluator
  alias IxMcp.Evaluator.IOProxy
  alias IxMcp.Jobs.Reaper
  alias IxMcp.MCP.Notifier
  alias IxMcp.OsProc
  alias IxMcp.Session
  alias IxMcp.Workspace

  require Logger

  # How often a running exec's eval process is stack-sampled into the action
  # log (Process.info(pid, :current_stacktrace) -- external, works on a
  # wedged process). Tests shrink it via app env to keep the suite fast.
  @stack_sample_interval_ms Application.compile_env(:ix_mcp, :stack_sample_interval_ms, 1000)

  # How often buffered output is flushed from the hot ETS buffer to the
  # durable `job_output` table. A hard kill loses at most this window of the
  # very latest output (already-flushed output survives, which is the point).
  @flush_interval_ms Application.compile_env(:ix_mcp, :output_flush_interval_ms, 250)

  # How long a job waits before retrying a terminal transition the ledger
  # could not take (#3874). The log restarts in milliseconds; a second is
  # generous without spamming a genuinely stuck database.
  @terminal_retry_ms 1_000

  # Per-job output cap. Beyond it, output is dropped (head kept) and the drop
  # is recorded, so one runaway cell cannot fill the ledger. 8 MiB is far
  # above any legitimate cell's output. Tests shrink it via app env so the
  # over-cap read paths can be exercised without producing 8 MiB.
  @output_cap Application.compile_env(:ix_mcp, :output_cap, 8 * 1024 * 1024)

  @doc "The per-job output cap, in bytes. Output past it is counted, not kept."
  @spec output_cap() :: pos_integer()
  def output_cap, do: @output_cap

  @enforce_keys [:id, :code]
  defstruct [
    :id,
    :code,
    :intent,
    :action_id,
    :session_id,
    :session,
    :topic,
    :watch,
    :workspace,
    :buffer,
    :counter,
    :io_proxy,
    :eval_pid,
    :eval_ref,
    :started_mono,
    :started_at,
    :finished_mono,
    :result,
    status: :running,
    quiet: false,
    terminal_recorded: false,
    diagnostics: [],
    subscribers: [],
    flushed_seq: nil,
    flushed_dropped: 0,
    flush_scheduled: false
  ]

  @type status :: :running | :done | :failed | :cancelled | :killed

  @type t :: %__MODULE__{
          id: String.t(),
          code: String.t(),
          intent: String.t() | nil,
          action_id: integer() | nil,
          session_id: integer() | nil,
          session: String.t() | nil,
          topic: String.t() | nil,
          watch: boolean(),
          workspace: String.t(),
          buffer: :ets.tid() | nil,
          counter: :counters.counters_ref() | nil,
          io_proxy: pid() | nil,
          eval_pid: pid() | nil,
          eval_ref: reference() | nil,
          started_mono: integer() | nil,
          started_at: DateTime.t() | nil,
          finished_mono: integer() | nil,
          result: {:value, term()} | {:failure, String.t()} | nil,
          status: status(),
          quiet: boolean(),
          terminal_recorded: boolean(),
          diagnostics: [String.t()],
          subscribers: [pid()],
          flushed_seq: integer() | nil,
          flushed_dropped: non_neg_integer(),
          flush_scheduled: boolean()
        }

  @type summary :: %{
          id: String.t(),
          status: status(),
          running: boolean(),
          intent: String.t() | nil,
          elapsed_s: float(),
          output_bytes: non_neg_integer(),
          output_dropped: non_neg_integer(),
          diagnostics: [String.t()],
          result: String.t() | nil
        }

  # -- client ----------------------------------------------------------------

  @spec start_link({String.t(), String.t(), keyword()}) :: GenServer.on_start()
  def start_link({id, code, opts}) do
    GenServer.start_link(__MODULE__, {id, code, opts}, name: via(id))
  end

  @spec via(String.t()) :: {:via, Registry, {IxMcp.Jobs.Registry, String.t()}}
  def via(id), do: {:via, Registry, {IxMcp.Jobs.Registry, id}}

  @spec summary(GenServer.server()) :: summary()
  def summary(server), do: GenServer.call(server, :summary)

  @doc "The job's result value. `{:error, :running}` while the job runs."
  @spec result(GenServer.server()) :: {:ok, term()} | {:error, :running | String.t()}
  def result(server), do: GenServer.call(server, :result)

  @spec cancel(GenServer.server()) :: :ok | {:error, :finished}
  def cancel(server), do: GenServer.call(server, :cancel)

  @doc """
  Mark this job a quiet wrapper (#3934): a cell that awaits another job is
  a read of that job's terminal state, not a job with news of its own, so
  its clean finish announces nothing. Its failure or death still does.
  """
  @spec quiet(GenServer.server()) :: :ok
  def quiet(server), do: GenServer.cast(server, :quiet)

  @doc "The evaluation process and IO proxy (for tracing from outside)."
  @spec procs(GenServer.server()) :: {pid(), pid()}
  def procs(server), do: GenServer.call(server, :procs)

  @doc """
  Attach a one-line diagnostic to a running job, shown with the job's
  diagnostics in its reply and summary. How `Cmd` reports a subprocess's
  nonzero exit: `status: done` says the cell returned, and this line is
  what keeps an inner command's failure from looking green (the report's
  silent `rg` exit 2). A cast, so a note can never block or crash a cell.
  """
  @spec note(GenServer.server(), String.t()) :: :ok
  def note(server, message) when is_binary(message), do: GenServer.cast(server, {:note, message})

  @doc """
  Wait until the job finishes, up to `timeout_ms`. Returns the final summary
  or `:timeout` -- in which case the job keeps running in the background,
  which is the entire budget-then-background contract.
  """
  @spec await(GenServer.server(), timeout()) :: {:ok, summary()} | :timeout
  def await(server, timeout_ms) do
    started_mono = System.monotonic_time(:millisecond)

    case subscribe(server, timeout_ms) do
      {:finished, summary} ->
        {:ok, summary}

      {:subscribed, id} ->
        ref = Process.monitor(server)

        receive do
          {:ix_job_finished, _id, summary} ->
            Process.demonitor(ref, [:flush])
            {:ok, summary}

          {:DOWN, ^ref, :process, _pid, _reason} ->
            # The control process died without reporting a terminal
            # transition -- a hard kill finish/1 cannot run through. The
            # reaper drives the ledger terminal from outside (#3839); read
            # that durable record instead of parking here forever, which is
            # what an :infinity await did when its job was killed.
            {:ok, killed_summary(id)}
        after
          remaining_ms(timeout_ms, started_mono) ->
            Process.demonitor(ref, [:flush])
            give_up(server)
        end

      :busy ->
        give_up(server)
    end
  end

  # Subscribing must never outlive the caller's own budget (#4082): a control
  # process parked in a ledger write under load cannot take the subscription
  # for up to the ledger's 30s call bound, and the old default-5s call here
  # died with `{:timeout, {GenServer, :call, [pid, {:subscribe, ...}]}}` --
  # killing the exec handler. A subscribe that cannot land within the budget
  # means the same thing as a budget that ran out: the job stays backgrounded.
  defp subscribe(server, timeout_ms) do
    GenServer.call(server, {:subscribe, self()}, subscribe_timeout(timeout_ms))
  catch
    :exit, {:timeout, {GenServer, :call, _args}} -> :busy
  end

  defp subscribe_timeout(:infinity), do: :infinity
  # The floor keeps a tiny budget from turning the subscription itself into
  # a guaranteed timeout race on a healthy process.
  defp subscribe_timeout(timeout_ms), do: max(timeout_ms, 1_000)

  defp remaining_ms(:infinity, _started_mono), do: :infinity

  defp remaining_ms(timeout_ms, started_mono) do
    max(timeout_ms - (System.monotonic_time(:millisecond) - started_mono), 0)
  end

  defp give_up(server) do
    GenServer.cast(server, {:unsubscribe, self()})

    # The finish notification may have raced the timeout; prefer it.
    receive do
      {:ix_job_finished, _id, summary} -> {:ok, summary}
    after
      0 -> :timeout
    end
  end

  # After a control process dies unreported its terminal state lives only in
  # the ledger, written by the reaper off the same :DOWN this await saw. The
  # two monitors race, so the read can arrive before the write; poll the
  # durable summary briefly until it goes terminal (bounded, never a hang).
  @killed_read_tries 100
  @killed_read_ms 50
  defp killed_summary(id, tries \\ @killed_read_tries) do
    summary = IxMcp.Jobs.get(id)

    if summary.status != :running or tries <= 0 do
      summary
    else
      Process.sleep(@killed_read_ms)
      killed_summary(id, tries - 1)
    end
  end

  @doc "Full captured output of the job as one binary (from the hot buffer)."
  @spec output(GenServer.server()) :: String.t()
  def output(server) do
    server
    |> GenServer.call(:buffer)
    |> read_buffer()
    |> Kernel.||("")
  end

  @doc """
  The job's summary read from its Registry-value snapshot -- an ETS read
  that never touches the control process (#4082): a process parked in a
  ledger write under load answers no GenServer.call within the default 5s,
  and reads must not inherit that stall. `nil` when the job is not resident
  (finished-and-gone, or the sliver before `init` publishes the snapshot);
  callers fall back to the call path or the durable ledger.
  """
  @spec read_summary(String.t()) :: summary() | nil
  def read_summary(id) do
    with {pid, %{} = snap} <- lookup_snapshot(id),
         true <- Process.alive?(pid) do
      finished = snap.finished_mono || System.monotonic_time(:millisecond)

      %{
        id: id,
        status: snap.status,
        running: snap.status == :running,
        intent: snap.intent,
        elapsed_s: (finished - snap.started_mono) / 1000,
        output_bytes: :counters.get(snap.counter, 1),
        output_dropped: :counters.get(snap.counter, 2),
        diagnostics: snap.diagnostics,
        result: snap.result
      }
    else
      _not_resident -> nil
    end
  end

  @doc "The job's output read straight from its hot buffer via the snapshot (#4082), or nil."
  @spec read_output(String.t()) :: String.t() | nil
  def read_output(id) do
    with {pid, %{buffer: buffer}} <- lookup_snapshot(id),
         true <- Process.alive?(pid) do
      read_buffer(buffer)
    else
      _not_resident -> nil
    end
  end

  @doc """
  Bytes this job discarded at `output_cap/0`, read from the snapshot -- `nil`
  when the job is not resident, so callers fall back to the ledger's
  `output_dropped`. Every output read path needs this: the buffer holds the
  HEAD, so a job over the cap has no tail left to return and `Jobs.grep`
  cannot see a pattern that was printed past the cap.
  """
  @spec read_dropped(String.t()) :: non_neg_integer() | nil
  def read_dropped(id) do
    with {pid, %{counter: counter}} <- lookup_snapshot(id),
         true <- Process.alive?(pid) do
      :counters.get(counter, 2)
    else
      _not_resident -> nil
    end
  end

  defp lookup_snapshot(id) do
    case Registry.lookup(IxMcp.Jobs.Registry, id) do
      [{pid, value}] -> {pid, value}
      [] -> nil
    end
  end

  # The buffer is a public ETS table owned by the control process; it can be
  # deleted between the aliveness check and the read when the process dies
  # right then, which means the same as "not resident".
  defp read_buffer(buffer) do
    buffer
    |> :ets.tab2list()
    |> Enum.sort()
    |> Enum.map_join("", fn {_seq, chunk} -> chunk end)
  rescue
    ArgumentError -> nil
  end

  # -- server ----------------------------------------------------------------

  @impl true
  def init({id, code, opts}) do
    # Trapping exits is what makes the control process survivable: a linked
    # IOProxy crash arrives as {:EXIT, io_proxy, reason} instead of taking
    # this process down with it (#3839).
    Process.flag(:trap_exit, true)

    session = Session.get()
    %{session_id: session_id} = Session.ids()

    state = %__MODULE__{
      id: id,
      code: code,
      intent: Keyword.get(opts, :intent),
      action_id: Keyword.get(opts, :action_id),
      watch: Keyword.get(opts, :watch, false),
      workspace: Keyword.get(opts, :workspace) || Workspace.main(),
      session_id: session_id,
      session: session.name,
      topic: session.topic,
      buffer: :ets.new(:job_output, [:ordered_set, :public]),
      counter: :counters.new(2, [:write_concurrency]),
      started_mono: System.monotonic_time(:millisecond),
      started_at: DateTime.utc_now()
    }

    # Publish the read snapshot before anything can block: from here on,
    # summary and output reads are ETS lookups against the Registry value,
    # never calls into this process (#4082).
    publish_snapshot(state)

    {:ok, state, {:continue, :spawn_eval}}
  end

  @impl true
  def handle_continue(:spawn_eval, state) do
    # Arm the reaper before the ledger write, not after (#4082): the write
    # below can park this process for the ledger's full call bound under
    # load, and a kill inside that window used to escape the reaper
    # entirely. The reaper carries the start metadata so it can finalize
    # this job even when the `job_started` row itself never landed.
    Reaper.watch(state.id, self(), start_meta(state))

    # Record the durable jobs row before the eval can die, so a death in the
    # first instants is still finalized (#3839). A ledger outage must not
    # abort the job itself (#3874): without the row, later reads degrade to
    # the hot buffer, and the terminal transition reconstructs the row from
    # the same metadata (#4082), which beats dying before the eval starts.
    safe_log(fn -> ActionLog.job_started(start_meta(state)) end)

    buffer = state.buffer
    counter = state.counter
    sink = fn chunk -> capture(buffer, counter, chunk) end
    {:ok, io_proxy} = IOProxy.start_link(sink)

    job = self()
    code = state.code
    id = state.id
    workspace = state.workspace
    writer = writer(state)

    {eval_pid, eval_ref} =
      spawn_monitor(fn ->
        Process.group_leader(self(), io_proxy)
        # The cell can learn which job it is (Jobs.await marks its own job
        # a quiet wrapper through this, #3934). Process dictionary, not a
        # closure: the id must be readable from inside the running cell.
        Process.put(:ix_job_id, id)
        # ... and which workspace it targets, so Workspace/Ix calls made
        # from inside the cell default to the cell's own REPL (#3967).
        Process.put(:ix_workspace, workspace)

        outcome = evaluate(code, writer, workspace)

        send(job, {:eval_finished, self(), outcome})
      end)

    if state.action_id, do: Process.send_after(self(), :sample_stack, @stack_sample_interval_ms)
    schedule_flush()

    {:noreply,
     %{state | io_proxy: io_proxy, eval_pid: eval_pid, eval_ref: eval_ref, flush_scheduled: true}}
  end

  # Who this cell is, for the shared workspace's provenance (#3967): one
  # kernel is one session but not one agent, so the job id is the finest
  # writer identity there is, and its intent is what makes it recognizable
  # to the agent reading the warning.
  defp writer(state) do
    %{
      job: state.id,
      intent: state.intent,
      session_id: state.session_id,
      session: state.session,
      started_at: state.started_at
    }
  end

  # The workspace speaks twice: once before the cell runs, about variables
  # another cell changed under it and modules it is about to take over, and
  # once after, about the variables this cell took from somebody else. The
  # first has to come before evaluation, because the cell holding a clobbered
  # value is usually the cell that raises, and a raising cell never merges.
  defp evaluate(code, writer, workspace) do
    case Evaluator.scan(code) do
      {:ok, quoted, refs} ->
        # One visit to the workspace, so the warnings describe exactly the
        # values this cell was handed rather than whatever a concurrent cell
        # merged between the snapshot and the question.
        {binding, env, before} = Workspace.begin_cell(refs, writer, workspace)
        hints = Map.get(refs, :hints, [])

        case Evaluator.eval_quoted(quoted, binding, env) do
          {:ok, value, binding, env, diags} ->
            {:done, value,
             hints ++ before ++ diags ++ Workspace.merge(binding, env, refs, writer, workspace)}

          {:runtime_error, formatted, diags} ->
            {:failed, formatted, hints ++ before ++ diags}
        end

      {:parse_error, message} ->
        {:failed, "parse error: " <> message, []}
    end
  end

  @impl true
  def handle_call(:summary, _from, state), do: {:reply, build_summary(state), state}

  def handle_call(:result, _from, %{status: :running} = state),
    do: {:reply, {:error, :running}, state}

  def handle_call(:result, _from, %{result: {:value, value}} = state),
    do: {:reply, {:ok, value}, state}

  def handle_call(:result, _from, %{result: {:failure, message}} = state),
    do: {:reply, {:error, message}, state}

  def handle_call(:cancel, _from, %{status: :running} = state) do
    # OS subprocesses first: once the BEAM processes die, the ports close and
    # the pids can no longer be discovered.
    state.io_proxy |> OsProc.os_pids() |> Enum.each(&OsProc.kill_tree/1)

    state.io_proxy
    |> OsProc.job_processes()
    |> Enum.each(fn pid -> Process.exit(pid, :kill) end)

    Process.demonitor(state.eval_ref, [:flush])
    {:reply, :ok, finish(state, :cancelled, {:failure, "cancelled"}, [])}
  end

  def handle_call(:cancel, _from, state), do: {:reply, {:error, :finished}, state}

  def handle_call({:subscribe, pid}, _from, %{status: :running} = state) do
    {:reply, {:subscribed, state.id}, %{state | subscribers: [pid | state.subscribers]}}
  end

  def handle_call({:subscribe, _pid}, _from, state) do
    {:reply, {:finished, build_summary(state)}, state}
  end

  def handle_call(:buffer, _from, state), do: {:reply, state.buffer, state}

  def handle_call(:procs, _from, state), do: {:reply, {state.eval_pid, state.io_proxy}, state}

  @impl true
  def handle_cast({:unsubscribe, pid}, state) do
    {:noreply, %{state | subscribers: List.delete(state.subscribers, pid)}}
  end

  def handle_cast(:quiet, state), do: {:noreply, %{state | quiet: true}}

  # Notes append in arrival order, capped so a loop of failing subprocesses
  # cannot grow the summary without bound; the snapshot republish makes a
  # running job's notes visible to Jobs.get right away.
  @max_notes 20
  def handle_cast({:note, message}, %{status: :running} = state) do
    if length(state.diagnostics) < @max_notes do
      state = %{state | diagnostics: state.diagnostics ++ [message]}
      {:noreply, publish_snapshot(state)}
    else
      {:noreply, state}
    end
  end

  def handle_cast({:note, _message}, state), do: {:noreply, state}

  @impl true
  def handle_info({:eval_finished, pid, outcome}, %{eval_pid: pid} = state) do
    Process.demonitor(state.eval_ref, [:flush])

    state =
      case outcome do
        {:done, value, diags} -> finish(state, :done, {:value, value}, diags)
        {:failed, message, diags} -> finish(state, :failed, {:failure, message}, diags)
      end

    {:noreply, state}
  end

  def handle_info(
        {:DOWN, ref, :process, _pid, reason},
        %{eval_ref: ref, status: :running} = state
      ) do
    # The cell's process died without reporting: a crash (exit, throw from a
    # linked process). The exit reason IS the crash report, state and all --
    # format it whole. This is the cell author's failure, hence `failed`
    # (distinct from `killed`, which is the machinery dying under the job).
    {:noreply, finish(state, :failed, {:failure, Exception.format_exit(reason)}, [])}
  end

  # The IOProxy (linked) crashed under a running job. Trapping exits turned
  # what #3839 made a brutal linked death into this trappable signal: record
  # a `killed` transition with the output captured so far, rather than
  # vanishing silently.
  def handle_info(
        {:EXIT, io_proxy, reason},
        %{io_proxy: io_proxy, status: :running} = state
      )
      when reason != :normal do
    Process.demonitor(state.eval_ref, [:flush])
    if is_pid(state.eval_pid), do: Process.exit(state.eval_pid, :kill)

    {:noreply,
     finish(state, :killed, {:failure, "killed: io proxy exited: " <> inspect(reason)}, [])}
  end

  # A supervisor shutdown (or a normal linked exit) is not a job death we
  # record, but we must still terminate: trapping exits means the signal no
  # longer stops us automatically, and ignoring it would stall whole-tree
  # shutdown until the supervisor's timeout brutally kills us.
  def handle_info({:EXIT, _pid, :normal}, state), do: {:stop, :normal, state}
  def handle_info({:EXIT, _pid, :shutdown}, state), do: {:stop, :shutdown, state}

  def handle_info({:EXIT, _pid, {:shutdown, _} = reason}, state),
    do: {:stop, reason, state}

  # Any other linked EXIT under a still-running job is machinery we don't
  # model dying; leave the job alone (the reaper covers a true control-process
  # death).
  def handle_info({:EXIT, _pid, _reason}, state), do: {:noreply, state}

  def handle_info(:record_terminal, %{terminal_recorded: false, status: status} = state)
      when status != :running do
    # Catch up the output the failed pre-terminal flush left behind, then
    # retry the transition itself (#3874).
    {:noreply, state |> flush() |> record_terminal()}
  end

  def handle_info(:record_terminal, state), do: {:noreply, state}

  def handle_info(:flush, %{status: :running} = state) do
    state = %{flush(state) | flush_scheduled: true}
    schedule_flush()
    {:noreply, state}
  end

  def handle_info(:flush, state), do: {:noreply, %{state | flush_scheduled: false}}

  # Sampling stops itself once the run finished (no reschedule); the
  # status='running' guard in the log makes a racing last sample harmless.
  def handle_info(:sample_stack, %{status: :running} = state) do
    case Process.info(state.eval_pid, :current_stacktrace) do
      {:current_stacktrace, frames} ->
        shown =
          case Evaluator.prune_stacktrace(frames) do
            [] -> frames
            pruned -> pruned
          end

        stack = JSON.encode!(Enum.map(shown, &Exception.format_stacktrace_entry/1))
        safe_log(fn -> ActionLog.update_stack(state.action_id, stack, cell_line(frames)) end)

      nil ->
        :ok
    end

    Process.send_after(self(), :sample_stack, @stack_sample_interval_ms)
    {:noreply, state}
  end

  def handle_info(:sample_stack, state), do: {:noreply, state}

  def handle_info(_msg, state), do: {:noreply, state}

  # -- internals ---------------------------------------------------------------

  defp cell_line(frames) do
    Enum.find_value(frames, fn {_mod, _fun, _arity, meta} ->
      line = meta[:line]
      if meta[:file] == ~c"cell" and is_integer(line) and line > 0, do: line
    end)
  end

  defp finish(state, status, result, diags) do
    state = %{
      state
      | status: status,
        result: result,
        # Notes attached while the cell ran (Cmd exit reports) precede the
        # evaluator's own diagnostics; finishing must merge, not clobber.
        diagnostics: state.diagnostics ++ diags,
        finished_mono: System.monotonic_time(:millisecond)
    }

    # Snapshot first (#4082): the flush and terminal write below can park
    # this process on the ledger, and readers must already see the terminal
    # state through the Registry value while that happens.
    publish_snapshot(state)

    # Persist all captured output before the transition, so a reader that
    # sees the job finish finds its output already complete on disk.
    state = flush(state)
    state = record_terminal(state)

    summary = build_summary(state)
    Enum.each(state.subscribers, fn pid -> send(pid, {:ix_job_finished, state.id, summary}) end)
    %{state | subscribers: []}
  end

  # The one atomic terminal transition: jobs row + actions row + outbox,
  # committed together (#3839). Whoever writes it first wins; the reaper's
  # racing `killed` attempt then no-ops. When the ledger is unavailable the
  # transition re-arms itself (#3874) -- the reaper stays armed until it
  # lands, so even this process dying in the window is still finalized.
  defp record_terminal(state) do
    recorded =
      safe_log(fn ->
        ActionLog.finish_job(state.id, state.status, render_result(state),
          quiet: state.quiet,
          start: start_meta(state)
        )
      end)

    case recorded do
      {:ok, {:notify, outbox}} ->
        # A row born acked was delivered by construction (a quiet wrapper's
        # clean finish, #3934); publishing it would announce a non-event.
        unless outbox.acked, do: Notifier.publish(outbox)
        Reaper.reported(state.id)
        %{state | terminal_recorded: true}

      {:ok, :already_final} ->
        Reaper.reported(state.id)
        %{state | terminal_recorded: true}

      :unavailable ->
        Process.send_after(self(), :record_terminal, @terminal_retry_ms)
        state
    end
  end

  # Capture one output chunk into the hot buffer, honoring the per-job cap.
  # Only binaries enter the buffer (IOProxy's convert/2 guarantees valid
  # UTF-8); the guard turns any regression into a failed put_chars at the
  # call site rather than a poisoned job record (#3538).
  defp capture(buffer, counter, chunk) when is_binary(chunk) do
    if :counters.get(counter, 1) < @output_cap do
      :ets.insert(buffer, {System.unique_integer([:monotonic]), chunk})
      :counters.add(counter, 1, byte_size(chunk))
    else
      :counters.add(counter, 2, byte_size(chunk))
    end
  end

  # Move buffer rows not yet persisted into the durable table, in one batch.
  defp flush(state) do
    after_seq = state.flushed_seq

    match =
      case after_seq do
        nil -> [{{:"$1", :"$2"}, [], [{{:"$1", :"$2"}}]}]
        seq -> [{{:"$1", :"$2"}, [{:>, :"$1", seq}], [{{:"$1", :"$2"}}]}]
      end

    case :ets.select(state.buffer, match) do
      [] ->
        maybe_flush_dropped(state)

      rows ->
        rows = Enum.sort(rows)
        dropped_now = :counters.get(state.counter, 2)

        append =
          safe_log(fn -> ActionLog.append_job_output(state.id, rows, dropped_now) end)

        case append do
          {:ok, :ok} ->
            {last_seq, _} = List.last(rows)
            %{state | flushed_seq: last_seq, flushed_dropped: dropped_now}

          # Nothing advanced: the same rows are still in the hot buffer and
          # the next tick retries the whole batch (idempotent per seq).
          :unavailable ->
            state
        end
    end
  end

  # No new rows, but the drop counter may have advanced (output over the cap
  # is counted, not buffered) -- record the delta so truncation is durable.
  defp maybe_flush_dropped(state) do
    dropped_now = :counters.get(state.counter, 2)

    with true <- dropped_now > state.flushed_dropped,
         {:ok, :ok} <-
           safe_log(fn -> ActionLog.append_job_output(state.id, [], dropped_now) end) do
      %{state | flushed_dropped: dropped_now}
    else
      _unchanged -> state
    end
  end

  # Run one ActionLog write, absorbing the exit inherited when the log dies
  # mid-request (#3874): the ledger degrades, the job survives. The log's
  # client API has already retried across the supervisor restart by the
  # time this fires, so a hit here means the log is truly down.
  defp safe_log(fun) do
    {:ok, fun.()}
  catch
    :exit, reason ->
      Logger.warning(
        "job #{inspect(self())}: action log unavailable: #{inspect(reason, limit: 5)}"
      )

      :unavailable
  end

  defp schedule_flush, do: Process.send_after(self(), :flush, @flush_interval_ms)

  # The job's start metadata, exactly as `ActionLog.job_started/2` takes it.
  # Built once per use from state and handed to the ledger, the reaper, and
  # the terminal transition (#4082): any of them can then (re)create the
  # jobs row, so losing the start write under load no longer loses the job.
  defp start_meta(state) do
    %{
      id: state.id,
      session_id: state.session_id,
      action_id: state.action_id,
      intent: state.intent,
      session_name: state.session,
      topic_name: state.topic,
      code: state.code,
      watch: state.watch,
      started_at: DateTime.to_iso8601(state.started_at)
    }
  end

  # The Registry value doubles as the job's hot read model (#4082): summary
  # fields plus the buffer/counter handles, so `Jobs.get`/`Jobs.output` are
  # plain ETS reads that keep answering while this process sits in a ledger
  # call. Only the owning process may update its own Registry value, which
  # is exactly who calls this.
  defp publish_snapshot(state) do
    Registry.update_value(IxMcp.Jobs.Registry, state.id, fn _old ->
      %{
        intent: state.intent,
        status: state.status,
        result: render_result(state),
        diagnostics: state.diagnostics,
        started_mono: state.started_mono,
        finished_mono: state.finished_mono,
        counter: state.counter,
        buffer: state.buffer
      }
    end)

    state
  end

  defp build_summary(state) do
    finished = state.finished_mono || System.monotonic_time(:millisecond)

    %{
      id: state.id,
      status: state.status,
      running: state.status == :running,
      intent: state.intent,
      elapsed_s: (finished - state.started_mono) / 1000,
      output_bytes: :counters.get(state.counter, 1),
      output_dropped: :counters.get(state.counter, 2),
      diagnostics: state.diagnostics,
      result: render_result(state)
    }
  end

  defp render_result(%{result: {:value, value}}), do: Evaluator.render(value)
  defp render_result(%{result: {:failure, message}}), do: message
  defp render_result(_), do: nil
end
