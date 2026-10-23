#!/bin/bash
# Stability soak of the Zell HTTP server (benchmarks/web_api/server.zl, the
# `web` framework with JSON bodies): the Go load generator (go/load) runs in
# a loop against one server for DURATION seconds while the server's RSS is
# sampled into a CSV. The server exposes no diag metrics, so RSS only.
#
# Fails (exit 1) if: the server died, any request failed (non-matching
# status/response, load tool error), or RSS grew more than MAX_GROWTH_PCT
# between the maximum of the first quarter of the samples and the maximum of
# the last quarter.
#
# Usage: scripts/soak-http.sh
# Environment: ZELL (default target/release/zell), DURATION (3600 s),
#   SAMPLE (10 s), CONNS (64), BATCH (requests per load run, 20000),
#   MAX_GROWTH_PCT (20), CSV (output file, default soak-http.csv in the cwd).
set -u
command -v go >/dev/null || PATH=$PATH:/usr/local/go/bin
root=$(cd "$(dirname "$0")/.." && pwd)
here=$root/benchmarks/web_api
ZELL=${ZELL:-$root/target/release/zell}
[ -x "$ZELL" ] || ZELL=$(ls "$root"/../../../target/release/zell 2>/dev/null)
DURATION=${DURATION:-3600}
SAMPLE=${SAMPLE:-10}
CONNS=${CONNS:-64}
BATCH=${BATCH:-20000}
MAX_GROWTH_PCT=${MAX_GROWTH_PCT:-20}
CSV=${CSV:-soak-http.csv}
out=$(mktemp -d)
spid=
lpid=
cleanup() { kill $spid $lpid 2>/dev/null; wait 2>/dev/null; rm -rf "$out"; }
trap cleanup EXIT
unset HTTP_PROXY HTTPS_PROXY http_proxy https_proxy
export CONNS REQUESTS=$BATCH

"$ZELL" build "$here/server.zl" --release -o "$out/server" || exit 2
(cd "$here/go" && go build -o "$out/load" ./load) || exit 2

# Started in this shell (not in a $(...) subshell) and killed by PID.
"$out/server" > /dev/null 2> "$out/port" &
spid=$!
for _ in $(seq 100); do [ -s "$out/port" ] && break; sleep 0.05; done
port=$(head -n 1 "$out/port")
[ -n "$port" ] || { echo "soak: server did not report a port" >&2; exit 2; }
addr=127.0.0.1:$port

rss() { awk '/VmRSS/ { print $2 }' "/proc/$spid/status" 2>/dev/null; }
requests=0; errors=0; died=0
echo "timestamp,rss_kb,requests,errors" > "$CSV"
start=$SECONDS
next=$start
while (( SECONDS - start < DURATION )); do
    # One load batch in the background; sample while it runs.
    "$out/load" "$addr" > "$out/load.out" 2> "$out/load.err" &
    lpid=$!
    while kill -0 $lpid 2>/dev/null; do
        if (( SECONDS >= next )); then
            r=$(rss)
            if [ -z "$r" ]; then died=1; break 2; fi
            echo "$(date +%s),$r,$requests,$errors" >> "$CSV"
            next=$((next + SAMPLE))
        fi
        sleep 0.2
    done
    wait $lpid; rc=$?
    lpid=
    n=$(awk '/^requests/ { print $2 }' "$out/load.out")
    ok=$(awk '/^requests/ { print $4 }' "$out/load.out")
    if [ -z "$n" ] || [ $rc -ne 0 ]; then
        errors=$((errors + 1)); echo "soak: load run failed (rc=$rc): $(tail -n 2 "$out/load.err" | tr '\n' ' ')" >&2
        kill -0 $spid 2>/dev/null || { died=1; break; }
        continue
    fi
    requests=$((requests + n)); errors=$((errors + n - ok))
done
if (( died == 0 )) && ! kill -0 $spid 2>/dev/null; then died=1; fi
r=$(rss); [ -n "$r" ] && echo "$(date +%s),$r,$requests,$errors" >> "$CSV"

fail=0
[ $died -eq 1 ] && { echo "FAIL: the server died"; fail=1; }
[ $errors -gt 0 ] && { echo "FAIL: $errors failed requests/runs"; fail=1; }
read -r min max last first_q last_q cnt < <(tail -n +2 "$CSV" | awk -F, '
    { v[NR] = $2 }
    END {
        if (NR == 0) { print "0 0 0 0 0 0"; exit }
        min = v[1]; max = v[1]
        for (i = 1; i <= NR; i++) { if (v[i] < min) min = v[i]; if (v[i] > max) max = v[i] }
        q = int(NR / 4); if (q < 1) q = 1
        f = 0; for (i = 1; i <= q; i++) if (v[i] > f) f = v[i]
        l = 0; for (i = NR - q + 1; i <= NR; i++) if (v[i] > l) l = v[i]
        print min, max, v[NR], f, l, NR
    }')
echo "soak: $cnt samples, $requests requests, $errors errors; RSS kB min $min max $max last $last; first-quarter max $first_q, last-quarter max $last_q"
if [ "$cnt" -lt 4 ]; then
    echo "FAIL: too few samples ($cnt) to judge growth"; fail=1
elif (( last_q * 100 > first_q * (100 + MAX_GROWTH_PCT) )); then
    echo "FAIL: RSS grew $(( (last_q - first_q) * 100 / first_q ))% (limit $MAX_GROWTH_PCT%)"; fail=1
fi
[ $fail -eq 0 ] && echo "soak: PASS"
exit $fail
