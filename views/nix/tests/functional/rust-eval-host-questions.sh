#!/usr/bin/env bash


source common.sh
source rust-eval-lib.sh

clearStoreIfPossible

flakeArm=$rustArm
work=$TEST_ROOT/rust-eval-host-questions
rm -rf "$work"
mkdir -p "$work"

# The counters this test reads exist: one served evaluation, with a perf block.
NIX_CONFIG=$rustArm \
    NIX_SHOW_STATS=1 \
    NIX_SHOW_STATS_PATH="$work/probe-stats.json" \
    nix-instantiate --eval --strict -E 1 > /dev/null
assertRustServed "$work/probe-stats.json"
jq -e '.rustEvalPerf | type == "object"' < "$work/probe-stats.json" > /dev/null || {
    echo "the Rust evaluator has no perf counters" >&2
    jq -c '{evaluator, rustEvalPerf}' < "$work/probe-stats.json" >&2
    exit 1
}

nonce="$$-$(date +%s)"
copyInput=$work/copy-input
copyRoot=$work/copy-root
copyArchive=$work/copy-input.tar.gz
mkdir -p "$copyInput"
# jj workspaces, not plain directories, for every flake root here: `path:`
# serves store objects only and refuses a mutable directory (`jjFlakeDir`).
jjFlakeDir "$copyRoot"
cat > "$copyInput/default.nix" <<'EOF'
let source = ./.; in {
  copies = builtins.genList (_: "${source}") 1000;
  pathRules = {
    mountedChangesIdentity = source != source + "";
    ambientDotDot = builtins.toString (source + "/../outside")
      == builtins.dirOf (builtins.toString source) + "/outside";
  };
  stored = builtins.storePath source;
}
EOF
# Imported through an ambient store-path STRING, the module's `./.` must be
# that same ambient spelling. Compared as strings: `builtins.toPath` returns
# a string, and a path never equals a string.
cat > "$copyInput/ambient.nix" <<'EOF'
builtins.toString ./. == builtins.dirOf __curPos.file
EOF
echo "$nonce" > "$copyInput/payload"
tar -C "$copyInput" -czf "$copyArchive" .
# A second, distinct mounted source beside the first: a StorePath memo that
# answered every mounted argument with its first answer would still count
# one unique argument for one source, and only a second source catches it.
copyInput2=$work/copy-input-2
copyArchive2=$work/copy-input-2.tar.gz
mkdir -p "$copyInput2"
cp "$copyInput/default.nix" "$copyInput/ambient.nix" "$copyInput2/"
echo "$nonce-second" > "$copyInput2/payload"
tar -C "$copyInput2" -czf "$copyArchive2" .

cat > "$copyRoot/flake.nix" <<EOF
{
  inputs.dep = {
    url = "tarball+file://$copyArchive";
    flake = false;
  };
  inputs.dep2 = {
    url = "tarball+file://$copyArchive2";
    flake = false;
  };
  outputs = { self, dep, dep2 }:
    let imported = import dep; imported2 = import dep2;
    in {
      copies = imported.copies ++ imported2.copies;
      inherit (imported) pathRules stored;
      ambientImport = import (builtins.toString dep + "/ambient.nix");
    };
}
EOF

copyStats=$work/copy-stats.json
copyInstallable="jj+file://$copyRoot#copies"
copyReadOnlyOut=$work/copy-read-only-out
copyRustOut=$work/copy-rust-out
NIX_CONFIG=$flakeArm nix eval \
    --no-write-lock-file \
    --read-only \
    --json "$copyInstallable" > "$copyReadOnlyOut"
copyLazyPath=$(jq -er '.[0]' < "$copyReadOnlyOut")
# Every ask of one source answers the same path, and the two sources differ.
jq -e '(.[0:1000] | unique | length) == 1 and (.[1000:2000] | unique | length) == 1 and .[0] != .[1000]' \
    < "$copyReadOnlyOut" > /dev/null || {
    echo "the two mounted sources did not answer as two distinct store paths" >&2
    exit 1
}
[[ ! -e $copyLazyPath ]] || {
    echo "the read-only copy oracle materialised $copyLazyPath" >&2
    exit 1
}

NIX_CONFIG=$flakeArm \
    NIX_SHOW_STATS=1 \
    NIX_SHOW_STATS_PATH="$copyStats" \
    nix eval \
        --no-write-lock-file \
        --read-only \
        --json "$copyInstallable" > "$copyRustOut"

