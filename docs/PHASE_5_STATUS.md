# Phase 5 — Distributed clustering

The opt-in implementation provides working consensus, persistence and remote deployments. Mandatory Phase 5 parity and operational requirements remain open. The behavioral reference is [Go vod at 452e9429](https://github.com/KingIronMan2011/restreamer-core/tree/452e9429c1fd7e1a90d75ed12883171ecabd80eb).

## Reference and architecture

The Go cluster has public v3 management handlers, internal cluster clients and forwarding, a Raft-backed store, and leader-side synchronization, placement and relocation. Its scheduler uses node resources and affinity, while authoritative data includes process definitions, IAM, zones, aliases and distributed coordination. Castvane uses an independent internal protocol.

OpenRaft 0.10 provides elections, quorum commits, learners, voting changes, compaction, snapshots and restart recovery. SQLx persists votes and logs using SQLite full synchronization. Committed data and membership apply together. Snapshot installation validates the applied index and membership and uses an atomic SQLite transaction. High-volume observations stay outside consensus.

The separate internal listener requires mutual TLS 1.3 with per-node certificates. It binds inbound votes, append messages and snapshot chunks to the configured leaf identity. Outbound clients verify the cluster CA and configured DNS names. Internal paths use the /internal/cluster/v1 prefix and a configured cluster scope. Requests have deadlines, connection limits and bounded bodies. Staging peers and rotating cluster leaf certificates require coordinated configuration changes and restarts.

Combined nodes vote and run jobs. Control-only voters schedule without executing jobs. A worker-only node executes jobs as a non-voting learner. Learners replicate the complete authoritative state, including IAM hashes, and must be trusted with that data. Standalone process state is separate from the ephemeral worker engine.

## State, placement and fencing

Process definitions, desired state, assignments, epochs, reservations, managed identities and policies, zones, aliases, KV values and expiring locks are committed through Raft. Resource observations expire independently of the log. Mandatory node, label, zone, CPU, memory, storage and protocol constraints precede preference ranking. Hardware encoder compatibility combines operator configuration, device visibility and the installed FFmpeg inventory. GPU memory reservations match reported device identifiers. Concurrent placement requests are checked against existing reservations in the applied state machine. Lease renewals update assignment state without changing the process definition revision.

The worker starts a configured process only while it holds a matching committed assignment and a valid lease. The process supervisor rechecks the deadline while the process is running. The leader waits through the prior lease and a safety interval before assigning a different owner. Remote progress is accepted only for a matching epoch. Unreachable nodes lose lease renewals. Optional CPU-based rebalancing also retires leases and records a single destination and cooldown in Raft. Draining blocks new placements and old leases expire. Relocation restarts a process at its destination. These guarantees assume synchronized clocks and a responsive supervisor. External publishers and destinations may require their own fencing.

Cluster-managed account and policy checks use quorum-backed fencing even on followers. Tokens remain node-local. The administrator uses the cluster IAM routes to change managed accounts. Aliases project as an ordered cluster overlay and zones project into worker process databases. Remote filesystem operations connect to the assigned node over mutually authenticated TLS. Node-local media remains on its original node.

## API coverage

Cluster paths are under /api/v3/cluster. Administrator authorization and the existing v3 error envelope apply. Authority-sensitive handlers require the leader. OpenAPI lists the implemented paths.

| Group | Implemented |
| --- | --- |
| Topology | GET root, health, snapshot indices, deployments, node list, node detail, live node build metadata, node process list and node state |
| Membership | POST preprovisioned node enrollment, PUT replacement voter set, targeted leadership transfer, node drain and disable, and removal of drained voters and learners |
| Process | GET, POST, PUT and DELETE definitions; configuration, sampled state, reports, probe, playback origin and metadata; command, relocation, and read-only placement preview via relocation PUT |
| Filesystem | Member file inventory, aggregate storage listing, node mount listing, ranged GET, HEAD, PUT up to 1 MiB, DELETE, and per-node copy and move |
| IAM | List, create, update and delete managed identities; list and replace policies |
| Zones and aliases | Zone CRUD and ordered alias/rule operations |
| Coordination | Committed KV with conditional revisions, lease locking, renewal and owner-checked unlock |

For configuration, backup, node roles and supervision assumptions see [cluster installation](CLUSTER.md).

## Verification and acceptance

The disposable four-node fixture uses three voters, a learner, isolated TLS identities and generated FFmpeg input. It verifies elections, targeted leadership transfer, learner enrollment after log compaction, rejection without a client certificate, KV compare-and-set, lock fencing, distributed IAM authorization and revocation, alias propagation, remote filesystem operations, FFmpeg deployment, relocation previews without writes, relocation, abrupt worker loss, quorum loss, restored voter state, and a bidirectional TCP partition isolating the leader. Unit tests exercise overcommitment and old ownership epochs. Native CI runs the fixture on AMD64 and ARM64.

One Linux AMD64 debug run measured 72 durable writes plus CAS and locks in 0.78 seconds, learner enrollment in 19 ms, remote deployment in 2.20 seconds, relocation in 11.82 seconds and recovery after worker loss in 11.18 seconds. The fixture uses eight-second leases and loopback networking. These are measurements for a small isolated cluster. A subsequent partition fixture accepted a write from the majority after 4.02 seconds and rejected a write from the isolated leader.

RustSec reported no vulnerabilities and the existing unmaintained chrono and humantime advisories. OpenRaft 0.10.1 and its compatibility adapter are MIT OR Apache-2.0. A rolling upgrade from the prior 0.9.26-based Castvane build passed in the four-node fixture without resetting SQLite or changing the internal v1 RPC contract.

First-party UI acceptance remains open for the Castvane UI. With a disposable cluster containing control and media workers, check the node list, job assignment, observed progress, relocation, policy administration, and aliases. Record the UI and Core revisions and remove credentials from collected traces.

## Open Phase 5 work

- Automatic leave forwarding and leadership handoff for a self-removing leader, time-limited enrollment and live identity rotation.
- Memory and GPU pressure rebalancing, pipeline affinity and failure-domain separation, relocation cancellation, and per-device encoder and decoder session limits.
- Distributed certificate issuance and renewal leadership. Cluster mode uses externally coordinated public TLS certificates and declines built-in automatic ACME.
- Legacy playback redirects, cross-node process references and automatic zone routing.
- Full Go cluster API parity, including original snapshot downloads and shapes, distributed event subscriptions, and additional legacy node and database routes. The Go /v1/* wire protocol and mixed Go/Rust Raft membership are outside scope.
- Hardened backup and disaster recovery, upgrade compatibility, additional packet loss and latency tests, larger topology and hardware validation, and checked Docker Compose deployment examples.

Phase 6 retains release distribution, GraphQL, final production acceptance, and tested Go import and rollback.
