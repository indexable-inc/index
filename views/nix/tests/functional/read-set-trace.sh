#!/usr/bin/env bash

# Read-set instrumentation: the trace has to name every kind of input the
# design calls load bearing, and it has to be absent by default. Each check
# here counts a named thing and requires it present, rather than requiring an
# absence, because a hook that never fires produces a trace that is empty and
# well formed.

source common.sh

TODO_NixOS

clearStoreIfPossible

trace="$TEST_ROOT/read-set-trace.jsonl"
rm -f "$trace"

mkdir -p "$TEST_ROOT/readset"
cd "$TEST_ROOT/readset"

cat > lib.nix <<'EOF'
{
  greeting = "hello";
  # unsafeGetAttrPos makes this file's line numbers an input, not just its bytes.
  where = builtins.unsafeGetAttrPos "greeting" { greeting = 1; };
}
EOF

# `a.nix` and `b.nix` both force `lib.nix` while their own top level is being
# forced. `default.nix` has already forced it by then, so both are served from
# the file cache and neither enters the boundary: this is the case phase 1
# could not see at all, and deleting the reuse recording makes both of the
# checks below fail.
cat > a.nix <<'EOF'
(import ./lib.nix).greeting
EOF

cat > b.nix <<'EOF'
(import ./lib.nix).greeting
EOF

cat > default.nix <<'EOF'
let
  lib = import ./lib.nix;
  probe = derivation {
    name = "read-set-probe";
    builder = "/bin/sh";
    system = builtins.currentSystem;
  };
in
{
  a = import ./a.nix;
  b = import ./b.nix;
  # A directory listing is an input distinct from any file's contents: adding
  # a file changes it without changing a byte anywhere.
  entries = builtins.attrNames (builtins.readDir ./.);
  line = lib.where.line;
  drv = probe;
  # Consuming `probe` puts its drvPath in this derivation's string context,
  # which is what makes it an input derivation and so the far end of an edge.
  drv2 = derivation {
    name = "read-set-probe-downstream";
    builder = "/bin/sh";
    system = builtins.currentSystem;
    upstream = probe;
  };
  # A path rather than a derivation, so this lands in the derivation's input
  # sources and in nothing else. No input derivation and no file read by this
  # boundary names it, so if input sources go unrecorded this derivation has
  # no input that moves when the file does.
  drv3 = derivation {
    name = "read-set-probe-src";
    builder = "/bin/sh";
    system = builtins.currentSystem;
    payload = ./payload.txt;
  };
}
EOF

echo "payload contents" > payload.txt

# Off by default: no trace, and no file appears where one would.
nix eval --impure --raw --expr 'toString (import '"$PWD"'/default.nix).line' > /dev/null
[[ ! -e "$trace" ]]

nix eval --impure --raw \
  --option read-set-trace-file "$trace" \
  --expr '
    let x = import '"$PWD"'/default.nix;
    in toString x.line + toString (builtins.length x.entries)
       + x.drv.drvPath + x.drv2.drvPath + x.drv3.drvPath + x.a + x.b
  ' > /dev/null

[[ -e "$trace" ]]

# The summary is the last record, and it is what the counts below are read
# from, so a truncated trace fails here rather than silently reporting zeros.
summary=$(grep '"t":"summary"' "$trace")
[[ -n "$summary" ]]

jsonField() {
  echo "$summary" | jq -r ".$1"
}

# One entry per file whose top level expression was forced: default.nix and
# lib.nix. Requiring at least 2 rather than exactly 2 because the harness may
# evaluate more.
importEntries=$(jsonField kind_import)
(( importEntries >= 2 )) || { echo "expected at least 2 import entries, got $importEntries"; exit 1; }

# The derivation boundary fired.
drvEntries=$(jsonField kind_derivation)
(( drvEntries >= 1 )) || { echo "expected at least 1 derivation entry, got $drvEntries"; exit 1; }

rootEntries=$(jsonField kind_root)
(( rootEntries == 1 )) || { echo "expected exactly 1 root entry, got $rootEntries"; exit 1; }

reads=$(jsonField reads)
(( reads > 0 )) || { echo "the trace recorded no reads at all"; exit 1; }