cmp -s "$copyReadOnlyOut" "$copyRustOut" || {
    echo "the Rust repeated-copy answer differs from the read-only baseline" >&2
    exit 1
}

jq -e '
    .evaluator == "rust" and
    .rustEvalPerf["q.StorePath"] == 2000 and
    .rustEvalPerf["q.StorePath_unique"] == 2 and
    .rustEvalPerf.copy_hits == 1998 and
    .rustEvalPerf.copyAmbient == 2 and
    .rustEvalPerf.copyMounted == 0
' < "$copyStats" > /dev/null || {
    echo "StorePath questions were not 2,000 asks over two ambient store-object sources, two distinct" >&2
    jq -c '.rustEvalPerf | {
        questions: .["q.StorePath"],
        unique: .["q.StorePath_unique"],
        copy_hits,
        copyMounted,
        copyAmbient
    }' \
        < "$copyStats" >&2
    exit 1
}

for check in pathRules ambientImport; do
    NIX_CONFIG=$flakeArm nix eval \
        --no-write-lock-file \
        --json "jj+file://$copyRoot#$check" > "$work/$check-rust"

done
jq -e '(.mountedChangesIdentity | not) and .ambientDotDot' < "$work/pathRules-rust" > /dev/null
[[ $(cat "$work/ambientImport-rust") == true ]]

for backend in rust; do
    NIX_CONFIG=$flakeArm nix eval \
        --impure \
        --no-write-lock-file \
        --raw "jj+file://$copyRoot#stored" > "$work/store-path-$backend"
done
[[ $(cat "$work/store-path-rust") == "$copyLazyPath" ]]

# Materialise the same input, then repeat the read-only evaluation. The VM's
# root is parse-time provenance, not a test of whether the store path happens
# to exist on disk, so all 1,000 asks must still use the mounted accessor.
NIX_CONFIG=$flakeArm nix eval \
    --no-write-lock-file \
    --json "$copyInstallable" > /dev/null
[[ -e $copyLazyPath ]] || {
    echo "the materialising arm did not create $copyLazyPath" >&2
    exit 1
}
copyMaterialisedStats=$work/copy-materialised-stats.json
NIX_CONFIG=$flakeArm \
    NIX_SHOW_STATS=1 \
    NIX_SHOW_STATS_PATH="$copyMaterialisedStats" \
    nix eval \
        --no-write-lock-file \
        --read-only \
        --json "$copyInstallable" > "$work/copy-materialised-out"
cmp -s "$copyReadOnlyOut" "$work/copy-materialised-out" || {
    echo "the materialised Rust repeated-copy answer differs from the read-only baseline" >&2
    exit 1
}
# The same accounting as before materialisation (see above): the spelling is
# ambient either way, so materialising the input changes nothing the
# evaluator can see, which is the claim.
jq -e '
    .rustEvalPerf["q.StorePath"] == 2000 and
    .rustEvalPerf["q.StorePath_unique"] == 2 and
    .rustEvalPerf.copy_hits == 1998 and
    .rustEvalPerf.copyAmbient == 2 and
    .rustEvalPerf.copyMounted == 0
' < "$copyMaterialisedStats" > /dev/null || {
    echo "the materialised input changed the copy accounting" >&2
    jq -c '.rustEvalPerf | {
        questions: .["q.StorePath"],
        unique: .["q.StorePath_unique"],
        copy_hits,
        copyMounted,
        copyAmbient
    }' < "$copyMaterialisedStats" >&2
    exit 1
}

ambientTree=$work/ambient
mkdir -p "$ambientTree"
echo ambient > "$ambientTree/file"
ambientStats=$work/ambient-stats.json
NIX_CONFIG=$rustArm \
    NIX_SHOW_STATS=1 \
    NIX_SHOW_STATS_PATH="$ambientStats" \
    nix-instantiate --eval --strict -E "\"\${$ambientTree}\"" > "$work/ambient-out"

jq -e '
    .evaluator == "rust" and
    .rustEvalPerf["q.StorePath"] == 1 and
    .rustEvalPerf.copyMounted == 0 and
    .rustEvalPerf.copyAmbient == 1
' < "$ambientStats" > /dev/null || {
    echo "the TEST_ROOT path did not count as one ambient copy" >&2
    jq -c '.rustEvalPerf | {
        questions: .["q.StorePath"],
        copyMounted,
        copyAmbient
    }' < "$ambientStats" >&2
    exit 1
}

