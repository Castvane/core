# Castvane Core implementation plan

## Goals and non-goals

- Preserve the public v3 wire contracts using the Go `vod` branch at `452e9429` as the behavioral reference and pinned Restreamer UIs as historical regression fixtures. Official UI support covers Castvane’s own UI.
- Retain the public `/api/v3` contract, including unversioned legacy routes (`/api`, `/api/login`, `/api/login/refresh`, `/ping`), GraphQL where clients use `/api/graph`, media mounts and optional Swagger access. Internal Rust architecture and storage formats may differ.
- Treat public API versions separately from application releases. Reserve `/api/v4` for future breaking changes; keep v3 adapters operational. Do not require an old UI to change its base URL, tokens, or request format.
- Independently implement internals from documented behavior; do not translate Go files. Review licensing before importing any reference artifacts. License independently authored Rust code under MIT; historical Go code retains its Apache 2.0 license.
- Do not attempt to replace FFmpeg codecs or protocols with bespoke implementations. Use supported FFmpeg and FFprobe executables and proven media-protocol libraries/services after license and maintenance review.

## Compatibility discipline

Phase 1 must inventory routes from `vod` and the pinned reference UI releases, recording method, path, query parameters, request/response field names, null versus omitted versus empty values, defaults, status codes, error envelopes, headers, auth requirements, feature flags, and side effects. Include GraphQL operations and media URL formats. Create independently authored sanitized fixtures from observed behavior, then compare Rust responses against a pinned reference instance in integration tests. Test older and current UIs in a browser without modification. Where versions disagree, preserve old behavior for old clients without breaking current clients; document any irreconcilable differences before release.

| Surface | Compatibility priority |
| --- | --- |
| `/ping`, `/api`, `/api/login`, `/api/login/refresh` | UI discovery, liveness and authentication |
| `/api/v3/process`, `/skills`, `/metadata`, `/report/process`, `/widget/process/{id}` (under v3) | Process CRUD, commands, probe, progress, reports and metadata |
| `/api/v3/config`, `/fs`, `/rtmp`, `/srt`, `/session`, `/log`, `/metrics` (under v3) | Configuration, storage, ingest, playback and monitoring |
| `/api/v3/iam`, `/alias`, `/zone`, `/events` (under v3) | Access control, media routing and events |
| `/api/v3/cluster/*` | Cluster nodes, processes, deployment, state, routing and management |
| `/api/graph`, `/api/swagger/*`, `/metrics`, media mounts | GraphQL, docs, metrics and media delivery |
| Separate cluster service `/v1/*` | Evaluate explicitly; preserve wire compatibility if mixed Go/Rust clusters are required; otherwise internal protocol can differ |

Claim v3 compatibility only for verified contracts. Preserve Castvane application version metadata in discovery, and use explicit API versions and capability information for first-party UI integration. Unsupported operations must fail clearly rather than return fake success. Old UI compatibility does not imply joining existing Go Raft clusters; mixed-node compatibility is a distinct decision requiring consensus/storage/protocol interoperability work.

## Architecture

Start with a single binary and library. Split into Cargo workspace crates as the boundaries mature: API compatibility adapters; domain model; FFmpeg supervisor; storage/media; authentication; resource monitoring; cluster control plane; workers. Keep domain types separate from legacy DTOs, with explicit conversions at API boundaries. Use Tokio and Axum, Serde, tracing, OpenAPI tooling, SQLite for single-node state (evaluate SQLx), and a reviewed Raft library for cluster consensus if self-contained clustering is selected. Evaluate Tonic for internal RPC; it must not replace public v3 endpoints. Use bounded queues, cancellation, deadlines, and explicit lifecycle state machines.

## Phases and exit gates

### 0 — Bootstrap (complete)

Create Rust package, liveness endpoint, contract test and CI. Use the MIT license. See [bootstrap decisions and exit checklist](docs/BOOTSTRAP.md) for supported platforms, FFmpeg distribution, pinned reference UI versions, committed lockfile, toolchain, and verification status. CI uses pinned actions and locked build commands.

### 1 — Contract and foundation (implementation verified)

