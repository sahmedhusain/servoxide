#!/usr/bin/env bash
# Exhaustive functional tests for the Localhost server.
# Run from the `lc` directory:  ./tests/run_tests.sh
#
# Starts the server with config/default.conf, exercises every audit case, then
# runs the configuration-error checks. Prints PASS/FAIL per case.

set -u
cd "$(dirname "$0")/.." || exit 1

BASE="http://127.0.0.1:8080"
PASS=0
FAIL=0
SERVER_PID=""

cleanup() { [ -n "$SERVER_PID" ] && kill "$SERVER_PID" 2>/dev/null; }
trap cleanup EXIT

ok()   { echo "  PASS: $1"; PASS=$((PASS+1)); }
bad()  { echo "  FAIL: $1 (got: $2)"; FAIL=$((FAIL+1)); }

# check_status <name> <expected-code> <curl-args...>
check_status() {
  local name="$1" want="$2"; shift 2
  local got; got=$(curl -s -o /dev/null -w "%{http_code}" "$@")
  [ "$got" = "$want" ] && ok "$name" || bad "$name" "$got"
}

# check_body_contains <name> <needle> <curl-args...>
check_body_contains() {
  local name="$1" needle="$2"; shift 2
  local body; body=$(curl -s "$@")
  echo "$body" | grep -q "$needle" && ok "$name" || bad "$name" "no '$needle'"
}

# check_header <name> <header-regex> <curl-args...>
check_header() {
  local name="$1" rx="$2"; shift 2
  local hdrs; hdrs=$(curl -s -D - -o /dev/null "$@")
  echo "$hdrs" | grep -iqE "$rx" && ok "$name" || bad "$name" "no header $rx"
}

echo "== building =="
cargo build --quiet || { echo "build failed"; exit 1; }

echo "== starting server =="
pkill -f target/debug/localhost 2>/dev/null
sleep 0.5
rm -f www/uploads/*
./target/debug/localhost config/default.conf >/tmp/lc_test.log 2>&1 &
SERVER_PID=$!
sleep 1

echo "== methods & status codes =="
check_status      "GET / -> 200"                 200 "$BASE/"
check_status      "GET missing -> 404"           404 "$BASE/nope"
check_body_contains "404 serves custom page"     "not here" "$BASE/nope"
check_status      "DELETE on GET-only -> 405"    405 -X DELETE "$BASE/"
check_header      "405 includes Allow"           "^Allow:" -X DELETE "$BASE/"
check_status      "wrong method -> 501 or 405"   405 -X PATCH "$BASE/"

echo "== body limit =="
check_status      "2MB body -> 413"              413 -X POST --data-binary @<(head -c 2000000 /dev/zero) "$BASE/uploads/big"

echo "== bad request =="
RAW=$(printf 'GET /\r\n\r\n' | nc -w1 127.0.0.1 8080 | head -1)
echo "$RAW" | grep -q "400" && ok "malformed -> 400" || bad "malformed -> 400" "$RAW"

echo "== redirect =="
check_status      "GET /old -> 301"              301 "$BASE/old"
check_header      "redirect has Location"        "^Location:" "$BASE/old"

echo "== static, autoindex, directory =="
check_header      "static Content-Type html"     "Content-Type: text/html" "$BASE/"
check_body_contains "autoindex lists files"      "Index of" "$BASE/listing/"

echo "== upload round-trip =="
check_status      "POST upload -> 201"           201 -X POST --data "roundtrip-data" "$BASE/uploads/rt.txt"
check_body_contains "GET uploaded file"          "roundtrip-data" "$BASE/uploads/rt.txt"
check_status      "DELETE uploaded -> 204"       204 -X DELETE "$BASE/uploads/rt.txt"
check_status      "GET deleted -> 404"           404 "$BASE/uploads/rt.txt"

echo "== chunked =="
check_body_contains "chunked POST echoed by CGI" "chunky" \
  -X POST -H "Transfer-Encoding: chunked" --data-binary "chunky" "$BASE/cgi/hello.py"

echo "== CGI =="
check_body_contains "CGI GET query string"       "QUERY_STRING: a=1" "$BASE/cgi/hello.py?a=1"
check_body_contains "CGI POST body"              "BODY: cgipost" -X POST --data "cgipost" "$BASE/cgi/hello.py"

echo "== traversal safety =="
check_status      "path traversal -> 403"        403 --path-as-is "$BASE/../../etc/passwd"

echo "== cookies / sessions =="
rm -f /tmp/lc_jar
check_header      "first visit Set-Cookie"       "^Set-Cookie: session_id=" -c /tmp/lc_jar "$BASE/"
V2=$(curl -s -D - -o /dev/null -b /tmp/lc_jar "$BASE/" | grep -i "X-Session-Visits" | tr -dc '0-9')
[ "$V2" = "2" ] && ok "session visit counter increments" || bad "session counter" "$V2"
rm -f /tmp/lc_jar

echo "== virtual hosts =="
check_body_contains "Host: example.com vhost"    "example.com virtual host" -H "Host: example.com" "$BASE/"
check_body_contains "unknown Host -> default"    "Localhost is serving" -H "Host: zzz.invalid" "$BASE/"

kill "$SERVER_PID" 2>/dev/null; SERVER_PID=""
sleep 0.3

echo "== configuration error handling =="
DUP=$(./target/debug/localhost tests/configs/duplicate_port.conf 2>&1 & sleep 0.6; kill %1 2>/dev/null)
echo "$DUP" | grep -qi "duplicate binding" && ok "duplicate port detected" || bad "duplicate port" "$DUP"

# Partially broken config: bad server dropped, good one (port 8091) serves.
./target/debug/localhost tests/configs/partially_broken.conf >/tmp/lc_pb.log 2>&1 &
SERVER_PID=$!
sleep 1
grep -qi "invalid port" /tmp/lc_pb.log && ok "broken server logged" || bad "broken server log" "$(cat /tmp/lc_pb.log)"
check_status      "valid server still serves (8091)" 200 "http://127.0.0.1:8091/"
kill "$SERVER_PID" 2>/dev/null; SERVER_PID=""

echo
echo "==================================="
echo " PASS: $PASS    FAIL: $FAIL"
echo "==================================="
[ "$FAIL" -eq 0 ]
