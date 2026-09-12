defmodule IxMcp.Stdlib.ForgeTest do
  use ExUnit.Case, async: false

  alias IxMcp.ActionLog
  alias IxMcp.Forge.Runs
  alias IxMcp.Stdlib.Forge

  # The two `iex>` examples in the module are the codec's documentation AND the
  # only place a reader sees the two alphabets side by side. Nothing in this repo
  # ran a doctest before, so those examples were decoration: an edit could make
  # them wrong and every gate would stay green. Now they execute.
  doctest IxMcp.Stdlib.Forge

  # Same fixtures the verdict feed uses: structurally faithful to the
  # reconciler's real run records, with all free text invented.
  @fixtures Path.expand("../../fixtures", __DIR__)

  setup do
    unique = System.unique_integer([:positive])
    path = Path.join(System.tmp_dir!(), "ix-mcp-forge-land-test-#{unique}.db")
    log = start_supervised!({ActionLog, path: path, name: :"forge_land_log_#{unique}"})
    Application.put_env(:ix_mcp, :action_log_server, log)
    Process.put(:ix_workspace, "test-#{unique}")

    on_exit(fn ->
      Application.delete_env(:ix_mcp, :action_log_server)
      File.rm(path)
    end)

    :ok
  end

  defp record(name) do
    @fixtures
    |> Path.join("forge-run-#{name}.json")
    |> File.read!()
    |> String.replace("\n", "")
  end

  defp reader(lines) do
    fn _since -> {:ok, Enum.join(lines, "\n") <> "\n"} end
  end

  # The change id the fixtures carry, so a wait can be keyed on a real key
  # rather than on the absence of one.
  defp failed_change, do: "f9e8d7c6b5a4930817263544352617f0"
  defp passed_change, do: "0a1b2c3d4e5f60718293a4b5c6d7e8f9"

  defp await(lines, opts) do
    Forge.await_verdict(
      Keyword.merge(
        [
          read: reader(lines),
          runs: "/fixture/runs",
          interval_ms: 1,
          timeout_ms: 50,
          consumer: fn -> {:alive, "active"} end
        ],
        opts
      )
    )
  end

  describe "the change id codec" do
    # Both pairs were read off live run records on 2026-08-12: the letters are
    # what jj printed, the hex is what the record stored, and a search for the
    # letters would have found neither record.
    test "jj's reverse-hex letters become the hex the run records store" do
      assert Forge.change_id_hex("kvvozztpzzrk") == {:ok, "f44b006a008f"}
      assert Forge.change_id_hex("xwwzlxoqpxnm") == {:ok, "2330e2b9a2cd"}
    end

    test "and back" do
      assert Forge.change_id_letters("f44b006a008f") == {:ok, "kvvozztpzzrk"}
      assert Forge.change_id_letters("2330e2b9a2cd") == {:ok, "xwwzlxoqpxnm"}
    end

    test "every digit round trips, both ways" do
      hex = "0123456789abcdef"
      letters = "zyxwvutsrqponmlk"

      assert Forge.change_id_letters(hex) == {:ok, letters}
      assert Forge.change_id_hex(letters) == {:ok, hex}
    end

    test "upper case hex is accepted, because jj and humans both print it" do
      assert Forge.change_id_letters("2330E2B9A2CD") == {:ok, "xwwzlxoqpxnm"}
    end

    # The property that makes "either form is accepted" safe rather than
    # ambiguous: no string can be read as both, so nothing has to guess.
    test "the two alphabets are disjoint" do
      for digit <- String.graphemes("0123456789abcdef") do
        assert {:error, _not_letters} = Forge.change_id_hex(digit)
      end

      for letter <- String.graphemes("zyxwvutsrqponmlk") do
        assert {:error, _not_hex} = Forge.change_id_letters(letter)
      end
    end

    test "anything that is not a change id is refused, not silently mangled" do
      assert {:error, _empty} = Forge.change_id_hex("")
      assert {:error, _empty} = Forge.change_id_letters("")
      assert {:error, _mixed} = Forge.change_id_hex("kvvo1234")
      assert {:error, _long} = Forge.change_id_letters(String.duplicate("a", 33))
    end
  end

  describe "remote quoting" do
    # ssh does not carry an argv: it joins the words and hands the string to a
    # shell on the far side, so this is the boundary that keeps a commit
    # message or a path from becoming a command.
    test "a quoted argument survives a real shell verbatim" do
      for hostile <- [
            "plain",
            "with spaces",
            "it's got a quote",
            "$(id)",
            "`id`",
            "a; rm -rf /",
            "new\nline",
            ~s(double"quote),
            "back\\slash"
          ] do
        quoted = Runs.shell_quote(hostile)
        {output, 0} = System.cmd("sh", ["-c", "printf %s #{quoted}"])

        assert output == hostile, "#{inspect(hostile)} did not survive as #{inspect(quoted)}"
      end
    end
  end

  describe "reading a verdict out of a run record" do
    test "a passed run is a pass, with the rebased commit named" do
      assert {:passed, report} = await([record("passed")], change_id: passed_change())

      assert report.run_id == "1f2e3d4c5b6a-1786546496920"
      assert report.status == "passed"
      assert report.landed_commit =~ "1f2e3d4c5b6a"
    end

    test "a failed run names the derivations that failed and which were already red" do
      assert {:failed, report} = await([record("failed")], change_id: failed_change())

      assert report.status == "failed"
      assert report.detail.verdict_line == "==== gate verdict: FAIL ===="
      assert report.detail.failed_stages == ["incr"]
      assert report.detail.log == "/fixture/logs/gate-20260101T000000Z-2.log"

      # The store hash differs every run and is noise; the name is the thing a
      # reader acts on. The tolerated flag is the difference between "you broke
      # this" and "this was already broken".
      assert report.detail.failing_derivations == [
               %{name: "fixture-lint-check", tolerated: false},
               %{name: "fixture-tolerated-check", tolerated: true},
               %{name: "fixture-treefmt-check", tolerated: false}
             ]
    end

    test "a run still building is not a verdict" do
      assert {:indeterminate, report} =
               await([record("building")], change_id: "1122334455667788990011223344556f")

      assert report.reason =~ "building"
    end

    test "no record for this change is not a verdict either" do
      assert {:indeterminate, report} =
               await([record("passed")], change_id: "ffffffffffffffffffffffffffffffff")

      assert report.reason =~ "no run record"
    end

    # The fault this module exists to prevent, as a test: the pre-submit commit
    # id appears in no record, because the queue rebases. Keyed on it, a wait
    # is silent forever rather than wrong-and-loud.
    test "a change is found by its change id even though the run is named after another commit" do
      passed = record("passed") |> JSON.decode!()

      refute String.starts_with?(passed["run_id"], String.slice(passed["change_id"], 0, 12))
      assert {:passed, _report} = await([record("passed")], change_id: passed_change())
    end

    test "the letter form of a change id finds the record the hex form is stored under" do
      {:ok, letters} = Forge.change_id_letters(passed_change())

      assert {:passed, _report} = await([record("passed")], change_id: letters)
    end

    test "a subject is a weaker key that still works" do
      subject =
        record("passed")
        |> JSON.decode!()
        |> get_in(["trigger", "description"])
        |> String.split("\n")
        |> hd()

      assert {:passed, _report} = await([record("passed")], change_id: nil, subject: subject)
    end

    # C3, first-class. One change owns SEVERAL records once it is resubmitted or
    # re-run, and the read arrives newest-first, so the reader prepends and
    # `Enum.find` returns the OLDEST: a resubmit would be answered with the
    # previous attempt's verdict. The two records here disagree on purpose, so
    # only the newest-by-updated_at rule can produce the right answer.
    test "two records, one change, newest wins" do
      change = passed_change()

      older =
        record("passed")
        |> JSON.decode!()
        |> Map.merge(%{
          "run_id" => "aaaaaaaaaaaa-1786500000000",
          "updated_at_ms" => 1_786_500_000
        })
        |> JSON.encode!()

      newer =
        record("failed")
        |> JSON.decode!()
        |> Map.merge(%{
          "change_id" => change,
          "run_id" => "bbbbbbbbbbbb-1786599999999",
          "updated_at_ms" => 1_786_599_999
        })
        |> JSON.encode!()

      # Newest first, exactly as `ls -t` hands them over.
      assert {:failed, report} = await([newer, older], change_id: change)
      assert report.run_id == "bbbbbbbbbbbb-1786599999999"

      # ...and the order of the read must not be what decides it.
      assert {:failed, same} = await([older, newer], change_id: change)
      assert same.run_id == report.run_id
    end

    # M2. `Runs.records/1` only promises a run_id, so a record whose shape this
    # code cannot read must WAIT (a later poll may see a complete one) and must
    # never raise out through land/2 or be scored as a verdict.
    test "a record with no status waits instead of raising or deciding" do
      shapeless = record("passed") |> JSON.decode!() |> Map.delete("status") |> JSON.encode!()

      assert {:indeterminate, report} = await([shapeless], change_id: passed_change())
      assert report.reason =~ "unreadable shape"
    end

    # M3. The docs promise either alphabet; an id pasted out of a terminal or a
    # web page is often upper case, and a codec that accepts only one case makes
    # that promise false for half its inputs.
    test "an upper-case letter id is accepted, in both directions" do
      {:ok, letters} = Forge.change_id_letters(passed_change())

      assert Forge.change_id_hex(String.upcase(letters)) == {:ok, passed_change()}
      assert {:passed, _report} = await([record("passed")], change_id: String.upcase(letters))

      assert Forge.change_id_letters(String.upcase(passed_change())) == {:ok, letters}
    end

    test "a wait with no key at all is refused rather than matching everything" do
      assert {:error, detail} = await([record("passed")], change_id: nil)
      assert detail =~ ":change_id or a :subject"
    end
  end

  describe "the liveness leg" do
    # Without this, a dead consumer and a slow queue are the same silence, and
    # a waiter reports "still building" about a queue nobody is draining.
    test "a dead CI consumer is indeterminate and says so, rather than waiting out the clock" do
      assert {:indeterminate, report} =
               await([],
                 change_id: passed_change(),
                 consumer: fn -> {:dead, "failed"} end,
                 timeout_ms: 60_000,
                 liveness_after_ms: 0
               )

      assert report.reason =~ "CI consumer is failed"
    end

    # C4. The liveness question is not answered by a record EXISTING. The
    # reconciler can die with its record parked at "building" forever, and a
    # waiter that latches liveness off the first sight of a record then reports
    # "the run is building" until its own timeout -- which reads as a slow queue,
    # not as a dead consumer, and is the exact confusion the liveness leg exists
    # to remove. So: progress resets the clock, it does not answer the question.
    test "a record parked at building with a dead consumer is indeterminate, and names the consumer" do
      assert {:indeterminate, report} =
               await([record("building")],
                 change_id: "1122334455667788990011223344556f",
                 consumer: fn -> {:dead, "inactive (dead)"} end,
                 # Short on purpose: a mutant that treats a dead consumer as
                 # alive must fail this test in a moment, not in a minute.
                 timeout_ms: 300,
                 liveness_after_ms: 0
               )

      assert report.reason =~ "CI consumer is inactive (dead)"
      assert report.reason =~ "building"
      refute report.reason =~ "timed out"
    end

    test "a consumer whose state cannot be read is indeterminate, never alive" do
      assert {:indeterminate, report} =
               await([],
                 change_id: passed_change(),
                 consumer: fn -> {:unknown, "systemctl said nothing"} end,
                 timeout_ms: 60_000,
                 liveness_after_ms: 0
               )

      assert report.reason =~ "could not tell"
    end

    test "a read that fails is retried, not reported as a red main" do
      assert {:indeterminate, report} =
               await([],
                 change_id: passed_change(),
                 read: fn _since -> {:error, "tailnet blinked"} end
               )

      assert report.reason =~ "tailnet blinked"
    end

    test "a unit name a shell could reinterpret is refused" do
      assert {:unknown, detail} = Runs.consumer(%{host: nil, dir: "/tmp"}, "jj-ci; id")
      assert detail =~ "refused"
    end
  end

  describe "landing" do
    defp runner(responses) do
      {:ok, calls} = Agent.start_link(fn -> [] end)

      run = fn _target, argv, _opts ->
        Agent.update(calls, &[argv | &1])

        Enum.find_value(responses, {:ok, ""}, fn {match, reply} ->
          if Enum.any?(argv, &String.contains?(&1, match)), do: reply
        end)
      end

      put = fn _target, path, _body ->
        Agent.update(calls, &[["put", path] | &1])
        :ok
      end

      {run, put, fn -> calls |> Agent.get(&Enum.reverse(&1)) |> Enum.map(&Enum.join(&1, " ")) end}
    end

    defp land(change, opts) do
      {run, put, calls} =
        runner(
          Keyword.get(opts, :responses, [
            {"--name-only", {:ok, "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex\n"}},
            # jj renders `change_id` in its LETTER alphabet. This exact value is
            # what the first live land read out of a real clone, and typing the
            # hex form here instead is what let an inverted conversion pass 33
            # tests.
            {"--no-graph",
             {:ok, "qyrnqtpwnvlrwwqrmpvzmuntnqzrynws\ndb418088c4bc\nA Name\nsomebody@example.com"}}
          ])
        )

      result =
        Forge.land(
          change,
          Keyword.merge(
            [
              author: [name: "A Name", email: "somebody@example.com"],
              land: "/fixture/lands",
              jj: "/fixture/bin/jj",
              server: "https://fixture.invalid/rpc",
              run: run,
              put: put,
              dry_run: true
            ],
            Keyword.delete(opts, :responses)
          )
        )

      {result, calls.()}
    end

    defp change do
      %{
        message: "mcp-ex: a fixture change\n\nBody.\n",
        files: %{"index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex" => "defmodule X do\nend\n"}
      }
    end

    test "a dry run does every step but the submit, and reports both id forms" do
      assert {{:dry_run, report}, calls} = land(change(), [])

      # Reported in hex, because hex is what a run record can be searched for.
      assert report.change_id == "918c96a3c4e83398da40d5c6c9081c37"
      assert report.change_letters == "qyrnqtpwnvlrwwqrmpvzmuntnqzrynws"
      assert {:ok, report.change_id} == Forge.change_id_hex(report.change_letters)
      assert report.files == ["index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex"]

      # A fresh clone, then identity, then the change, then the description --
      # and no submit.
      clone_call = Enum.find(calls, &(&1 =~ "clone https://fixture.invalid/rpc"))
      assert clone_call
      # `--repo ix`: the server URL carries no fragment, so the flag is the
      # only place the repo is named. The trailing "." is load-bearing too:
      # `jj clone` defaults DEST to a NEW directory named after the repo, so
      # without it the checkout lands in <workspace>/ix while every later
      # step runs with cd: <workspace>.
      assert clone_call =~ "--repo ix"
      assert String.ends_with?(clone_call, " .")
      assert Enum.any?(calls, &(&1 =~ "config set --repo user.email somebody@example.com"))
      assert Enum.any?(calls, &(&1 =~ "describe --stdin"))
      refute Enum.any?(calls, &(&1 =~ "submit"))
    end

    # The law this test defends is the one the fourth land attempt discovered by
    # dying of it: a file body must never be an argument. Any body big enough to
    # matter blows MAX_ARG_STRLEN, and small fixtures never show it, so the
    # assertion is about the SHAPE (no body in any argv), not about a size.
    test "no file body, and no encoding of one, ever appears in an argv" do
      assert {{:dry_run, _report}, calls} = land(change(), [])

      body = change().files |> Map.values() |> hd()

      for call <- calls do
        refute call =~ "defmodule X"
        refute call =~ Base.encode64(body)
      end

      # ...and the bodies did move, through the put seam.
      assert Enum.any?(calls, &(&1 =~ "put " and &1 =~ "forge.ex"))
    end

    # The commit message must not become a file in the tree it describes: a
    # message file inside the clone is a path the tier check would refuse, and
    # rightly.
    test "the message file lives beside the clone, never inside it" do
      assert {{:dry_run, report}, calls} = land(change(), [])

      message_writes = Enum.filter(calls, &(&1 =~ "land-message"))

      assert message_writes != []

      for write <- message_writes do
        refute write =~ "#{report.workspace}/land-message"
      end
    end

    test "an unattributed land is refused before anything is cloned" do
      assert {{:error, detail}, calls} = land(change(), author: [])

      assert detail =~ "author"
      assert calls == []
    end

    # The tier check reads jj's own diff, not the caller's file list, so a path
    # the tree GAINED is what it sees.
    test "a path outside the allowed prefixes stops the land" do
      responses = [
        {"--name-only",
         {:ok, "index/packages/mcp-ex/lib/x.ex\nclaude/auto-memory/personal-life.md\n"}}
      ]

      assert {{:error, detail}, _calls} = land(change(), responses: responses)

      assert detail =~ "paths outside"
      assert detail =~ "claude/auto-memory/personal-life.md"
    end

    test "an empty change is refused rather than submitted" do
      assert {{:error, detail}, _calls} =
               land(change(), responses: [{"--name-only", {:ok, "\n"}}])

      assert detail =~ "empty"
    end

    test "a commit jj reports with no author is refused" do
      responses = [
        # The written path, because the presence check now sits between the diff
        # and the identity read: a fixture diff that omits it never reaches the
        # step this test is about.
        {"--name-only", {:ok, "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex\n"}},
        {"--no-graph", {:ok, "qyrnqtpwnvlrwwqrmpvzmuntnqzrynws\ndb418088c4bc\n\n"}}
      ]

      assert {{:error, detail}, _calls} = land(change(), responses: responses)

      assert detail =~ "no author"
    end

    test "a failing step names the step, and nothing after it runs" do
      responses = [{"ix", {:error, "clone exited 2: no route to host"}}]

      assert {{:error, detail}, calls} = land(change(), responses: responses)

      assert detail =~ "no route to host"
      refute Enum.any?(calls, &(&1 =~ "describe"))
    end

    # The other alphabet, so a jj that renders hex is read correctly too. Nothing
    # has to decide WHICH form arrived: the alphabets are disjoint.
    test "an identity rendered in hex is read as readily as one in letters" do
      responses = [
        {"--name-only", {:ok, "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex\n"}},
        {"--no-graph",
         {:ok, "918c96a3c4e83398da40d5c6c9081c37\ndb418088c4bc\nA Name\nsomebody@example.com"}}
      ]

      assert {{:dry_run, report}, _calls} = land(change(), responses: responses)

      assert report.change_id == "918c96a3c4e83398da40d5c6c9081c37"
      assert report.change_letters == "qyrnqtpwnvlrwwqrmpvzmuntnqzrynws"
    end

    test "an identity in neither alphabet is refused rather than guessed at" do
      responses = [
        {"--name-only", {:ok, "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex\n"}},
        {"--no-graph", {:ok, "not-an-id-at-all\ndb418088c4bc\nA Name\nsomebody@example.com"}}
      ]

      assert {{:error, detail}, calls} = land(change(), responses: responses)

      assert detail =~ "reading the commit identity"
      refute Enum.any?(calls, &(&1 =~ "submit"))
    end

    test "a malformed change is refused with what it is missing" do
      assert {{:error, detail}, _calls} = land(%{message: "x"}, [])
      assert detail =~ ":files"

      assert {{:error, blank}, _calls} = land(%{message: "  ", files: %{"index/a" => "b"}}, [])
      assert blank =~ "commit message"
    end

    test "a path that could escape the repo is refused" do
      bad = %{message: "m", files: %{"../../etc/passwd" => "x"}}

      assert {{:error, detail}, _calls} = land(bad, [])
      assert detail =~ "path refused"
    end

    test "an unconfigured forge is an error that names the variable" do
      assert {:error, detail} =
               Forge.land(change(),
                 author: [name: "A", email: "a@b.c"],
                 jj: "/fixture/bin/jj",
                 server: "https://fixture.invalid/rpc",
                 land: nil,
                 run: fn _t, _a, _o -> {:ok, ""} end
               )

      assert detail =~ "IX_MCP_FORGE_LAND"
    end

    # Whole-file writes cannot tell a rebase from a revert, so the guard is
    # what the shell recipe got from `git apply --check`: a moved base FAILS.
    test "an existing file with no stated expectation is refused" do
      responses = [
        {"already exists", {:error, "sh exited 9: already exists; pass :expect"}},
        {"--name-only", {:ok, "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex\n"}}
      ]

      assert {{:error, detail}, calls} = land(change(), responses: responses)

      assert detail =~ "already exists"
      refute Enum.any?(calls, &(&1 =~ "describe"))
    end

    test "a stated expectation that does not match the target bookmark stops the land" do
      responses = [
        {"not what this land expected",
         {:error, "sh exited 9: the file on the target bookmark is not what this land expected"}}
      ]

      assert {{:error, detail}, _calls} =
               land(change(),
                 responses: responses,
                 expect: %{"index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex" => "old\n"}
               )

      assert detail =~ "base moved under this land"
    end

    test "overwrite: true is the escape hatch, and it skips the check entirely" do
      assert {{:dry_run, _report}, calls} = land(change(), overwrite: true)

      refute Enum.any?(calls, &(&1 =~ "already exists"))
    end

    # C6. A write that does not appear in the snapshot is a file silently absent
    # from the commit (an ignore rule matching the path, bytes that landed
    # somewhere else), and the tier check would then pass on whatever DID show
    # up and the land would report a landing of an incomplete change. The
    # remaining path is inside the allowed prefix on purpose: the tier check
    # cannot catch this one, only the presence check can.
    test "a written file missing from the commit is refused by name" do
      responses = [{"--name-only", {:ok, "index/packages/mcp-ex/lib/other.ex\n"}}]

      assert {{:error, detail}, calls} = land(change(), responses: responses)

      assert detail =~ "written but not in the commit"
      assert detail =~ "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex"
      refute Enum.any?(calls, &(&1 =~ "describe"))
    end

    # ...and the order of the two checks that read the same diff is itself a
    # decision, so it is pinned. Both faults are present here: a private-tier
    # path the tree gained, AND a written file the snapshot does not carry. Only
    # the first is unrecoverable once published, and a presence complaint would
    # hide it behind what reads like a local mistake.
    test "a tier violation is reported even when a written file is also missing" do
      responses = [{"--name-only", {:ok, "claude/auto-memory/personal-life.md\n"}}]

      assert {{:error, detail}, calls} = land(change(), responses: responses)

      assert detail =~ "paths outside"
      assert detail =~ "claude/auto-memory/personal-life.md"
      refute detail =~ "written but not in the commit"
      refute Enum.any?(calls, &(&1 =~ "describe"))
    end

    # C7. The message file and the `:expect` pre-images are written under the
    # land root, and `jj describe` snapshots AFTER the tier check said :ok, so a
    # workspace CONTAINING the land root would ship those sidecars to the target
    # bookmark past a check that already passed. Path construction cannot prevent
    # it when the caller chooses the path.
    test "a workspace that contains the land root is refused before anything is written" do
      {run, put, calls} = runner([])

      assert {:error, detail} =
               Forge.land(change(),
                 author: [name: "A Name", email: "somebody@example.com"],
                 land: "/fixture/lands",
                 jj: "/fixture/bin/jj",
                 server: "https://fixture.invalid/rpc",
                 run: run,
                 put: put,
                 dry_run: true,
                 workspace: "/fixture"
               )

      assert detail =~ "contains the land root"
      refute Enum.any?(calls.(), &(&1 =~ "describe"))
      refute Enum.any?(calls.(), &(&1 =~ "put "))
    end

    # C2. A submit whose outcome could not be READ says nothing about what the
    # far side did. Reporting "refused" is the expensive lie: the obvious
    # response is a retry, and retrying a submit that succeeded double-submits
    # the change. So the ids are handed over and the verdict is awaited exactly
    # as for a known submit, and the report says the submit outcome is unknown.
    test "a submit whose outcome could not be read still hands over the ids and awaits" do
      responses = [
        {"--name-only", {:ok, "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex\n"}},
        {"--no-graph",
         {:ok, "qyrnqtpwnvlrwwqrmpvzmuntnqzrynws\ndb418088c4bc\nA Name\nsomebody@example.com"}},
        {"submit", {:unknown, "step did not finish within its budget"}}
      ]

      {run, put, _calls} = runner(responses)
      test_pid = self()

      passed =
        record("passed") |> String.replace(passed_change(), "918c96a3c4e83398da40d5c6c9081c37")

      assert {:passed, report} =
               Forge.land(change(),
                 author: [name: "A Name", email: "somebody@example.com"],
                 land: "/fixture/lands",
                 jj: "/fixture/bin/jj",
                 server: "https://fixture.invalid/rpc",
                 run: run,
                 put: put,
                 runs: "/fixture/runs",
                 read: reader([passed]),
                 consumer: fn -> {:alive, "active"} end,
                 interval_ms: 1,
                 timeout_ms: 50,
                 on_submit: fn r -> send(test_pid, {:submitted, r}) end
               )

      assert report.submit =~ "outcome unknown"
      assert_received {:submitted, handed_over}
      assert handed_over.change_id == "918c96a3c4e83398da40d5c6c9081c37"
    end

    # ...and the other side of that verdict: a submit that was REFUSED is a
    # failure, not an unknown, and nothing is awaited.
    test "a refused submit is an error, and no verdict is awaited" do
      responses = [
        {"--name-only", {:ok, "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex\n"}},
        {"--no-graph",
         {:ok, "qyrnqtpwnvlrwwqrmpvzmuntnqzrynws\ndb418088c4bc\nA Name\nsomebody@example.com"}},
        {"submit", {:error, "submit exited 1: target bookmark main not found"}}
      ]

      {run, put, _calls} = runner(responses)

      assert {:error, detail} =
               Forge.land(change(),
                 author: [name: "A Name", email: "somebody@example.com"],
                 land: "/fixture/lands",
                 jj: "/fixture/bin/jj",
                 server: "https://fixture.invalid/rpc",
                 run: run,
                 put: put,
                 runs: "/fixture/runs",
                 read: fn _since -> flunk("a refused submit must not be awaited") end,
                 interval_ms: 1,
                 timeout_ms: 50
               )

      assert detail =~ "submit refused"
      assert detail =~ "bookmark main not found"
    end

    test "a land is recorded in the stdlib fitness log" do
      land(change(), [])

      assert [%{module: "IxMcp.Stdlib.Forge", function: "land", calls: 1}] =
               IxMcp.Stdlib.fitness()
    end
  end

  # A clone of this repo is 3.4 GB and 13 to 26 minutes, and the first three
  # live land attempts all died AFTER that download, on things unrelated to it
  # (a regex, an id codec, a tailnet drop). Reuse is therefore worth having --
  # and worth gating, because a workspace that already carries somebody's work
  # is how one land silently ships another's change.
  describe "adopting a materialized workspace" do
    defp adopting(diffs, description) do
      {:ok, state} = Agent.start_link(fn -> %{calls: [], diffs: diffs} end)

      run = fn _target, argv, _opts ->
        Agent.update(state, fn s -> %{s | calls: [Enum.join(argv, " ") | s.calls]} end)

        cond do
          Enum.any?(argv, &String.contains?(&1, "description")) ->
            {:ok, description}

          Enum.any?(argv, &String.contains?(&1, "--name-only")) ->
            pop_diff(state)

          Enum.any?(argv, &String.contains?(&1, "change_id")) ->
            {:ok, "qyrnqtpwnvlrwwqrmpvzmuntnqzrynws\ndb418088c4bc\nA Name\nsomebody@example.com"}

          true ->
            {:ok, ""}
        end
      end

      result =
        Forge.land(change(),
          author: [name: "A Name", email: "somebody@example.com"],
          land: "/fixture/lands",
          jj: "/fixture/bin/jj",
          server: "https://fixture.invalid/rpc",
          run: run,
          put: fn _target, _path, _body -> :ok end,
          dry_run: true,
          workspace: "/fixture/lands/land-earlier"
        )

      {result, Agent.get(state, &Enum.reverse(&1.calls))}
    end

    # Adoption runs `diff -r @ --name-only` twice with identical argv -- once as
    # its precondition, once as the tier check -- so the fixture answers by
    # POSITION, which is also what makes the ordering assertion meaningful.
    defp pop_diff(state) do
      Agent.get_and_update(state, fn s ->
        case s.diffs do
          [head | rest] -> {{:ok, head}, %{s | diffs: rest}}
          [] -> {{:ok, ""}, s}
        end
      end)
    end

    # `:target` is the bookmark to land on and `:runs` is where the records
    # live, and land/2 forwards ONE option list to the awaiter. When both were
    # spelled `:target`, saying where the records live silently turned into
    # `jj new hil-compute-1:/root/jj-forge/ci-state/runs`. Caught while writing
    # the live driver, so this test is the reason it stays caught.
    test "the ids are handed over at submit, before the verdict wait" do
      run = fn _target, argv, _opts ->
        cond do
          Enum.any?(argv, &String.contains?(&1, "--name-only")) ->
            {:ok, "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex\n"}

          Enum.any?(argv, &String.contains?(&1, "change_id")) ->
            {:ok, "qyrnqtpwnvlrwwqrmpvzmuntnqzrynws\ndb418088c4bc\nA Name\nsomebody@example.com"}

          true ->
            {:ok, ""}
        end
      end

      test_pid = self()

      # No record for this change, so the wait runs to its timeout: the
      # callback has to have fired anyway, which is the whole point.
      assert {:indeterminate, _report} =
               Forge.land(change(),
                 author: [name: "A Name", email: "somebody@example.com"],
                 land: "/fixture/lands",
                 jj: "/fixture/bin/jj",
                 server: "https://fixture.invalid/rpc",
                 run: run,
                 put: fn _target, _path, _body -> :ok end,
                 runs: "/fixture/runs",
                 read: reader([]),
                 consumer: fn -> {:alive, "active"} end,
                 interval_ms: 1,
                 timeout_ms: 20,
                 on_submit: fn report -> send(test_pid, {:submitted, report}) end
               )

      assert_received {:submitted, report}
      assert report.change_id == "918c96a3c4e83398da40d5c6c9081c37"
      assert report.change_letters == "qyrnqtpwnvlrwwqrmpvzmuntnqzrynws"
      assert report.commit_id == "db418088c4bc"
    end

    # A land is not lost because the caller's callback is broken: the submit has
    # already happened by then, so the only honest thing left is to keep waiting.
    test "a raising on_submit callback does not lose the land" do
      run = fn _target, argv, _opts ->
        cond do
          Enum.any?(argv, &String.contains?(&1, "--name-only")) ->
            {:ok, "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex\n"}

          Enum.any?(argv, &String.contains?(&1, "change_id")) ->
            {:ok, "qyrnqtpwnvlrwwqrmpvzmuntnqzrynws\ndb418088c4bc\nA Name\nsomebody@example.com"}

          true ->
            {:ok, ""}
        end
      end

      passed =
        record("passed") |> String.replace(passed_change(), "918c96a3c4e83398da40d5c6c9081c37")

      assert {:passed, _report} =
               Forge.land(change(),
                 author: [name: "A Name", email: "somebody@example.com"],
                 land: "/fixture/lands",
                 jj: "/fixture/bin/jj",
                 server: "https://fixture.invalid/rpc",
                 run: run,
                 put: fn _target, _path, _body -> :ok end,
                 runs: "/fixture/runs",
                 read: reader([passed]),
                 consumer: fn -> {:alive, "active"} end,
                 interval_ms: 1,
                 timeout_ms: 50,
                 on_submit: fn _report -> raise "a caller's bug" end
               )
    end

    test "the bookmark and the runs directory are different options" do
      {:ok, calls} = Agent.start_link(fn -> [] end)

      run = fn _target, argv, _opts ->
        Agent.update(calls, &[Enum.join(argv, " ") | &1])

        cond do
          Enum.any?(argv, &String.contains?(&1, "--name-only")) ->
            {:ok, "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex\n"}

          Enum.any?(argv, &String.contains?(&1, "change_id")) ->
            {:ok, "qyrnqtpwnvlrwwqrmpvzmuntnqzrynws\ndb418088c4bc\nA Name\nsomebody@example.com"}

          true ->
            {:ok, ""}
        end
      end

      # A land that submits and then waits, so the awaiter's option reading is
      # actually exercised: with one name for both meanings, "/fixture/runs"
      # reached `jj new` and "main" reached the run-record reader, which refuses
      # it as not an absolute directory. Either half is a failed land.
      passed =
        record("passed") |> String.replace(passed_change(), "918c96a3c4e83398da40d5c6c9081c37")

      result =
        Forge.land(change(),
          author: [name: "A Name", email: "somebody@example.com"],
          land: "/fixture/lands",
          jj: "/fixture/bin/jj",
          server: "https://fixture.invalid/rpc",
          run: run,
          put: fn _target, _path, _body -> :ok end,
          target: "main",
          runs: "/fixture/runs",
          read: reader([passed]),
          consumer: fn -> {:alive, "active"} end,
          interval_ms: 1,
          timeout_ms: 50
        )

      assert {:passed, report} = result
      assert report.change_id == "918c96a3c4e83398da40d5c6c9081c37"

      argvs = Agent.get(calls, &Enum.reverse(&1))

      assert Enum.any?(argvs, &(&1 =~ "new main"))
      assert Enum.any?(argvs, &(&1 =~ "submit --target main"))
      refute Enum.any?(argvs, &(&1 =~ "new /fixture/runs"))
    end

    test "a clean workspace is used as-is, and nothing is cloned" do
      assert {{:dry_run, report}, calls} =
               adopting(["\n", "index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex\n"], "\n")

      assert report.workspace == "/fixture/lands/land-earlier"
      refute Enum.any?(calls, &(&1 =~ "clone https://"))

      # Adoption replaces the clone, not the discipline: the same `jj new main`
      # and the same tier check still run.
      assert Enum.any?(calls, &(&1 =~ "new main"))
      assert report.files == ["index/packages/mcp-ex/lib/ix_mcp/stdlib/forge.ex"]
    end

    test "a workspace that already carries changes is refused before anything is written" do
      assert {{:error, detail}, calls} = adopting(["index/some/other/lane.rs\n"], "\n")

      assert detail =~ "already carries changes"
      assert detail =~ "index/some/other/lane.rs"
      refute Enum.any?(calls, &(&1 =~ "describe"))
    end

    test "a workspace holding a described change is refused" do
      assert {{:error, detail}, _calls} = adopting(["\n"], "somebody else's land\n")

      assert detail =~ "not a fresh base"
    end
  end
end
