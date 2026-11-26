# Castvane Core deployment and recovery

## Native candidate

Build on a supported Linux AMD64 or ARM64 host. The project pins Rust in `rust-toolchain.toml`.

```sh
cargo build --locked --release
python3 scripts/package-release.py target/release/restreamer-core dist
(cd dist && sha256sum -c ./*.sha256)
```

GitHub's **Candidate artifacts** workflow builds and exercises the executable on native Ubuntu 24.04 runners and stores unsigned packages in the workflow run. The binaries depend on glibc. Use Ubuntu 24.04 or test the binary against the libc on your own distribution. Each tarball contains the executable, a Rust CycloneDX SBOM, the declared Cargo package license inventory, and notices found in the crates' sources.

Install FFmpeg and FFprobe separately. Use a dedicated unprivileged account and create private configuration and persistent media directories. Keep SQLite, ACME data, TLS keys, and the environment file outside the served media directory. Start with [the environment example](../examples/castvane.env). On first start inject a strong `CORE_API_AUTH_PASSWORD` from your service secret store. Keep API authentication enabled for non-loopback listeners.

## Container candidate

The candidate image contains Core and trusted root certificates. A compatible runtime must supply FFmpeg and FFprobe in `/opt/media-tools` or provide a derived image with those dependencies. MediaMTX is optional and required for native RTMP, RTMPS, and SRT. Match the executables and their shared libraries to the image architecture and Ubuntu 24.04 runtime. The image uses UID/GID 10001 and stores state in `/var/lib/castvane`.

```sh
image_context=$(mktemp -d)
cp target/release/restreamer-core LICENSE "$image_context/"
docker build -t castvane-core:local -f packaging/Dockerfile "$image_context"
rm -r "$image_context"
docker run --rm --name castvane -p 127.0.0.1:8080:8080 \
  --env-file /path/to/private/container.env \
  --mount type=volume,src=castvane-state,dst=/var/lib/castvane \
  --mount type=bind,src=/path/to/media-tools,dst=/opt/media-tools,readonly \
  castvane-core:local
```

For this example the environment file supplies `CORE_API_AUTH_ENABLE=true` and bootstrap credentials. The image already sets container-specific bind and storage paths. Keep the admin port private or front it with a TLS reverse proxy. Probe `/healthz` for liveness and `/readyz` before admitting traffic. Readiness reports FFmpeg unavailable if the runtime has no executable. For the transport and TLS options see the [operations status](PHASE_4_STATUS.md).

## Backup, restore and rollback

1. Record the active binary version, configuration overlay, relevant executable versions and mount locations. Record references to deployment secrets in the backup inventory and retain the secrets in their existing secret store.
2. Use SQLite's online backup command, for example `sqlite3 /var/lib/castvane/core.sqlite3 ".backup /private/backups/core.sqlite3"`. Set restrictive permissions on the backup. A live copy of the main database file can omit committed data from its WAL.
3. Back up persistent media, private certificate/ACME data, and any separately managed configuration. Pause publishers or snapshot the underlying volumes when media consistency across files matters. Memory-backed media is transient.
4. Check the backup with `sqlite3 /private/backups/core.sqlite3 'PRAGMA integrity_check;'`. On an isolated listener, restore a copy, supply the matching deployment settings, and check login, persisted configuration, job state, media access and authoritative backups. Disable autostart or isolate all native outputs before rehearsing with production process definitions.
5. Before an upgrade, save a fresh backup. If rollback is required, stop the upgraded node, preserve its current data for diagnosis and restore the pre-upgrade binary, database and settings together. Use a database restored with its matching application version and reject stale WAL or SHM files in the restore location.

The isolated recovery harness uses disposable state and generated test media. Short runs are suitable for CI. Increase duration and concurrency to collect sustained results on a representative host.

```sh
python3 scripts/test-recovery.py target/release/restreamer-core --seconds 60 --clients 8
```

One local Linux AMD64 run of the release binary completed 70,200 requests with eight clients in 20 seconds. The Core process's peak RSS increased from 57,276 KiB before load to 60,228 KiB afterward. The fixture verified FFmpeg child cleanup after forced termination and replayed the persisted job and media after both restart and backup restore. This is a loopback workload with one synthetic FFmpeg job.
