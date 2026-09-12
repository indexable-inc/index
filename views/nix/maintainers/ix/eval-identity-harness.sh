#!/usr/bin/env bash
# Verify that an evaluator setting produces byte-identical derivation paths to
# the default configuration, for a set of ix fleet hosts.
#
# Every gate here exists because an earlier version had none of them. It
# compared sha256 across runs and nothing else, so a crashing run, which writes
# an empty output file whose sha256 is the stable constant e3b0c442...b855, was
# recorded as just another matching row. A SIGSEGV went unnoticed through nine
# runs that way.
#
#   run    LABEL NIXBIN ARM SCOPE RESULTFILE     one evaluation, one RESULT line
#   verify RESULTFILE SCOPE LABEL...             every named label present and ok
#
# ARM:   the evaluator configuration under test, as SETTING=VALUE, or "none" to
#        omit the option entirely -- the plain upstream path every other arm
#        must match. Was CORES; generalised because the identity question is the
#        same for any setting that must not move a drvPath (eval-cores,
#        eval-cache, ...) and hardcoding one setting meant copying the file to
#        ask about the next.
#
#        eval-cores=N also enables the parallel-eval experimental feature, which
#        that setting needs and which nothing else does.
#
#        Every arm records the value nix ACTUALLY used, from `nix config show`
#        under the same args, as eff= on the RESULT line. This exists because a
#        bug in arm construction is invisible in the worst way: an option that
#        never reaches nix leaves two arms byte-identical, they agree perfectly,
#        and the run reports a pass that means nothing. `verify` therefore
#        refuses a set of runs whose eff= values are all equal -- identical
#        output is only evidence when the inputs really differed.
#
#        KNOWN LIMIT of that gate: arm "none" records the sentinel eff=none
#        rather than a value read from nix, because it names no setting to ask
#        about. So `none` against `SETTING=<nix's own default>` reads as two
#        differing arms while being one configuration twice, which is the very
#        thing the gate is meant to refuse. Prefer naming both arms explicitly
#        (eval-cache=false against eval-cache=true) whenever the baseline is a
#        value rather than the absence of a flag; then every eff= comes from nix
#        and the comparison is honest. "none" stays for eval-cores, whose
#        baseline really is "no option and no experimental feature".
# SCOPE: one|twelve, which also fixes the exact set of keys the output must have.
#
# PE_IX must point at an ix flake checkout.
set -u

PE_IX=${PE_IX:?set PE_IX to the ix flake checkout}
PE_TWELVE=(dev-compute-1 dev-compute-2 dev-compute-3 dev-compute-4 dev-compute-5 dev-compute-6 hil-compute-1 hil-compute-2 hil-compute-3 hil-stor-2 vin-compute-1 vin-compute-2)
PE_ONE=(hil-compute-1)

# shasum is darwin, sha256sum is coreutils. Fail loudly rather than silently
# skipping the hash, because a missing hash would make every run "match".
pe_sha256() {
  if command -v sha256sum > /dev/null 2>&1; then
    sha256sum "$1" | awk '{print $1}'
  elif command -v shasum > /dev/null 2>&1; then
    shasum -a 256 "$1" | awk '{print $1}'
  else
    echo "pe_sha256: neither sha256sum nor shasum on PATH" >&2
    return 2
  fi
}

# Pull one timing field out of a time(1) log, accepting both the POSIX
# "real 1.23" order and BSD's default "     1.23 real". Echoes NA when absent,
# so a missing timer can never masquerade as a fast run.
pe_time() { # $1 = real|user|sys, $2 = log file
  local v
  v=$(sed -nE "s/^[[:space:]]*$1[[:space:]]+([0-9.]+).*/\\1/p; s/^[[:space:]]*([0-9.]+)[[:space:]]+$1[[:space:]]*$/\\1/p" "$2" | head -1)
  echo "${v:-NA}"
}

