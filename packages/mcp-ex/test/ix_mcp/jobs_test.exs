defmodule IxMcp.JobsTest do
  use ExUnit.Case, async: false

  import IxMcpTest.Eventually

  alias IxMcp.ActionLog
  alias IxMcp.Jobs
  alias IxMcp.Jobs.Reaper
  alias IxMcp.MCP.Notifier
  alias IxMcp.Session

  setup do
    IxMcp.Workspace.reset()
    :ok
  end

  test "budget-then-background: a slow cell returns a running handle and finishes later" do
    {summary, _out} = Jobs.run("Process.sleep(300); :finally", budget: 0.05, intent: "slow")
    assert summary.running

    final = Jobs.await(summary.id, 5_000)
    assert final.status == :done
    assert final.result == ":finally"
  end

  test "a blocking cell never delays other jobs (the whole point)" do
    {blocked, _out} = Jobs.run("Process.sleep(:infinity)", budget: 0.05, intent: "block forever")
    assert blocked.running

    started = System.monotonic_time(:millisecond)
    {quick, _out} = Jobs.run("1 + 1", intent: "quick")
    elapsed = System.monotonic_time(:millisecond) - started

    assert quick.status == :done
    assert quick.result == "2"
    assert elapsed < 1_000

    assert :ok = Jobs.cancel(blocked.id)
    assert Jobs.get(blocked.id).status == :cancelled
  end

  test "output paging: tail, head, lines, slice, grep" do
    code = "for i <- 1..50, do: IO.puts(\"line \#{i}\")"
    {summary, _out} = Jobs.run(code, intent: "print lines")
    assert summary.status == :done

    assert Jobs.tail(summary.id, 2) =~ "line 50"
    assert Jobs.head(summary.id, 1) == "line 1"
    assert Jobs.lines(summary.id, 2, 3) == "line 2\nline 3"
    assert Jobs.slice(summary.id, 0, 2) == "line 1\nline 2"
    assert Jobs.grep(summary.id, ~r/line 4[89]/) == "line 48\nline 49"
    assert Jobs.grep(summary.id, "line 50") == "line 50"
  end

  test "result/1 returns the value term, or :running while unfinished" do
    # A long sleep against a tiny budget keeps the "still running" window wide
    # enough that a loaded CI builder cannot let the job finish before the
    # assertion (the 200ms/50ms margin flaked under load).
    {running, _out} = Jobs.run("Process.sleep(2000); %{a: 1}", budget: 0.05, intent: "slow map")
    assert {:error, :running} = Jobs.result(running.id)

    Jobs.await(running.id, 5_000)
    assert {:ok, %{a: 1}} = Jobs.result(running.id)
  end

  test "history records runs with session and topic grouping" do
    IxMcp.Session.set_name("test session")
    IxMcp.Session.set_topic("test topic")

    {summary, _out} = Jobs.run(":recorded", intent: "history entry")
    entry = Enum.find(Jobs.history(10), &(&1.id == summary.id))

    assert entry.intent == "history entry"
    assert entry.session == "test session"
    assert entry.topic == "test topic"
    assert entry.status == :done
  end

  test "a crashed cell reports the exit reason, jobs registry survives" do
    {summary, _out} = Jobs.run("exit({:kaboom, %{state: 42}})", intent: "crash")
    assert summary.status == :failed
    assert summary.result =~ "kaboom"
    assert summary.result =~ "42"
  end

  @tag :os_procs
  test "cancelling a job kills OS processes its cell spawned (no orphans)" do
    marker = "ix-mcp-test-#{System.unique_integer([:positive])}"

    # A compound command keeps `sh` alive (a simple command would be exec'd,
    # dropping the marker from any argv pgrep can see).
    code = """
    System.cmd("sh", ["-c", "sleep 600; echo #{marker}"])
    """

    {summary, _out} = Jobs.run(code, budget: 0.2, intent: "spawn subprocess")
    assert summary.running

    # The subprocess exists while the job runs...
    assert {_, 0} = System.cmd("pgrep", ["-f", marker])

    assert :ok = Jobs.cancel(summary.id)
    Process.sleep(200)

    # ...and is gone, with its whole tree, after cancellation.
    assert {_, 1} = System.cmd("pgrep", ["-f", marker])
  end

  # -- #3538: raw binary bytes in cell output ---------------------------------

  test "a cell printing raw binary bytes still finishes, escaped, with a terminal history row" do
    # The incident shape (IO.puts of a compiled binary): pre-fix,
    # :unicode.characters_to_binary/1 returned an {:error, ...} tuple that
    # landed in the output buffer as if it were output, byte_size/1 crashed
    # the job GenServer mid-finish, the history row sat at :running forever,
    # and this very call exited :noproc after its budget.
    {summary, output} = Jobs.run(~S|IO.puts(<<0xFF>> <> "tail")|, budget: 2, intent: "binary out")

    assert summary.status == :done
    assert output == "\\xFFtail\n"

    entry = Enum.find(Jobs.history(10), &(&1.id == summary.id))
    assert entry.status == :done
  end

  test "IO.binwrite: bytes valid as UTF-8 pass byte-identical, invalid bytes are escaped" do
    {utf8, utf8_out} = Jobs.run(~S|IO.binwrite("snow ☃")|, budget: 2, intent: "binwrite utf8")
    assert utf8.status == :done
    assert utf8_out == "snow ☃"

    {bin, bin_out} = Jobs.run(~S|IO.binwrite(<<0xFF>>)|, budget: 2, intent: "binwrite binary")
    assert bin.status == :done
    assert bin_out == "\\xFF"
  end

  test "a job whose process dies mid-run gets a terminal history row, and cancel reports it" do
    {running, _out} = Jobs.run("Process.sleep(:infinity)", budget: 0.05, intent: "to be killed")
    assert running.running

    {:ok, pid} = Jobs.lookup(running.id)
    ref = Process.monitor(pid)
    Process.exit(pid, :kill)
    assert_receive {:DOWN, ^ref, :process, ^pid, :killed}

    # History finalizes on its own DOWN signal; poll for the flip.
    entry =
      eventually(fn ->
        Enum.find(Jobs.history(10), &(&1.id == running.id && &1.status != :running))
      end)

    # A control process killed without reporting is `killed`, not `failed`:
    # the machinery died under the job, it was not the cell's own crash. The
    # reaper writes the transition from outside (#3839).
    assert entry.status == :killed
    assert is_float(entry.elapsed_s)

    # The job process is gone but the run is on record: report its state
    # (pre-fix this raised "no such job" about an id history still listed).
    assert Jobs.cancel(running.id) == {:error, :killed}
  end

  test "cancel on an id that never existed still raises" do
    assert_raise ArgumentError, ~r/no such job/, fn -> Jobs.cancel("00000000") end
  end

  # -- #3839: durable ledger, every death notifies -----------------------------

  test "killing a job's process drives the ledger terminal and notifies the channel" do
    %{session_id: session_id, topic_id: topic_id} = Session.ids()
    intent = "killed-notify-#{System.unique_integer([:positive])}"

    action_id =
      ActionLog.start_action(%{
        session_id: session_id,
        topic_id: topic_id,
        tool: "exec",
        intent: intent,
        arguments: "{}"
      })

    {running, _out} =
      Jobs.run("Process.sleep(:infinity)", budget: 0.05, intent: intent, action_id: action_id)

    assert running.running

    # A connected transport hears the death. Register and sync on the
    # Notifier first, so its replay of any already-unacked rows drains before
    # the kill -- then this death arrives as its own published event, not
    # folded into a replay digest.
    :ok = Notifier.register(self())
    _ = :sys.get_state(Notifier)

    {:ok, pid} = Jobs.lookup(running.id)
    Process.exit(pid, :kill)

    id = running.id

    # The death arrives alone (meta names the job) or pooled with a
    # straggler finish from an earlier test into one digest (#3934); either
    # way the channel names this job, loudly.
    {content, meta} = receive_channel_mentioning(id)
    assert content =~ "killed"
    assert meta["severity"] == "failure"

    # The jobs row is terminal at `killed`...
    assert %{status: :killed} = ActionLog.job(id)

    # ...and its actions row went terminal in the same transition (killed
    # jobs record as a failed action -- the actions vocabulary has no killed).
    action = eventually(fn -> Enum.find(ActionLog.recent(50), &(&1.intent == intent)) end)
    assert action.status == "failed"
    assert action.is_error
  end

  test "output is readable via the ledger after the job process is dead" do
    {running, _out} =
      Jobs.run(
        ~s|IO.puts("durable-line-1"); IO.puts("durable-line-2"); Process.sleep(:infinity)|,
        budget: 0.05,
        intent: "durable output"
      )

    assert running.running

    # Give the flush timer (20ms under test config) a beat to persist output.
    Process.sleep(120)

    {:ok, pid} = Jobs.lookup(running.id)
    ref = Process.monitor(pid)
    Process.exit(pid, :kill)
    assert_receive {:DOWN, ^ref, :process, ^pid, :killed}

    # The process is gone (the Registry may still be unregistering the dead
    # pid asynchronously); reads fall back to the durable table either way.
    assert Jobs.output(running.id) == "durable-line-1\ndurable-line-2\n"
    assert Jobs.tail(running.id, 3) == "durable-line-1\ndurable-line-2\n"
    assert Jobs.grep(running.id, "durable-line-1") == "durable-line-1"
  end

  test "outbox replay: a transport registering after a job finished gets a digest" do
    # A job whose terminal transition was recorded while no transport was
    # connected leaves an unacked outbox row.
    %{session_id: session_id} = Session.ids()
    id = "rp" <> Base.encode16(:crypto.strong_rand_bytes(3), case: :lower)
    intent = "replayed-#{System.unique_integer([:positive])}"

    :ok =
      ActionLog.job_started(%{
        id: id,
        session_id: session_id,
        action_id: nil,
        intent: intent,
        session_name: nil,
        topic_name: nil,
        code: ":ok",
        watch: false,
        started_at: DateTime.to_iso8601(DateTime.utc_now())
      })

    assert {:notify, _outbox} = ActionLog.finish_job(id, :done, "the result")
    assert Enum.any?(ActionLog.unacked_outbox(), &(&1.ref == id))

    # Registering a transport replays the unacked rows as one digest.
    :ok = Notifier.register(self())

    assert_receive {:mcp_send, %{"params" => %{"content" => content, "meta" => meta}}}, 2_000
    assert content =~ id
    assert content =~ "while you were away"
    assert Map.has_key?(meta, "replay")

    # And the replayed row is now acked, so it will not replay again.
    refute Enum.any?(ActionLog.unacked_outbox(), &(&1.ref == id))
  end

  test "a running job survives an ActionLog crash mid-flush (#3874)" do
    # File-backed ledger for this test: the suite's default :memory:
    # database would come back empty after the crash-restart and prove
    # nothing about durability.
    path = Path.join(System.tmp_dir!(), "ix-jobs-3874-#{System.unique_integer([:positive])}.db")
    previous = Application.get_env(:ix_mcp, :actions_db)
    Application.put_env(:ix_mcp, :actions_db, path)
    restart_action_log()

    on_exit(fn ->
      Application.put_env(:ix_mcp, :actions_db, previous)
      restart_action_log()
      File.rm(path)
    end)

    {summary, _out} =
      Jobs.run(
        "for i <- 1..40 do IO.puts(\"tick \#{i}\"); Process.sleep(25) end; :ok",
        budget: 0.05,
        intent: "ticker"
      )

    assert summary.running
    {:ok, control} = Jobs.lookup(summary.id)
    ref = Process.monitor(control)

    # Suspend the log so the next output flush parks inside GenServer.call,
    # then kill it: the incident shape (#3874) -- that call exit used to
    # take the job control process, its registry entry, and any terminal
    # notification down with it.
    log = Process.whereis(ActionLog)
    :sys.suspend(log)
    Process.sleep(100)
    Process.exit(log, :kill)

    final = Jobs.await(summary.id, 10_000)
    assert final.status == :done
    refute_received {:DOWN, ^ref, :process, ^control, _reason}
    assert {:ok, _pid} = Jobs.lookup(summary.id)

    # The restarted log caught the terminal transition and the output.
    assert %{status: :done} =
             eventually(fn ->
               case ActionLog.job(summary.id) do
                 %{status: :done} = job -> job
                 _not_yet -> nil
               end
             end)

    assert Jobs.tail(summary.id, 2) =~ "tick 40"
  end

  # -- #4082: reads and finishes survive a parked or lossy ledger --------------

  test "a ledger parked in a slow write cannot kill the exec path (#4082)" do
    # Force the lazy session row before parking the log: Session.ids/0 must
    # not itself queue behind the suspended server.
    _ = Session.ids()

    log = Process.whereis(ActionLog)
    :ok = :sys.suspend(log)

    {summary, output} =
      try do
        # The job's control process parks in `job_started` against the
        # suspended log. Pre-fix, Jobs.run died here: the {:subscribe, pid}
        # call hit its default 5s timeout and the exit killed the caller
        # (the exec handler, in production).
        Jobs.run(~s|IO.puts("parked"); :ok|, budget: 0.2, intent: "under parked ledger")
      after
        :ok = :sys.resume(log)
      end

    # The budget elapsed while the ledger was parked: the run degrades to
    # the budget-then-background contract instead of an exit, and reads
    # answer from the snapshot.
    assert summary.running
    assert is_binary(output)
    assert Jobs.get(summary.id).id == summary.id

    final = Jobs.await(summary.id, 10_000)
    assert final.status == :done

    # And the run is on the durable record once the ledger drains.
    assert %{status: :done} =
             eventually(fn ->
               case ActionLog.job(summary.id) do
                 %{status: :done} = row -> row
                 _not_yet -> nil
               end
             end)

    assert Enum.find(Jobs.history(20), &(&1.id == summary.id))
  end

  test "a killed job whose start write was lost is still finalized via reaper meta (#4082)" do
    %{session_id: session_id} = Session.ids()
    id = "gh" <> Base.encode16(:crypto.strong_rand_bytes(3), case: :lower)

    start = %{
      id: id,
      session_id: session_id,
      action_id: nil,
      intent: "ghost start",
      session_name: nil,
      topic_name: nil,
      code: ":ok",
      watch: false,
      started_at: DateTime.to_iso8601(DateTime.utc_now())
    }

    # No job_started write ever happens: the load shape where safe_log
    # absorbed it. The reaper holds the start metadata and reconstructs the
    # row when it finalizes the kill.
    victim = spawn(fn -> Process.sleep(:infinity) end)
    :ok = Reaper.watch(id, victim, start)
    Process.exit(victim, :kill)

    assert %{status: :killed, intent: "ghost start"} =
             eventually(fn ->
               case ActionLog.job(id) do
                 %{status: :killed} = row -> row
                 _not_yet -> nil
               end
             end)

    assert Enum.find(Jobs.history(20), &(&1.id == id))
  end

  # Graceful stop/start through the supervisor: unlike a kill, this does
  # not count against restart intensity, so the test's one real crash is
  # the only one on the books. Session restarts too: its lazily-created
  # session/topic row ids belong to the previous database, and stale ids
  # would leave every later row orphaned from the joins (recent/history).
  defp restart_action_log do
    :ok = Supervisor.terminate_child(IxMcp.Supervisor, ActionLog)
    {:ok, _pid} = Supervisor.restart_child(IxMcp.Supervisor, ActionLog)
    :ok = Supervisor.terminate_child(IxMcp.Supervisor, Session)
    {:ok, _pid} = Supervisor.restart_child(IxMcp.Supervisor, Session)
  end

  # Consume channel messages until one mentions `id`: coalescing (#3934) may
  # pool unrelated finishes ahead of the one this test is waiting on.
  defp receive_channel_mentioning(id) do
    assert_receive {:mcp_send, %{"params" => %{"content" => content, "meta" => meta}}}, 2_000

    if content =~ id do
      {content, meta}
    else
      receive_channel_mentioning(id)
    end
  end
  describe "output past the per-job cap" do
    # The cap keeps the HEAD, so before #4306 `tail` returned lines from the
    # middle of the run as if they were the last ones, and `grep` answered ""
    # about a pattern the cell really did print. Both read as fact.
    setup do
      cap = IxMcp.Jobs.Job.output_cap()
      line = String.duplicate("x", 64)
      lines = div(cap, 65) * 3

      code =
        "Enum.each(1..LINES, fn n -> IO.puts(\"L\" <> to_string(n) <> \" LINE\") end); IO.puts(\"SENTINEL\"); :over"
        |> String.replace("LINES", to_string(lines))
        |> String.replace("LINE", line)

      {summary, _out} = Jobs.run(code, budget: 30, intent: "over cap")
      assert summary.status == :done
      %{summary: summary, cap: cap}
    end

    test "the summary reports what was dropped, not only what was kept", ctx do
      assert ctx.summary.output_dropped > 0
      assert ctx.summary.output_bytes >= ctx.cap
      assert Jobs.get(ctx.summary.id).output_dropped == ctx.summary.output_dropped
    end

    test "every derived read carries the notice, so none of them lies", ctx do
      dropped = ctx.summary.output_dropped

      for {label, text} <- [
            {"output", Jobs.output(ctx.summary.id)},
            {"tail", Jobs.tail(ctx.summary.id, 3)},
            {"grep", Jobs.grep(ctx.summary.id, "truncated")}
          ] do
        assert text =~ "output truncated", "#{label} gave no truncation notice"
        assert text =~ to_string(dropped), "#{label} did not name the dropped byte count"
      end
    end

    test "the notice says the tail is what is missing, since that is the wrong guess", ctx do
      notice = Jobs.tail(ctx.summary.id, 2)
      assert notice =~ "HEAD"
      refute Jobs.output(ctx.summary.id) =~ "SENTINEL"
    end
  end

  test "a job under the cap gets no truncation notice" do
    {summary, _out} = Jobs.run(~S|IO.puts("small"); :ok|, intent: "under cap")
    assert summary.output_dropped == 0
    refute Jobs.output(summary.id) =~ "output truncated"
    assert Jobs.tail(summary.id, 1) == "" or Jobs.output(summary.id) =~ "small"
  end
end
