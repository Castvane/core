#!/usr/bin/env bash
set -euo pipefail

case "$(uname -m)" in
    x86_64)
        arch=amd64
        mtx_digest=be0c05d9c8af8338116977e7f1c3fcb409fbf882075e5ab075cf8baad76a59d2
        weed_digest=f156c609eeaf9e5fcfeea33fd70411222005f83760aa95893c9d58eff006b45c
        ;;
    aarch64)
        arch=arm64
        mtx_digest=47784081da54c0608ea4dc359f5cfb19880209434769c532b3ed3a67ea99c29d
        weed_digest=ced87c09a2a5e87c7c94fd25e36c421670b97fded064026738a69f13ee8c7b96
        ;;
    *) exit 1 ;;
esac

test_dir=$(mktemp -d)
weed_pid=
cleanup() {
    if [[ -n "$weed_pid" ]]; then
        kill -KILL "$weed_pid" 2>/dev/null || true
        wait "$weed_pid" 2>/dev/null || true
    fi
    rm -rf -- "$test_dir"
}
trap cleanup EXIT
download() {
    local url=$1 digest=$2 file=$3
    curl --fail --silent --show-error --location --retry 3 "$url" -o "$test_dir/fixture.tar.gz"
    printf '%s  %s\n' "$digest" "$test_dir/fixture.tar.gz" | sha256sum --check
    tar -xzf "$test_dir/fixture.tar.gz" -C "$test_dir" "$file"
}

if [[ -z "${PHASE3_MEDIAMTX_BINARY:-}" ]]; then
    download "https://github.com/bluenviron/mediamtx/releases/download/v1.21.4/mediamtx_v1.21.4_linux_$arch.tar.gz" "$mtx_digest" mediamtx
    export PHASE3_MEDIAMTX_BINARY="$test_dir/mediamtx"
fi
if [[ -z "${PHASE3_S3_ENDPOINT:-}" ]]; then
    if [[ -z "${PHASE3_WEED_BINARY:-}" ]]; then
        download "https://github.com/seaweedfs/seaweedfs/releases/download/4.55/linux_$arch.tar.gz" "$weed_digest" weed
        PHASE3_WEED_BINARY="$test_dir/weed"
    fi
    mkdir "$test_dir/objects"
    cat > "$test_dir/iam.json" <<'JSON'
{"identities":[{"name":"fixture","credentials":[{"accessKey":"fixture-key","secretKey":"fixture-secret"}],"actions":["Admin","Read","Write","List","Tagging"]}]}
JSON
    read -r master volume filer s3 admin < <(python3 - <<'PYPORT'
import random
import socket
from contextlib import ExitStack, closing

for base in random.sample(range(17000, 32000), 256):
    ports = [base + i for i in range(5)]
    with ExitStack() as stack:
        try:
            for port in ports + [port + 10000 for port in ports]:
                sock = stack.enter_context(closing(socket.socket()))
                sock.bind(("127.0.0.1", port))
        except OSError:
            continue
        print(*ports)
        break
else:
    raise SystemExit("could not allocate local media test ports")
PYPORT
)
    "$PHASE3_WEED_BINARY" mini -ip=127.0.0.1 -ip.bind=127.0.0.1 -dir="$test_dir/objects" \
        -master.port="$master" -volume.port="$volume" -filer.port="$filer" -s3.port="$s3" \
        -admin.port="$admin" -admin.port.legacy=0 -bucket=phase3 \
        -s3.config="$test_dir/iam.json" > "$test_dir/seaweed.log" 2>&1 &
    weed_pid=$!
    export PHASE3_S3_ENDPOINT="http://127.0.0.1:$s3"
    ready=false
    for _ in {1..30}; do
        if curl --fail --silent --max-time 1 --aws-sigv4 'aws:amz:us-east-1:s3' \
            --user 'fixture-key:fixture-secret' "$PHASE3_S3_ENDPOINT/phase3" > /dev/null; then
            ready=true
            break
        fi
        sleep 1
    done
    if [[ "$ready" != true ]]; then
        cat "$test_dir/seaweed.log" >&2
        exit 1
    fi
fi
cargo test --locked --test media -- --ignored --nocapture --skip benchmark_http_media
