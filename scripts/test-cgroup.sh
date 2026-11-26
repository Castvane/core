#!/usr/bin/env bash
set -euo pipefail

root=$(realpath "${1:?usage: scripts/test-cgroup.sh DELEGATED_CGROUP_V2_DIRECTORY}")
if [[ $(stat -f -c %T "$root") != cgroup2fs || ! -w "$root" || ! -w "$root/cgroup.procs" ]] \
    || ! grep -qw cpu "$root/cgroup.subtree_control" \
    || ! grep -qw memory "$root/cgroup.subtree_control"; then
    echo "The requested cgroup must be writable and delegate both cpu and memory." >&2
    exit 1
fi
CASTVANE_TEST_CGROUP_ROOT="$root" cargo test --locked --test process_engine \
    kernel_limits_attach_and_remove_an_ffmpeg_cgroup -- --exact --ignored --nocapture
