#!/usr/bin/env bash
set -euo pipefail

image=${1:?expected registry and repository}
version=${2:?expected release candidate version}
revision=${3:?expected commit}
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+-rc\.[0-9]+$ && "$revision" =~ ^[a-f0-9]{40}$ ]]

if existing=$(docker buildx imagetools inspect "$image:$version" 2>&1); then
    echo "The requested version already exists in the registry" >&2
    exit 1
elif ! grep -qiE 'not found|manifest unknown|name unknown' <<< "$existing"; then
    echo "Unable to verify that the requested registry tag is available" >&2
    exit 1
fi

sources=()
for arch in amd64 arm64; do
    directory="candidates/castvane-core-linux-$arch"
    local_id=$(jq -er '.[0].Id' "$directory/container-image.json")
    docker load --input "$directory/castvane-core-container.tar.gz" >&2
    [[ $(docker image inspect --format '{{.Architecture}}' "$local_id") == "$arch" ]]
    [[ $(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.revision"}}' "$local_id") == "$revision" ]]
    [[ $(docker image inspect --format '{{index .Config.Labels "org.opencontainers.image.version"}}' "$local_id") == "$version" ]]
    staging="$image:build-${GITHUB_RUN_ID:?}-${GITHUB_RUN_ATTEMPT:?}-$arch"
    docker tag "$local_id" "$staging"
    result=$(docker push "$staging")
    digest=$(sed -nE 's/^.*digest: (sha256:[a-f0-9]{64}) size:.*$/\1/p' <<< "$result" | tail -n 1)
    [[ "$digest" =~ ^sha256:[a-f0-9]{64}$ ]]
    sources+=("$image@$digest")
done
docker buildx imagetools create --tag "$image:$version" "${sources[@]}" >&2
docker buildx imagetools inspect "$image:$version"
