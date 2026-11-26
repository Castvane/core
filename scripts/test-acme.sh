#!/usr/bin/env bash
set -euo pipefail

binary=$(realpath "${1:?usage: scripts/test-acme.sh executable}")
temp=$(mktemp -d)
core_pid=
acme_pid=
dns_pid=
cleanup() {
    for pid in "$core_pid" "$acme_pid" "$dns_pid"; do
        if [[ -n "$pid" ]]; then
            kill -KILL "$pid" 2>/dev/null || true
            wait "$pid" 2>/dev/null || true
        fi
    done
    rm -rf -- "$temp"
}
trap cleanup EXIT

if [[ -z "${PHASE4_PEBBLE:-}" || -z "${PHASE4_CHALLTESTSRV:-}" ]]; then
    case "$(uname -m)" in
        x86_64)
            arch=amd64
            issuer_sha=7e33cf3e426b67e9e647c44e8cc91683caa8ab331f7a99e5e27fcee291c4c7e2
            dns_sha=00b2c5ef525cd8ad56cda898a7cac6169a10b0fd3f2a7456fe0cec92e7b36cb6
            ;;
        aarch64)
            arch=arm64
            issuer_sha=1dc8fa9b1dd68529eb62fe685d3a40d5929cea8a17fefbff0940aed0901a1a47
            dns_sha=3e8af6b09afbe06f8274caeeabccf4972fd433226d6592e6cc7bf90006cc1982
            ;;
        *) exit 1 ;;
    esac
    unpack() {
        local exe=$1 digest=$2 asset="$1-linux-$arch"
        curl --fail --silent --show-error --location --retry 3 \
            "https://github.com/letsencrypt/pebble/releases/download/v2.10.3/$asset.tar.gz" -o "$temp/pebble.tar.gz"
        printf '%s  %s\n' "$digest" "$temp/pebble.tar.gz" | sha256sum --check
        tar -xzf "$temp/pebble.tar.gz" -C "$temp" "$asset/linux/$arch/$exe"
        install -m 700 "$temp/$asset/linux/$arch/$exe" "$temp/$exe"
    }
    unpack pebble "$issuer_sha"
    unpack pebble-challtestsrv "$dns_sha"
    PHASE4_PEBBLE="$temp/pebble"
    PHASE4_CHALLTESTSRV="$temp/pebble-challtestsrv"
fi

read -r directory_port root_port http_port tls_port control_port dns_port < <(python3 - <<'PY'
import socket
from contextlib import ExitStack, closing

with ExitStack() as stack:
    result = []
    for kind in [socket.SOCK_STREAM] * 5 + [socket.SOCK_DGRAM]:
        listener = stack.enter_context(closing(socket.socket(type=kind)))
        listener.bind(("127.0.0.1", 0))
        result.append(listener.getsockname()[1])
    print(*result)
PY
)
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:P-256 -noenc \
    -subj /CN=CastvaneFixtureAuthority -days 1 -keyout "$temp/ca.key" -out "$temp/ca.pem" 2>/dev/null
openssl req -new -newkey ec -pkeyopt ec_paramgen_curve:P-256 -noenc \
    -subj /CN=PebbleFixture -keyout "$temp/server.key" -out "$temp/server.csr" 2>/dev/null
printf 'subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth\n' > "$temp/extensions"
openssl x509 -req -days 1 -in "$temp/server.csr" -CA "$temp/ca.pem" -CAkey "$temp/ca.key" \
    -CAcreateserial -extfile "$temp/extensions" -out "$temp/server.pem" 2>/dev/null

python3 - "$temp" "$directory_port" "$root_port" "$http_port" "$tls_port" <<'PY'
import json
import pathlib
import sys

home = pathlib.Path(sys.argv[1])
directory, management, http, tls = map(int, sys.argv[2:])
with (home / "pebble.json").open("w") as out:
    json.dump({"pebble": {
        "listenAddress": f"127.0.0.1:{directory}",
        "managementListenAddress": f"127.0.0.1:{management}",
        "certificate": str(home / "server.pem"),
        "privateKey": str(home / "server.key"),
        "httpPort": http,
        "tlsPort": tls,
        "domainBlocklist": [],
        "retryAfter": {"authz": 1, "order": 1},
    }}, out)
PY

"$PHASE4_CHALLTESTSRV" -defaultIPv4 127.0.0.1 -defaultIPv6 "" \
    -dnsserver "127.0.0.1:$dns_port" -management "127.0.0.1:$control_port" \
    -http01 "" -https01 "" -doh "" -tlsalpn01 "" > "$temp/dns.log" 2>&1 &
dns_pid=$!
PEBBLE_VA_NOSLEEP=1 PEBBLE_WFE_NONCEREJECT=0 \
    "$PHASE4_PEBBLE" -config "$temp/pebble.json" -dnsserver "127.0.0.1:$dns_port" > "$temp/acme.log" 2>&1 &
acme_pid=$!
ready=false
for _ in {1..50}; do
    if curl --silent --fail --max-time 1 --cacert "$temp/ca.pem" \
        "https://127.0.0.1:$root_port/roots/0" > "$temp/issuer.pem"; then
        ready=true
        break
    fi
    sleep 0.2
done
[[ "$ready" == true ]] || { cat "$temp/acme.log" "$temp/dns.log" >&2; exit 1; }

start_core() {
    CORE_BIND_ADDR="127.0.0.1:$http_port" CORE_DATABASE_PATH="$temp/node.sqlite" \
        CORE_STORAGE_DISK_DIR="$temp/media" CORE_HOST_NAME=castvane.test CORE_HOST_AUTO=false \
        CORE_TLS_ENABLE=true CORE_TLS_AUTO=true CORE_TLS_STAGING=true CORE_TLS_ADDRESS="127.0.0.1:$tls_port" \
        CORE_TLS_ACME_AGREE=true CORE_TLS_EMAIL=admin@castvane.test CORE_TLS_ACME_STORAGE="$temp/issued" \
        CORE_TLS_ACME_DIRECTORY="https://127.0.0.1:$directory_port/dir" CORE_TLS_ACME_CA_FILE="$temp/ca.pem" \
        "$binary" >> "$temp/core.log" 2>&1 &
    core_pid=$!
}
check_certificate() {
    local attempts=$1
    for ((i=0;i<attempts;i++)); do
        if [[ $(curl --silent --fail --noproxy '*' --max-time 1 --cacert "$temp/issuer.pem" \
            --resolve "castvane.test:$tls_port:127.0.0.1" "https://castvane.test:$tls_port/ping") == pong ]]; then
            return 0
        fi
        if ! kill -0 "$core_pid" 2>/dev/null; then break; fi
        sleep 0.25
    done
    cat "$temp/core.log" "$temp/acme.log" >&2
    return 1
}
start_core
check_certificate 200
[[ $(stat -c %a "$temp/issued/certificate.json") == 600 ]]
[[ $(stat -c %a "$temp/issued/custom.json") == 600 ]]
kill -TERM "$core_pid"
wait "$core_pid"
core_pid=
kill -KILL "$acme_pid"
wait "$acme_pid" 2>/dev/null || true
acme_pid=
start_core
check_certificate 50
kill -TERM "$core_pid"
wait "$core_pid"
core_pid=
printf 'ACME HTTP-01 issuance, trusted HTTPS, and offline certificate restart passed.\n'
