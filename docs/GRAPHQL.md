# GraphQL compatibility

Use `POST /api/graph/query` with a JSON request. Send an access JWT in `Authorization: Bearer …` when authentication is enabled. The same private-network policy applies as for API v3. Download the independently registered schema from `GET /api/graph/schema.graphql`. It includes the compatibility types, input objects, enums, custom scalars and `ping` mutation. `GET /api/graph` serves a local documentation page.

```graphql
query StreamStatus($id: ID!, $domain: String!) {
  ping
  about { app id version { number repository_commit } }
  process(id: $id, domain: $domain) {
    id
    state { state progress { frame fps } }
  }
}
```

In the JSON envelope, supply the document as `query` and values such as `{"id":"example","domain":""}` as `variables`. Use `operationName` when the document declares several operations. Logical GraphQL failures use `errors` alongside `data` when available. Invalid JSON returns HTTP 400. The authentication layer retains the legacy 401/403 error envelope.

| Operation | Rules and known differences |
| --- | --- |
| `ping`, `mutation { ping }`, `about` | Available to authorized API clients. Version metadata identifies Castvane. |
| `processes`, `process` | Uses the local v3 process inventory, glob filtering and IAM. Configuration, state, report and metadata are loaded when selected. Use the cluster REST API for remote state. |
| `probe` | Requires process write permission. A delegated account can probe only configurations already subject to delegated FFmpeg restrictions. Read-only mode refuses execution. |
| `log`, `metrics` | Require the corresponding v3 read permission. Metrics uses the enabled local metrics catalog. Samples return `[unix_seconds,value]`. |
| `playoutStatus` | Checks process access and input existence. The standard FFmpeg build has no Restreamer playout control interface. The resolver returns a GraphQL error. |

Limits include 128 KiB per request, a depth of 14, a complexity score of 192, bounded responses, eight in-flight GraphQL executions per Core and an execution deadline below the HTTP timeout. Query batching, uploads, persisted queries and subscriptions are outside the implemented compatibility surface.

The pinned reference returned HTTP 400 `transport not supported` for authenticated JSON queries even though the GraphQL landing page returned 200. Castvane implements the observed schema contract and provides working JSON transport. Runtime wire comparison of successful queries against that pinned binary is therefore unavailable. [Behavioral reference](https://github.com/KingIronMan2011/restreamer-core/tree/452e9429c1fd7e1a90d75ed12883171ecabd80eb/http/graph)
