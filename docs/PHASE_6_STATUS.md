# Phase 6 — Production release

The source version is `0.2.0-rc.1`. Phase 6 remains open. The candidate improves release operations and covers a functional standalone and clustered subset of the pinned Go reference. Public API v3 compatibility is tracked by contract tests and the [API inventory](API_COMPATIBILITY.md). No stable release has been approved.

| Gate | Status |
| --- | --- |
| Implementation-complete | Open. See the unfinished roadmap checks below. |
| Automated validation-complete | Current candidate verification is recorded below. Full roadmap validation remains open. |
| Manual acceptance | Pending, separately tracked in the [owner checklist](RELEASE_ACCEPTANCE.md). |
| Release-approved | Pending explicit owner approval. |

## Candidate changes

- Authenticated GraphQL for discovery, ping, processes, reports, probe, local metrics and logs, with bounded execution and per-field authorization. The Go JSON transport returned 400 during isolated verification. [GraphQL guide](GRAPHQL.md)
- A versioned offline migration command imports supported Go JSON configurations, process definitions and metadata, zone and alias exports, and dormant users with compatible policies. Unsupported configuration areas are listed in the import guide. [Migration guide](MIGRATION.md)
- Consistent SQLite backups include application state and the local member's Raft durable state. Restoration checks SHA-256, SQLite integrity, foreign keys and supported SQL migration history, and refuses existing destinations. Binary metadata includes the source commit. [Deployment and recovery](DEPLOYMENT.md)
- Build and candidate packaging cover Linux AMD64 and ARM64 with glibc. The Docker build can use Ubuntu's FFmpeg and records configure flags and exact package/source versions. The export uses operator-supplied tools while FFmpeg source distribution and publication remain pending. The [release guide](RELEASING.md) describes policy, checksums, provenance and the independently versioned contracts.
- The dependency policy checks both native Linux graphs. The full lockfile RustSec review found zero known vulnerabilities and two unmaintained dependency notices. OpenRaft still uses the affected packages. See the [dependency policy](../deny.toml) and [third-party notices](../THIRD_PARTY_NOTICES.md).

## Automated verification

