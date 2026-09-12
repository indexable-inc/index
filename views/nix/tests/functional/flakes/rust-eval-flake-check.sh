#!/usr/bin/env bash

source ./common.sh
source ../rust-eval-lib.sh
requireGit

work="$TEST_ROOT/rust-flake-check"
flakeDir="$work/flake"
mkdir -p "$work"
createGitRepo "$flakeDir" ""
cp "$config_nix" "$flakeDir/config.nix"
cache="$work/cache"

commitFixture() {
    git -C "$flakeDir" add .
    git -C "$flakeDir" commit -m fixture
}
runCheck() {
    nix flake check --no-write-lock-file --builders '' --option eval-cache-dir "$cache" \
        --option eval-cache-verify-rate 0 "$flakeDir" "$@"
}
fails() {
    local name=$1
    shift
    if runCheck "$@" > "$work/$name.out" 2> "$work/$name.err"; then
        echo "flake check unexpectedly succeeded: $name" >&2
        exit 1
    fi
}

cat > "$flakeDir/flake.nix" <<EOF_NIX
{
  outputs = { self }: {
    checks.$system = { bad = 123; alsoBad = throw "second invalid check"; };
    packages.other-system.bad = throw "foreign system evaluated";
  };
}
EOF_NIX
commitFixture
fails invalid --no-build --keep-going
grep -F 'bad' "$work/invalid.err"
grep -F 'second invalid check' "$work/invalid.err"

cat > "$flakeDir/flake.nix" <<'NIX'
{ outputs = { self }: { packages.other-system.bad = throw "foreign system evaluated"; }; }
NIX
commitFixture
runCheck --no-build > "$work/omitted.out" 2> "$work/omitted.err"
grep -F 'omitted incompatible systems' "$work/omitted.err"
fails foreign --no-build --all-systems
grep -F 'foreign system evaluated' "$work/foreign.err"

mkdir "$flakeDir/template"
echo template > "$flakeDir/template/content"
cat > "$flakeDir/flake.nix" <<EOF_NIX
{
  outputs = { self }: {
    overlays.default = final: throw "overlay body must stay lazy";
    nixosModules.default = {};
    bundlers.$system.default = value: value;
    apps.$system.default = { type = "app"; program = "/bin/tool"; };
    templates.default = { path = ./template; description = "template"; };
  };
}
EOF_NIX
commitFixture
runCheck --no-build

cat > "$flakeDir/flake.nix" <<'NIX'
{ outputs = { self }: { overlay = final: prev: {}; }; }
NIX
commitFixture
fails deprecated --no-build
grep -F "unsupported flake output 'overlay'" "$work/deprecated.err"

# Validations are reusable; buildability is still checked on every invocation.
# Two aliases retain their names even though the store builds their drv once.
cat > "$flakeDir/flake.nix" <<EOF_NIX
{
  outputs = { self }:
    let cfg = import ./config.nix;
        failing = cfg.mkDerivation {
          name = "rust-flake-check-failure";
          buildCommand = "echo rust-check-builder-ran >&2; exit 1";
        };
    in { checks.$system = { first = failing; second = failing; }; };
}
EOF_NIX
commitFixture
for attempt in cold warm; do
    NIX_SHOW_STATS=1 NIX_SHOW_STATS_PATH="$work/$attempt.stats" fails "$attempt" --keep-going
    grep -F 'running 1 flake checks' "$work/$attempt.err"
    grep -F 'first' "$work/$attempt.err"
    grep -F 'second' "$work/$attempt.err"
done
assertDrvWrites "$work/cold.stats"
assertMemoServed "$work/warm.stats"
assertNoDrvWrites "$work/warm.stats"
# Both independently keyed validation phases must be served; neither may compile.
jq -e '(.rustEvalPerf.memo_served // 0) >= 3 and .rustEvalPerf.memo_served == .evaluatorCalls.rust and (.rustEvalPerf.compiles // 0) == 0' \
    < "$work/warm.stats"

cat > "$flakeDir/flake.nix" <<EOF_NIX
{
  outputs = { self }:
    let cfg = import ./config.nix;
    in { checks.$system.passing = cfg.mkDerivation {
      name = "rust-flake-check-success";
      buildCommand = "mkdir \$out";
    }; };
}
EOF_NIX
commitFixture
runCheck
runCheck

