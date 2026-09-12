#!/usr/bin/env bash

source common.sh

# These checks observe actual local processes and shared state outside the
# sandbox; the mandatory Rust gate runs them against its private local store.
needLocalStore "build scheduler process and slot assertions require the private local store"
clearStoreIfPossible

fixture="$TEST_ROOT/build-scheduler"
mkdir -p "$fixture"
cp "$config_nix" "$fixture/config.nix"
cat > "$fixture/builds.nix" <<'EOF'
{ stateDir }:
let
  inherit (import ./config.nix) mkDerivation;
  counted = name: mkDerivation {
    inherit name;
    buildCommand = ''
      while ! mkdir '${stateDir}/counter.lock' 2>/dev/null; do sleep 0.02; done
      current=$(cat '${stateDir}/current')
      current=$((current + 1))
      echo "$current" > '${stateDir}/current'
      maximum=$(cat '${stateDir}/maximum')
      if [ "$current" -gt "$maximum" ]; then echo "$current" > '${stateDir}/maximum'; fi
      rmdir '${stateDir}/counter.lock'
      sleep 0.3
      while ! mkdir '${stateDir}/counter.lock' 2>/dev/null; do sleep 0.02; done
      current=$(cat '${stateDir}/current')
      echo "$((current - 1))" > '${stateDir}/current'
      rmdir '${stateDir}/counter.lock'
      echo complete > "$out"
    '';
  };
in {
  one = counted "scheduler-one";
  two = counted "scheduler-two";
  three = counted "scheduler-three";
  four = counted "scheduler-four";
  afterCancellation = counted "scheduler-after-cancellation";
  success = mkDerivation {
    name = "scheduler-success";
    buildCommand = ''echo success > "$out"'';
  };
  failure = mkDerivation {
    name = "scheduler-failure";
    buildCommand = ''echo intentional-builder-failure >&2; exit 42'';
  };
  silent = mkDerivation {
    name = "scheduler-silent";
    buildCommand = ''sleep 60; echo late > "$out"'';
  };
  cancelled = mkDerivation {
    name = "scheduler-cancelled";
    buildCommand = ''
      echo $$ > '${stateDir}/builder-pid'
      exec sleep 120
    '';
  };
}
EOF

build=(nix build --file "$fixture/builds.nix" --argstr stateDir "$fixture"
    --no-link --option sandbox false --option builders "" --option substituters "")

# Admission limits actual concurrency, not merely the counters returned by the
# Rust API. Four ready goals must use both slots and must never exceed two.
echo 0 > "$fixture/current"
echo 0 > "$fixture/maximum"
"$coreutils/timeout" 20 "${build[@]}" -j2 one two three four
[[ $(cat "$fixture/current") == 0 ]]
[[ $(cat "$fixture/maximum") == 2 ]]

# A failed child releases its slot while --keep-going completes the other goal.
status=0
"$coreutils/timeout" 20 "${build[@]}" -j1 --keep-going failure success \
    > "$fixture/failure.out" 2> "$fixture/failure.err" || status=$?
[[ $status != 0 && $status != 124 && $status != 137 ]]
grep -q 'intentional-builder-failure' "$fixture/failure.err"
successPath=$(nix eval --raw --file "$fixture/builds.nix" --argstr stateDir "$fixture" success.outPath)
[[ $(cat "$successPath") == success ]]

# Deadline delivery kills a silent builder. The outer timeout is a failure
# witness, so a stuck worker cannot turn this regression into an endless test.
status=0
"$coreutils/timeout" 15 "${build[@]}" -j1 --timeout 2 silent \
    > "$fixture/timeout.out" 2> "$fixture/timeout.err" || status=$?
[[ $status != 0 && $status != 124 && $status != 137 ]]
grep -q 'timed out after 2 seconds' "$fixture/timeout.err"

# No deadline or output is armed here: only the interrupt wakeup pipe can wake
# the host poll. Cancellation must terminate the real child and release locks.
buildPid=
builderPid=
cleanup() {
    if [[ -n $buildPid ]] && kill -0 "$buildPid" 2> "$fixture/cleanup.err"; then
        kill -KILL "$buildPid" || true
        wait "$buildPid" || true
    fi
    if [[ -n $builderPid ]] && kill -0 "$builderPid" 2> "$fixture/cleanup.err"; then
        kill -KILL "$builderPid" || true
    fi
}
trap cleanup EXIT
"${build[@]}" -j1 --timeout 0 --max-silent-time 0 cancelled \
    > "$fixture/cancel.out" 2> "$fixture/cancel.err" &
buildPid=$!
for _ in $(seq 1 100); do
    [[ ! -s $fixture/builder-pid ]] || break
    sleep 0.1
done
[[ -s $fixture/builder-pid ]]
builderPid=$(cat "$fixture/builder-pid")
[[ $builderPid =~ ^[1-9][0-9]*$ ]]
kill -0 "$builderPid"
kill -TERM "$buildPid"
"$coreutils/timeout" 10 "$bash" -c '
    while kill -0 "$1" 2> "$2"; do sleep 0.1; done
' _ "$buildPid" "$fixture/cancel-wait.err"
status=0
wait "$buildPid" || status=$?
buildPid=
[[ $status != 0 ]]
if kill -0 "$builderPid" 2> "$fixture/cancel-child.err"; then
    fail "cancelled worker left its builder alive"
fi
builderPid=
trap - EXIT

# Slot unregistration is independently exercised in Rust with duplicate stop
# calls. This final real build checks the host remains usable after cleanup.
"$coreutils/timeout" 15 "${build[@]}" -j1 afterCancellation
[[ $(cat "$fixture/current") == 0 ]]