pe_nodes() { # $1 = scope; echoes the node names, one per line
  case $1 in
    one)    printf '%s\n' "${PE_ONE[@]}" ;;
    twelve) printf '%s\n' "${PE_TWELVE[@]}" ;;
    *) echo "pe_nodes: unknown scope '$1'" >&2; return 2 ;;
  esac
}

# Gate the output file itself, so a crash, a truncation, or a partial object can
# never reach the sha256 comparison. Echoes "ok" or a reason.
pe_check_out() { # $1 = output file, $2 = scope
  local out=$1 scope=$2 sz keys want got
  [ -e "$out" ] || { echo "output-absent"; return 1; }
  sz=$(wc -c < "$out" | tr -d ' ')
  [ "$sz" -gt 0 ] || { echo "output-empty"; return 1; }
  jq -e 'type == "object"' "$out" > /dev/null 2>&1 || { echo "output-not-json-object"; return 1; }
  # Exactly the expected node names, no more and no fewer. A partial object is
  # still valid JSON, so size and parseability alone do not catch a short write.
  want=$(pe_nodes "$scope" | sort | tr '\n' ',')
  got=$(jq -r 'keys[]' "$out" | sort | tr '\n' ',')
  [ "$want" = "$got" ] || { echo "output-keys-mismatch want=$want got=$got"; return 1; }
  # Every value a derivation store path. An attribute that evaluated to null or
  # to an empty string would otherwise hash consistently across runs.
  keys=$(jq -r 'to_entries[] | select((.value|type) != "string" or (.value|test("^/nix/store/.*\\.drv$")|not)) | .key' "$out")
  [ -z "$keys" ] || { echo "output-bad-values=$keys"; return 1; }
  echo ok
}

