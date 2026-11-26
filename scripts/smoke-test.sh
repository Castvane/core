#!/usr/bin/env bash
set -euo pipefail

binary=${1:?usage: smoke-test.sh path/to/binary}
work_dir=$(mktemp -d)
pid=
cleanup() {
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        kill -KILL "$pid" 2>/dev/null || true
        wait "$pid" 2>/dev/null || true
    fi
    rm -f -- "$work_dir/server.log" "$work_dir/body" "$work_dir/test.sqlite3" "$work_dir/test.sqlite3-shm" "$work_dir/test.sqlite3-wal"
    rmdir -- "$work_dir"
}
trap cleanup EXIT

: >"$work_dir/server.log"
CORE_BIND_ADDR=127.0.0.1:0 CORE_DATABASE_PATH="$work_dir/test.sqlite3" RUST_LOG=info "$binary" >"$work_dir/server.log" 2>&1 &
pid=$!

# Discover the ephemeral port from the startup log; avoid collisions in parallel CI.
address=
for _ in {1..100}; do
    address=$(sed -nE 's/.*address=([0-9.]+:[0-9]+).*/\1/p' "$work_dir/server.log" | head -n 1)
    if [[ -n "$address" ]]; then break; fi
    if ! kill -0 "$pid" 2>/dev/null; then break; fi
    sleep 0.1
done
if [[ -z "$address" ]]; then
    cat "$work_dir/server.log" >&2
    exit 1
fi

status=$(curl --silent --show-error --max-time 5 --output "$work_dir/body" --write-out '%{http_code}' "http://$address/ping")
[[ "$status" == 200 && $(<"$work_dir/body") == pong ]]
status=$(curl --silent --show-error --max-time 5 --output "$work_dir/body" --write-out '%{http_code}' "http://$address/api")
[[ "$status" == 200 && $(<"$work_dir/body") == *'"app":"datarhei-core"'* ]]
status=$(curl --silent --show-error --max-time 5 --output "$work_dir/body" --write-out '%{http_code}' "http://$address/api/v3/config")
[[ "$status" == 200 && $(<"$work_dir/body") == *'"version":3'* ]]
status=$(curl --silent --show-error --max-time 5 --output "$work_dir/body" --write-out '%{http_code}' "http://$address/api/v3/process")
[[ "$status" == 200 && $(<"$work_dir/body") == "[]" ]]

kill -TERM "$pid"
for _ in {1..100}; do
    if ! kill -0 "$pid" 2>/dev/null; then
        wait "$pid"
        pid=
        echo 'Smoke test passed: ping, discovery, config, process list, graceful SIGTERM.'
        exit 0
    fi
    sleep 0.1
done
echo 'Server did not exit after SIGTERM.' >&2
exit 1
