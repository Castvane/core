#!/usr/bin/env bash
set -euo pipefail

reference=$(realpath "${1:?usage: scripts/test-differential.sh GO_REFERENCE CASTVANE_BINARY}")
core=$(realpath "${2:?provide the Castvane executable}")
scripts=$(cd "$(dirname "$0")" && pwd)
work=$(mktemp -d)
go_pid=
rust_pid=
cleanup() {
    for pid in "$go_pid" "$rust_pid"; do
        if [[ -n "$pid" ]]; then
            kill -TERM "$pid" 2>/dev/null || true
            wait "$pid" || true
        fi
    done
    rm -rf -- "$work"
}
trap cleanup EXIT
mkdir -p "$work/go/config" "$work/go/data" "$work/rust/data"
read -r go_port rust_port < <(python3 - <<'PY'
import socket

with socket.socket() as left, socket.socket() as right:
    for listener in (left, right):
        listener.bind(("127.0.0.1", 0))
    print(left.getsockname()[1], right.getsockname()[1])
PY
)

start() {
    local directory=$1 binary=$2 port=$3
    cd "$directory"
    exec env -i PATH="$PATH" HOME="$directory" NO_PROXY=127.0.0.1         CORE_ADDRESS="127.0.0.1:$port" CORE_STORAGE_DISK_DIR="$directory/data"         CORE_API_AUTH_ENABLE=true CORE_API_AUTH_DISABLE_LOCALHOST=false         CORE_API_AUTH_USERNAME=fixture CORE_API_AUTH_PASSWORD=differential-test-fixture         CORE_HOST_AUTO=false CORE_UPDATE_CHECK=false CORE_METRICS_ENABLE=true RUST_LOG=warn         "$binary"
}
(start "$work/go" "$reference" "$go_port") > "$work/go.log" 2>&1 &
go_pid=$!
(start "$work/rust" "$core" "$rust_port") > "$work/rust.log" 2>&1 &
rust_pid=$!
for port in "$go_port" "$rust_port"; do
    for _ in {1..60}; do
        if [[ $(curl --noproxy '*' --silent --max-time 1 "http://127.0.0.1:$port/ping") == pong ]]; then
            break
        fi
        sleep 0.25
    done
    [[ $(curl --noproxy '*' --fail --silent --max-time 2 "http://127.0.0.1:$port/ping") == pong ]] || {
        echo "A differential fixture did not start on port $port" >&2
        exit 1
    }
done
export PHASE1_TEST_USERNAME=fixture PHASE1_TEST_PASSWORD=differential-test-fixture
export NO_PROXY=127.0.0.1 no_proxy=127.0.0.1
status=0
for suite in ops process media api; do
    python3 "$scripts/differential-$suite.py" "http://127.0.0.1:$go_port" "http://127.0.0.1:$rust_port" || status=1
done
exit "$status"