# Every kind of input the design names has to be present by name. A read set
# missing listings or positions is unsound for the cache phase 2 builds on it,
# and both are exactly what a naive implementation drops.
inputKinds=$(grep '"t":"in"' "$trace" | jq -r .kind | sort -u)
for kind in contents listing metadata position; do
  echo "$inputKinds" | grep -qx "$kind" || {
    echo "no input of kind '$kind' in the trace; got:"; echo "$inputKinds"; exit 1;
  }
done

# The listing that was read is this directory, not some incidental one.
grep '"t":"in"' "$trace" | jq -e --arg d "$PWD" 'select(.kind == "listing" and (.path | endswith($d)))' > /dev/null

# A position observation carries the line and column it observed, so that an
# edit which only shifts lines still changes the recorded input.
grep '"t":"in"' "$trace" | jq -e 'select(.kind == "position" and (.path | test(":[0-9]+:[0-9]+$")))' > /dev/null

# Every read records what it observed, not merely that it happened. Without
# this a trace cannot tell an unchanged answer from one never compared, which
# is the failure that looks like success.
for kind in contents listing metadata; do
  id=$(grep '"t":"in"' "$trace" | jq -r --arg k "$kind" 'select(.kind == $k) | .id' | head -n 1)
  [[ -n "$id" ]] || { echo "no input of kind $kind"; exit 1; }
  grep '"t":"obs"' "$trace" | jq -e --argjson i "$id" 'select(.id == $i)' > /dev/null || {
    echo "input $id of kind $kind has no recorded observation"; exit 1;
  }
done

# A derivation's input sources are inputs of its entry. Without this the only
# evidence that a derivation embedding a tree path moved is the tree path
# itself, which nothing else records, and the derivations below it in the
# input-derivation graph are unreachable too.
srcInput=$(grep '"t":"in"' "$trace" \
  | jq -r 'select(.kind == "store" and (.rel | endswith("-payload.txt"))) | .id' | head -n 1)
[[ -n "$srcInput" ]] || {
  echo "no store input naming the derivation's source path; input sources are unrecorded"
  grep '"t":"in"' "$trace" | jq -c 'select(.kind == "store")'
  exit 1
}
grep '"t":"entry"' "$trace" \
  | jq -e --argjson i "$srcInput" \
      'select(.kind == "derivation" and .key == "read-set-probe-src" and (.inputs | index($i)))' \
  > /dev/null || {
  echo "the read-set-probe-src entry does not list its source path (input $srcInput) as an input"
  grep '"t":"entry"' "$trace" | jq -c 'select(.key == "read-set-probe-src")'
  exit 1
}

# A stat records the type it saw, so a path changing from file to directory is
# a changed input even though it existed both times.
grep '"t":"obs"' "$trace" | jq -e 'select(.v == "regular" or .v == "directory")' > /dev/null

# A file's contents are hashed rather than carried, however short.
grep '"t":"obs"' "$trace" | jq -e 'select(.v | test("^[0-9a-f]{32}$"))' > /dev/null

# An input is named by the tree that answered for it and its path within that
# tree, so that an edit elsewhere in the tree does not rename it.
grep '"t":"in"' "$trace" | jq -e 'select(has("tree") and has("rel"))' > /dev/null
grep '"t":"tree"' "$trace" | jq -e 'select(has("root"))' > /dev/null

# Every tree carries a name that survives a run, and a view, so that two runs
# can be compared without pairing records by the order they were first seen in.
# A record with neither a fingerprint nor a display has nothing that survives,
# and one such record is enough to make an analysis pair by position: that is
# what reported 99.2% of an evaluation invalidated where the answer is 4.3%.
trees=$(grep -c '"t":"tree"' "$trace")
(( trees > 0 )) || { echo "no tree records at all"; exit 1; }
named=$(grep '"t":"tree"' "$trace" | jq -r 'select(has("identity") and has("view")) | .id' | wc -l)
(( named == trees )) || {
  echo "only $named of $trees tree records carry an identity and a view"
  grep '"t":"tree"' "$trace" | jq -c 'select((has("identity") and has("view")) | not)'
  exit 1
}
if grep '"t":"tree"' "$trace" | jq -e 'select(.anonymous == true)' > /dev/null; then
  echo "a tree record carries no identity, so two runs could only pair it by position:"
  grep '"t":"tree"' "$trace" | jq -c 'select(.anonymous == true)'
  exit 1