| Check | Result |
| --- | --- |
| Linux AMD64 local | Format, Clippy with warnings denied, 48 automated Rust tests, locked release build and HTTP/SIGTERM smoke passed. External service fixtures and repeatable benchmarks have separate invocations. |
| Pinned Go standalone contracts | Matched 75 checks across discovery, configuration, FFmpeg, storage, IAM, event negotiation, zones and metrics. |
| Migration | Generated a version 4 process store using the pinned Go executable. Dry run and transactional import passed. Source checksums were unchanged. The Rust tests additionally cover duplicate input, blocked overwrite, zone and alias import, inactive accounts and policy import. |
| SQLite backup | Online backup, SHA-256 verification, private permissions, overwrite refusal, restart, corrupt checksum rejection and preserved login/metadata passed. |
| Cluster | The four-node fixture rolled from `9ab72dc` to the candidate while retaining quorum. Voter restoration preserved the committed record, process migration and ownership cleanup passed, and the control-only member became ready with an unavailable FFmpeg binary. |
| Storage and media | Local generated HLS/DASH, byte-range delivery and concurrent sessions passed. MediaMTX and two isolated S3-compatible services are covered by the native CI suite. |
| TLS | Isolated Pebble HTTP-01 issuance, renewal and cached-certificate startup after the issuer went offline passed. |
| Dependency checks | Cargo audit reported zero known vulnerable lockfile entries and two unmaintained notices. Cargo deny passed the Linux advisory, license, dependency and source policy. Duplicate-version warnings remain for shared transitive ecosystems. [Dependency CI](https://github.com/Castvane/core/actions/runs/45904807481) |
| Compose | Standalone and VAAPI overlay configurations parse. Actual physical GPU and multi-host configuration acceptance remains pending. |
| Chromium | Login and refresh passed on the three upstream fixtures with working builds. The version gate blocked their dashboards. The owner fixture never submitted a login. [UI regression report](UI_REGRESSION.md) |

All local media inputs were generated in disposable directories. The environment was Linux AMD64 with three assigned AMD EPYC 9V74 CPUs, Rust 1.100.0, Go 1.27.4 and FFmpeg 7.1.5. The sixty-second recovery run drove 87,960 API and range requests with eight clients. Core peak RSS went from 41,764 to 45,108 KiB. The same run verified FFmpeg crash cleanup, restart and restored job state using the CLI backup.

The four-node loopback upgrade fixture measured 72 durable writes plus CAS and locks in 1.448 s, enrollment in 0.019 s, FFmpeg deployment in 2.999 s, relocation in 10.346 s and worker failover in 12.393 s. It committed through the majority after 4.024 s, including the rejected isolated write. These timings use eight-second fixture leases and include local scheduling effects. No direct Go cluster speed comparison was made.

A separate three-run management benchmark measured around 23% higher Castvane liveness throughput and around 15% higher Go discovery throughput. The [benchmark report](BENCHMARKS.md) includes absolute times, per-run ranges, environment, repeat commands and test profile details.

## Security and isolation

GraphQL uses the REST network and access-token middleware, scoped process authorization, bounded request/response work, and an execution deadline. Both REST and GraphQL probes require write scope. Managed users can probe only jobs previously validated under the delegated FFmpeg restrictions. Migration rejects duplicate JSON keys and metadata conflicts before publication. Backups are private, integrity-checked and refuse overwrites.

The control API still runs beside third-party media executables. The process supervisor uses Linux process groups and can use delegated cgroups. Full filesystem and network isolation require the deployment environment. Treat managed media, FFmpeg arguments, backups and cluster members according to the documented trust boundaries. Existing token audiences and cross-node authority follow the [IAM](PHASE_4_STATUS.md) and [cluster](CLUSTER.md) contracts. TLS and cluster CA keys require a separately protected backup.

## Tracked roadmap and release gaps

| Area | Landed | Remaining |
| --- | --- | --- |
| 0–1 Bootstrap, API and authentication | Native CI baseline, persistent config, local JWT, API fixtures | All advertised UI versions need to finish the real app workflows. The semantic-version gate and older fork failures are recorded in the [browser report](UI_REGRESSION.md). |
| 2 Process engine | Real FFmpeg supervision, progress, scheduling, recovery, persistence and scoped process settings | Full fields and limits for custom FFmpeg extensions and additional legacy shapes remain open. |
| 3 Media and storage | Disk, memory, S3, MediaMTX-compatible RTMP/RTMPS/SRT, HLS, DASH, HTTP-FLV, sessions and aliases | Live playout requires extended FFmpeg. Some wire telemetry, provider and player acceptance remain open. |
| 4 Operations and IAM | Local IAM and explicit deny, metrics, health, bounded events/audit, TLS and ACME | Auth0, some delegated administration forms, broader metrics and physical hardware tests remain open. |
| 5 Clustering | Raft and mTLS, quorum-fenced scheduling, remote diagnostics, leases, IAM, zones, aliases and KV | Leadership transfer, complete membership lifecycle, live identity/CA rotation, distributed TLS, richer placement, cross-node references and legacy cluster route parity remain open. See [Phase 5](PHASE_5_STATUS.md#open-phase-5-work). |
| 6 Distribution | CLI, candidate recipes, migration, backups, release policy and packaging | Complete API/OpenAPI per-operation parity, broad migration acceptance, remaining cluster recovery procedures, sustained testing, physical GPU profiles, Docker Hub and stable publication. |

Developer and operator commands are in the [deployment guide](DEPLOYMENT.md). Do not publish a stable release before the manual gates and unresolved implementation items have been reviewed.

## Release checklist

- [x] Use an independent candidate application version and embed the source revision.
- [x] Exercise deployment, authenticated control, standby member restoration and the supported rolling-upgrade rehearsal.
- [x] Exercise backup and independently authored migration fixtures, including a store created by the pinned Go executable.
- [x] Exercise pinned UI discovery/login/refresh with existing bundles and retain exact blockers.
- [ ] Complete the remaining route/schema compatibility checks and legacy UI version policy before claiming support across the advertised UI range.
- [ ] Complete Phase 5 membership and leadership administration, distributed certificate management and cross-node workflow parity.
- [ ] Decide the Auth0 integration and extended FFmpeg playout support for the stable compatibility profile.
- [ ] Validate the 24-hour and 72-hour workloads, real-provider S3, physical GPU and multi-host deployment on representative infrastructure.
- [ ] Complete corresponding source review and preservation before publicly distributing an FFmpeg-bundled image.
- [ ] Configure protected registry promotion and obtain explicit approval for a first stable release.
