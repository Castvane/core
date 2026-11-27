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
read -r executable version commit < <("$binary" --version)
[[ "$executable" == restreamer-core && "$version" =~ ^[0-9] ]]
commit=${commit#(}
commit=${commit%)}
docker build --build-arg "VERSION=$version" --build-arg "REVISION=$commit" \
    --tag "$image" --file "$root/packaging/Dockerfile" "$work"
container=$(docker run --detach --read-only --tmpfs /tmp:rw,nosuid,nodev,noexec,size=32m --mount type=volume,dst=/var/lib/castvane --publish 127.0.0.1::8080     --env CORE_API_AUTH_ENABLE=true --env CORE_API_AUTH_PASSWORD=candidate-fixture-password "$image")
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
docker exec "$container" sh -ec '! command -v ffmpeg && ! command -v ffprobe'
[[ $(curl --noproxy '*' --silent --max-time 3 -o "$work/readiness" -w '%{http_code}' "$url/readyz") == 503 ]]
docker exec "$container" restreamer-core health-check
if [[ -n "$output" ]]; then
    mkdir -p "$output"
    for item in runtime-packages.tsv ffmpeg-build.txt; do
        docker cp "$container:/usr/share/doc/castvane-core/$item" "$output/$item"
    done
    docker image inspect "$image" > "$output/container-image.json"
fi
docker stop --time 15 "$container" >/dev/null
[[ $(docker inspect --format '{{.State.ExitCode}}' "$container") == 0 ]]
docker rm -v "$container" > /dev/null
container=
if [[ -n "$output" ]]; then
    docker save "$image" | gzip -n > "$output/castvane-core-container.tar.gz"
    (cd "$output" && sha256sum castvane-core-container.tar.gz > castvane-core-container.tar.gz.sha256)
    python3 "$root/scripts/container-sbom.py" "$output" "$version"
fi
