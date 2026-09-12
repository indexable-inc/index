defmodule IxMcp.Stdlib.Forge do
  @moduledoc ~S"""
  Landing a change on the forge's protected `main`, and waiting for the
  verdict that says whether it landed.

  The forge's merge queue is the only way onto a protected bookmark: `submit`
  writes an operation carrying the intent, the CI reconciler rebases the
  change onto whatever `main` has become, runs the gate against the REBASED
  commit, and moves the bookmark on a pass. Everything hard about landing is
  downstream of that rebase.

  ## Provenance

  2026-08-12. Four lanes landed changes through this queue on one day, each
  working from its own retyped copy of the recipe, and every copy grew a
  different fault:

    * a typo in the submit verb, so the submit never happened and the lane
      waited on a queue it was not in;
    * a waiter polling `queue.json` for ABSENCE, which is a lagging snapshot
      -- an entry can be missing while the run is live, so the poll reported
      a landing that had not happened;
    * a comparison fed by a `sort` under the ambient locale, which orders
      fractional epochs by their integer part and picks the wrong newest
      record;
    * a heredoc feeding a pipe, where the writing stage's failure was
      invisible because a pipeline reports only its last stage.

  None of those four faults was a misunderstanding of the forge. Each was an
  artifact of RETYPING a procedure that already existed and had already been
  debugged. This module is that procedure, once, so the next lane copies a
  call instead of a recipe.

  Two more corrections are encoded here because they are silent rather than
  loud, which makes them worse:

    * The verdict search keys on `change_id`, never on the commit id held
      before submitting. The queue rebases, so the pre-submit commit id
      appears in no run record, and a waiter keyed on it waits forever with
      no error.
    * `change_id` is keyed in its 32-HEX form. jj prints change ids in
       reverse-hex letters (`z..k` for `0..f`), the run records store hex,
       and grepping the letter form finds nothing forever -- also silently.
       `change_id_hex/1` and `change_id_letters/1` convert, and are pinned
       by a test against ids verified against live run records.

  ## What a verdict is, and what it is not

  `await_verdict/1` returns `{:passed, report}` or `{:failed, report}` only
  for the two terminal statuses the reconciler writes. Everything else is
  `{:indeterminate, report}`, including the case that matters most: the CI
  consumer being dead. A waiter that cannot tell "nobody is draining the
  queue" from "still building" reports the first as the second forever, so
  liveness is a leg of the answer rather than a footnote
  (`IxMcp.Forge.Runs.consumer/2`).

  ## Configuration

  This tree is public and a forge host is a specific machine, so no host,
  path or URL is compiled in. Landing needs all four; reading verdicts needs
  only the first.

    * `IX_MCP_FORGE_CI` -- `[user@host:]<runs-dir>`, the reconciler's run
      records, shared with `IxMcp.Forge.Verdicts`.
    * `IX_MCP_FORGE_LAND` -- `[user@host:]<workspace-root>`, a directory on
      the forge host under which each land gets its own fresh clone.
    * `IX_MCP_FORGE_JJ` -- absolute path to the forge-aware `jj` binary on
      that host.
    * `IX_MCP_FORGE_SERVER` -- the forge's RPC URL.
    * `IX_MCP_FORGE_UNIT` -- the CI reconciler's systemd unit, for the
      liveness leg. Defaults to `jj-forge-ci`.

  ## A tree whose only content is this land

  By default `land/2` clones into a new directory every time. A carried-over
  workspace can hold the previous land's working-copy commit, its stale index,
  or a half-applied change, and the failure that produces looks like a defect in
  the change being landed rather than in the workspace. What the fresh clone
  buys is a tree whose only content is what this call put there, which is also
  what makes the path-tier check meaningful.

  That property, not the download, is the requirement, so `:workspace` may point
  at an already-materialized clone and it is verified rather than trusted: no
  changes in the working copy, no description on `@`, or the land is refused.
  The download is 3.4 GB and 13 to 26 minutes, and the first four attempts to
  land this module all died AFTER it, on faults it had nothing to do with.

  The clone is not the only thing that has to stay out of the tree: the commit
  message file and the `:expect` pre-images are written BESIDE the workspace,
  never inside it, or the tier check would see them as paths this land is trying
  to ship.
  """

  alias IxMcp.Forge.Runs
  alias IxMcp.Stdlib

  require Logger

  @env_runs "IX_MCP_FORGE_CI"
  @env_land "IX_MCP_FORGE_LAND"
  @env_jj "IX_MCP_FORGE_JJ"
  @env_server "IX_MCP_FORGE_SERVER"
  @env_unit "IX_MCP_FORGE_UNIT"
  @default_unit "jj-forge-ci"

  # jj renders a change id in reverse hex: the digits run backwards through
  # the alphabet from `z`, so `z` is 0 and `k` is 15. Deliberately not the
  # same alphabet as a commit id, so a change id can never be mistaken for
  # one -- and equally, so a hex search for one never matches the other.
  @letters "zyxwvutsrqponmlk"
  @hex "0123456789abcdef"
  @letter_to_hex Map.new(Enum.zip(String.graphemes(@letters), String.graphemes(@hex)))
  @hex_to_letter Map.new(Enum.zip(String.graphemes(@hex), String.graphemes(@letters)))

  @change_id_chars 32
  # An argument vector is not a file transfer: file bytes ride in one base64
  # argument, and a remote `sh` has a finite argv. Well above any source
  # file and well below the limit.
  @max_file_bytes 262_144
  # Repo-relative, no shell metacharacters even though every path is quoted
  # before it reaches a shell. The charset alone is NOT enough: `..` is made
  # entirely of allowed characters, so a traversal passes every charset check
  # ever written and lands a write outside the workspace. Segments are checked
  # separately for that reason.
  @safe_path ~r{^[A-Za-z0-9._-]+(?:/[A-Za-z0-9._-]+)*$}
  @traversal [".", ".."]
  @default_prefixes ["index/"]
  @default_timeout_ms 3_600_000
  @default_interval_ms 20_000
  # How long a wait tolerates seeing no record for its change at all before
  # it asks whether anything is draining the queue. A run takes a minute to
  # appear even on an idle queue, and a busy queue is the normal case.
  @liveness_after_ms 180_000
  # A clone of this repo has been measured at 12m54s on an idle host and 26
  # minutes under contention, so its budget is an hour.
  @clone_timeout_ms 3_600_000
  # Clock skew between this machine and the forge host, plus the run that was
  # already building when the submit happened: the read window opens before
  # the submit rather than at it.
  @since_slack_s 300

  @typedoc """
  The change to land: a commit message and the file contents to put in the
  tree. Paths are repo-relative and are created if absent.
  """
  @type change :: %{
          required(:message) => String.t(),
          required(:files) => %{String.t() => String.t()}
        }

  @typedoc """
  What happened, in as much detail as the run record carried. `change_id` is
  the 32-hex form and `change_letters` the form jj prints, so a report can be
  matched against either. `commit_id` is the PRE-SUBMIT commit, kept because
  it is what the local tree had; `landed_commit` is the rebased one the gate
  actually ran, which is the one to quote.
  """
  @type report :: %{
          optional(:reason) => String.t(),
          optional(:run_id) => String.t(),
          optional(:landed_commit) => String.t(),
          optional(:status) => String.t(),
          optional(:detail) => Runs.detail(),
          optional(:change_id) => String.t(),
          optional(:change_letters) => String.t(),
          optional(:commit_id) => String.t(),
          optional(:workspace) => String.t(),
          optional(:files) => [String.t()]
        }

  @type outcome ::
          {:passed, report()}
          | {:failed, report()}
          | {:indeterminate, report()}
          | {:dry_run, report()}
          | {:error, String.t()}

  @doc """
  Land `change` on the forge's `main` and wait for its verdict.

  Options:

    * `:author` -- `[name: ..., email: ...]`, required. An unattributed
      commit can otherwise land, and there is no fixing that afterwards.
    * `:allow_prefixes` -- path prefixes the commit may touch, default
      `["index/"]`. Every changed path is checked against these AFTER the
      files are written, from jj's own diff rather than from the caller's
      list, because the point is to catch what the tree gained and not what
      the caller meant.
    * `:target` -- the bookmark to land on, default `"main"`.
    * `:workspace` -- adopt an already-materialized clone instead of making a
      fresh one, for a retry that should not pay for a 3.4 GB download twice.
      Refused unless the workspace is provably unused: no changes, no
      description.
    * `:run`, `:put` -- test seams for remote execution and remote file writes.
    * `:on_submit` -- a one-argument function called with the report (change id
      in both forms, commit id, workspace, files) the moment the submit is
      accepted, before the verdict wait begins.
    * `:expect` -- `%{path => contents}` for every path that ALREADY exists on
      the target bookmark. A whole-file write cannot tell a rebase from a
      revert, so a file present in the clone is refused unless the caller
      states what it expected to find; `overwrite: true` skips the check.
    * `:dry_run` -- do everything up to and including the describe, then
      return `{:dry_run, report}` without submitting. The whole pipeline
      minus the queue slot.
    * `:timeout_ms`, `:interval_ms` -- passed to `await_verdict/1`.
    * `:run` -- a `(target, argv, opts -> {:ok, output} | {:error, detail})`
      seam, so the sequence is testable without a forge.

  Returns `{:passed, report}` when the gate passed and the bookmark moved,
  `{:failed, report}` with the failing derivations named, `{:indeterminate,
  report}` when no verdict could be established, or `{:error, detail}` when
  the land could not be attempted at all.
  """
  @spec land(change(), keyword()) :: outcome()
  def land(change, opts \\ []) do
    Stdlib.observe(__MODULE__, :land, fn -> do_land(change, opts) end)
  end

  @doc """
  Wait for the verdict of a change already in the queue.

  Options:

    * `:change_id` -- the change to wait for, in 32-hex or in jj's letter
      form (either is accepted and converted). A prefix is enough.
    * `:subject` -- alternative key: the first line of the commit message,
      matched against the record's `trigger.description`. Useful when the
      change id was not captured, and weaker, because two changes can share
      a subject.
    * `:since` -- how far back to read records, default five minutes before
      now. A run record is found by its content, not by this window; the
      window only bounds the read.
    * `:timeout_ms` (default one hour), `:interval_ms` (default 20s).
    * `:liveness_after_ms` -- how long to see no record for this change at all
      before asking whether anything is draining the queue (default three
      minutes; a run takes a minute to appear even on an idle queue).
    * `:runs`, `:unit` -- overrides for the runs directory and the CI unit.
      The runs directory is deliberately NOT called `:target`: `land/2` forwards
      its whole option list here, and in `land/2` `:target` is the BOOKMARK.
      One name for two meanings would have made `jj new <a runs path>` out of a
      land that merely said where the records live.
    * `:read`, `:consumer` -- test seams for the record read and the
      liveness probe.
  """
  @spec await_verdict(keyword()) :: outcome()
  def await_verdict(opts) do
    Stdlib.observe(__MODULE__, :await_verdict, fn -> do_await(opts) end)
  end

  @doc """
  A change id in jj's reverse-hex letters, as 32 hex digits.

      iex> IxMcp.Stdlib.Forge.change_id_hex("kvvozztpzzrk")
      {:ok, "f44b006a008f"}
  """
  @spec change_id_hex(String.t()) :: {:ok, String.t()} | {:error, String.t()}
  def change_id_hex(letters) when is_binary(letters) do
    # Downcased for the same reason change_id_letters/1 downcases: the docs
    # promise that either form is accepted, and an id pasted out of a terminal
    # or a web page is often upper case. Accepting one case and not the other
    # is a promise the code does not keep.
    transcode(String.downcase(letters), @letter_to_hex, "change id letters")
  end

  @doc """
  A hex change id in the reverse-hex letters jj prints.

      iex> IxMcp.Stdlib.Forge.change_id_letters("2330e2b9a2cd")
      {:ok, "xwwzlxoqpxnm"}
  """
  @spec change_id_letters(String.t()) :: {:ok, String.t()} | {:error, String.t()}
  def change_id_letters(hex) when is_binary(hex) do
    transcode(String.downcase(hex), @hex_to_letter, "change id hex")
  end

  # ── landing ───────────────────────────────────────────────────────────

  @spec do_land(change(), keyword()) :: outcome()
  defp do_land(change, opts) do
    with {:ok, message, files} <- validate(change),
         {:ok, author} <- author(opts),
         {:ok, env} <- env(opts),
         {:ok, paths} <- base(env, opts),
         env = Map.merge(env, paths),
         :ok <- identify(env, author),
         :ok <- step(env, ["new", opts[:target] || "main"]),
         :ok <- guard_files(env, files, opts),
         :ok <- write_files(env, files),
         {:ok, changed} <- diff(env),
         :ok <- tier_check(changed, Keyword.get(opts, :allow_prefixes, @default_prefixes)),
         :ok <- all_present(files, changed),
         :ok <- describe(env, message),
         {:ok, ids} <- identity(env) do
      base = Map.merge(ids, %{workspace: env.cwd, files: Enum.sort(Map.keys(files))})

      if Keyword.get(opts, :dry_run, false) do
        {:dry_run, base}
      else
        submit_and_wait(env, base, opts)
      end
    end
  end

  @spec submit_and_wait(map(), report(), keyword()) :: outcome()
  defp submit_and_wait(env, base, opts) do
    since = DateTime.add(DateTime.utc_now(), -@since_slack_s, :second)

    case submit(env, opts[:target] || "main") do
      :ok ->
        # The ids exist here and the verdict does not arrive for minutes to
        # hours, and something is usually waiting on the ids rather than on the
        # verdict: a follow-on lane rebasing onto this change cannot key a
        # watcher on a value that only exists after the wait. So the report is
        # handed over at submit time, before the polling starts.
        notify(opts, base)

        opts
        |> Keyword.merge(change_id: base.change_id, since: since)
        |> do_await()
        |> merge_report(base)

      {:error, detail} ->
        {:error, "submit refused: #{detail}"}

      # The submit MAY have reached the queue: a timeout or a dead socket says
      # nothing about what the far side did. Reporting "refused" here is the
      # expensive lie, because the obvious response to it is a retry, and a
      # retry of a submit that succeeded double-submits the change. So the ids
      # are handed over and the verdict is awaited exactly as for a known
      # submit; if nothing was queued, the wait ends indeterminate and says so.
      {:unknown, detail} ->
        notify(opts, base)

        opts
        |> Keyword.merge(change_id: base.change_id, since: since)
        |> do_await()
        |> merge_report(Map.put(base, :submit, "outcome unknown: #{detail}"))
    end
  end

  # A step whose outcome could not be READ is distinct from a step that FAILED,
  # and only the submit can use the distinction: for every other step, an
  # unknown outcome must fail closed, because the pipeline's next step would be
  # building on a state nobody verified.
  @spec unknown_is_closed({atom(), String.t()}) :: {:ok, String.t()} | {:error, String.t()}
  defp unknown_is_closed({:unknown, detail}), do: {:error, "could not determine: #{detail}"}
  defp unknown_is_closed(other), do: other

  @spec submit(map(), String.t()) :: :ok | {:error, String.t()} | {:unknown, String.t()}
  defp submit(env, target) do
    env.run.(env.land, [env.jj, "submit", "--target", target],
      cd: env.cwd,
      state: env.land.dir
    )
    |> case do
      {:ok, _output} -> :ok
      {:error, detail} -> {:error, detail}
      {:unknown, detail} -> {:unknown, detail}
    end
  end

  @spec notify(keyword(), report()) :: :ok
  defp notify(opts, base) do
    case Keyword.get(opts, :on_submit) do
      nil ->
        :ok

      fun when is_function(fun, 1) ->
        fun.(base)
        :ok
    end
  catch
    kind, reason ->
      # A caller's callback must not be able to lose a submitted change: the
      # submit already happened, so the only honest response to a broken
      # callback is to keep waiting for the verdict and say so.
      Logger.warning("on_submit callback raised: #{Exception.format(kind, reason)}")
      :ok
  end

  defp merge_report({state, report}, base), do: {state, Map.merge(base, report)}
  defp merge_report(other, _base), do: other

  @spec validate(change()) ::
          {:ok, String.t(), %{String.t() => String.t()}} | {:error, String.t()}
  defp validate(%{message: message, files: files})
       when is_binary(message) and is_map(files) and map_size(files) > 0 do
    cond do
      String.trim(message) == "" ->
        {:error, "a commit message is required"}

      unsafe = Enum.find(Map.keys(files), &(not safe_path?(&1))) ->
        {:error, "path refused (repo-relative, no traversal): #{inspect(unsafe)}"}

      oversized = Enum.find(files, fn {_path, body} -> byte_size(body) > @max_file_bytes end) ->
        {:error, "file over #{@max_file_bytes} bytes: #{inspect(elem(oversized, 0))}"}

      true ->
        {:ok, message, files}
    end
  end

  defp validate(_malformed), do: {:error, "a change needs :message and a non-empty :files map"}

  @spec safe_path?(String.t()) :: boolean()
  defp safe_path?(path) do
    Regex.match?(@safe_path, path) and
      not Enum.any?(String.split(path, "/"), &(&1 in @traversal))
  end

  @spec author(keyword()) :: {:ok, %{name: String.t(), email: String.t()}} | {:error, String.t()}
  defp author(opts) do
    author = Keyword.get(opts, :author, [])
    name = author[:name]
    email = author[:email]

    if is_binary(name) and is_binary(email) and String.trim(name) != "" and
         String.trim(email) != "" do
      {:ok, %{name: name, email: email}}
    else
      {:error, "an author [name:, email:] is required; an unattributed commit cannot be fixed"}
    end
  end

  @spec env(keyword()) :: {:ok, map()} | {:error, String.t()}
  defp env(opts) do
    with {:ok, land} <- configured_target(opts, :land, @env_land),
         {:ok, jj} <- configured(opts, :jj, @env_jj),
         {:ok, server} <- configured(opts, :server, @env_server) do
      {:ok,
       %{
         land: land,
         jj: jj,
         server: server,
         run: Keyword.get(opts, :run, &Runs.run_detached/3),
         put: Keyword.get(opts, :put, &Runs.put_file/3)
       }}
    end
  end

  @spec base(map(), keyword()) :: {:ok, map()} | {:error, String.t()}
  defp base(env, opts) do
    case Keyword.get(opts, :workspace) do
      nil -> clone(env)
      dir when is_binary(dir) -> adopt(env, dir)
    end
  end

  @spec clone(map()) :: {:ok, map()} | {:error, String.t()}
  defp clone(env) do
    stamp = DateTime.utc_now() |> DateTime.to_iso8601(:basic) |> String.replace(":", "")
    workspace = Path.join(env.land.dir, "land-#{stamp}")

    with :ok <- shell(env, "set -eu\nmkdir -p #{Runs.shell_quote(workspace)}"),
         {:ok, _output} <-
           env.run.(
             env.land,
             [
               env.jj,
               "clone",
               env.server,
               "--repo",
               "ix",
               "--workspace",
               "land-#{stamp}",
               "."
             ],
             cd: workspace,
             state: env.land.dir,
             timeout_ms: @clone_timeout_ms
           ) do
      {:ok, %{cwd: workspace, message: Path.join(env.land.dir, "land-message-#{stamp}")}}
    end
  end

  # Adopting an already-materialized workspace, which exists because a clone of
  # this repo is 3.4 GB and 13 to 26 minutes and a transport failure three
  # steps later should not have to pay for it twice. The doctrine above is
  # unchanged -- a land still gets a working copy whose only content is what
  # this call put there -- so adoption is allowed only for a workspace that is
  # PROVABLY in the state a fresh clone would be in: nothing written, nothing
  # described. Both are checked here rather than assumed, because "it looked
  # clean" is how a half-applied change gets attributed to the next lane.
  @spec adopt(map(), String.t()) :: {:ok, map()} | {:error, String.t()}
  defp adopt(env, dir) do
    stamp = DateTime.utc_now() |> DateTime.to_iso8601(:basic) |> String.replace(":", "")
    candidate = Map.put(env, :cwd, dir)

    with :ok <- outside(env.land.dir, dir),
         {:ok, changed} <- diff(candidate),
         {:ok, description} <- description(candidate) do
      cond do
        changed != [] ->
          {:error, "workspace #{dir} already carries changes: #{Enum.join(changed, ", ")}"}

        String.trim(description) != "" ->
          {:error, "workspace #{dir} already has a described change; it is not a fresh base"}

        true ->
          {:ok, %{cwd: dir, message: Path.join(env.land.dir, "land-message-#{stamp}")}}
      end
    end
  end

  # The message file and the `:expect` pre-images are written under the land
  # root, and `jj describe` snapshots AFTER the tier check has already said :ok.
  # So a workspace that CONTAINS the land root would ship those sidecars to the
  # target bookmark past a check that already passed. Keeping them out by path
  # construction is not enough when the caller chooses one of the paths.
  @spec outside(String.t(), String.t()) :: :ok | {:error, String.t()}
  defp outside(land_dir, workspace) do
    land = Path.expand(land_dir)
    tree = Path.expand(workspace)

    if land == tree or String.starts_with?(land, tree <> "/") do
      {:error,
       "workspace #{workspace} contains the land root #{land_dir}: its message and pre-image " <>
         "sidecars would be snapshotted into the commit after the tier check"}
    else
      :ok
    end
  end

  @spec description(map()) :: {:ok, String.t()} | {:error, String.t()}
  defp description(env) do
    env.run.(
      env.land,
      [env.jj, "--ignore-working-copy", "log", "-r", "@", "--no-graph", "-T", "description"],
      cd: env.cwd,
      state: env.land.dir
    )
  end

  # Identity before any commit exists: jj warns rather than fails on an
  # unattributed working copy, and a warning on stderr is not a thing a
  # pipeline notices.
  @spec identify(map(), map()) :: :ok | {:error, String.t()}
  defp identify(env, author) do
    with :ok <- step(env, ["config", "set", "--repo", "user.name", author.name]) do
      step(env, ["config", "set", "--repo", "user.email", author.email])
    end
  end

  # The safety the shell recipe got from `git apply --check`, which whole-file
  # writes would otherwise throw away: a land whose base has moved under it
  # must FAIL, not silently revert somebody's change. So a file that already
  # exists in the clone is only written when the caller says what it expected
  # to find there (`:expect`), and a new file is only written when it really is
  # new. `overwrite: true` is the escape hatch, and it has to be typed.
  @spec guard_files(map(), %{String.t() => String.t()}, keyword()) :: :ok | {:error, String.t()}
  defp guard_files(env, files, opts) do
    expect = Keyword.get(opts, :expect, %{})
    overwrite = Keyword.get(opts, :overwrite, false)

    Enum.reduce_while(files, :ok, fn {path, _body}, :ok ->
      case {Map.fetch(expect, path), overwrite} do
        {{:ok, pre_image}, _overwrite} -> halt_on_error(guard_matches(env, path, pre_image))
        {:error, true} -> {:cont, :ok}
        {:error, false} -> halt_on_error(guard_absent(env, path))
      end
    end)
  end

  defp halt_on_error(:ok), do: {:cont, :ok}
  defp halt_on_error({:error, detail}), do: {:halt, {:error, detail}}

  # Compared on the far side with `cmp`, so the pre-image never has to be
  # encoded, shipped back, and decoded to be checked.
  @spec guard_matches(map(), String.t(), String.t()) :: :ok | {:error, String.t()}
  defp guard_matches(env, path, pre_image) do
    expected = env.message <> ".expect"
    target = Runs.shell_quote(Path.join(env.cwd, path))

    with :ok <- write(env, expected, pre_image) do
      case shell(
             env,
             """
             set -eu
             if [ ! -e #{target} ]; then
               echo 'expected to modify a file that is not there' >&2
               exit 9
             fi
             cmp -s #{target} #{Runs.shell_quote(expected)} || {
               echo 'the file on the target bookmark is not what this land expected' >&2
               exit 9
             }
             rm -f #{Runs.shell_quote(expected)}
             """
           ) do
        :ok -> :ok
        {:error, detail} -> {:error, "#{path}: base moved under this land: #{detail}"}
      end
    end
  end

  @spec guard_absent(map(), String.t()) :: :ok | {:error, String.t()}
  defp guard_absent(env, path) do
    target = Runs.shell_quote(Path.join(env.cwd, path))

    case shell(
           env,
           """
           set -eu
           if [ -e #{target} ]; then
             echo 'already exists; pass :expect with its current contents, or overwrite: true' >&2
             exit 9
           fi
           """
         ) do
      :ok -> :ok
      {:error, detail} -> {:error, "#{path}: #{detail}"}
    end
  end

  @spec write_files(map(), %{String.t() => String.t()}) :: :ok | {:error, String.t()}
  defp write_files(env, files) do
    Enum.reduce_while(files, :ok, fn {path, body}, :ok ->
      case write(env, Path.join(env.cwd, path), body) do
        :ok -> {:cont, :ok}
        {:error, detail} -> {:halt, {:error, "writing #{path}: #{detail}"}}
      end
    end)
  end

  @spec write(map(), String.t(), String.t()) :: :ok | {:error, String.t()}
  # A file body is copied, never spoken: an argv-borne base64 payload dies at
  # MAX_ARG_STRLEN, and it dies only for the big files, which is how the first
  # three attempts at this land never met the limit. See Runs.put_file/3.
  defp write(env, path, body) do
    env.put.(env.land, path, body)
  end

  @spec diff(map()) :: {:ok, [String.t()]} | {:error, String.t()}
  defp diff(env) do
    # No --ignore-working-copy: the files were written by a shell, so this is
    # the call that has to snapshot them.
    case env.run.(env.land, [env.jj, "diff", "-r", "@", "--name-only"],
           cd: env.cwd,
           state: env.land.dir
         ) do
      {:ok, output} -> {:ok, output |> String.split("\n", trim: true) |> Enum.sort()}
      {:error, detail} -> {:error, "reading the change: #{detail}"}
    end
  end

  # The tier check reads jj's diff, not the caller's file list: a commit that
  # gained a path nobody meant to add is exactly the case this refuses, and a
  # check against the intended list cannot see it.
  # A write that does not show up in the snapshot is a file silently missing
  # from the commit -- an ignore rule matching the path, or bytes that landed
  # somewhere else -- and the land would then report a landing of an incomplete
  # change.
  #
  # This runs AFTER the tier check, not before, and the order is load-bearing:
  # both faults can be present at once, and of the two only a tier violation is
  # unrecoverable once published. A presence complaint that speaks first masks
  # the private-tier path behind "your file is missing", which reads like a
  # local mistake and invites a retry. So the dangerous verdict is the one that
  # gets said out loud.
  @spec all_present(%{String.t() => String.t()}, [String.t()]) :: :ok | {:error, String.t()}
  defp all_present(files, changed) do
    case Map.keys(files) -- changed do
      [] -> :ok
      missing -> {:error, "written but not in the commit: #{Enum.join(Enum.sort(missing), ", ")}"}
    end
  end

  @spec tier_check([String.t()], [String.t()]) :: :ok | {:error, String.t()}
  defp tier_check([], _prefixes), do: {:error, "the change is empty; nothing to submit"}

  defp tier_check(changed, prefixes) do
    case Enum.reject(changed, fn path -> Enum.any?(prefixes, &String.starts_with?(path, &1)) end) do
      [] -> :ok
      stray -> {:error, "paths outside #{inspect(prefixes)}: #{Enum.join(stray, ", ")}"}
    end
  end

  # The message reaches jj through a file and `--stdin`, so prose crosses
  # verbatim: as a `-m` argument it would be one element of an argv, and an
  # argv is where quoting mistakes silently reshape a commit message. The file
  # lives beside the clone rather than inside it -- a message file in the tree
  # is a path the tier check would (correctly) refuse.
  @spec describe(map(), String.t()) :: :ok | {:error, String.t()}
  defp describe(env, message) do
    with :ok <- write(env, env.message, message) do
      shell(
        env,
        """
        set -eu
        cd #{Runs.shell_quote(env.cwd)}
        #{Runs.shell_quote(env.jj)} describe --stdin < #{Runs.shell_quote(env.message)}
        rm -f #{Runs.shell_quote(env.message)}
        """
      )
    end
  end

  @spec identity(map()) :: {:ok, map()} | {:error, String.t()}
  defp identity(env) do
    template =
      ~s[change_id ++ "\\n" ++ commit_id ++ "\\n" ++ author.name() ++ "\\n" ++ author.email()]

    # `--ignore-working-copy` because `diff` and `describe` have already
    # snapshotted: a read that re-snapshots 172,565 files costs minutes and
    # opens one more window for a concurrent-modification warning.
    with {:ok, output} <-
           env.run.(
             env.land,
             [
               env.jj,
               "--ignore-working-copy",
               "log",
               "-r",
               "@",
               "--no-graph",
               "-T",
               template
             ],
             cd: env.cwd,
             state: env.land.dir
           ),
         [rendered, commit_id, name, email] <- String.split(String.trim(output), "\n"),
         {:ok, letters, hex} <- change_id_forms(rendered),
         true <- String.trim(name) != "" and String.trim(email) != "" do
      {:ok,
       %{
         change_id: hex,
         change_letters: letters,
         commit_id: commit_id
       }}
    else
      {:error, detail} -> {:error, "reading the commit identity: #{detail}"}
      _unattributed -> {:error, "the commit has no author; refusing to submit"}
    end
  end

  # jj's `change_id` template keyword renders the LETTER alphabet, and the run
  # records store hex. The first live land of this module died right here, at the
  # last step before submit, because this code assumed the template handed over
  # hex and converted it the wrong way -- and 33 tests agreed with the mistake,
  # because the stub's change id was one I had typed myself in the alphabet I
  # wrongly expected. A fixture written by the same belief as the code cannot
  # falsify it, so the value in the regression test below is jj's real output,
  # copied from the failure.
  #
  # Both alphabets are accepted because they are disjoint (no string is valid in
  # both), so there is nothing to disambiguate and no reason to care which form a
  # future jj prints. Both forms are always reported: hex is what a run record
  # can be searched for, letters are what a human sees in `jj log`.
  @spec change_id_forms(String.t()) :: {:ok, String.t(), String.t()} | {:error, String.t()}
  defp change_id_forms(rendered) do
    case {change_id_hex(rendered), change_id_letters(rendered)} do
      {{:ok, hex}, _not_hex} -> {:ok, rendered, hex}
      {_not_letters, {:ok, letters}} -> {:ok, letters, String.downcase(rendered)}
      {{:error, detail}, {:error, _also}} -> {:error, detail}
    end
  end

  @spec step(map(), [String.t()]) :: :ok | {:error, String.t()}
  defp step(env, args) do
    case unknown_is_closed(env.run.(env.land, [env.jj | args], cd: env.cwd, state: env.land.dir)) do
      {:ok, _output} -> :ok
      {:error, detail} -> {:error, "#{Enum.join(args, " ")}: #{detail}"}
    end
  end

  # Every script is given absolute paths, so it needs no working directory of
  # its own and cannot be surprised by one.
  @spec shell(map(), String.t()) :: :ok | {:error, String.t()}
  defp shell(env, script) do
    case unknown_is_closed(env.run.(env.land, ["sh", "-c", script], state: env.land.dir)) do
      {:ok, _output} -> :ok
      {:error, detail} -> {:error, detail}
    end
  end

  # ── waiting ───────────────────────────────────────────────────────────

  @spec do_await(keyword()) :: outcome()
  defp do_await(opts) do
    with {:ok, key} <- key(opts),
         {:ok, target} <- configured_target(opts, :runs, @env_runs),
         {:ok, read} <- read_fun(opts, target) do
      since =
        Keyword.get(opts, :since) || DateTime.add(DateTime.utc_now(), -@since_slack_s, :second)

      deadline =
        System.monotonic_time(:millisecond) + Keyword.get(opts, :timeout_ms, @default_timeout_ms)

      poll(%{
        key: key,
        read: read,
        since: since,
        deadline: deadline,
        interval_ms: Keyword.get(opts, :interval_ms, @default_interval_ms),
        consumer: Keyword.get(opts, :consumer, fn -> Runs.consumer(target, unit(opts)) end),
        liveness_after_ms: Keyword.get(opts, :liveness_after_ms, @liveness_after_ms),
        last_progress_at: System.monotonic_time(:millisecond),
        fingerprint: nil
      })
    end
  end

  @spec poll(map()) :: outcome()
  defp poll(state) do
    case state.read.(state.since) do
      {:ok, output} ->
        output |> Runs.records() |> newest_match(state.key) |> decide(state)

      {:error, detail} ->
        # A read that could not answer is not a verdict, and retrying it is
        # right up until the deadline: a blinked tailnet must not be reported
        # as a red main.
        wait(state, "the run records could not be read: #{detail}")
    end
  end

  @spec decide(map() | nil, map()) :: outcome()
  defp decide(%{"status" => status} = record, _state) when status in ["passed", "failed"] do
    verdict = if status == "passed", do: :passed, else: :failed

    {verdict,
     %{
       run_id: to_string(record["run_id"]),
       landed_commit: to_string(record["commit_id"]),
       status: status,
       detail: Runs.detail(record["log_tail"])
     }}
  end

  defp decide(%{"status" => status} = record, state) do
    # A record existing does NOT answer liveness: the reconciler can die with a
    # record sitting at "building", and a waiter that latches liveness off on
    # first sight then reports "the run is building" until its timeout -- the
    # exact confusion this module's moduledoc claims to have removed. What
    # progress resets is the CLOCK, not the question.
    wait(progress(state, record), "the run is #{status}")
  end

  defp decide(nil, state) do
    wait(state, "no run record names this change yet")
  end

  # A record whose shape this code does not understand is not a verdict. The
  # reader only guarantees a `run_id`, so a missing `status` must wait rather
  # than raise out through `land/2`.
  defp decide(record, state) when is_map(record) do
    wait(state, "a run record for this change is in an unreadable shape")
  end

  # Progress is a CHANGED record, not a seen one.
  @spec progress(map(), map()) :: map()
  defp progress(state, record) do
    fingerprint = {record["status"], updated_at(record)}

    if fingerprint == state.fingerprint do
      state
    else
      %{state | fingerprint: fingerprint, last_progress_at: System.monotonic_time(:millisecond)}
    end
  end

  @spec wait(map(), String.t()) :: outcome()
  defp wait(state, reason) do
    now = System.monotonic_time(:millisecond)

    cond do
      now >= state.deadline ->
        {:indeterminate, %{reason: "timed out waiting: #{reason}"}}

      now - state.last_progress_at >= state.liveness_after_ms ->
        check_consumer(state, reason)

      true ->
        Process.sleep(state.interval_ms)
        poll(state)
    end
  end

  # The liveness leg. A dead consumer means no verdict is coming, so the
  # answer is INDETERMINATE and named -- never "still waiting", which is what
  # a waiter without this leg reports until its timeout.
  @spec check_consumer(map(), String.t()) :: outcome()
  defp check_consumer(state, reason) do
    case state.consumer.() do
      {:alive, _state} ->
        Process.sleep(state.interval_ms)
        poll(%{state | last_progress_at: System.monotonic_time(:millisecond)})

      {:dead, consumer_state} ->
        {:indeterminate,
         %{reason: "the CI consumer is #{consumer_state}, so no verdict is coming (#{reason})"}}

      {:unknown, detail} ->
        {:indeterminate,
         %{reason: "could not tell whether the CI consumer is alive (#{detail}); #{reason}"}}
    end
  end

  # One change can own SEVERAL records: a resubmit after a red, a queue re-run,
  # an amend. `Enum.find` answers with whichever the reader happened to put
  # first, and `Runs.records/1` prepends while consuming a newest-first read, so
  # `find` systematically returns the OLDEST -- i.e. it answers a resubmit with
  # the previous attempt's `failed`. The newest by `updated_at_ms` is the only
  # record that describes this attempt. `IxMcp.Forge.Verdicts` already sorted
  # this way and the extraction lost it, which is the recurring shape of an
  # extraction bug: the behaviour lived in a sort nobody named.
  @spec newest_match([map()], {:change_id, String.t()} | {:subject, String.t()}) :: map() | nil
  defp newest_match(records, key) do
    records
    |> Enum.filter(&matches?(&1, key))
    |> Enum.max_by(&updated_at(&1), fn -> nil end)
  end

  @spec updated_at(map()) :: integer()
  defp updated_at(record) do
    case record["updated_at_ms"] || record["started_at_ms"] do
      value when is_integer(value) -> value
      _absent -> 0
    end
  end

  @spec matches?(map(), {:change_id, String.t()} | {:subject, String.t()}) :: boolean()
  defp matches?(record, {:change_id, hex}) do
    case record["change_id"] do
      value when is_binary(value) -> String.starts_with?(value, hex)
      _absent -> false
    end
  end

  defp matches?(record, {:subject, subject}) do
    case record["trigger"] do
      %{"description" => description} when is_binary(description) ->
        description |> String.split("\n", parts: 2) |> hd() |> String.trim() == subject

      _absent ->
        false
    end
  end

  @spec key(keyword()) ::
          {:ok, {:change_id, String.t()} | {:subject, String.t()}} | {:error, String.t()}
  defp key(opts) do
    case {Keyword.get(opts, :change_id), Keyword.get(opts, :subject)} do
      {id, _subject} when is_binary(id) -> change_key(id)
      {nil, subject} when is_binary(subject) and subject != "" -> {:ok, {:subject, subject}}
      _neither -> {:error, "a :change_id or a :subject is required to find a run record"}
    end
  end

  # Either form is accepted from a caller and exactly one is used to search,
  # because the records store hex and a letter-form search is silently empty.
  @spec change_key(String.t()) :: {:ok, {:change_id, String.t()}} | {:error, String.t()}
  defp change_key(id) do
    case change_id_hex(id) do
      {:ok, hex} -> {:ok, {:change_id, hex}}
      {:error, _not_letters} -> hex_key(id)
    end
  end

  defp hex_key(id) do
    case change_id_letters(id) do
      {:ok, _letters} ->
        {:ok, {:change_id, String.downcase(id)}}

      {:error, detail} ->
        {:error, "#{id} is neither a hex nor a letter-form change id (#{detail})"}
    end
  end

  # ── shared ────────────────────────────────────────────────────────────

  @spec read_fun(keyword(), Runs.target()) ::
          {:ok, (DateTime.t() -> {:ok, String.t()} | {:error, String.t()})} | {:error, String.t()}
  defp read_fun(opts, target) do
    case Keyword.fetch(opts, :read) do
      {:ok, read} ->
        {:ok, read}

      :error ->
        case Runs.reader(target) do
          {:ok, read} -> {:ok, read}
          :error -> {:error, "the run records at #{inspect(target)} are not readable from here"}
        end
    end
  end

  @spec configured(keyword(), atom(), String.t()) :: {:ok, String.t()} | {:error, String.t()}
  defp configured(opts, key, variable) do
    case Keyword.get(opts, key, System.get_env(variable)) do
      value when is_binary(value) and value != "" -> {:ok, value}
      _unset -> {:error, "#{variable} is not set, and this tree compiles in no forge host"}
    end
  end

  @spec configured_target(keyword(), atom(), String.t()) ::
          {:ok, Runs.target()} | {:error, String.t()}
  defp configured_target(opts, key, variable) do
    with {:ok, value} <- configured(opts, key, variable) do
      case Runs.parse_target(value) do
        {:ok, target} ->
          {:ok, target}

        :error ->
          {:error, "#{variable} is not a safe [user@host:]/absolute/dir: #{inspect(value)}"}
      end
    end
  end

  defp unit(opts) do
    Keyword.get(opts, :unit) || System.get_env(@env_unit) || @default_unit
  end

  @spec transcode(String.t(), %{String.t() => String.t()}, String.t()) ::
          {:ok, String.t()} | {:error, String.t()}
  defp transcode(value, table, what) do
    graphemes = String.graphemes(value)

    cond do
      graphemes == [] ->
        {:error, "empty #{what}"}

      length(graphemes) > @change_id_chars ->
        {:error, "#{what} longer than #{@change_id_chars} digits: #{inspect(value)}"}

      Enum.all?(graphemes, &Map.has_key?(table, &1)) ->
        {:ok, Enum.map_join(graphemes, &Map.fetch!(table, &1))}

      true ->
        {:error, "not #{what}: #{inspect(value)}"}
    end
  end
end
