# Phase 1 status

Phase 1 implementation and automated verification are complete for the Rust API foundation. Manual browser acceptance for existing UIs remains open. See the [browser regression results](UI_REGRESSION.md) and the [current API matrix](API_COMPATIBILITY.md) for behavior added since Phase 1.

## Implementation

- `GET /ping`, `GET /api`, `POST /api/login`, `GET /api/login/refresh`
- `GET /api/v3/config`, `PUT /api/v3/config`, `GET /api/v3/config/reload`
- `GET /api/swagger/index.html` and `GET /api/swagger/doc.json`
- Centralized legacy error envelopes, request IDs, structured logs, configurable verbosity and graceful SIGINT/SIGTERM shutdown.

The Axum adapter owns routes, wire DTOs, and error responses. App state coordinates the store and authentication. Configuration settings, JWT claims, and permission checks live outside the HTTP DTOs. SQLx owns SQLite access behind a store module, with independent SQL migrations. SQLite uses WAL, a bounded pool, private database permissions, and immediate transactions for configuration and administrator changes.

## Configuration and credentials

Startup creates the SQLite database and generates a stable instance ID and random 256-bit signing key. `CORE_DATABASE_PATH` selects a file and takes precedence over `CORE_DB_DIR`, which selects a directory for `restreamer.sqlite3`. The default directory is `./config`. Back up the database using SQLite’s online backup tools or after graceful shutdown.

Built-in defaults form the base. Stored settings overlay defaults, and environment overrides take precedence in the effective view. Partial PUT updates are serialized and committed transactionally. The config schema version is independent of both application semver and the SQL migration version. Legacy configuration payloads on PUT and old schema rows in SQLite are normalized to schema 3. Direct import of a Go configuration file or its underlying database is deferred.

| Setting | Environment |
| --- | --- |
| Listener | `CORE_BIND_ADDR` before `CORE_ADDRESS`. Default loopback at port 8080. `:8080` means all IPv4 interfaces |
| Identity and host | `CORE_ID`, `CORE_NAME`, `CORE_HOST_NAME`, `CORE_HOST_AUTO`, `CORE_UPDATE_CHECK` |
| Logs | `RUST_LOG` before `CORE_LOG_LEVEL` and `log.level`. `CORE_LOG_TOPICS`, `CORE_LOG_MAX_LINES` and its alias `CORE_LOG_MAXLINES` are represented |
| Auth | `CORE_API_AUTH_ENABLE`, `CORE_API_AUTH_DISABLE_LOCALHOST`, `CORE_API_AUTH_USERNAME` (alias `RS_USERNAME`), `CORE_API_AUTH_PASSWORD` (alias `RS_PASSWORD`), `CORE_API_AUTH_JWT_SECRET` |
| API policy | `CORE_API_READ_ONLY`, `CORE_API_ACCESS_HTTP_ALLOW`, `CORE_API_ACCESS_HTTP_BLOCK`. The HTTPS variants are represented for later TLS support |
| Deferred feature settings | `CORE_API_AUTH_AUTH0_ENABLE`, `CORE_STORAGE_CORS_ORIGINS`, `CORE_FFMPEG_BINARY`, `CORE_ROUTER_UI_PATH`, `CORE_TLS_ENABLE`, `CORE_RTMP_ENABLE`, `CORE_SRT_ENABLE`, `CORE_METRICS_ENABLE`, `CORE_CLUSTER_ENABLE` |

