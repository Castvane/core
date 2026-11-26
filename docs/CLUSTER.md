# Castvane cluster installation and administration

Standalone mode remains the default. For clustering, set CORE_CLUSTER_ENABLE=true and CORE_CLUSTER_CONFIG to a node JSON file. Configure API authentication and disable the localhost bypass. Use three or five voters across different failure domains. Install FFmpeg and FFprobe on media workers.

Create a dedicated offline cluster CA and one TLS keypair per node. Include both server and client authentication usages and the advertised node DNS name in its SAN. Install the CA and peers' public leaf certificates on each node. Keep the private key mode 0600 outside all media roots. Restrict the internal listener to the control network. Configure public API HTTPS separately.

## Three-node example

The following file configures the first voter. Copy the peer list to all three nodes. Change each local ID and keypair paths, and use an empty bootstrap array on the other two. Install the referenced certificates and replace the advertised names with actual DNS names.

    {
      "cluster": "video",
      "id": 1,
      "bind": "0.0.0.0:8020",
      "certificate": "/etc/castvane/cluster/node-1.pem",
      "private_key": "/etc/castvane/cluster/node-1.key",
      "ca": "/etc/castvane/cluster/ca.pem",
      "lease_ms": 15000,
      "bootstrap": [1, 2, 3],
      "peers": {
        "1": {"address": "https://node-1.example.test:8020", "certificate": "/etc/castvane/cluster/node-1.pem", "node": {"name": "node-1", "role": "combined"}},
        "2": {"address": "https://node-2.example.test:8020", "certificate": "/etc/castvane/cluster/node-2.pem", "node": {"name": "node-2", "role": "combined"}},
        "3": {"address": "https://node-3.example.test:8020", "certificate": "/etc/castvane/cluster/node-3.pem", "node": {"name": "node-3", "role": "control"}}
      }
    }

Use combined nodes for voting and media, control nodes for voting without media, and worker nodes for media without a vote. For dedicated control-plane deployment, set all three voter roles to control and provision separate workers. A worker is a non-voting learner. Include it in the trust files and enroll it through POST /api/v3/cluster/node/{id} on the leader. PUT /api/v3/cluster/membership accepts a complete set of provisioned voting IDs. The bootstrap list applies only to an empty durable Raft store. Listener, trust, peer bindings and lease changes require restart.

To reproduce the local four-node deployment with generated TLS material and real FFmpeg, run the following from the repository.

    cargo build --locked --release
    python3 scripts/test-cluster.py target/release/restreamer-core

## Operations

1. Use GET /api/v3/cluster to find the leader. Use an administrator token issued by the receiving node. Authoritative reads and writes require a leader with quorum.
2. Create jobs through POST /api/v3/cluster/process with a unique ID and FFmpeg configuration. Set autostart or issue a start command. The creation response reports the committed definition. Check the state route for actual worker progress.
3. Set a required numeric worker with placement.node, an optional preference with placement.preferred, or constraints through placement.zone, placement.labels, placement.protocols, placement.storage and placement.encoders. The reservation model uses millicores and bytes. FFmpeg CPU and memory limits also contribute to the reservation.
4. Request a specific destination through PUT /api/v3/cluster/reallocation with a body such as {"id":"stream","node":2}. A null node clears the required destination. To drain a member, set draining to true on its state route. Stop a process and wait for its reservation to clear before deleting its definition.
5. Administer the files on the assigned node through the node filesystem routes. Uploads through those routes are capped at 1 MiB. Play media on the assigned worker or use a shared object-storage mount. Set a peer playback_origin to expose its HTTPS origin through the process playback discovery route. The route confirms the running worker and epoch before returning that origin. Disk and memory files remain local to the node that wrote them.

Advertise verified hardware encoders through the node.hardware_encoders setting. Workers check visible GPU inventory and FFmpeg's compiled encoder list before advertising them. Specify GPU memory by discovered device identifier under placement.capacity.gpu_memory. Validate the actual device and encoder with the intended FFmpeg command on each hardware target.

Optional CPU rebalancing is disabled by default. To enable it, set rebalance to {"above_percent":90,"below_percent":60,"cooldown_ms":60000} in the cluster node file. Use the same settings on voters. The scheduler chooses one eligible destination for a process without a required node. It records the decision in Raft, stops renewing the old lease, and waits for the ownership handoff. Raft persists the cooldown across leader changes.

## Availability and recovery

Writes require Raft quorum. After a write timeout, read the committed revision before retrying. KV supports expected revisions. Process updates reject conflicting revisions.

Keep node clocks synchronized within one second. The worker stops before its assignment expires. The scheduler adds a three-second safety interval before changing the owner. Jobs can continue for the remainder of a valid lease during a temporary control-plane outage. Failover and relocation interrupt the stream. Media destinations have their own buffering and fencing behavior. Lease enforcement depends on timely worker scheduling and the supervisor's ability to stop the child process. Use delegated cgroups to contain descendants.

Use SQLite online backup or a copy after clean shutdown. Backups contain password hashes and may contain stream credentials and private KV entries. Restore a member only to the matching node and cluster identity. Use a newly provisioned learner for a replacement with no valid peer database. Preserve every surviving database and key if quorum is permanently lost. Automated quorum reconstruction remains open.

Cluster-managed identities replicate their hashes and policies. Receivers on followers and learners consult a quorum-backed read fence and deny an identity request while their applied index is behind. Tokens, signing keys and bootstrap administrators are node-local. The separate cluster CA and node keys authenticate internal RPC.