fi
if grep '"t":"tree"' "$trace" | jq -e 'select(.identity == "")' > /dev/null; then
  echo "a tree record has an empty identity"; exit 1;
fi



# A tree's version is an input in its own right. Nothing in the filesystem
# stands in for it, so without this class an evaluation that embeds a revision
# in a derivation validates against unchanged file inputs and a cache serves
# the previous commit's answer.
traceRev="$TEST_ROOT/read-set-trace-rev.jsonl"
mkdir -p "$TEST_ROOT/readsetflake"
cat > "$TEST_ROOT/readsetflake/flake.nix" <<'EOF'
{
  outputs = { self, ... }: {
    # Reading lastModified makes this flake's version an input of the
    # evaluation, which is the whole point of the class.
    stamp = toString (self.lastModified or 0);
  };
}
EOF
git -C "$TEST_ROOT/readsetflake" init --quiet
git -C "$TEST_ROOT/readsetflake" add flake.nix
git -C "$TEST_ROOT/readsetflake" -c user.email=t@t -c user.name=t commit --quiet -m init

# Give the flake a second revision, so its version is not simply its first.
# The edit is committed because it has to be: a Git working tree with
# uncommitted changes to tracked files has no revision to lock to and is
# refused before an accessor is built (git.cc, pinCheckedOutCommit).
echo '# edited' >> "$TEST_ROOT/readsetflake/flake.nix"
git -C "$TEST_ROOT/readsetflake" -c user.email=t@t -c user.name=t commit --quiet -am edit

nix eval --raw \
  --option read-set-trace-file "$traceRev" \
  "git+file://$TEST_ROOT/readsetflake#stamp" > /dev/null

# The version input is present, named without carrying the version, and its
# observed value is the version itself.
grep '"t":"in"' "$traceRev" | jq -e 'select(.kind == "tree-attr")' > /dev/null
if grep '"t":"in"' "$traceRev" | jq -e 'select(.kind == "tree-attr" and (.rel | test("[0-9a-f]{40}")))' > /dev/null; then
  echo "a tree-attr input is named with a revision in it, so a moved revision would look like a renamed input"
  exit 1
fi
# A tree attribute is not in a tree. Recording one used to manufacture a tree
# record with no root, no fingerprint and no display, holding inputs whose
# `rel` is a serialised flake input rather than a path, and it was one of the
# records that forced two runs to be paired by position.
if grep '"t":"in"' "$traceRev" | jq -e 'select(.kind == "tree-attr" and has("tree"))' > /dev/null; then
  echo "a tree-attr input claims to belong to a tree"
  exit 1
fi

# Nothing here asserts that a fingerprint's version component is kept out of
# its identity, because no fetcher builds a fingerprint with one any more. A
# Git working tree with uncommitted changes is refused before an accessor
# exists, so git.cc fingerprints a commit and its content flags and nothing
# else, and a jj fingerprint is `jj-tree:<treeHash>` in full. Recognising the
# same tree across an edit relied on the version being a separable suffix;
# under tree identity the version IS the identity, so that recognition is gone
# for both fetchers rather than expressible in some other form. What survives
# of it is `identityOfFingerprint` in src/libexpr/eval-readset.cc, which now
# strips a suffix nothing writes.

revId=$(grep '"t":"in"' "$traceRev" | jq -r 'select(.kind == "tree-attr" and (.rel | endswith("#lastModified"))) | .id' | head -n 1)
[[ -n "$revId" ]] || { echo "no lastModified input recorded"; exit 1; }
grep '"t":"obs"' "$traceRev" | jq -e --argjson i "$revId" 'select(.id == $i and (.v | test("^[0-9]+$")))' > /dev/null

# The import entry for lib.nix has lib.nix's contents in its read set. This is
# the check that the recording is attributed to the innermost entry rather
# than to whatever entry happened to be open.
libId=$(grep '"t":"in"' "$trace" | jq -r --arg p "$PWD/lib.nix" 'select(.kind == "contents" and (.path | endswith("/lib.nix"))) | .id' | head -n 1)
[[ -n "$libId" ]]
grep '"t":"entry"' "$trace" | jq -e --argjson i "$libId" \
  'select(.kind == "import" and (.key | endswith("/lib.nix")) and (.inputs | index($i)))' > /dev/null

