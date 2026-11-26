# Castvane deployment and recovery

## Native installation

Use a supported Linux AMD64 or ARM64 host with a compatible glibc. Candidate binaries build on native Ubuntu 24.04 runners. Statically linked and other operating-system builds are outside the supported distribution. Check the archive SHA-256 and GitHub provenance before installing. [Release guide](RELEASING.md)

Install trusted, patched FFmpeg and FFprobe separately. Install MediaMTX when RTMP, RTMPS, SRT or HTTP-FLV service is required. Check the actual executable pair with `restreamer-core ffmpeg-check`. Install Core under `/usr/local/bin/restreamer-core` and create a dedicated, unprivileged `castvane` account. Use the [systemd example](../packaging/systemd/castvane.service) or call `restreamer-core serve`. Running the binary with no arguments still starts the server.

For a disposable local evaluation, start with an empty working directory and listen on loopback. Do not expose the unauthenticated default to other users on a shared host. For production, use a 0600 service-owned environment file derived from the [environment example](../examples/castvane.env). Remove the shell `export ` prefix for systemd or Docker Compose. Provision a strong `CORE_API_AUTH_PASSWORD` through a private secret store. Keep API authentication enabled and keep the localhost bypass disabled. Retain configuration, SQLite, ACME data and certificates outside served media roots.

```sh
restreamer-core --version
restreamer-core config-check
restreamer-core ffmpeg-check
restreamer-core serve
```

`config-init` writes a private default JSON template without replacing an existing file. `config-check --file` checks a versioned JSON document with the environment overlay. The server reads persistent settings from SQLite. Import a legacy configuration with the [migration tool](MIGRATION.md), or submit a reviewed partial v3 configuration update to an already provisioned instance. The startup environment takes precedence over stored settings. Listener and log changes have explicit reload paths. Database, cluster trust, service transports, request budgets and environment changes require restart. See the phase contracts for feature-specific conditions. `config-check` validates settings without starting services or proving backend reachability.

## Container installation

Build the default operator image with Ubuntu's FFmpeg and FFprobe, or use the Core-only candidate with an administrator-provided compatible pair. The image uses UID/GID 10001 and stores data under `/var/lib/castvane`. RTMP, RTMPS, SRT and HTTP-FLV require an independently supplied MediaMTX service binary in addition to the Core and FFmpeg media tools.

```sh
cargo build --locked --release
build_context=$(mktemp -d)
cp target/release/restreamer-core LICENSE "$build_context/"
docker build -t castvane-core:local -f packaging/Dockerfile "$build_context"
CASTVANE_ENV_FILE=/etc/castvane/container.env docker compose -f examples/compose.yml up -d
```

The private environment file needs `CORE_API_AUTH_ENABLE=true` and a bootstrap secret. The image sets a container-specific bind and private SQLite path. Leave the Compose host listener bound to loopback unless a controlled network policy and TLS protect it. Supply enough storage space and preserve the named volume across upgrades. Set resource limits for the workload and keep the root filesystem read-only when feasible. Use `/healthz` for liveness and `/readyz` to admit traffic. The container health check probes liveness. A Core-only image reports media readiness unavailable until it has FFmpeg.

Intel and AMD deploy the same image with matching userspace drivers and explicit render-device access. The [VAAPI overlay](../examples/compose.vaapi.yml) maps one render device and supplemental group. NVIDIA needs NVIDIA Container Toolkit and the driver on the host. For Raspberry Pi, select an ARM64 kernel, driver and FFmpeg build that exposes the board's real acceleration path. Confirm encode and decode on physical hardware before advertising it in node inventory. FFmpeg hardware encoders do not automatically fall back to CPU. Maintain a compatible alternative job configuration. [Hardware acceptance](RELEASE_ACCEPTANCE.md)

## Network, TLS and monitoring

Use a firewall or dedicated network to isolate admin, metrics, cluster RPC and media ingress according to role. Keep TLS permissions and renewal storage private. With a reverse proxy, overwrite forwarded address/protocol headers at the trust boundary, set only the applicable proxy ranges, and preserve the incoming hostname and range headers. Disable loopback authentication bypass for a proxy on the same host. See [local TLS, ACME and API policy](PHASE_4_STATUS.md). Use the [cluster guide](CLUSTER.md) for combined, control-only and worker nodes.

Use a scraper on a private listener and restrict metrics with network policy. See the [Prometheus example](../examples/monitoring/prometheus.yml). Tune alerting to readiness failures, FFmpeg restarts, decreasing disk space, viewer saturation and certificate deadlines. Use authenticated cluster health to check quorum. `/readyz` checks that a clustered node observes a leader and sees itself in membership, which can lag during a partition. Collect bounded logs to an external service when extended retention is required. [Operations metrics](PHASE_4_STATUS.md)

## Backup and restore

Use a private destination with enough free space for a complete database copy. A source SQLite database may be live. The backup captures committed data including WAL contents and emits a SHA-256 hash. Use a second trusted channel or signed inventory to store the checksum. Backup files may contain password hashes, tokens, signing material, stream URLs and cluster credentials embedded in data.

```sh
export CORE_DATABASE_PATH=/var/lib/castvane/core.sqlite3
checksum=$(restreamer-core backup /private/backups/core-2026-11-26.sqlite3)
printf '%s\n' "$checksum" > /private/backups/core-2026-11-26.sha256

CORE_DATABASE_PATH=/private/rehearsal/core.sqlite3 restreamer-core restore \
  /private/backups/core-2026-11-26.sqlite3 --sha256 "$checksum"
CORE_DATABASE_PATH=/private/rehearsal/core.sqlite3 restreamer-core database-check
```

Keep deployment overlays, private certificates/ACME data, media volumes and remote bucket backups alongside the database inventory through their appropriate backup mechanisms. Pause publishers or take a consistent filesystem snapshot when cross-file consistency matters. Memory-backed media is transient. Rehearse a restored database on an isolated listener with outputs disabled or firewalled before permitting autostart. Restore refuses to overwrite an existing database or a path with SQLite sidecars.

A cluster SQLite image includes the member's persisted log, vote and snapshot data. Back up each member and its matching TLS identity and deployment files. When recovering a stopped member, keep its original database and sidecars separately, and restore only to the same cluster and node identity. Keep the rest of the live quorum available throughout planned work. Take no action based on a single stale database when quorum is lost permanently. Automatic quorum reconstruction and restoration to new node identities remain outside automated recovery. [Cluster recovery](CLUSTER.md)

Before upgrading, check the configuration and database, then rehearse on a backup. Keep the old binary and state together for rollback. Restore a matching pre-upgrade database after migration when required. [Upgrade policy](RELEASING.md)
