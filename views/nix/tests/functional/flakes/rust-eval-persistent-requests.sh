#!/usr/bin/env bash

source ./common.sh

work="$TEST_ROOT/persistent-requests"
mkdir -p "$work"
cat > "$work/root.nix" <<'NIX'
{
  value = 41;
  broken = throw "intentional persistent request failure";
}
NIX

jq -n '{version:1, requests:[
  {id:"first\nopaque",installable:"value",apply:"x: x + 1"},
  {id:"second",installable:"value",apply:"x: x * 2"},
  {id:"third",installable:"value",apply:null}
]}' > "$work/requests.json"
nix eval-persistent --builders '' --option eval-cache-dir "$work/cache" \
    --file "$work/root.nix" --request-file "$work/requests.json" \
    < /dev/null > "$work/results"
jq -s -e '
  map(.id) == ["first\nopaque","second","third"] and
  map(.value) == [42,82,41] and all(.version == 1 and .status == "ok")
' "$work/results"

# A source edit is observed by the same API used by request-file mode; returning
# to the previous revision may reuse a validated disk answer. The companion
# rust-eval-persistent.sh exercises edits between requests in one process.
sed -i 's/value = 41/value = 7/' "$work/root.nix"
nix eval-persistent --builders '' --option eval-cache-dir "$work/cache" \
    --file "$work/root.nix" --request-file "$work/requests.json" > "$work/edited"
jq -s -e 'map(.value) == [8,14,7]' "$work/edited"
sed -i 's/value = 7/value = 41/' "$work/root.nix"
nix eval-persistent --builders '' --option eval-cache-dir "$work/cache" \
    --file "$work/root.nix" --request-file "$work/requests.json" > "$work/returned"
jq -s -e 'map(.value) == [42,82,41]' "$work/returned"

# Application is a byte string, never shell syntax or a line-delimited request.
literal=$'"quotes" \'apostrophe\' $HOME $(touch never) `tick`\nnext'
jq -n --arg text "$literal" '{version:1,requests:[
  {id:"literal",installable:"value",apply:("x: " + ($text|tojson))}
]}' > "$work/literal.json"
nix eval-persistent --file "$work/root.nix" --request-file "$work/literal.json" > "$work/literal.out"
jq -e --arg text "$literal" '.value == $text' "$work/literal.out"

# No request may execute before complete validation, including a bad late row.
jq '.requests[2].unknown = true' "$work/requests.json" > "$work/invalid.json"
if nix eval-persistent --file "$work/absent.nix" --request-file "$work/invalid.json" \
    > "$work/invalid.out" 2> "$work/invalid.err"; then
    echo 'accepted an invalid request file'; exit 1
fi
test ! -s "$work/invalid.out"
grep -F 'unknown field' "$work/invalid.err"

jq '.requests[1].installable = "broken"' "$work/requests.json" > "$work/failing.json"
if nix eval-persistent --file "$work/root.nix" --request-file "$work/failing.json" \
    > "$work/failing.out" 2> "$work/failing.err"; then
    echo 'accepted a partially failed batch'; exit 1
fi
jq -s -e '
  length == 2 and .[0].status == "ok" and
  .[1].id == "second" and .[1].status == "error" and
  (.[1] | has("value") | not)
' "$work/failing.out"
grep -F 'intentional persistent request failure' "$work/failing.err"

for option in --interactive value; do
    if nix eval-persistent --file "$work/root.nix" --request-file "$work/requests.json" "$option" \
        > "$work/mixed.out" 2> "$work/mixed.err"; then
        echo 'accepted mixed request protocols'; exit 1
    fi
    test ! -s "$work/mixed.out"
done

# The file reader rejects special files without blocking or reading stdin.
mkfifo "$work/fifo"
if nix eval-persistent --request-file "$work/fifo" > "$work/fifo.out" 2> "$work/fifo.err"; then
    echo 'accepted a FIFO request file'; exit 1
fi
grep -F 'regular file' "$work/fifo.err"
truncate -s 4194305 "$work/oversize"
if nix eval-persistent --request-file "$work/oversize" > "$work/oversize.out" 2> "$work/oversize.err"; then
    echo 'accepted an oversized request file'; exit 1
fi
grep -F 'exceeds' "$work/oversize.err"
