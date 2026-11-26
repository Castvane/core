#!/usr/bin/env bash
set -euo pipefail

binary=$(realpath "${1:?usage: scripts/test-container.sh CORE_BINARY [OUTPUT_DIRECTORY]}")
output=${2:-}
root=$(cd "$(dirname "$0")/.." && pwd)
work=$(mktemp -d)
container=
image=
cleanup() {
    [[ -z "$container" ]] || docker rm -f -v "$container" > /dev/null 2>&1 || true
    [[ -z "$image" ]] || docker image rm "$image" > /dev/null 2>&1 || true
    rm -rf -- "$work"
}
trap cleanup EXIT
image="castvane-core:candidate-${work##*.}"
install -m 755 "$binary" "$work/restreamer-core"
cp "$root/LICENSE" "$work/"
docker build --tag "$image" --file "$root/packaging/Dockerfile" "$work"
container=$(docker run --detach --publish 127.0.0.1::8080     --env CORE_API_AUTH_ENABLE=true --env CORE_API_AUTH_PASSWORD=candidate-fixture-password "$image")
url="http://$(docker port "$container" 8080/tcp)"
for _ in {1..40}; do
    if [[ $(curl --noproxy '*' --silent --max-time 1 "$url/ping") == pong ]]; then
        break
    fi
    sleep 0.25
done
if [[ $(curl --noproxy '*' --fail --silent --max-time 3 "$url/ping") != pong ]]; then
    docker logs "$container" >&2
    exit 1
fi
[[ $(curl --noproxy '*' --silent --max-time 3 -o "$work/readiness" -w '%{http_code}' "$url/readyz") == 503 ]]
python3 - "$work/readiness" <<'PY'
import json
import sys
from pathlib import Path
readiness = json.loads(Path(sys.argv[1]).read_text())
assert readiness["database"] is True and readiness["ffmpeg"] is False, readiness
PY
if [[ -n "$output" ]]; then
    mkdir -p "$output"
    docker exec "$container" dpkg-query -W '--showformat=${Package}\t${Version}\n' \
        > "$output/container-packages.tsv"
fi
docker stop --time 15 "$container" >/dev/null
[[ $(docker inspect --format '{{.State.ExitCode}}' "$container") == 0 ]]
docker rm -v "$container" > /dev/null
container=
if [[ -n "$output" ]]; then
    docker save "$image" | gzip -n > "$output/castvane-core-container.tar.gz"
    (cd "$output" && sha256sum castvane-core-container.tar.gz > castvane-core-container.tar.gz.sha256)
fi