filteredInput=$work/filtered-input
filteredRoot=$work/filtered-root
mkdir -p "$filteredInput/keep"
jjFlakeDir "$filteredRoot"
echo keep > "$filteredInput/keep/value"
echo drop > "$filteredInput/drop"
echo '/drop export-ignore' > "$filteredInput/.gitattributes"
cat > "$filteredInput/default.nix" <<'EOF'
builtins.path {
  path = ./.;
  name = "rust-filtered-input";
}
EOF
git -C "$filteredInput" init -q
git -C "$filteredInput" add .
git -C "$filteredInput" \
    -c user.name=rust-eval-test \
    -c user.email=rust-eval-test.invalid \
    commit -qm 'filtered accessor fixture'
cat > "$filteredRoot/flake.nix" <<EOF
{
  inputs.dep = { url = "git+file://$filteredInput?exportIgnore=1"; flake = false; };
  outputs = { self, dep }: { filtered = import dep; };
}
EOF
filteredInstallable="jj+file://$filteredRoot#filtered"
NIX_CONFIG=$flakeArm nix eval \
    --no-write-lock-file \
    --raw "$filteredInstallable" > "$work/filtered-rust-out"
filteredPath=$(cat "$work/filtered-rust-out")
[[ -f "$filteredPath/keep/value" && ! -e "$filteredPath/drop" ]]