# The Hydra question captures IFD=false even when the user enables IFD.
cat > "$flakeDir/flake.nix" <<'NIX'
{
  outputs = { self }:
    let cfg = import ./config.nix;
        generated = cfg.mkDerivation {
          name = "rust-hydra-check-ifd";
          buildCommand = "printf generated > $out";
        };
    in { hydraJobs.job = cfg.mkDerivation {
      name = builtins.readFile generated;
      buildCommand = "mkdir $out";
    }; };
}
NIX
commitFixture
fails hydra --option allow-import-from-derivation true
grep -F "option 'allow-import-from-derivation' is disabled" "$work/hydra.err"

# Register the generated derivation without building it so readonly evaluation
# reaches the IFD policy check instead of failing a missing-drv lookup first.
cat > "$flakeDir/generated.nix" <<'NIX'
let cfg = import ./config.nix;
in cfg.mkDerivation {
  name = "rust-regular-check-ifd";
  buildCommand = "printf generated > $out";
}
NIX
cat > "$flakeDir/flake.nix" <<EOF_NIX
{
  outputs = { self }:
    let cfg = import ./config.nix;
        generated = import ./generated.nix;
    in { checks.$system.job = cfg.mkDerivation {
      name = builtins.readFile generated;
      buildCommand = "mkdir \$out";
    }; };
}
EOF_NIX
commitFixture
nix eval --raw --file "$flakeDir/generated.nix" drvPath > "$work/regular.drv"
for mode in cached uncached; do
    cacheMode=()
    if [[ "$mode" == uncached ]]; then cacheMode=(--option eval-cache-dir ''); fi
    fails "no-build-ifd-$mode" --no-build --option allow-import-from-derivation true "${cacheMode[@]}"
    grep -F "option 'allow-import-from-derivation' is disabled" "$work/no-build-ifd-$mode.err"
done
runCheck --option allow-import-from-derivation true
# Both cache modes must refuse the same Built context after its output exists.
for mode in cached uncached; do
    cacheMode=()
    if [[ "$mode" == uncached ]]; then cacheMode=(--option eval-cache-dir ''); fi
    fails "explicit-ifd-$mode" --option allow-import-from-derivation false "${cacheMode[@]}"
    grep -F "option 'allow-import-from-derivation' is disabled" "$work/explicit-ifd-$mode.err"
    fails "readonly-ifd-$mode" --no-build --option allow-import-from-derivation true "${cacheMode[@]}"
    grep -F "option 'allow-import-from-derivation' is disabled" "$work/readonly-ifd-$mode.err"
done

# Trusted root configuration is applied before child flake documents are read.
# It may enable IFD normally, but cannot lift the --no-build ceiling.
childDir="$work/child"
createGitRepo "$childDir" ""
cp "$config_nix" "$childDir/config.nix"
cat > "$childDir/generated.nix" <<'NIX'
let cfg = import ./config.nix;
in cfg.mkDerivation {
  name = "rust-child-document-ifd";
  buildCommand = "printf child-document-generated > $out";
}
NIX
cat > "$childDir/flake.nix" <<'NIX'
{
  description = builtins.readFile (import ./generated.nix);
  outputs = { self }: {};
}
NIX
nix eval --raw --file "$childDir/generated.nix" drvPath > "$work/child.drv"
git -C "$childDir" add .
git -C "$childDir" commit -m 'child metadata IFD'
cat > "$flakeDir/flake.nix" <<EOF_NIX
{
  nixConfig.allow-import-from-derivation = true;
  inputs.child.url = "git+file://$childDir";
  outputs = { self, child }: {};
}
EOF_NIX
commitFixture
for mode in cached uncached; do
    cacheMode=()
    if [[ "$mode" == uncached ]]; then cacheMode=(--option eval-cache-dir ''); fi
    fails "child-document-no-build-$mode" --no-build --accept-flake-config \
        --option allow-import-from-derivation false "${cacheMode[@]}"
    grep -F "option 'allow-import-from-derivation' is disabled" "$work/child-document-no-build-$mode.err"
done
# The positive control starts with IFD disabled too: accepted configuration
# must still take effect for ordinary locking.
runCheck --accept-flake-config --option allow-import-from-derivation false
# Readonly's strict IFD policy also applies once the generated output exists.
for mode in cached uncached; do
    cacheMode=()
    if [[ "$mode" == uncached ]]; then cacheMode=(--option eval-cache-dir ''); fi
    fails "child-document-readonly-warm-$mode" --no-build --accept-flake-config \
        --option allow-import-from-derivation false "${cacheMode[@]}"
    grep -F "option 'allow-import-from-derivation' is disabled" "$work/child-document-readonly-warm-$mode.err"
done

echo 'rust-eval-flake-check: ok'
