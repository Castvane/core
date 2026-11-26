# REST API compatibility matrix

The behavioral reference is the Go `vod` commit [452e9429](https://github.com/KingIronMan2011/restreamer-core/tree/452e9429c1fd7e1a90d75ed12883171ecabd80eb). The route inventory comes from its HTTP registration and handlers. The Phase 1 fixtures were independently authored from source inspection and an isolated Go instance. UI pins are listed in [BOOTSTRAP.md](BOOTSTRAP.md).

## Discovery, authentication and configuration

All JSON successes use `application/json`. Clients send `Authorization: Bearer <JWT>` with the access token unless the row says otherwise. They send `Content-Type: application/json` on JSON writes. The discovery, login, refresh and configuration routes listed here use no query parameters. The reference can use `apidomain` for some delegated IAM policies. Castvane applies local account policies and route-level resource scopes. Neither query-string session tokens nor Basic authentication authorize management API calls.

| Method and path | Request and authorization | Success | Error and availability |
| --- | --- | --- | --- |
| GET `/ping` | Public, no body | 200, bytes `pong`, `text/plain; charset=UTF-8` | Independent of auth and state |
| GET `/api` | Public or optional bearer | 200 discovery document | 401 invalid or expired bearer, 403 wrong token purpose or disallowed network |
| POST `/api/login` | JSON object with string `username`, `password` | 200 object with string `access_token` and `refresh_token` | 403 invalid credentials or malformed request, 429 rate limited with `Retry-After`. Auth0 exchange belongs to Phase 4 |
| GET `/api/login/refresh` | Refresh bearer, no body | 200 object with string `access_token`, no refresh field | 401 invalid or expired bearer, 403 missing token or access token used as refresh |
| GET `/api/v3/config` | Access bearer or explicitly permitted legacy bypass | 200 configuration envelope | 401 invalid or expired bearer, 403 absent credentials or policy failure |
| PUT `/api/v3/config` | Same auth, writable API, partial JSON configuration with integer `version` 1, 2 or 3 | 200 JSON string `"OK"` | 400 invalid syntax, field type or schema version. 409 validation map. 405 in read-only mode |
| GET `/api/v3/config/reload` | Same auth, writable API | 200 JSON string `"OK"` | 404 in read-only mode, 409 if the listener cannot bind. Applies reloadable state and updates `loaded_at` |
| GET `/api/swagger/index.html`, `/api/swagger/doc.json` | Access bearer or legacy bypass | 200 HTML landing page or OpenAPI 3.1 JSON | Same auth policy as configuration. Rust additionally redirects `/api/swagger` and `/api/swagger/` to the index |

General failures are objects with integer `code`, string `message` and a `details` array of strings (empty when no public detail is available). Examples include `403 Forbidden` with `["access denied"]`, `["invalid username or password"]`, or `["invalid token"]`. Malformed input details are intentionally generic and do not echo the submitted JSON. Configuration conflicts have the separate legacy object-of-string-arrays shape, keyed by dotted configuration field. Rust adds `X-Request-ID`, `Cache-Control: no-store` and `X-Content-Type-Options: nosniff`. Overloaded or timed-out requests have 503 or 408 envelopes.

### Discovery and tokens

The minimal response contains `app` (always `"datarhei-core"`), `auths` (array of strings), and `version.number` (major formatted as semver, currently `"0.0.0"`). `auths` includes `"localjwt"` when local credentials exist and is otherwise empty. The full response adds these fields.

| Field | Type and presence |
| --- | --- |
| `id`, `name` | String. Omitted in minimal discovery |
| `created_at` | RFC 3339 string for service startup. Omitted in minimal discovery |
| `uptime_seconds` | Nonnegative integer since startup. Omitted in minimal discovery |
| `version.number` | Application semver, currently `0.1.0`. Independent of v3 |
| `version.repository_commit`, `repository_branch`, `build_date`, `arch`, `compiler` | Build strings, omitted in minimal discovery. Build date is empty and commit is empty when unavailable |
| `resources` | Object, omitted in minimal discovery. Boolean `is_throttling`, numeric `ncpu`, `cpu_used`, `cpu_limit`, `cpu_core`, `memory_used_bytes`, `memory_limit_bytes`, `memory_total_bytes`, `memory_core_bytes`, array `gpu`. Castvane populates CPU and memory data when available. For device inventory and optional counters use `GET /api/v3/resources` |
| `capabilities` | Rust extension, present in both forms. Integer `api_version` (3), booleans `configuration`, `configuration_write`, `process_engine`, `local_login`, `media`, `clustering`. The last is false |

HS256 JWTs carry string `iss` (`datarhei-core`), `sub` (local account username), `usefor` (`access` or `refresh`), and `jti` (UUID). Integer `iat` and `exp` use Unix seconds. Integer `exi` is lifetime in seconds, 600 or 86,400 by default. Both lifetimes are configurable. Rust adds a string `generation` claim for credential revocation. Existing installations must log in again when moving between Go and Rust. The UI caches and reuses the same refresh token until it expires.

### Configuration schema

GET returns `created_at`, `updated_at`, `loaded_at` RFC 3339 strings, a `config` object, and `overrides` as an array of dotted field names, possibly empty. The PUT body is the inner configuration object. The table groups fields by type and links to the independently authored [default document](../src/config-defaults.json) for every nested field and default.

| Configuration section | Shape and effect |
| --- | --- |
| Node | `version` integer, `id`, `name`, `address`, `created_at` strings, `update_check` boolean. ID and timestamps are generated. Update checks are deferred |
| `log`, `db`, `host` | Log level string, topic string array and line count integer. DB directory string. Host names string array and auto-detection boolean |
| `api` | `read_only` boolean. HTTP/HTTPS allow and block arrays of CIDR strings. Nested auth booleans `enable`, `disable_localhost`, username/password strings, JWT secret string, and Auth0 settings with tenant objects. HTTP and HTTPS policy and local JWT are active. Auth0 remains deferred |
| `tls`, `compress` | TLS listener and certificate settings are active, including local ACME HTTP-01. Legacy response compression settings are retained without an equivalent configurable middleware |
| `storage` | Disk, memory, cache, S3, CORS and MIME mapping settings are active. Provider coverage is recorded in the media status |
| `rtmp`, `srt`, `ffmpeg`, `playout` | FFmpeg and FFprobe selection, RTMP/RTMPS, SRT, process settings and secrets are active. Live playout requires the reference FFmpeg extension and returns 501 for an enabled known input |
| `debug`, `metrics`, `sessions`, `service`, `router`, `resources`, `cluster` | Metrics, viewer sessions and node resource controls are active. Legacy Go profiler, external service, bundled UI serving and Raft settings have no runtime equivalent |

Omitted fields are preserved. JSON null on scalar and nested struct fields leaves the value unchanged. Null clears array fields to null, while `[]` stores an empty array. Map values are merged and unknown struct members are ignored. The default route map is empty. Null clears a map. Empty credential fields on a GET are redactions. A blank API password on PUT preserves the current hash. TLS, RTMP, SRT, service and S3 credentials are redacted at the HTTP boundary. Environment secrets do not appear in the override list as values.

Schema 1 uses the RTMP listener as the TLS address when TLS is enabled. Migration sets the plain RTMP port to one less than the TLS port. Schema 1 and 2 use a disk cache extension array, migrated to `storage.disk.cache.types.allow`. Schema 3 uses `types.allow` and `types.block`.

## UI initialization by pin

All five clients call `GET /api` and use presence of an instance ID to tell full discovery from login-required discovery. They restore a cached session through `GET /api/login/refresh`, send the refresh token in the bearer header, and use `exi` to schedule renewal. Local login uses the same JSON and token fields on all five. Automated browser observations are recorded below. Owner acceptance remains open.

| UI pin | Core / FFmpeg version checks | Initialization differences |
| --- | --- | --- |
| v1.0.0 `07996860` | Core ^15 or ^16, FFmpeg ^4.1 | Loads config, skills, then process metadata. Older client uses RTMP and HLS configuration |
| v1.8.0 `3d9d663e` | Core ^16.11, FFmpeg ^5.1 | Same discovery, login and core initialization routes |
| v1.14.0 `c1a9c715` | Core ^16.11, FFmpeg ^5.1 or ^6.1 | Same core initialization and additional protocol channel calls |
| Upstream HEAD `7164b38c` | Core ^16.11, FFmpeg ^5.1 or ^6.1 | Same protocol calls and core initialization as v1.14 |
| Owner main `69feb33c` | Core ^16.11, FFmpeg ^5.1/^6.1/^7/^8.1/^9 | Pre-checks Core version inside initialization before loading config, process metadata, then skills. Additional protocol and filesystem views |

The first admin/dashboard calls beyond discovery and auth are configuration, skills and `GET /api/v3/process` with metadata filtering. Further dashboard pages use global `restreamer-ui` metadata, session collectors, files, reports, logs and process state. The media player can call `GET /api/v3/widget/process/{id}` independently of the management page. Castvane reports its own application version. `GET /api/v3/skills` reports the installed FFmpeg and FFprobe capabilities. The production browser check still prevents the verified upstream pins from reaching dashboard configuration.

## Browser observations and route coverage

Run the unchanged pinned fixtures with the [browser harness](UI_REGRESSION.md).

The process, media and operations rows are available with the limits recorded in [Phase 2](PHASE_2_STATUS.md), [Phase 3](PHASE_3_STATUS.md) and [Phase 4](PHASE_4_STATUS.md). Cluster and GraphQL rows are planned. Paths in this table are relative to `/api/v3` unless the row specifies another base.

| Phase | Method and path groups | Reference contract and conditions |
| --- | --- | --- |
| 2 | GET `/skills`, `/skills/reload`, `/process`, `/process/{id}`, `/process/{id}/{config,state,report,probe}`, `/report/process` | FFmpeg inventory, process list and per-process state/probe/report data. Process list supports `id`, `reference`, `filter`. Report search uses query filters |
| 2 | POST `/process`, `/process/{probe,validate}`. PUT/DELETE `/process/{id}`. PUT `/process/{id}/{command,report}` | JSON process definitions, command object and report changes. Admin mutations gated by read-only mode |
| 2 | GET `/metadata[/{key}]`, `/process/{id}/metadata[/{key}]`. PUT corresponding keyed paths | JSON metadata scoped to Core or process; unknown key/process can return 404 |
| 3 | GET `/fs`, `/fs/{storage}`, `/fs/{storage}/{path}`. HEAD file path. PUT `/fs` and file path. DELETE folder listing or file | Storage discovery, list and filter by `pattern`/sort, raw file contents, downloads and mutations |
| 3 | GET `/rtmp`, `/rtmp/channels`, `/srt`, `/srt/channels`. PUT protocol `/disconnect` | Channel information and disconnect operations for enabled listeners |
| 3 | GET `/session`, `/session/active`, `/widget/process/{id}`. PUT `/session/token/{username}` | Session summaries including `collectors` query, media authorization tokens and player-facing data |
| 3 | GET `/process/{id}/playout/{inputid}/{status,reopen,keyframe/*,errorframe/encode}`. POST/PUT playout `errorframe/*`. PUT playout `stream` | Playout proxy control and image/binary operations |
| 3 | GET/HEAD media mounts `/{path}`, `/memfs/{path}`, configured S3 mounts. Optional write methods and configured HTTP-FLV mount | Byte ranges, MIME and cache headers. Optional `filename` query affects downloads. HLS playlists and segments, DASH manifests and segments, and `token` query for media session authorization |
| 4 | GET `/iam/user[/{name}]`. POST `/iam/user`. PUT `/iam/user/{name}[/policy]`. DELETE `/iam/user/{name}` | Local identities and scoped policies. Auth0 is deferred |
| 4 | GET `/alias`, `/alias/rule`, `/zone[/{id}]`. POST/PUT aliases and zones. DELETE alias rule or zone | Routing/zone inventory and mutation |
| 4 | GET `/log`, GET/POST `/metrics`, POST `/events`, `/events/log`, `/events/media/{type}`, `/events/process`. Unversioned optional GET `/metrics` | Logs, metrics schema/query with Prometheus integration, and bounded event subscriptions |
| 5 | GET `/cluster`, `/cluster/{healthy,snapshot,deployments}`. GET and applicable mutations under `/cluster/{process,node,iam,alias,zone,fs,db,events}`. PUT `/cluster/{transfer/{id},leave,reallocation}` | Conditional cluster control plane and replicated entities. Separate cluster service `/v1/*` is evaluated in Phase 5 |
| 6 | GET `/api/graph`, POST `/api/graph/query` | Playground and GraphQL JSON request with `query`, optional `operationName` and `variables`. GraphQL field-level coverage tracks the underlying features in Phases 2–5 |

`/api/v4` is reserved and contains no endpoints.

## Phase 3 media routes

See the [media API matrix](PHASE_3_STATUS.md#rest-api) for methods, query fields, storage and alias contracts, feature gates, media mounts and known interoperability limits.

## Differential regression

Run all four suites against locally built binaries in temporary environments. Supply a Go binary built from the pinned behavioral reference and install FFmpeg and FFprobe.

```sh
bash scripts/test-differential.sh /path/to/pinned/go/core target/release/restreamer-core
```

The follow-up suite passed 75 checks across discovery, configuration, process control, storage, IAM, zones, event stream negotiation and the process-state metric descriptor.