drvStats=$work/drv-stats.json
# Two distinct derivations, each produced by two independent applications:
# equal within a pair, different between the pairs, so a known-derivation
# cache keyed as one global value cannot pass.
drvExpr="
let
  mk = which: n: derivation {
    name = \"rust-host-question-\" + which + \"-$nonce\";
    system = builtins.currentSystem;
    builder = \"/bin/sh\";
    args = [ \"-c\" \"exit 0\" ];
    marker = \"$nonce\" + builtins.toString (n - n);
  };
in {
  a = (mk \"x\" 1).drvPath; b = (mk \"x\" 2).drvPath;
  c = (mk \"y\" 1).drvPath; d = (mk \"y\" 2).drvPath;
}
"
NIX_CONFIG=$rustArm \
    NIX_SHOW_STATS=1 \
    NIX_SHOW_STATS_PATH="$drvStats" \
    nix-instantiate --eval --strict --json --read-write-mode -E "$drvExpr" > "$work/drv-out"
jq -e '.a == .b and .c == .d and .a != .c' < "$work/drv-out" > /dev/null || {
    echo "the two derivation pairs did not come out equal within and different between:" >&2
    cat "$work/drv-out" >&2
    exit 1
}

# Four derivationStrict calls over two distinct derivations: the VM answers
# the two repeats itself (q.WriteDrv_skipped), the host is asked twice, answers
# both from the bytes (q.WriteDrv_deferred) and hands them to the store in one
# batch (drvFlushes) that writes both (drvWrites).
jq -e '
    .evaluator == "rust" and
    .rustEvalPerf["q.WriteDrv"] == 2 and
    .rustEvalPerf["q.WriteDrv_unique"] == 2 and
    .rustEvalPerf["q.WriteDrv_skipped"] == 2 and
    .rustEvalPerf["q.WriteDrv_deferred"] == 2 and
    .rustEvalPerf["q.WriteDrv_flushes"] == 1 and
    .rustEvalPerf.drvWrites == 2 and
    .rustEvalPerf.drvFlushes == 1
' < "$drvStats" > /dev/null || {
    echo "four derivationStrict calls over two derivations did not produce two skips, two deferred writes and one batch of two" >&2
    jq -c '.rustEvalPerf | {
        questions: .["q.WriteDrv"],
        unique: .["q.WriteDrv_unique"],
        skipped: .["q.WriteDrv_skipped"],
        deferred: .["q.WriteDrv_deferred"],
        flushes: .["q.WriteDrv_flushes"],
        drvWrites,
        drvFlushes
    }' < "$drvStats" >&2
    exit 1
}

# A tarball input is backed by Nix's immutable unpack cache, so lazy-trees can
# mount it without the platform-specific mutable-worktree snapshot facility.
# A read-only evaluation supplies the expected answer without writing its
# source or derivation paths. The Rust arm must return that answer byte for
# byte, both before deletion and when reevaluated after deletion. The counters
# also prove the second Rust run reached the canonical derivation writer and
# reused the recovered mount for another coercion.
gcInput=$work/gc-input
gcRoot=$work/gc-root
gcArchive=$work/gc-input.tar.gz
mkdir -p "$gcInput"
jjFlakeDir "$gcRoot"
system=$(nix-instantiate --eval --strict -E builtins.currentSystem | tr -d '"')
cat > "$gcInput/default.nix" <<EOF
let
  source = ./.;
  drv = derivation {
    name = "rust-host-gc-$nonce";
    system = "$system";
    builder = "/bin/sh";
    args = [ "-c" "exit 0" ];
    sourceA = source;
    sourceB = source;
  };
in {
  materialised = "\${source}";
  inherit drv;
  answer = builtins.toJSON {
    materialised = "\${source}";
    drvPath = drv.drvPath;
  };
}
EOF
echo lazy-payload > "$gcInput/payload"
tar -C "$gcInput" -czf "$gcArchive" .

cat > "$gcRoot/flake.nix" <<EOF
{
  inputs.dep = {
    url = "tarball+file://$gcArchive";
    flake = false;
  };
  outputs = { self, dep }: import dep;
}
EOF

gcInstallable="jj+file://$gcRoot#answer"
readOnlyGcOut=$work/gc-read-only-out
rustGcBefore=$work/gc-rust-before
rustGcAfter=$work/gc-rust-after

NIX_CONFIG=$flakeArm nix eval \
    --no-write-lock-file \
    --read-only \
    --raw "$gcInstallable" > "$readOnlyGcOut"
lazyPath=$(jq -er '.materialised' < "$readOnlyGcOut")
drvPath=$(jq -er '.drvPath' < "$readOnlyGcOut")
[[ ! -e $lazyPath ]] || {
    echo "the read-only oracle materialised $lazyPath" >&2
    exit 1
}
[[ ! -e $drvPath ]] || {
    echo "the read-only oracle wrote $drvPath" >&2
    exit 1
}

NIX_CONFIG=$flakeArm nix eval \
    --no-write-lock-file \
    --raw "$gcInstallable" > "$rustGcBefore"
cmp -s "$readOnlyGcOut" "$rustGcBefore" || {
    echo "the first Rust GC answer differs from the read-only baseline" >&2
    echo "baseline: $(cat "$readOnlyGcOut")" >&2
    echo "rust: $(cat "$rustGcBefore")" >&2
    exit 1
}
[[ -e $lazyPath ]] || {
    echo "the first Rust GC evaluation did not materialise $lazyPath" >&2
    exit 1
}
[[ -e $drvPath ]] || {
    echo "the first Rust GC evaluation did not write $drvPath" >&2
    exit 1
}
nix-store --delete "$drvPath"
nix-store --delete "$lazyPath"
[[ ! -e $lazyPath ]] || {
    echo "the lazy-input store path remained after deletion: $lazyPath" >&2
    exit 1
}
[[ ! -e $drvPath ]] || {
    echo "the derivation path remained after deletion: $drvPath" >&2
    exit 1
}

gcStats=$work/gc-stats.json
NIX_CONFIG=$flakeArm \
    NIX_SHOW_STATS=1 \
    NIX_SHOW_STATS_PATH="$gcStats" \
    nix eval \
        --no-write-lock-file \
        --raw "$gcInstallable" > "$rustGcAfter"

cmp -s "$readOnlyGcOut" "$rustGcAfter" || {
    echo "the Rust GC answer after deletion differs from the read-only baseline" >&2
    echo "baseline: $(cat "$readOnlyGcOut")" >&2
    echo "rust: $(cat "$rustGcAfter")" >&2
    exit 1
}
[[ -e $lazyPath ]] || {
    echo "the Rust evaluator did not rematerialise the GC'd lazy input $lazyPath" >&2
    exit 1
}
[[ -e $drvPath ]] || {
    echo "the Rust evaluator did not rewrite $drvPath after deletion" >&2
    exit 1
}
jq -e '
    .evaluator == "rust" and
    .rustEvalPerf["q.StorePath"] >= 2 and
    (.rustEvalPerf.copyMounted + .rustEvalPerf.copyAmbient) >= 1 and
    .rustEvalPerf.drvWrites == 1 and
    .rustEvalPerf.drvFlushes == 1
' < "$gcStats" > /dev/null || {
    echo "lazy-input GC recovery missed a mounted copy or canonical derivation writer" >&2
    jq -c '.rustEvalPerf | {
        storePathQuestions: .["q.StorePath"],
        copyMounted,
        copyAmbient,
        drvWrites,
        drvFlushes
    }' < "$gcStats" >&2
    exit 1
}

echo "rust-eval-host-questions: ok"
