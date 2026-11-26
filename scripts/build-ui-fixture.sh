#!/usr/bin/env bash
set -euo pipefail

pin=${1:?usage: scripts/build-ui-fixture.sh PIN OUTPUT_DIRECTORY}
output=${2:?specify a new output directory}
output=$(realpath -m "$output")
[[ ! -e "$output" ]] || { echo "Output already exists: $output" >&2; exit 1; }

repository=https://github.com/datarhei/restreamer-ui.git
case "$pin" in
    v1.0.0) revision=07996860851d6906711260cb077cb3f2f03158ff ;;
    v1.8.0) revision=3d9d663e35005b9aa2a5811fadb577eb70181100 ;;
    v1.14.0) revision=c1a9c715a4a2947fe84f789e9126e02955b0fcd7 ;;
    upstream) revision=7164b38cb3a8655f7cd896e941093a1ca166b19b ;;
    owner)
        revision=69feb33cedc8a93754cc0c8d7895221cfe16803f
        repository=https://github.com/KingIronMan2011/restreamer-ui.git
        ;;
    *) echo "Unknown UI pin: $pin" >&2; exit 2 ;;
esac
work=$(mktemp -d)
trap 'rm -rf -- "$work"' EXIT
git init --quiet "$work/src"
git -C "$work/src" fetch --quiet --depth 1 "$repository" "$revision"
git -C "$work/src" checkout --quiet --detach FETCH_HEAD
[[ $(git -C "$work/src" rev-parse HEAD) == "$revision" ]]
cd "$work/src"
if [[ -f pnpm-lock.yaml ]]; then
    corepack pnpm install --frozen-lockfile --store-dir "$work/pnpm-store"
    corepack pnpm build
    bundle=dist
else
    corepack yarn@1.22.22 install --frozen-lockfile --non-interactive --cache-folder "$work/yarn-cache"
    PUBLIC_URL=./ GENERATE_SOURCEMAP=false NODE_OPTIONS="${NODE_OPTIONS:---openssl-legacy-provider}"         corepack yarn@1.22.22 build
    bundle=build
fi
[[ -f "$bundle/index.html" ]]
mkdir -p "$(dirname "$output")"
mv -- "$bundle" "$output"
printf '%s\n' "$revision" > "$output/.fixture-revision"
