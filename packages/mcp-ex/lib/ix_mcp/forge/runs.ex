defmodule IxMcp.Forge.Runs do
  @moduledoc """
  The forge CI reconciler's run records, and the one reader over them.

  A gate run writes `<runs-dir>/<commit12>-<epoch_ms>/progress.json` and
  rewrites it on every status transition, so that file IS the verdict and its
  mtime is the moment the verdict was reached. Two consumers read those
  records -- `IxMcp.Forge.Verdicts` sweeps them by mtime to announce verdicts
  nobody asked for, and `IxMcp.Stdlib.Forge` waits on the one record
  belonging to a change it just submitted -- so the reader lives here and a
  correction to it lands once instead of twice.

  ## Two instruments that must never be used instead

    * `queue.json` is a LAGGING snapshot: an entry can be absent from it
      while its run is live, so "not in the queue" never means "not landed".
      A poll for absence reports a landing that has not happened.
    * `jj forge dump` is authoritative and takes 100-160s under load
      while holding the repo, which is longer than any sane poll cadence.

  A run record cannot lie either way, because it is written by the thing
  doing the work.

  ## The commit id in the directory name is the REBASED one

  The queue rebases a submit onto whatever `main` has become, so the commit
  id a submitter holds is not the commit id the run directory is named after,
  and searching for the pre-submit id finds nothing forever. The record's
  `change_id` survives the rebase, which is why that is the key callers are
  given (`IxMcp.Stdlib.Forge.await_verdict/1`), in the 32-hex form the record
  stores rather than jj's reverse-hex letters.

  ## Failing closed

  A read reports its own failure rather than an empty answer: an absent runs
  directory exits 3 and a broken `find` exits 4, both distinguishable from
  "no new runs", which is exit 0 with no output. `find`'s status is checked
  explicitly because a shell pipeline reports only its last stage, so with
  `find` dead the pipeline still exits 0 and its empty output is
  byte-identical to a quiet window.
  """

  require Logger

  @typedoc "Where the records are: a host to ssh to, or nil for this machine."
  @type target :: %{host: String.t() | nil, dir: String.t()}

  @typedoc """
  What a gate run's `log_tail` says beyond its status. Every field degrades
  to empty or nil rather than blocking a verdict: the status is the signal
  and this is the detail.
  """
  @type detail :: %{
          failed_stages: [String.t()],
          tolerated: [String.t()],
          failing_derivations: [%{name: String.t(), tolerated: boolean()}],
          verdict_line: String.t() | nil,
          log: String.t() | nil
        }

  # One sweep reads at most this many changed records. In steady state a
  # minute's window holds one or two; the cap only bounds the first sweep
  # after a long detachment, and hitting it is reported rather than swallowed.
  @read_cap 40
  # A jj command that snapshots this workspace has been measured at just under
  # four minutes, so a step budget has to be well clear of that.
  @step_timeout_ms 900_000
  @step_poll_ms 5_000
  # BatchMode: a feed must never sit on a password prompt. The keepalives
  # bound a hung session, since System.cmd/3 has no timeout of its own.
  @ssh_opts [
    "-o",
    "BatchMode=yes",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=10",
    "-o",
    "ServerAliveCountMax=3"
  ]
  # The target is interpolated into a shell script, so its charset is the
  # security boundary: no quotes, no spaces, no metacharacters, and the
  # directory must be absolute. A target that fails this is treated as
  # absent rather than sanitized, because a half-understood path is not
  # something to guess at.
  # The user part is optional because `host:/dir` is what an ssh config alias
  # looks like, and demanding `user@` rejects a perfectly ordinary target as
  # if it were malformed (measured 2026-08-12: the first live land refused
  # `hil-compute-1:/root/...` this way).
  @safe_target ~r{^(?:(?:[A-Za-z0-9._-]+@)?[A-Za-z0-9._-]+:)?/[A-Za-z0-9._/-]+$}
  # Deliberately no space, no quote, no metacharacter: see put_file/3 on why a
  # quoted path is the WRONG answer for an scp destination. The segment rules
  # are separate (see safe_path?/1): this alphabet stops REINTERPRETATION, and
  # traversal is a different question that needs a different check.
  @safe_path ~r{^/[A-Za-z0-9._/-]+$}
  # A systemd unit name reaches a remote shell the same way, and the same
  # rule applies.
  @safe_unit ~r{^[A-Za-z0-9._@-]+$}
  @detail_chars 200

  # The gate prints its stage table with the verdict in a fixed column, and
  # its tolerated set as a header plus indented names.
  # The gate's stage table, with the stage's exit code when it printed one. The
  # code is not decoration: the gate wraps each stage in `timeout`, whose 124
  # means the BUDGET expired rather than the change being wrong, and a caller
  # that cannot tell those apart resubmits a change nothing was wrong with.
  # Measured 2026-08-13 on a live record: `lint FAIL rc=124 5401s` against a
  # 5400s budget, with no failing derivation named because nothing got far
  # enough to fail.
  @fail_stage ~r/^[ \t]+([a-z][\w-]*)[ \t]+FAIL(?:[ \t]+rc=(\d+))?\b/m
  # ...and FAIL is not the only word the same table uses for a fatal stage. A
  # stage killed by its own budget is printed `eval   TIMEOUT after 3600s`, with
  # NO rc at all, so a reader that greps for FAIL reports zero failed stages on a
  # run whose verdict line says FAIL. Found 2026-08-13 the expensive way: this
  # reader said `stage_failures: []` for run 84a6f4e7e42's record and the empty
  # result was read as data rather than as a broken instrument, which put a false
  # citation into an attribution handed to another lane.
  @timeout_stage ~r/^[ \t]+([a-z][\w-]*)[ \t]+TIMEOUT\b/m
  # `timeout(1)` reports a killed command as 124, and the gate runs every stage
  # under it.
  @timeout_rc 124
  # The gate prints its log path twice with different padding (once in its
  # header, once in the verdict block); both name the same file, so the first
  # match is enough and the padding must be tolerated.
  @log_path ~r/^log:[ \t]+(\S+)$/m
  @indented ~r/^[ \t]+\S/
  # `==== gate verdict: FAIL ====`. Anchored to the whole line so the
  # `incr-gate` sub-verdict the same log also prints cannot be mistaken for
  # the gate's own.
  @verdict_line ~r/^==== gate verdict: [A-Z]+ ====$/m
  # An indented derivation, with or without a `[...]` annotation. The gate
  # names some derivations by store path and some bare, so the path shape is
  # not part of the match: the NAME is extracted afterwards, because a
  # derivation name may itself contain `-` (`ix-shell-fence-check`) and no
  # regex over that can also strip a store hash correctly.
  @derivation ~r/^[ \t]+(\S+\.drv)[ \t]*(?:\[([^\]]*)\])?[ \t]*$/m
  # `/nix/store/<32-char base32 hash>-<name>.drv`: the hash differs on every
  # run and is noise to a reader deciding whether a failure is theirs.
  @store_hash ~r/^[0-9a-z]{32}-/

  @doc """
  Split `[user@host:]/abs/dir` into a target, refusing anything a remote
  shell could reinterpret.
  """
  @spec parse_target(String.t()) :: {:ok, target()} | :error
  def parse_target(value) when is_binary(value) do
    trimmed = String.trim(value)

    if Regex.match?(@safe_target, trimmed) do
      case String.split(trimmed, ":", parts: 2) do
        [host, dir] -> {:ok, %{host: host, dir: dir}}
        [dir] -> {:ok, %{host: nil, dir: dir}}
      end
    else
      :error
    end
  end

  @doc """
  A reader for `target`: a one-argument function taking the `DateTime` to
  read from and returning the records changed since then.

  Local mode requires the directory to exist NOW, because a bare path naming
  nothing is a misconfiguration rather than a machine that is merely asleep;
  the remote case is the one that must survive being unreachable at boot.
  """
  @spec reader(target()) ::
          {:ok, (DateTime.t() -> {:ok, String.t()} | {:error, String.t()})} | :error
  def reader(%{host: nil, dir: dir} = target) do
    if File.dir?(dir), do: {:ok, &read(target, &1)}, else: :error
  end

  def reader(%{host: _host} = target) do
    if System.find_executable("ssh"), do: {:ok, &read(target, &1)}, else: :error
  end

  @doc """
  Read every run record whose file changed at or after `since`, newest
  first, as one JSON object per line.

  The whole sweep is one `sh` invocation, local or over one `ssh`, because
  the cost of a remote read is round trips and nothing else.
  """
  @spec read(target(), DateTime.t()) :: {:ok, String.t()} | {:error, String.t()}
  def read(target, since) do
    run(target, ["sh", "-c", script(target.dir, since)], parse: true)
  end

  @doc """
  Decode a read's output into records, oldest-to-newest as the reader
  emitted them.

  One record per line: the reader collapses each pretty-printed file to a
  single line, and a JSON string cannot contain a raw newline, so nothing is
  lost. Non-`{` lines are ssh's own chatter on the shared stream, which is
  kept rather than discarded so a failure's detail survives.
  """
  @spec records(String.t()) :: [map()]
  def records(output) when is_binary(output) do
    {records, undecodable} =
      output
      |> String.split("\n", trim: true)
      |> Enum.filter(&String.starts_with?(&1, "{"))
      |> Enum.reduce({[], 0}, fn line, {records, undecodable} ->
        case JSON.decode(line) do
          {:ok, %{"run_id" => _id} = record} -> {[record | records], undecodable}
          _undecodable -> {records, undecodable + 1}
        end
      end)

    # A record caught mid-write is expected and transient -- the next sweep
    # re-reads the same window -- but a silent skip would also hide a format
    # change, so it is counted out loud.
    if undecodable > 0 do
      Logger.warning("forge run records: skipped #{undecodable} unreadable record(s)")
    end

    records
  end

  @doc """
  What a `log_tail` says beyond the status: which stages failed, which
  checks were already red on the target tip, which derivations failed and
  whether each was tolerated, the gate's own verdict line, and the log path.

  Best-effort by design. Anything unparseable degrades to empty rather than
  blocking the verdict, and `nil` (no tail at all) is not an error.
  """
  @spec detail(String.t() | nil) :: detail()
  def detail(tail) when is_binary(tail) do
    %{
      failed_stages: failed_stages(tail),
      stage_failures: stage_failures(tail),
      tolerated: tolerated(tail),
      failing_derivations: failing_derivations(tail),
      verdict_line: verdict_line(tail),
      log: log_path(tail)
    }
  end

  def detail(_absent) do
    %{
      failed_stages: [],
      stage_failures: [],
      tolerated: [],
      failing_derivations: [],
      verdict_line: nil,
      log: nil
    }
  end

  @doc "The ceiling on records one read will list; a read that hits it is truncated."
  @spec read_cap() :: pos_integer()
  def read_cap, do: @read_cap

  @doc """
  Whether the thing that turns submits into verdicts is running.

  Without this leg a dead consumer is indistinguishable from a slow one, and
  a waiter reports "still waiting" forever about a queue nobody is draining.
  `systemctl is-active` exits non-zero for every state but `active` while
  still naming the state on stdout, so the status is not read as the answer:
  the WORD is. A state this function cannot make sense of is `:unknown`,
  never `:dead` and never `:alive`.
  """
  @spec consumer(target(), String.t()) ::
          {:alive, String.t()} | {:dead, String.t()} | {:unknown, String.t()}
  def consumer(target, unit) when is_binary(unit) do
    if Regex.match?(@safe_unit, unit) do
      probe(target, unit)
    else
      {:unknown, "unit name refused: #{inspect(unit)}"}
    end
  end

  @doc """
  Quote one argument for a remote POSIX shell.

  `ssh host a b c` does not pass an argv: it joins the words and hands the
  string to a shell on the far side, so every argument that reaches a remote
  shell is quoted here or is an injection.
  """
  @spec shell_quote(String.t()) :: String.t()
  def shell_quote(arg) when is_binary(arg) do
    "'" <> String.replace(arg, "'", "'\\''", global: true) <> "'"
  end

  @doc """
  Run one command, locally or on `target`'s host, and return its output.

  `argv` is a real argument vector: on this machine it never meets a shell,
  and for a remote host every element is quoted by `shell_quote/1` before the
  words are joined, because ssh's far side always has one.

  `parse: true` for any command whose stdout is going to be READ rather than
  merely checked: stderr is then kept out of the returned output, so a progress
  or warning line cannot be parsed as data.
  """
  @spec run(target(), [String.t()], keyword()) :: {:ok, String.t()} | {:error, String.t()}
  def run(target, argv, opts \\ [])

  def run(target, argv, opts) do
    if Keyword.get(opts, :parse, false) do
      parsed(target, argv, opts)
    else
      raw_run(target, argv, opts)
    end
  end

  # Output that is going to be PARSED cannot share a stream with diagnostics.
  # jj writes "Concurrent modification detected, resolving automatically." to
  # stderr whenever two processes touch a repo at once, and with the streams
  # merged that sentence arrives as one more line of whatever was being read.
  # Verified 2026-08-12: it reached a tier check as if it were a repo path, and
  # the check refused a land -- correctly, but for a reason that had nothing to
  # do with the change. The same line would have broken a four-line identity
  # read into five.
  #
  # So the far side keeps stderr in a file and appends it after a sentinel this
  # call invents, which is why the sentinel is random rather than a constant:
  # a constant is a string the command's own output could contain.
  @spec parsed(target(), [String.t()], keyword()) :: {:ok, String.t()} | {:error, String.t()}
  defp parsed(target, argv, opts) do
    sentinel = "ix-stderr-" <> Base.encode16(:crypto.strong_rand_bytes(8), case: :lower)
    command = Enum.map_join(argv, " ", &shell_quote/1)

    command =
      case Keyword.get(opts, :cd) do
        nil -> command
        dir -> "cd #{shell_quote(dir)} && " <> command
      end

    # No `mktemp`: TMPDIR is what a build sandbox sets, and the sentinel is
    # already unique, so the file needs no extra source of uniqueness and the
    # script needs no extra tool to exist on the far side.
    script = """
    set -u
    err="${TMPDIR:-/tmp}/#{sentinel}"
    { #{command} ; } 2>"$err"
    rc=$?
    printf '%s' '#{sentinel}'
    cat "$err"
    rm -f "$err"
    exit $rc
    """

    case raw_run(target, ["sh", "-c", script], Keyword.delete(opts, :cd)) do
      {:ok, merged} ->
        {:ok, merged |> String.split(sentinel, parts: 2) |> hd()}

      {:error, detail} ->
        {:error, String.replace(detail, sentinel, " -- stderr: ")}
    end
  end

  @spec raw_run(target(), [String.t()], keyword()) :: {:ok, String.t()} | {:error, String.t()}
  defp raw_run(%{host: nil}, [bin | args], opts) do
    cmd(bin, args, opts)
  end

  defp raw_run(%{host: host}, argv, opts) do
    remote = Enum.map_join(argv, " ", &shell_quote/1)

    # `:cd` names a directory on the FAR side, so it becomes part of the
    # remote command. Passing it to System.cmd would move the local ssh
    # process instead and leave the remote command running in $HOME, which
    # is the kind of wrong that succeeds.
    remote =
      case Keyword.get(opts, :cd) do
        nil -> remote
        dir -> "cd #{shell_quote(dir)} && " <> remote
      end

    cmd("ssh", @ssh_opts ++ [host, remote], Keyword.drop(opts, [:cd, :parse]))
  end

  @doc """
  Put `contents` at `path` on the far side, creating parent directories.

  File bodies NEVER travel in argv. Linux caps a SINGLE argument string at
  MAX_ARG_STRLEN, 32 pages (131,072 bytes), independently of the 2 MiB ARG_MAX
  this host reports for the whole list, and base64 of a body inside the base64
  wrapper a detached step needs is 1.78x the file. Measured 2026-08-12: land
  attempt 4 of this very module died with `bash: Argument list too long` writing
  a 48 KB source file, having succeeded on a 14 KB one, which is the shape of a
  limit that lets small tests pass forever.

  `scp` moves the bytes over the same ssh transport with no shell in between, so
  size stops being a correctness question. The remote path is checked against a
  strict allowlist rather than quoted, because modern scp speaks SFTP where the
  path is NOT shell-expanded and quotes would become part of the filename.
  """
  @spec put_file(target(), String.t(), binary()) :: :ok | {:error, String.t()}
  def put_file(target, path, contents) when is_binary(path) and is_binary(contents) do
    case safe_path?(path) do
      :ok -> place(target, path, contents)
      {:error, why} -> {:error, "#{path} is not a safe absolute path for a remote write: #{why}"}
    end
  end

  # Three separate refusals, because they fail differently. A bad alphabet is a
  # reinterpretation risk. A `..` segment means the caller's idea of where the
  # bytes go is not where they go. A TRAILING SLASH is the quiet one: scp copies
  # INTO a directory and names the file after the local temp, so put_file would
  # return :ok having written `<dir>/ix-put-<hex>` and nothing at the path the
  # caller asked for.
  @spec safe_path?(String.t()) :: :ok | {:error, String.t()}
  defp safe_path?(path) do
    segments = String.split(path, "/")

    cond do
      not Regex.match?(@safe_path, path) -> {:error, "not an absolute path in a safe alphabet"}
      String.ends_with?(path, "/") -> {:error, "a trailing slash names a directory, not a file"}
      Enum.any?(segments, &(&1 in [".", ".."])) -> {:error, "a . or .. segment"}
      true -> :ok
    end
  end

  @spec place(target(), String.t(), binary()) :: :ok | {:error, String.t()}
  defp place(%{host: nil}, path, contents) do
    with :ok <- File.mkdir_p(Path.dirname(path)) do
      File.write(path, contents)
    end
    |> case do
      :ok -> :ok
      {:error, posix} -> {:error, "#{path}: #{:file.format_error(posix)}"}
    end
  end

  defp place(%{host: host} = target, path, contents) do
    local = Path.join(System.tmp_dir!(), "ix-put-" <> Base.encode16(:crypto.strong_rand_bytes(8)))

    try do
      with :ok <- File.write(local, contents),
           {:ok, _made} <-
             run(target, ["mkdir", "-p", Path.dirname(path)], []),
           {:ok, _copied} <-
             cmd("scp", @ssh_opts ++ ["-q", local, "#{host}:#{path}"], []) do
        :ok
      else
        {:error, detail} -> {:error, "#{path}: #{inspect(detail)}"}
      end
    after
      File.rm(local)
    end
  end

  @doc """
  Run `argv` as a DETACHED step: launched under `nohup` with stdout, stderr and
  the exit code going to files, then collected by polling short connections.

  A jj command in a 172,565-file workspace takes minutes, and a minutes-long
  ssh session is a command whose completion you cannot determine once the
  socket dies. Measured 2026-08-12: a land of this repo was lost exactly that
  way, to `ssh exited 255: Timeout, server not responding` in the middle of a
  `jj config set`, and the tailnet was dropping for every process on the
  machine at the time. Detached, a dropped connection costs one poll, and the
  only exposure left is the launch itself -- about a hundred milliseconds, and
  even that is decidable rather than ambiguous, because the step's own
  directory says whether it started.

  Stream separation is inherent here: stdout and stderr are different files,
  so a progress line cannot be parsed as data.

  Requires `:state`, a writable directory on the far side. `:cd` names the
  directory the command runs in, `:timeout_ms` bounds the whole step
  (default #{@step_timeout_ms} ms).
  """
  @spec run_detached(target(), [String.t()], keyword()) ::
          {:ok, String.t()} | {:error, String.t()} | {:unknown, String.t()}
  def run_detached(target, argv, opts \\ []) do
    step = "ix-step-" <> Base.encode16(:crypto.strong_rand_bytes(8), case: :lower)

    deadline =
      System.monotonic_time(:millisecond) + Keyword.get(opts, :timeout_ms, @step_timeout_ms)

    case Keyword.get(opts, :state) do
      state when is_binary(state) ->
        dir = Path.join(state, step)

        with :ok <- launch(target, argv, opts, dir) do
          collect(target, dir, step, deadline, opts)
        end

      _missing ->
        {:error, "a detached step needs :state, a writable directory on the far side"}
    end
  end

  # The command rides as base64 and is decoded from a FILE, never a pipe: a
  # pipeline reports only its last stage, so a failed decode would leave an
  # empty script that runs and "succeeds".
  @spec launch(target(), [String.t()], keyword(), String.t()) ::
          :ok | {:error, String.t()} | {:unknown, String.t()}
  defp launch(target, argv, opts, dir) do
    command = Enum.map_join(argv, " ", &shell_quote/1)

    command =
      case Keyword.get(opts, :cd) do
        nil -> command
        cd -> "cd #{shell_quote(cd)} && " <> command
      end

    quoted = shell_quote(dir)

    # The wrapper is a FILE, not an argument to `sh -c`, and that is the whole
    # design of this function rather than a style choice. Written inline, its
    # `$?` is expanded by the LAUNCHING shell at the moment the argument is
    # built, so the rc file records the launcher's status and every step passes.
    # Two tests here caught exactly that (`exit 3` and `false` both read as
    # {:ok, ...}) before this ran against the forge; an executor that cannot
    # report a failure would have let a broken land step continue silently.
    wrapper = """
    : > #{quoted}/started
    sh #{quoted}/cmd.sh > #{quoted}/out 2> #{quoted}/err
    echo $? > #{quoted}/rc
    """

    script = """
    set -eu
    d=#{quoted}
    mkdir -p "$d"
    printf '%s' #{shell_quote(Base.encode64(command <> "\n"))} > "$d/cmd.b64"
    base64 -d < "$d/cmd.b64" > "$d/cmd.sh"
    printf '%s' #{shell_quote(Base.encode64(wrapper))} > "$d/run.b64"
    base64 -d < "$d/run.b64" > "$d/run.sh"
    nohup sh "$d/run.sh" </dev/null >/dev/null 2>&1 &
    echo launched
    """

    case run(target, ["sh", "-c", script], []) do
      {:ok, _launched} ->
        :ok

      {:error, detail} ->
        # A lost acknowledgement is not a lost launch: the step's directory is
        # the fact. Only a directory that never appeared means it never ran.
        # A lost acknowledgement is not a lost launch, but the file that proves
        # it has to be one the WRAPPER writes. `cmd.sh` exists four lines
        # before `nohup`, so probing for it says "started" for a connection
        # that died in that window, and the step then burns its whole budget
        # before reporting a timeout for something that never ran.
        case run(target, ["sh", "-c", "test -e #{quoted}/started && echo yes || echo no"],
               parse: true
             ) do
          {:ok, output} ->
            if String.contains?(output, "yes"),
              do: :ok,
              else: {:error, "launching the step: #{detail}"}

          {:error, unreadable} ->
            # Cannot tell whether it started. That is not a failure and it is
            # not a success; saying either would be a guess.
            {:unknown, "launching the step: #{detail} (and the probe failed: #{unreadable})"}
        end
    end
  end

  @spec collect(target(), String.t(), String.t(), integer(), keyword()) ::
          {:ok, String.t()} | {:error, String.t()} | {:unknown, String.t()}
  defp collect(target, dir, step, deadline, opts) do
    quoted = shell_quote(dir)

    # Every field is delimited by the step's own marker, INCLUDING the exit
    # code, because the poll's stdout does not begin where this script begins:
    # ssh's chatter shares the stream ("Warning: Permanently added ...", a
    # server banner, "Connection to X closed"), and a head-anchored `rc=` parse
    # reads one such line as "still running". That turns a FINISHED step into a
    # budget-length wait and then a timeout, which is the worst shape of wrong:
    # confident, late, and about a step that already succeeded.
    script = """
    set -u
    d=#{quoted}
    if [ -f "$d/rc" ]; then
      printf '%s' '#{step}'
      cat "$d/rc"
      printf '%s' '#{step}'
      cat "$d/out"
      printf '%s' '#{step}'
      cat "$d/err"
    else
      echo running
    fi
    """

    # A transport error on a poll is not information about the step, so it is
    # simply polled again; only the deadline ends the wait.
    outcome =
      case run(target, ["sh", "-c", script], parse: true) do
        {:ok, output} -> parse_step(output, step)
        {:error, _transport} -> :running
      end

    case outcome do
      {:done, 0, out, _err} ->
        _cleaned = run(target, ["sh", "-c", "rm -rf #{quoted}"], [])
        {:ok, out}

      {:done, rc, out, err} ->
        # Concatenating the streams and taking the last line silently prefers
        # stdout, which is the stream that does NOT explain the failure;
        # measured on `sh -c 'echo out; echo why >&2; exit 3'`, which reported
        # "out". stderr wins when it said anything at all -- and the step's
        # directory SURVIVES a failure, because one 200-character line is not
        # enough to debug a failed land and the full output is already there.
        reason = if String.trim(err) == "", do: tail_of(out), else: tail_of(err)
        {:error, "step exited #{rc}: #{reason} (full output kept at #{dir})"}

      :running ->
        if System.monotonic_time(:millisecond) >= deadline do
          # NOT an error: a step still running at its deadline has an outcome
          # nobody has read yet. A caller that treats this as "it failed" will
          # retry an action that may have succeeded.
          {:unknown, "step did not finish within its budget; its output is at #{dir}"}
        else
          Process.sleep(Keyword.get(opts, :poll_ms, @step_poll_ms))
          collect(target, dir, step, deadline, opts)
        end
    end
  end

  @doc false
  # Public only so the chatter law above can be pinned by a test: the failure it
  # prevents lives in text that arrives from ssh, which a local test cannot
  # inject through the transport.
  @spec parse_step(String.t(), String.t()) ::
          {:done, integer(), String.t(), String.t()} | :running
  def parse_step(output, step) do
    case String.split(output, step, parts: 4) do
      [_chatter, rc, out, err] ->
        case Integer.parse(String.trim(rc)) do
          {code, _rest} -> {:done, code, out, err}
          :error -> :running
        end

      _still_running ->
        :running
    end
  end

  # ── internals ─────────────────────────────────────────────────────────

  @spec probe(target(), String.t()) ::
          {:alive, String.t()} | {:dead, String.t()} | {:unknown, String.t()}
  defp probe(target, unit) do
    # `is-active` is allowed to exit non-zero: that IS the inactive case, and
    # treating it as a failed read would turn every dead consumer into an
    # unreadable one.
    case raw(target, ["systemctl", "is-active", unit]) do
      {output, _status} ->
        case String.trim(output) do
          "active" -> {:alive, "active"}
          state when state in ["inactive", "failed", "deactivating"] -> {:dead, state}
          "" -> {:unknown, "systemctl said nothing about #{unit}"}
          state -> {:unknown, String.slice(state, 0, @detail_chars)}
        end

      :error ->
        {:unknown, "could not probe #{unit}"}
    end
  end

  @spec raw(target(), [String.t()]) :: {String.t(), integer()} | :error
  defp raw(%{host: nil}, [bin | args]) do
    System.cmd(bin, args, stderr_to_stdout: true)
  rescue
    _error -> :error
  end

  defp raw(%{host: host}, argv) do
    System.cmd("ssh", @ssh_opts ++ [host, Enum.map_join(argv, " ", &shell_quote/1)],
      stderr_to_stdout: true
    )
  rescue
    _error -> :error
  end

  @spec cmd(String.t(), [String.t()], keyword()) :: {:ok, String.t()} | {:error, String.t()}
  defp cmd(bin, args, opts) do
    extra = opts |> Keyword.take([:cd, :env]) |> Enum.reject(&match?({_key, nil}, &1))

    case System.cmd(bin, args, [stderr_to_stdout: true] ++ extra) do
      {output, 0} -> {:ok, output}
      {output, status} -> {:error, "#{bin} exited #{status}: #{tail_of(output)}"}
    end
  rescue
    error -> {:error, "#{bin} failed: #{Exception.message(error)}"}
  end

  # The last thing the failing command said, bounded. CI output carries no
  # private content, but a log line is not the place for a page of it.
  @spec tail_of(String.t()) :: String.t()
  defp tail_of(output) do
    output
    |> String.split("\n", trim: true)
    |> List.last("no output")
    |> String.slice(0, @detail_chars)
  end

  # `find`'s status is read directly instead of through the pipeline, because
  # a pipeline reports only its last stage: with `find` dead, `while` still
  # exits 0 and the empty output is byte-identical to a quiet window.
  #
  # LC_ALL=C is not decoration. `-printf '%T@'` prints a fractional epoch
  # with a `.`, and under a locale whose decimal separator is `,` a numeric
  # sort compares the integer parts only and then reorders equal-second runs
  # arbitrarily -- which silently picks the wrong newest record.
  @spec script(String.t(), DateTime.t()) :: String.t()
  defp script(dir, since) do
    """
    set -u
    LC_ALL=C
    export LC_ALL
    runs='#{dir}'
    if [ ! -d "$runs" ]; then
      echo "forge CI runs dir absent: $runs" >&2
      exit 3
    fi
    changed=$(find "$runs" -mindepth 2 -maxdepth 2 -name progress.json \
      -newermt '#{DateTime.to_iso8601(since)}' -printf '%T@ %p\\n') || {
      echo "find over $runs failed" >&2
      exit 4
    }
    printf '%s\\n' "$changed" | sort -rn | head -n #{@read_cap} | cut -d' ' -f2- |
      while IFS= read -r record; do
        [ -n "$record" ] || continue
        tr -d '\\n' < "$record" && printf '\\n'
      done
    """
  end

  @spec failed_stages(String.t()) :: [String.t()]
  defp failed_stages(tail) do
    tail |> stage_failures() |> Enum.map(& &1.stage)
  end

  # One entry per failed stage: its name, the exit code if the gate printed one,
  # and whether that code is the budget expiring. A reader deciding whether to
  # resubmit needs the third field, and deriving it here is what keeps that
  # decision from being re-derived (differently) at every call site.
  @spec stage_failures(String.t()) :: [
          %{stage: String.t(), rc: integer() | nil, timed_out: boolean()}
        ]
  defp stage_failures(tail) do
    fails =
      @fail_stage
      |> Regex.scan(tail)
      |> Enum.map(fn
        [_line, stage, rc] -> stage_failure(stage, Integer.parse(rc))
        [_line, stage] -> stage_failure(stage, :error)
      end)

    # A budget-killed stage has no rc to read, so `timed_out` comes from the word
    # rather than from a code. Deliberately NOT matched here: `incr fallback
    # rc=124`, which is the gate DEGRADING to the serial path and continuing --
    # a 124 that is not a failure at all. Widening this to any `rc=124` would
    # report a healthy fallback as a dead stage, which is the same class of lie
    # in the other direction.
    timeouts =
      @timeout_stage
      |> Regex.scan(tail)
      |> Enum.map(fn [_line, stage] -> %{stage: stage, rc: nil, timed_out: true} end)

    Enum.uniq_by(fails ++ timeouts, & &1.stage)
  end

  @spec stage_failure(String.t(), {integer(), String.t()} | :error) :: %{
          stage: String.t(),
          rc: integer() | nil,
          timed_out: boolean()
        }
  defp stage_failure(stage, {rc, _rest}),
    do: %{stage: stage, rc: rc, timed_out: rc == @timeout_rc}

  defp stage_failure(stage, :error), do: %{stage: stage, rc: nil, timed_out: false}

  @spec log_path(String.t()) :: String.t() | nil
  defp log_path(tail) do
    case Regex.run(@log_path, tail) do
      [_line, path] -> path
      _absent -> nil
    end
  end

  @spec verdict_line(String.t()) :: String.t() | nil
  defp verdict_line(tail) do
    case Regex.run(@verdict_line, tail) do
      [line] -> line
      _absent -> nil
    end
  end

  # The tolerated set is a header line followed by indented names and ended by
  # the first line at column zero. Naming it matters as much as naming the
  # failure: a stage that failed only on an already-red check is not this
  # change's fault, and a reader who cannot tell will go debug the wrong tree.
  @spec tolerated(String.t()) :: [String.t()]
  defp tolerated(tail) do
    tail
    |> String.split("\n")
    |> Enum.drop_while(&(not String.starts_with?(&1, "TOLERATED")))
    |> Enum.drop(1)
    |> Enum.take_while(&Regex.match?(@indented, &1))
    |> Enum.map(&String.trim/1)
    |> Enum.uniq()
  end

  # Which derivations failed, by name rather than by store path: the hash in
  # `/nix/store/<hash>-treefmt-check.drv` differs on every run and is noise
  # to a reader deciding whether the failure is theirs. The gate annotates a
  # derivation that also fails on the target tip, and that annotation is the
  # difference between "you broke this" and "this was already red", so it is
  # carried rather than dropped.
  @spec failing_derivations(String.t()) :: [%{name: String.t(), tolerated: boolean()}]
  defp failing_derivations(tail) do
    already_red = tolerated(tail)

    @derivation
    |> Regex.scan(tail)
    |> Enum.map(fn match ->
      [_line, path | rest] = match
      name = derivation_name(path)
      annotation = List.first(rest) || ""

      {name, String.contains?(annotation, "tolerated") or name in already_red}
    end)
    # The same derivation is usually named twice -- once in the fatal or
    # tolerated summary and once with its store path -- and only one of those
    # two lines carries the annotation, so the flags are OR-ed rather than
    # first-wins. First-wins here would silently downgrade "already red" to
    # "you broke this", which is the one thing this field decides.
    |> Enum.reduce([], fn {name, tolerated}, acc ->
      case Enum.find_index(acc, &(&1.name == name)) do
        nil -> acc ++ [%{name: name, tolerated: tolerated}]
        index -> List.update_at(acc, index, &%{&1 | tolerated: &1.tolerated or tolerated})
      end
    end)
  end

  @spec derivation_name(String.t()) :: String.t()
  defp derivation_name(path) do
    path
    |> Path.basename()
    |> String.replace_suffix(".drv", "")
    |> String.replace(@store_hash, "")
  end
end
