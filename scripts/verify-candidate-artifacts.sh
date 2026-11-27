#!/usr/bin/env bash
set -euo pipefail

root=${1:?expected directory containing the candidate archives}
repository=${2:?expected source repository}
revision=${3:?expected source revision}
version=${4:?expected candidate version}

[[ "$revision" =~ ^[0-9a-f]{40}$ && "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+-rc\.[0-9]+$ ]]
verify_archive() {
    local archive=$1 predicate
    sha256sum -c "$archive.sha256"
    for predicate in https://slsa.dev/provenance/v1 https://cyclonedx.org/bom; do
        gh attestation verify "$archive" --repo "$repository" --source-digest "$revision" \
            --signer-workflow "$repository/.github/workflows/candidates.yml" --predicate-type "$predicate"
    done
}

for arch in amd64 arm64; do
    (
        cd "$root/castvane-core-linux-$arch"
        native="castvane-core-$version-linux-$arch.tar.gz"
        container=castvane-core-container.tar.gz
        jq -e --arg version "$version" --arg revision "$revision" \
            '.version == $version and .commit == $revision' native-build.json >/dev/null
        jq -e --arg version "$version" --arg revision "$revision" --arg arch "$arch" \
            '.[0] | .Architecture == $arch and .Config.Labels["org.opencontainers.image.version"] == $version
             and .Config.Labels["org.opencontainers.image.revision"] == $revision' container-image.json >/dev/null
        verify_archive "$native"
        verify_archive "$container"
        printf '%s archive sizes in bytes\n' "$arch"
        wc -c "$native" "$container"
    )
done

(
    cd "$root/castvane-core-windows-amd64"
    native="castvane-core-$version-windows-amd64.zip"
    jq -e --arg version "$version" --arg revision "$revision" --arg archive "$native" \
        '.version == $version and .commit == $revision and .archive == $archive
         and .target == "x86_64-pc-windows-msvc"' native-build.json >/dev/null
    verify_archive "$native"
    wc -c "$native"
)
