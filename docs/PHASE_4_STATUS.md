# Phase 4 — Operations and IAM

Castvane's operations work continues from the independent Rust history through `dff8959`. The behavioral reference is [Restreamer Core vod at 452e9429](https://github.com/KingIronMan2011/restreamer-core/tree/452e9429c1fd7e1a90d75ed12883171ecabd80eb). The legacy discovery name, JWT issuer, API v3 prefix and executable name remain part of the Restreamer compatibility surface. Castvane is the project and deployment name. The Rust project is maintained on the [Castvane/core main branch](https://github.com/Castvane/core/tree/main).

## Identity and permissions

The account store keeps Argon2id password hashes, status, role, timestamps and an authentication generation. Ordered policy assignments are separate SQLite records and are reloaded when authenticating subsequent requests. Password changes, role changes, renames, deactivation and explicit revocation replace the generation. Bootstrap credentials retain their Phase 1 configuration path and provide recovery access. A last active administrator cannot be removed or deactivated. API responses redact all credentials. Administrative writes are recorded without credentials.

Managed accounts use the existing `/api/login` and refresh flow. The token issuer, use claims and default access and refresh lifetimes remain 600 and 86,400 seconds. `CORE_API_AUTH_JWT_ACCESS_TTL_SEC` and `CORE_API_AUTH_JWT_REFRESH_TTL_SEC` set a 30–86,400-second access lifetime and up to a 30-day refresh lifetime. Increasing lifetimes applies to new tokens. Replacing the database-managed signing key invalidates previous API and media tokens. An environment-managed key changes through secret replacement and restart.

Rules contain a domain, resource types, resource glob and actions. Types and actions are case-insensitive. An empty domain means `$none`. Exact domain matches and the `*` wildcard are supported. Patterns are case-sensitive. API, filesystem and native stream paths treat `/` as a separator. Use `**` where nested paths are intended. An omitted effect allows access and explicit matching denials take precedence for managed identities. A single `any` action matches all actions for that resource. The default is deny. Bootstrap and managed administrators have full administrative access.

A managed user can list and access jobs only within their allowed scopes. Delegated process editing requires configured FFmpeg input and output allowlists and the restricted argument set. Scoped jobs cannot use secret or environment placeholders, unresolved cross-job references or file cleanup rules. Direct file operations check storage-relative paths such as `/disk/live/part.ts`. Global configuration changes, bulk filesystem changes, viewer-token issuance, event streaming and policy administration remain administrator operations. Phase 3 maintains separate service credentials for internal storage and transport. External Auth0 identity federation and all reference forms of delegated IAM administration remain compatibility gaps.

## Zones and resource control

SQLite migration 4 adds managed identities, policy assignments, zone limits and bounded audit history. It creates `$default` and adopts existing process zone IDs. Process claims are checked transactionally, including when concurrent create requests target the same zone. Deleting a configured job releases its zone claim. The default zone and any zone still referenced by a job are protected. Zone policy scopes use the process domain. Zone definitions are stored once per node.

The node samples CPU, topology, memory, disk capacity, network counters, process counts and uptime from Linux procfs and filesystem statistics. Effective cgroup v2 CPU and memory limits include ancestor limits. Missing values are omitted from Prometheus and represented as null in the resource endpoint. Per-job memory and CPU usage continues to come from the Phase 2 supervisor.

Setting `resources.max_cpu_usage` or `resources.max_memory_usage` applies an aggregate live reservation budget based on effective CPU and memory capacity. A pending job waits for claimed capacity and releases it on exit. The budget accounts for explicit requested limits. A job with zero configured limits contributes no claim. This scheduler budget is separate from OS enforcement. Per-process hard limits require an explicitly delegated cgroup v2 subtree and restart. The process state distinguishes hard enforcement from sampled enforcement. FFmpeg and MediaMTX run as the unprivileged service user. Running a separate service account remains part of the deployment boundary.

DRM sysfs inventory recognizes NVIDIA, Intel, AMD and Broadcom devices. `nvidia-smi` uses the installed NVIDIA driver to report model, identifier, memory, GPU utilization and encoder or decoder utilization. AMD data is collected where the DRM driver exports it. Intel and Raspberry Pi detection depends on DRM or V4L2 devices visible to the service and may provide inventory only. The FFmpeg skills endpoint continues to report compiled-in capabilities independently of device discovery. This phase has no automatic device assignment or VRAM enforcement. GPU paths have hardware-independent parser tests. GPU execution on installed hardware requires acceptance on each supported driver.

## Operations endpoints

All v3 control routes use centralized authentication and authorization. A write route honors the read-only setting. Stream subscriptions have a finite lifetime, recheck authentication on each heartbeat and cap the number of concurrent consumers.