Set a 12–1024 byte `CORE_API_AUTH_PASSWORD` and optionally `CORE_API_AUTH_USERNAME` when starting a new authenticated instance. Credentials are persisted as an Argon2id hash, and supplying a different startup password rotates it. With no account configured, a local client can provision an administrator through configuration PUT. An external listener requires authentication or explicit `CORE_API_AUTH_ENABLE=false`. The loopback bypass trusts the socket peer. When proxying requests to a loopback listener, configure authentication and keep `CORE_API_AUTH_DISABLE_LOCALHOST=false`. Phase 4 added explicitly trusted proxy addresses and forwarded protocol handling. See [Phase 4 deployment](PHASE_4_STATUS.md#deployment).

The signing key is either generated and stored in a private database file or supplied at startup with `CORE_API_AUTH_JWT_SECRET` (minimum 32 bytes). The override stays outside the database and must be supplied on every restart to retain sessions signed with that value. Password or effective username changes through PUT revoke existing tokens. Login attempts are limited per peer and globally, and Argon2id work runs in a bounded blocking pool. The path, body and authorization header are never logged. Logs contain matched route templates and request IDs.

Configuration policy updates take effect on successful PUT. Reload revalidates effective settings, updates `loaded_at`, and applies the configured log level. For a listener change, reload binds the replacement before switching traffic and leaves the existing listener available when binding fails. The database is selected at startup by its environment settings, independently of the legacy document’s `db.dir` value. See the [API matrix](API_COMPATIBILITY.md) for exact contracts, remaining differences and deferred endpoint families.

## Automated verification

On Linux AMD64, formatting, Clippy with warnings denied, seven Rust tests, the locked release build and the HTTP/SIGTERM smoke test passed. The isolated differential run matched 24 contract checks against Go 1.27.4 and FFmpeg 7.1.5.

The native Rust suite covers discovery, the legacy fixture envelopes, credential validation and throttling, JWT claims and expiration, policy, versioned partial updates, concurrent writes, rollback on validation error, SQLite initialization, migration and persistence across restarts. The shell smoke test exercises the actual HTTP listener and SIGTERM shutdown. The native Linux AMD64 and ARM64 jobs both passed on implementation commit `d01b8d6`. [GitHub Actions run 45613193081](https://github.com/KingIronMan2011/restreamer-core/actions/runs/45613193081)

For differential testing, build the pinned Go reference with Go 1.27 and install FFmpeg/FFprobe. Run the Go binary in an empty disposable directory with `config/` and `data/` subdirectories. Run the Rust binary with a separate temporary database. Use distinct loopback ports and the same `CORE_API_AUTH_ENABLE=true`, `CORE_HOST_AUTO=false`, `CORE_UPDATE_CHECK=false`, `CORE_API_AUTH_USERNAME` and `CORE_API_AUTH_PASSWORD` on both. Provide the fixture credentials to the script via `PHASE1_TEST_USERNAME` and `PHASE1_TEST_PASSWORD`.

```sh
python3 scripts/differential-api.py http://127.0.0.1:18880 http://127.0.0.1:18881
```

The comparison covers route status and wire shapes, and normalizes version values, generated IDs, resource measurements and sanitized error details. It writes test configuration and requests reload on both instances.

## Manual UI acceptance

1. Check out each of the five UI commits listed in [BOOTSTRAP.md](BOOTSTRAP.md) in a separate directory. Use the lockfile and Node/package manager specified by each revision. The refactored owner snapshot uses pnpm. Record any build failure with the exact revision and tool versions.
2. Build the unchanged UI. Serve its built files on a disposable loopback origin, forwarding `/api` and `/api/*` to the Core on that same origin. Keep the browser Network panel open and preserve the request log. Configure a test administrator using the environment above and use a fresh browser profile or clear site storage for each run.
3. Confirm anonymous discovery omits instance identity and advertises local login. Submit an invalid password, then the valid password. Verify the 403 envelope for the failed attempt, both token fields on success, and full discovery with the access bearer token.
4. Reload the UI with its session cached. Observe the refresh GET with the refresh bearer and a reply containing only `access_token`. Leave the UI open through the scheduled renewal at about nine minutes and confirm it keeps using the refreshed bearer.
5. Record whether the UI issues `GET /api/v3/config` automatically. Where the version gate prevents this, use the Network panel or an authenticated HTTP client to exercise config GET, a partial update with `version: 3`, and reload. Verify the update and its persistence after restart. An authenticated HTTP check does not count as browser acceptance.
6. Record any compatibility banner and which dashboard calls receive 404. Compare the same UI against the pinned Go reference where the UI and installed FFmpeg versions allow it. Save only sanitized traces with tokens, passwords, signing keys and private hostnames removed.

The reference’s Auth0 and advanced IAM policy management, media and process engine, GraphQL, UI asset serving, production TLS, resource telemetry, and clustering are later-phase work. Existing UI semantic-version gates still block full app initialization, as detailed in the [API matrix](API_COMPATIBILITY.md).