Build the versioned v3 adapter, configuration validation, app discovery, error mappings and authentication/refresh behavior. Add persistent, transactional configuration with migration/versioning, structured logs and authorization checks. Exercise the pinned reference through differential API tests. Track owner-run browser acceptance separately from automated validation. See [Phase 1 status](docs/PHASE_1_STATUS.md) and the [API compatibility matrix](docs/API_COMPATIBILITY.md) for the implemented foundation, settings, and known UI version gates.

### 2 — Single-node process engine (implementation verified)

Implement process CRUD, desired versus observed state, start/stop/restart, probing, per-process logs, progress and reports, references/placeholders, scheduled starts, validation, retry/backoff, stall detection and bounded history. Use argument vectors rather than shell interpolation. Supervise process groups, enforce cancellation/timeouts and prevent child leaks. Store public metadata separately from secrets. Test concurrent commands, crashes, restart reconciliation, absent FFmpeg, and long-running streams. The process API can create and manage a real local stream. Track manual UI acceptance, version gates and later media dependencies separately. See [Phase 2 status](docs/PHASE_2_STATUS.md) for compatibility and verification.

### 3 — Media and storage (single-node implementation, compatibility acceptance open)

The Rust media service integrates disk, memory and S3 mounts with HTTP delivery, HLS/DASH, MediaMTX RTMP/RTMPS and SRT, HTTP-FLV, viewer sessions, aliases, caching and cleanup. [Phase 3 status](docs/PHASE_3_STATUS.md) tracks automated verification, manual UI acceptance and the extended FFmpeg dependency for live playout control.

### 4 — Operations and IAM (local implementation, compatibility gaps open)

The standalone operations layer uses SQLite for managed identities, policy assignments, zone declarations and durable audit events. Linux sampling, request budgets, bounded event streams and rustls certificate management operate locally. [Phase 4 status](docs/PHASE_4_STATUS.md) records automated checks, remaining v3 differences, and exit gates.

### 5 — Distributed control plane (implemented subset, exit criteria open)

Provide standalone and clustered modes, with stable node identity, enrollment/authentication, heartbeat and resource inventory. Replicate durable desired state with a reviewed consensus library; distinguish quorum decisions from ephemeral telemetry. Implement reconciliation, reservations, affinity, zones, relocation, rebalancing, failover and distributed locks/leases with fencing. Test partitions, leader elections, duplicate commands, node removal, recovery, snapshots and rolling upgrades. Expose v3 cluster APIs; decide mixed Go/Rust cluster support separately. Prevent conflicting process ownership when quorum is unavailable.

The opt-in Raft implementation includes a rehearsed OpenRaft upgrade, targeted leader transfer, and inventory of member builds, processes and files. [Phase 5 status](docs/PHASE_5_STATUS.md) tracks the remaining exit criteria. See [cluster installation](docs/CLUSTER.md) for node roles and ownership assumptions.

### 6 — Release and maintenance (candidate, exit criteria open)

The candidate adds GraphQL, operator CLI, offline Go JSON migration, verified SQLite restore, and same-commit Linux AMD64, Linux ARM64 and Windows AMD64 release packaging. The [release status](docs/PHASE_6_STATUS.md) tracks the dependency policy, verification evidence and unfinished roadmap work. The first stable Core release requires completion and verification of the standalone and clustered backend roadmap. Core must expose the documented contracts needed for first-party UI development. Browser acceptance applies to the UI when it ships or is advertised. Stable Core publication requires the applicable [acceptance checklist](docs/RELEASE_ACCEPTANCE.md) and explicit owner approval.

## Quality gates

Run formatting, Clippy with warnings denied, unit/integration tests, API contract tests, UI browser tests, dependency audit and license checks. Use fixture streams and isolated test environments. Test persistence after crashes, graceful shutdown, resource exhaustion, slow viewers, invalid requests, auth scope and denial cases, and multi-node failure scenarios. Performance conclusions require reproducible side-by-side benchmarks with equal FFmpeg settings and hardware.

## Estimate

Planning ranges for cumulative active AI-agent implementation time, not release promises: single-node feature-complete 40–90 hours; distributed beta 150–250 hours; broad `vod` parity including legacy API 250–450 hours; production validation 450–750+ hours. Hardware availability, required old UI versions and mixed-cluster compatibility can expand scope materially.