| Path | Method | Contract |
| --- | --- | --- |
| `/api/v3/iam/user` | GET, POST | List accessible identities or create an identity. Creation requires `name` and `auth.api.userpass`. |
| `/api/v3/iam/user/{name}` | GET, PUT, DELETE | Read, modify or delete a managed identity. PUT supports partial updates with optional `active`. |
| `/api/v3/iam/user/{name}/policy` | PUT | Atomically replace the identity's array of policy rules. |
| `/api/v3/iam/user/{name}/revoke` | PUT | Invalidate existing access and refresh claims for the identity. |
| `/api/v3/iam/signing-key/rotate` | PUT | Replace the database-managed key and invalidate previously signed tokens. |
| `/api/v3/zone` | GET, POST | List zones with declared limits and claims, or create a named zone. |
| `/api/v3/zone/{id}` | GET, PUT, DELETE | Read, replace or delete a zone. In-use deletion returns 409. |
| `/metrics` | GET | Authenticated Prometheus scrape when enabled. |
| `/api/v3/metrics` | GET, POST | Metric descriptors or samples selected by name, interval and time range when enabled. |
| `/api/v3/resources` | GET | Linux and cgroup resource snapshot, node reservations and certificate expiry. |
| `/api/v3/log` | GET | Bounded console or raw logs. Supports `format` and `component`. |
| `/api/v3/events`, `/api/v3/events/log` | POST | Filtered SSE or JSON stream. |
| `/api/v3/events/media/{kind}` | POST | Mount or transport publisher inventory, keepalive and change events. |
| `/api/v3/events/process` | POST | Filtered process lifecycle JSON stream. |
| `/api/v3/audit` | GET | Administrative history, optionally after an event cursor. |
| `/healthz`, `/readyz`, `/ping` | GET | Liveness, dependency readiness and legacy plaintext liveness. |

Prometheus uses the `castvane_` namespace, fixed metric names and bounded HTTP status labels. Request throughput and handler duration, active connections, supervisor process counts, failures, CPU and memory, media viewers and publisher counts, transfer totals, cache residency and filesystem request timings are available. Metrics never label media URLs, user IDs or remote addresses. Legacy metric queries cover aggregate descriptors and time series. The legacy `restream_state` series supports the fixed `state` dimension. Prometheus exports optional GPU gauges for up to 16 device indices and excludes serial numbers, model names and paths. Some Go metrics with per-process, per-interface and URL dimensions have no equivalent. Optional unconfigured GPU or storage values remain absent.

Operational logs and process/media events use bounded in-memory history and a bounded broadcast queue. Slow subscribers disconnect when they fall behind the retained queue. Raw logs retain safe fields and omit arbitrary tracing fields. `CORE_LOG_FORMAT=json` enables JSON stdout for external logging. Audit history is distinct and limited to 4,096 low-rate administrative records. The legacy event layer exposes lifecycle and inventory changes. Log subscriptions support case-insensitive regular expressions for message and level on selected components. Process subscriptions support regex filters for process ID, domain and event type. They publish sampled progress for running FFmpeg jobs with time and speed. Standard FFmpeg supplies no per-stream event values. Aggregate HTTP viewer-count changes are available to log subscribers without viewer identifiers. The stream does not publish arbitrary tracing fields or individual viewer events.

## Deployment

[The environment example](../examples/castvane.env) enables a loopback API and configurable request, connection and log limits. Create media and database directories owned by the service user and keep configuration, credentials and TLS material outside the media root. Run a dedicated account under systemd or as a non-root container user. Use `/healthz` for container liveness and `/readyz` for load balancer readiness. Readiness checks the database, the configured FFmpeg executable, required storage and configured HTTPS. Native media outages appear in readiness diagnostics. Set `CORE_RTMP_REQUIRED=true` or `CORE_SRT_REQUIRED=true` when native media must gate readiness. S3 mount objects support `required:true`. Optional GPU data and optional media services can fail while liveness remains healthy.

Place a reverse proxy in front of the loopback listener for external access. Leave `CORE_API_AUTH_DISABLE_LOCALHOST=false` for proxied traffic. Set `CORE_API_ACCESS_TRUSTED_PROXIES` to actual proxy source CIDRs if using `X-Forwarded-For`. Untrusted socket peers cannot supply their own identity through the header. A configured proxy with a missing peer header gets no loopback bypass. A trusted proxy may set a single `X-Forwarded-Proto: https` value for HTTPS ACLs and secure media cookies. Filter private media tokens from proxy access logs.

For user-managed TLS, set `CORE_TLS_ENABLE=true`, `CORE_TLS_ADDRESS`, `CORE_TLS_CERT_FILE` and `CORE_TLS_KEY_FILE`. Use a PEM certification chain and a regular mode-0600 key file. Castvane validates dates and the key pair using rustls. It watches for replacement of both files and holds the previous valid pair until a complete, valid replacement arrives. RTMPS supports the provided PEM material through MediaMTX. Changes to the companion certificate require restart.