pe_run() { # $1 label, $2 nixbin, $3 arm, $4 scope, $5 resultfile
  local label=$1 nixbin=$2 arm=$3 scope=$4 res=$5
  local out=/tmp/paraeval-out-$label.json
  local log=/tmp/paraeval-run-$label.log
  local stats=/tmp/paraeval-stats-$label.json
  local nodes apply args rc status sha wall user sys timer t setting value eff

  # Resolve the binary before the cd below, and require it executable. A
  # relative path silently stops resolving once we cd into the flake, and the
  # run then exits 127, which reads as a failed evaluation when in fact nothing
  # was ever evaluated.
  if [ ! -x "$nixbin" ]; then
    echo "RESULT label=$label arm=$arm scope=$scope rc=NA status=FAIL-nixbin-not-executable sha256=NONE eff=NA wall=NA user=NA sys=NA" >> "$res"
    echo "HARNESS FAIL: run '$label' rejected: '$nixbin' is not an executable file" >&2
    echo "  pass an absolute path; this function cd's to \$PE_IX before running" >&2
    return 1
  fi
  nixbin=$(cd "$(dirname "$nixbin")" && pwd)/$(basename "$nixbin")

  rm -f "$out" "$log" "$stats"
  nodes="[ $(pe_nodes "$scope" | sed 's/^/"/;s/$/"/' | tr '\n' ' ')]"
  apply="ns: builtins.listToAttrs (map (x: { name = x; value = ns.\${x}.config.system.build.toplevel.drvPath; }) $nodes)"
  args="--extra-experimental-features nix-command --extra-experimental-features flakes"
  if [ "$arm" != none ]; then
    # SETTING=VALUE, split on the first = only, so a value containing = survives.
    setting=${arm%%=*}
    value=${arm#*=}
    if [ "$setting" = "$arm" ] || [ -z "$setting" ] || [ -z "$value" ]; then
      echo "RESULT label=$label arm=$arm scope=$scope rc=NA status=FAIL-arm-malformed sha256=NONE eff=NA wall=NA user=NA sys=NA" >> "$res"
      echo "HARNESS FAIL: run '$label' rejected: arm '$arm' is not SETTING=VALUE or \"none\"" >&2
      return 1
    fi
    # parallel-eval is eval-cores' own gate. Naming it for every setting would
    # make an arm that forgot it look like one that did not need it.
    [ "$setting" = eval-cores ] && args="$args --extra-experimental-features parallel-eval"
    args="$args --option $setting $value"
  fi

  # /usr/bin/time does not exist on NixOS, where it lives in
  # /run/current-system/sw/bin. Resolve it rather than hardcoding the path: an
  # absent wrapper exits 127, and the rc gate below would then report a failed
  # evaluation when nothing was ever evaluated.
  #
  # `type -P` and not `command -v`, because `time` is a shell keyword and
  # `command -v time` answers "time", which is not a path. That silently
  # produced wall=NA. Elapsed over processor time is the contention signal, so
  # losing it quietly is exactly the class of failure this file exists to stop.
  #
  # `-p` and not `-l`: POSIX format, understood by both GNU and BSD time, so one
  # parser covers both. It prints "real <n>" where BSD's default prints
  # "<n> real"; pe_time accepts either.
  timer=()
  t=$(type -P time 2>/dev/null || true)
  [ -n "$t" ] && [ -x "$t" ] && timer=("$t" -p)

  # Ask nix what it will actually use for the setting under test, under exactly
  # the args the evaluation gets. This is the gate against the failure that
  # cannot be seen downstream: an option that silently fails to apply produces
  # two identical arms whose matching sha256 proves nothing. Recorded per run
  # and cross-checked in pe_verify.
  #
  # `config show <name>` and not `config show | grep`: a name nix does not know
  # is an error here, where a grep over the whole dump would print nothing and
  # be indistinguishable from a setting that is off. Its stderr joins the run
  # log rather than being discarded, so a refusal is readable afterwards.
  if [ "$arm" = none ]; then
    eff=none
  else
    # shellcheck disable=SC2086
    eff=$("$nixbin" config show "$setting" $args 2>> "$log" | head -1)
    [ -n "$eff" ] || eff=UNREADABLE
  fi

  # $args is deliberately unquoted: it is a flag list that must word-split.
  # Quoting it passes one giant argument and nix rejects it.
  # shellcheck disable=SC2086
  ( cd "$PE_IX" && NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$stats" \
      "${timer[@]}" "$nixbin" eval --json $args --apply "$apply" .#nixosConfigurations ) \
      > "$out" 2> "$log"
  rc=$?
  echo "rc=$rc" >> "$log"

  # rc first: a signal death is the failure the sha comparison could not see.
  # 128+N is a signal, and 139 in particular is SIGSEGV.
  if [ "$rc" -ne 0 ]; then
    if [ "$rc" -gt 128 ]; then status="FAIL-signal-$((rc-128))"; else status="FAIL-exit-$rc"; fi
  else
    status=$(pe_check_out "$out" "$scope")
    [ "$status" = ok ] || status="FAIL-$status"
  fi

  if [ "$status" = ok ]; then sha=$(pe_sha256 "$out"); else sha=NONE; fi
  # Elapsed and processor time are recorded for the contention check: the ratio
  # of elapsed to processor time shares a window with the run, whereas a
  # one-minute load average describes the minute that just ended. Compare a
  # parallel arm against its own uncontended baseline, never against 1.0.
  wall=$(pe_time real "$log"); user=$(pe_time user "$log"); sys=$(pe_time sys "$log")

  echo "RESULT label=$label arm=$arm scope=$scope rc=$rc status=$status sha256=$sha eff=$eff wall=$wall user=$user sys=$sys" >> "$res"
  if [ "$status" != ok ]; then
    echo "HARNESS FAIL: run '$label' (arm=$arm) rejected: $status" >&2
    echo "  rc=$rc  output=$out  log=$log" >&2
    return 1
  fi
  return 0
}

pe_verify() { # $1 resultfile, $2 scope, $3.. expected labels
  local res=$1 scope=$2; shift 2
  local expected=("$@") bad=0 label line n st sha shas=() first eff effs=() e
  echo "== verifying $res against ${#expected[@]} expected runs =="
  [ -e "$res" ] || { echo "VERIFY FAIL: result file $res absent"; return 1; }

  # Present by name, one line each. A count of matching rows is satisfied by a
  # table with runs missing, so require each expected label individually.
  for label in "${expected[@]}"; do
    n=$(grep -c "^RESULT label=$label " "$res")
    if [ "$n" -eq 0 ]; then
      echo "VERIFY FAIL: expected run '$label' is absent from $res"; bad=1; continue
    fi
    if [ "$n" -gt 1 ]; then
      echo "VERIFY FAIL: expected run '$label' appears $n times (ambiguous)"; bad=1; continue
    fi
    line=$(grep "^RESULT label=$label " "$res")
    st=${line##*status=}
    st=${st%% *}
    sha=${line##*sha256=}
    sha=${sha%% *}
    if [ "$st" != ok ]; then
      echo "VERIFY FAIL: run '$label' status=$st"; bad=1; continue
    fi
    # Belt and braces: never accept the empty-file hash, even if some later
    # edit lets a zero-byte output past pe_check_out.
    if [ "$sha" = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855 ]; then
      echo "VERIFY FAIL: run '$label' hashed the empty file"; bad=1; continue
    fi
    eff=${line##*eff=}
    eff=${eff%% *}
    if [ "$eff" = UNREADABLE ] || [ "$eff" = NA ]; then
      echo "VERIFY FAIL: run '$label' did not report the setting nix actually used (eff=$eff)"; bad=1; continue
    fi
    shas+=("$label=$sha")
    effs+=("$label=$eff")
  done

  # An unexpected extra run is a mismatch between what was asked for and what ran.
  while read -r line; do
    label=${line#RESULT label=}
    label=${label%% *}
    printf '%s\n' "${expected[@]}" | grep -qx "$label" || {
      echo "VERIFY FAIL: unexpected run '$label' in $res"; bad=1; }
  done < <(grep '^RESULT label=' "$res")

  [ "$bad" -eq 0 ] || { echo "VERIFY FAIL: $res did not pass the per-run gates"; return 1; }

  # The arms must actually have differed. Identical drvPaths across runs that
  # were all secretly the same configuration is the pass this harness exists to
  # refuse: it is the arm-construction equivalent of hashing the empty file.
  # Only meaningful for more than one run, so a single-run table is exempt.
  if [ "${#effs[@]}" -gt 1 ]; then
    first=${effs[0]#*=}
    for e in "${effs[@]}"; do
      if [ "${e#*=}" != "$first" ]; then first=DIFFERED; break; fi
    done
    if [ "$first" != DIFFERED ]; then
      echo "VERIFY FAIL: every run used the same configuration, so identical output proves nothing:"
      printf '  %s\n' "${effs[@]}"
      return 1
    fi
  fi

  first=${shas[0]#*=}
  for sha in "${shas[@]}"; do
    if [ "${sha#*=}" != "$first" ]; then
      echo "VERIFY FAIL: drvPath output differs across runs:"
      printf '  %s\n' "${shas[@]}"
      return 1
    fi
  done
  echo "VERIFY OK: ${#expected[@]}/${#expected[@]} runs present, rc=0, output well formed, sha256=$first identical"
  echo "  arms actually used: ${effs[*]}"
  return 0
}

case ${1:-} in
  run)    shift; pe_run "$@" ;;
  verify) shift; pe_verify "$@" ;;
  check)  shift; pe_check_out "$@" ;;
  *) echo "usage: $0 run LABEL NIXBIN ARM SCOPE RESULTFILE | verify RESULTFILE SCOPE LABEL..." >&2; exit 2 ;;
esac