# Edges between entries, which is what makes an entry whose own read set did
# not change decidable at all. Phase 1 recorded only the nesting, so every
# consumer of a memoised result had no recorded relationship to it.
edges=$(jsonField edges)
(( edges > 0 )) || { echo "the trace recorded no edges between entries"; exit 1; }

entryId() {
  # The id of the one entry of the given kind whose key ends in $2.
  grep '"t":"entry"' "$trace" \
    | jq -r --arg k "$1" --arg s "$2" 'select(.kind == $k and (.key | endswith($s))) | .id' \
    | head -n 1
}

hasEdge() {
  # Does entry $1 record an edge to entry $2?
  grep '"t":"entry"' "$trace" \
    | jq -e --argjson c "$1" --argjson p "$2" 'select(.id == $c and (.edges | index($p)))' > /dev/null
}

libEntry=$(entryId import /lib.nix)
aEntry=$(entryId import /a.nix)
bEntry=$(entryId import /b.nix)
for v in libEntry aEntry bEntry; do
  [[ -n "${!v}" ]] || { echo "no import entry for $v"; exit 1; }
done

hasEdge "$aEntry" "$libEntry" || {
  echo "a.nix has no edge to lib.nix, which it imported"; exit 1;
}
hasEdge "$bEntry" "$libEntry" || {
  echo "b.nix has no edge to lib.nix, so a result served from the file cache "
  echo "records no relationship to the entry that produced it"
  exit 1
}
reuseEdges=$(jsonField edge_reuse)
(( reuseEdges >= 1 )) || { echo "no edge of kind reuse, got $reuseEdges"; exit 1; }

# A derivation that consumes another derivation's output records an edge to it.
# This is the class the phase 1 measurement named as the gap: of the derivations
# one commit moves, one reads the edited bytes and the rest move only because
# their input derivations did.
drvEntry=$(grep '"t":"entry"' "$trace" | jq -r 'select(.kind == "derivation" and .key == "read-set-probe") | .id' | head -n 1)
drv2Entry=$(grep '"t":"entry"' "$trace" | jq -r 'select(.kind == "derivation" and .key == "read-set-probe-downstream") | .id' | head -n 1)
[[ -n "$drvEntry" && -n "$drv2Entry" ]] || { echo "missing one of the two derivation entries"; exit 1; }
hasEdge "$drv2Entry" "$drvEntry" || {
  echo "the downstream derivation has no edge to the derivation it consumes"; exit 1;
}
drvEdges=$(jsonField edge_derivation)
(( drvEdges >= 1 )) || { echo "no edge of kind derivation, got $drvEdges"; exit 1; }

# The other direction: a boundary entered for the first time while another was
# innermost. Nothing had imported default.nix before this expression did, so
# the root entered its boundary and the edge is the nesting rather than a reuse.
rootEntry=$(grep '"t":"entry"' "$trace" | jq -r 'select(.kind == "root") | .id' | head -n 1)
defEntry=$(entryId import /default.nix)
[[ -n "$rootEntry" && -n "$defEntry" ]] || { echo "missing the root or default.nix entry"; exit 1; }
hasEdge "$rootEntry" "$defEntry" || {
  echo "the root entry has no edge to default.nix, which it entered"; exit 1;
}
demandEdges=$(jsonField edge_demand)
(( demandEdges >= 1 )) || { echo "no edge of kind demand, got $demandEdges"; exit 1; }

# Every entry that demanded something records it, so the graph is not one edge
# wide. This is what fails if only some of the three recordings survive.
entriesWithEdges=$(jsonField entries_with_edges)
(( entriesWithEdges >= 4 )) || {
  echo "only $entriesWithEdges entries record any edge, so the graph is a stub"
  exit 1
}

# Turning position tracking off removes position inputs and nothing else, which
# is how the cost of tracking them is measured.
traceNoPos="$TEST_ROOT/read-set-trace-nopos.jsonl"
nix eval --impure --raw \
  --option read-set-trace-file "$traceNoPos" \
  --option read-set-track-positions false \
  --expr 'toString (import '"$PWD"'/default.nix).line' > /dev/null
if grep '"t":"in"' "$traceNoPos" | jq -e 'select(.kind == "position")' > /dev/null; then
  echo "position inputs recorded even though read-set-track-positions is false"
  exit 1
fi
grep '"t":"in"' "$traceNoPos" | jq -e 'select(.kind == "contents")' > /dev/null