ACME HTTP-01 needs a DNS hostname in `CORE_HOST_NAME`, an HTTP listener reachable by the ACME server on the challenge port, and explicit terms-of-service acceptance. Forward `/.well-known/acme-challenge/*` to Castvane when using a front-end proxy. Wildcards require different validation and are outside this implementation.

```sh
export CORE_TLS_ENABLE=true CORE_TLS_AUTO=true CORE_TLS_STAGING=true
export CORE_TLS_ADDRESS=127.0.0.1:8443
export CORE_HOST_NAME=media.example.org CORE_HOST_AUTO=false
export CORE_TLS_EMAIL=operator@example.org CORE_TLS_ACME_AGREE=true
export CORE_TLS_ACME_STORAGE=/var/lib/castvane/acme
restreamer-core
```

Set `CORE_TLS_STAGING=false` for the production Let's Encrypt directory once deployment is ready. For a private issuer, `CORE_TLS_ACME_DIRECTORY` and `CORE_TLS_ACME_CA_FILE` select the directory and its trust anchor. Persist the ACME folder across restarts. The default renewal window is 30 days and can be set through `CORE_TLS_ACME_RENEW_BEFORE_SECONDS`. Account and certificate bundles use private permissions and atomic file replacement. If a renewal fails, a still-valid cached certificate remains available. The HTTP-01 fixture tests issuance and replacement within the renewal window. It takes Pebble offline and makes the renewal CA unavailable to verify that a failed renewal preserves the persisted certificate.

Configuration and IAM policies are read on subsequent API requests. The log level and HTTP listener can be reloaded. The HTTPS PEM files can be replaced in place. Request and proxy limits, media listener addresses, mount configuration, ACME settings and node resource budgets are read at startup. Renewing HTTPS through ACME does not rotate RTMPS automatically.

## Verification and acceptance

Use the repository's standard format, Clippy, locked tests, release build and smoke commands. The workflow also runs isolated SeaweedFS, MediaMTX and Pebble fixtures on both native Linux runners. `scripts/test-acme.sh` provisions pinned local Pebble and DNS executables. All certificate requests in that fixture target loopback.

Run `cargo test --locked --test operations benchmark_parallel -- --ignored --nocapture` for the router benchmark. It measures a single Argon2id login, concurrent JWT and policy-checked requests, metrics scraping and bounded log dispatch. `scripts/test-media-services.sh` exercises native stream protocols and object storage. The earlier phase benchmarks remain available for media delivery. Tests exercise GPU parsing with fixtures. Production GPU and native CI acceptance are tracked separately.

The local Linux AMD64 test run passed format and lint checks, all 39 standard tests, the release build, and the HTTP and SIGTERM smoke test. The two isolated media tests passed with SeaweedFS and MediaMTX, including real FFmpeg RTMP, RTMPS and SRT playback. The Pebble fixture validated the certificate against its local trust root and restarted the service after shutting down the issuer. The original differential script matched 11 IAM, zone and log contract checks on the pinned Go binary after normalizing generated timestamps and excluding response credentials. Run it as `python3 scripts/differential-ops.py http://127.0.0.1:18880 http://127.0.0.1:18881` with disposable test accounts configured through the same fixture variables as the Phase 1 script.

One debug-profile run on Linux AMD64 with a two-CPU cgroup quota measured 278,000 policy decisions per second with 12 rules, 0.27 ms per host snapshot, 404 ms for an Argon2id login, and 171,000 captured log events per second. Eight clients achieved 775 configuration reads and 795 metrics scrapes per second through the in-process router. Four clients transferred 64 one-MiB memory-backed responses at 214 MiB/s without scrapes and 235 MiB/s with concurrent scrapes. These short runs omit network transport and are subject to scheduling noise. They do not establish the impact of scraping on live media throughput.

RustSec reported zero vulnerabilities in the lockfile, with existing unmaintained warnings for `chrono` and `humantime`. The active AMD64 and ARM64 dependency metadata lists permissive licenses. `instant-acme` and the Pebble test binaries are Apache-2.0. The [Castvane Core Actions workflow](https://github.com/Castvane/core/actions/workflows/rust.yml) records native AMD64 and ARM64 CI results.

On each pinned Restreamer UI version, check discovery, administrator login and refresh, then create a managed user with a narrow scope. Compare the process list and detail views for the administrator and scoped account. Modify the policy and revoke the account. Exercise zone assignment, deletion of an in-use zone, log filtering, metrics pages, live media, and automated and manual certificate modes. Record exact UI versions, version gates and response discrepancies. Manual browser acceptance remains open.

Clustering, consensus, node enrollment, replication, multi-node scheduling, distributed certificate management and failover remain Phase 5 work.

The follow-up run passed 42 standard tests locally, including filtered process progress, bounded GPU labels, and event-stream revocation. Native media, MinIO and certificate renewal passed in isolated fixtures. The browser results, candidate package commands and recovery fixtures are recorded separately. [Browser results](UI_REGRESSION.md) · [Deployment and recovery](DEPLOYMENT.md)
