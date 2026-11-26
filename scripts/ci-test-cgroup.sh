#!/usr/bin/env bash
set -euo pipefail

unit="castvane-cgroup-fixture-${GITHUB_RUN_ID:?}.service"
cleanup() {
    sudo systemctl stop "$unit"
}
sudo systemd-run --quiet --collect --unit="$unit" --service-type=exec \
    --property="Delegate=cpu memory" --property=DelegateSubgroup=fixture-runner \
    /usr/bin/sleep infinity
trap cleanup EXIT
group=$(sudo systemctl show --property=ControlGroup --value "$unit")
[[ "$group" == /*/"$unit" ]]
root="/sys/fs/cgroup$group"
[[ -d "$root/fixture-runner" ]]
sudo bash -eu -c '
    root=$1
    user=$2
    owner=$3
    shift 3
    printf "+cpu +memory\n" > "$root/cgroup.subtree_control"
    chown "$owner" "$root" "$root/cgroup.procs" "$root/cgroup.subtree_control"
    printf "%s\n" "$$" > "$root/fixture-runner/cgroup.procs"
    exec runuser -u "$user" -- env "$@" bash scripts/test-cgroup.sh "$root"
' _ "$root" "$(id -un)" "$(id -u):$(id -g)" "PATH=$PATH" "HOME=$HOME" \
    "CARGO_HOME=${CARGO_HOME:-$HOME/.cargo}" "RUSTUP_HOME=${RUSTUP_HOME:-$HOME/.rustup}"
