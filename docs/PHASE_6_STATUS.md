# Phase 6 — Production release

The source version is `0.2.0-rc.1`. Phase 6 remains open. The candidate improves release operations and covers a functional standalone and clustered subset of the pinned Go reference. Public API v3 compatibility is tracked by contract tests and the [API inventory](API_COMPATIBILITY.md). No stable release has been approved.

| Gate | Status |
| --- | --- |
| Implementation-complete | Open. See the unfinished roadmap checks below. |
| Automated validation-complete | Verification is in progress for the current candidate. |
| Manual acceptance | Pending, separately tracked in the [owner checklist](RELEASE_ACCEPTANCE.md). |
| Release-approved | Pending explicit owner approval. |

## Candidate changes

- Authenticated GraphQL for discovery, ping, processes, reports, probe, local metrics and logs, with bounded execution and per-field authorization. The Go JSON transport returned 400 during isolated verification. [GraphQL guide](GRAPHQL.md)
- A versioned offline migration command imports supported Go JSON configurations, process definitions and metadata, zone and alias exports, and dormant users with compatible policies. Unsupported configuration areas are listed in the import guide. [Migration guide](MIGRATION.md)
- Consistent SQLite backups include application state and the local member's Raft durable state. Restoration checks SHA-256, SQLite integrity, foreign keys and supported SQL migration history, and refuses existing destinations. Binary metadata includes the source commit. [Deployment and recovery](DEPLOYMENT.md)
- Build and candidate packaging cover Linux AMD64 and ARM64 with glibc. The Docker build can use Ubuntu's FFmpeg and records configure flags and exact package/source versions. The export uses operator-supplied tools while FFmpeg source distribution and publication remain pending. The [release guide](RELEASING.md) describes policy, checksums, provenance and the independently versioned contracts.
- The dependency policy checks both native Linux graphs. The full lockfile RustSec review found zero known vulnerabilities and two unmaintained dependency notices. OpenRaft still uses the affected packages. See the [dependency policy](../deny.toml) and [third-party notices](../THIRD_PARTY_NOTICES.md).

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
