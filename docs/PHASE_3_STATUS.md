# Phase 3 — Media and storage

The pinned behavioral reference is `vod` at `452e9429c1fd7e1a90d75ed12883171ecabd80eb`. The Rust implementation uses the legacy v3 control surface and the existing SQLite, authentication and process engine. Manual browser acceptance remains open.

## Architecture

- `media::storage` defines the async storage interface and key boundary. Disk uses directory capabilities, streamed temporary files, atomic rename and parent-directory synchronization. Memory publishes immutable reference-counted buffers and charges active readers against resident capacity. S3 uses the Apache Arrow `object_store` SDK, paginated object listing and bounded multipart upload on the same storage interface.
- `api::media_http` handles file publication, streaming HTTP GET and HEAD, single and multipart byte ranges, conditional requests, content types, CORS and request cancellation. Public mounts are `/` for disk, `/memfs` for memory and the configured mountpoint for each S3 bucket. Keep the SQLite file and TLS private key outside the disk media root. Lists and writes use separate bounds. The version-keyed disk cache excludes playlists and personalized responses. Large media and byte ranges stream without buffering complete files.
- `media::playlist` rewrites local HLS variant, segment, map, rendition and key URLs to retain viewer identity and existing query parameters. External origins receive no added session or token. FFmpeg can generate HLS using TS or fragmented MP4 and DASH using MPD, initialization and media segments. The server serves DASH manifests as generated. DASH uses a path-scoped session cookie so browsers can reuse a viewer across manifest and segment requests. Private DASH adds an HTTP-only media-token cookie for same-origin access.
- `media::transport` supervises an administrator-installed MediaMTX v1.21.4 companion. It handles RTMP/RTMPS ingest and reads, SRT publish/read, passphrases, and control calls for channel state and disconnection. Core performs authorization in a private HTTP callback and protects the companion control API with a random credential. The companion disables unrelated listeners, including the MediaMTX MoQ defaults. It runs in a private temporary working directory so generated files stay outside media mounts. Core remuxes an RTMP reader to streaming FLV using the configured FFmpeg.
- `media::sessions` accounts for active viewers, transfer bytes, expiry, scoped viewer tokens and bandwidth. HLS playlist and segment requests reuse the same opaque session identifier. Direct transport sessions come from the companion's connection inventory. Individual address/session histories are kept out of the database.
- `media::alias` validates ordered regular-expression rules and resolves local targets with a bounded number of redirects internally. SQLite migration `0003_media_aliases.sql` persists rule ordering. The first-match scan is capped at 128 rules. Regex input length and compiled program size are bounded.
- The Phase 2 process engine resolves `{diskfs}`, `{memfs}`, `{fs:mem}`, `{fs:name}`, `{rtmp}`, `{rtmp,name=stream}`, `{srt}` and `{srt,name=stream}` from runtime service availability. Names in braces refer to configured storage or channel names. Temporary credentials are used on local write URLs and redacted from process diagnostics. Restored jobs with local HTTP storage endpoints wait for the new listener. Cleanup rules are scoped to mounted storage and skip known outputs and patterns claimed by other configured processes.

## REST API

The methods below are registered under `/api/v3`. Control routes use Phase 1 authorization when API authentication is enabled. Write routes use the existing read-only policy. Transport routes require an active corresponding listener.

| Path | Methods | Contract |
| --- | --- | --- |
| `/fs` | GET, PUT | List storage and optional `details`. PUT takes `operation` (`copy` or `move`), `source`, `target` in `storage:/path` form, and optional `bandwidth_limit_kbit`. |
| `/fs/{storage}` | GET, DELETE | List or delete glob matches. Filters include `glob`, `size_min`, `size_max`, `lastmod_start`, `lastmod_end`, `sort` and `order`. List items have `name`, `size_bytes`, `last_modified`. Deletion requires a glob and returns file names. |
| `/fs/{storage}/{path}` | GET, HEAD, PUT, DELETE | File bytes and metadata headers. PUT accepts a byte stream and returns 201 on create or 204 on replacement. DELETE returns the legacy text result. |
| `/rtmp`, `/rtmp/channels` | GET | Active publisher names and publisher/subscriber details respectively. |
| `/rtmp/disconnect` | PUT | Disconnect by `path`. A literal `*` disconnects all. |
| `/srt`, `/srt/channels` | GET | Active SRT channels with legacy connection envelopes and available counters. |
| `/srt/disconnect` | PUT | Disconnect active SRT channels. |
| `/session`, `/session/active` | GET | Summary or active sessions by comma-separated `collectors`. |
| `/session/token/{username}` | PUT | Batch of viewer token requests with `match`, `remote`, `ttl_sec` and optional `extra`. |
| `/widget/process/{id}` | GET | Public uptime and viewer totals with optional `domain`. |
| `/alias` | GET, POST, PUT | List, add or replace ordered `{pattern,target}` rules. |
| `/alias/rule` | GET, PUT, DELETE | Read, update or delete a rule identified by the `pattern` query parameter. |
| `/process/{id}/playout/{input}/status`, `/reopen`, `/keyframe/{name}`, `/errorframe/encode` | GET | 404 when disabled or input is unknown. |
| `/process/{id}/playout/{input}/stream`, `/errorframe/{name}` | PUT, plus POST for error frame | 404 when disabled or input is unknown. |

Playout uses HTTP controls exported by Restreamer's extended FFmpeg, including live input reconfiguration, stream switching and error frames. For a known input with playout enabled, Rust returns 501 and identifies the dependency. Standard FFmpeg can switch sources through a v3 process update or restart and extract still frames through a separate process. Those operations interrupt playback or start a new execution. Live extension control is unavailable in this phase.

Swagger JSON is served at `/api/swagger/doc.json`. The corresponding public HTTP-FLV mount defaults to `/rtmp/{path}` when enabled.

## Deployment

Install FFmpeg, FFprobe and, for native ingest/playback, MediaMTX from trusted packages or pinned releases. Core installs no executables. Relative storage paths resolve from the working directory. Configure media listeners and mounts before startup. Persist configuration and disk media in separate writable volumes for container deployments. Changing mount, TLS, FLV executable, transport or session settings requires a Core restart.

```sh
export CORE_STORAGE_DISK_DIR=/srv/restreamer/media
export CORE_STORAGE_DISK_MAX_SIZE_MBYTES=16384
export CORE_STORAGE_MEMORY_MAX_SIZE_MBYTES=128
export CORE_STORAGE_MEMORY_AUTH_ENABLE=true
export CORE_SESSIONS_MAX_SESSIONS=128
export CORE_SESSIONS_MAX_VIEWERS_PER_STREAM=32
export CORE_MEDIA_TRANSPORT_BINARY=/usr/local/bin/mediamtx
export CORE_RTMP_ENABLE=true CORE_RTMP_ADDRESS=127.0.0.1:1935 CORE_RTMP_APP=/live
export CORE_RTMP_HTTPFLV_ENABLE=true
export CORE_SRT_ENABLE=true CORE_SRT_ADDRESS=127.0.0.1:6000
# Inject these from the deployment's secret manager before starting Core.
export CORE_RTMP_TOKEN CORE_SRT_TOKEN CORE_SRT_PASSPHRASE
restreamer-core
```

An ingest URL has the form `rtmp://127.0.0.1:1935/live/example?token=…`. The corresponding HTTP URL is `http://127.0.0.1:8080/rtmp/live/example?token=…`. A native SRT caller uses `streamid=publish:example:token=…` to publish and `streamid=read:example:token=…` to receive, with literal colons for interoperability with FFmpeg 6 and newer. Use an alphanumeric or URL-safe token for portable caller URLs. When a passphrase is configured, both sides need the passphrase and publisher encryption options. RTMPS uses `CORE_RTMP_ENABLE_TLS=true`, `CORE_RTMP_ADDRESS_TLS` and PEM files supplied through `CORE_TLS_CERT_FILE` and `CORE_TLS_KEY_FILE`. For HTTPS on the standalone executable, also set `CORE_TLS_ENABLE=true` and `CORE_TLS_ADDRESS`. Use a certificate whose names cover the client-facing endpoints. Set `CORE_TLS_TRUST_FILE` to the private CA used by Core’s FFmpeg HTTP-FLV reader in RTMPS-only deployments.

S3 can use AWS or a custom HTTPS endpoint. Supply bucket definitions in `CORE_STORAGE_S3_JSON` through a protected environment file or the deployment's secret manager. Fields are `name`, `mountpoint`, `bucket`, `region`, `endpoint`, `use_ssl`, `access_key_id` and `secret_access_key`. Example for an already-created R2 bucket with the last two fields supplied by the operator:

```json
[{"name":"archive","mountpoint":"/archive","bucket":"restreamer-media",
  "region":"auto","endpoint":"https://ACCOUNT_ID.r2.cloudflarestorage.com",
  "use_ssl":true,"access_key_id":"SUPPLIED_BY_OPERATOR","secret_access_key":"SUPPLIED_BY_OPERATOR"}]
```

Use a blank endpoint for AWS, or the MinIO service address for a self-hosted deployment. A bucket mount can require HTTP Basic authentication with `auth.enable`, `auth.username` and `auth.password`. Path-style addressing is the default. Set `virtual_hosted_style` to true for providers that require virtual-hosted requests. The legacy `CORE_STORAGE_S3` URL list is supported. The integration suite exercises SeaweedFS and a digest-pinned, isolated historical MinIO Community build. AWS and R2 deployments require provider acceptance. MinIO is a test fixture and is outside the runtime distribution.

Additional controls include `CORE_STORAGE_PRIVATE`, `CORE_STORAGE_UPLOAD_BITRATE_KBIT`, `CORE_STORAGE_DOWNLOAD_BITRATE_KBIT`, `CORE_STORAGE_CORS_ORIGINS`, disk cache `CORE_STORAGE_DISK_CACHE_*` settings, `CORE_SESSIONS_SESSION_TIMEOUT_SEC`, `CORE_SESSIONS_MAX_BITRATE_MBIT`, `CORE_SESSIONS_STREAM_BITRATE_KBIT` and `CORE_SESSIONS_SESSION_BITRATE_KBIT`. Zero bandwidth limits leave throughput unpaced. Zero session limits select bounded node defaults. Single API uploads are capped at 4 GiB. The media aliases and storage/configuration secrets are protected by the same filesystem and SQLite permissions as Phase 1. Prefer passing secrets through the deployment environment and protect server-side configuration and database backups.

## Interoperability and security boundaries

- FFmpeg/FFprobe and MediaMTX run as the service user. Run the service as a dedicated unprivileged user. Grant that user only the needed certificate, SQLite and media access. Apply the Phase 2 input/output allowlists and operating-system isolation when media or process configurations are untrusted. Disk path confinement applies to the Rust storage API. A direct FFmpeg disk output uses OS permissions and bypasses Rust's upload quota and atomic publication. FFmpeg HLS disk outputs should use `-hls_flags temp_file` and a deletion/retention policy. Remote bucket quotas are managed by the provider.
- HTTP bandwidth limits pace body delivery and uploads. RTMP/RTMPS/SRT limits sample connection counters and disconnect connections over a configured limit. The wire-protocol and HTTP budget counters are maintained separately. Session counts can briefly reflect the provider's poll interval. SRT low-level socket metrics and logs are limited to counters made available through the managed companion API. The old `/srt` socket number is a provider UUID projection. Legacy SRT shorthand outside the companion's accepted native/standard stream-ID formats needs migration.
- Encrypted SRT is verified with MediaMTX v1.21.4 and the Linux distribution FFmpeg fixtures. Gyan and BtbN FFmpeg 9 builds reproduced a packet decryption error during playback from MediaMTX. The companion's configured passphrases still apply to publishers and readers. Encrypted SRT playback is unsupported on native Windows, which is a development target. Linux keeps required encrypted SRT coverage. [Windows interoperability notes](WINDOWS.md#srt-interoperability)
- The published Castvane Alpine FFmpeg 9.1 image passed encrypted SRT publishing and playback on Linux AMD64 with MediaMTX v1.21.4. The tested platform manifest is `sha256:6d236aacbfd9f37f404ba6eff5f622a191f69ea611e578619fcfb0ad8154af6b` and reports build `9.1-castvane-9f01631`. It emits JSON progress and HLS statistics. The inspected image contains FFmpeg without FFprobe and exposes no Restreamer live playout control interface. Its license output identifies it as non-redistributable. Production distribution requires a redistributable build and another interoperability check against that artifact. [Castvane FFmpeg recipes](https://github.com/Castvane/ffmpeg/tree/ece7bdd0fdf60e865dc4c55c6bfb1dd1a02fb5ca)
- Global `CORE_STORAGE_PRIVATE=true` protects HTTP media with a Core access token or an expiring signed viewer token. RTMP, RTMPS and SRT listeners use their respective stream tokens. Set explicit stream secrets and bind the companion listeners to private addresses where possible. API Bearer tokens and viewer tokens have different purposes and claims. Session query strings should be excluded from reverse-proxy access logs.
- S3 keys and endpoints are configured per mount, with TLS certificate verification enabled for HTTPS. Explicit cleartext S3 endpoints are intended for isolated local services. Aliases cannot target external origins. Overlapping process output declarations are guarded during cleanup; unrelated tools writing into the same storage path are outside this ownership model.
- Core and the companion are MIT-licensed. Apache Arrow `object_store`, cap-std and rustls use permissive licenses (Apache-2.0 or the alternatives listed in their manifests). SeaweedFS is Apache-2.0 and used only by tests. FFmpeg is external and its license depends on build options. The reference Go implementation was used to inspect behavior and remains Apache-2.0.

## Verification and manual acceptance

Run `cargo test --locked --all-targets --all-features` for the application suite, `bash scripts/test-media-services.sh` for isolated object storage and real FFmpeg transport checks, and `cargo test --locked --test media benchmark_http_media -- --ignored --nocapture` for the repeatable loopback benchmark. The fixture script provisions checksum-pinned binaries for Linux amd64 and arm64. The benchmark exercises concurrent 4 MiB disk and memory responses, 1 MiB ranges and rewritten 500-entry HLS manifests. It records wall time and peak RSS of the combined test process. It provides no cross-implementation comparison.

The local Linux x86-64 test host exposed three AMD EPYC vCPUs and used FFmpeg 7.1.5. The unoptimized test executable served media to eight loopback viewers. Metrics include server and client work.

| Measurement | Result |
| --- | ---: |
| Full disk reads | 159.3 MiB/s |
| Full memory reads | 211.5 MiB/s |
| 1 MiB memory ranges | 245.3 MiB/s |
| HLS rewrites | 143 requests/s |
| 4 KiB publication, disk p50/p95 | 1.17 / 1.52 ms |
| 4 KiB publication, memory p50/p95 | 0.45 / 0.53 ms |
| 4 MiB download with four paced readers | 89.75 ms |
| Peak combined RSS | 105,320 KiB |
| RTMP, RTMPS and SRT playback plus six-frame decode | 6.06 / 6.12 / 6.05 s |

The transport fixture also reports elapsed time from FFmpeg launch to publisher visibility in the channel API. That measurement includes process startup and inventory polling. The recovery harness reports Core-only peak RSS under concurrent media and API traffic. One local fixture run reached publisher inventory in 1.97 seconds for RTMP, 1.82 seconds for RTMPS, and 0.32 seconds for SRT. Dedicated handshake overhead and cross-host throughput remain unmeasured. Repeatability depends on CPU and disk contention.

The local matrix ran formatting, Clippy with warnings denied, 29 non-ignored tests, the two service integration tests, the release build and the smoke test. The workflow runs these checks, including MediaMTX and S3 interoperability, on native AMD64 and ARM64. Both runners passed on `37b86ff`. Check the branch's current run for newer commits. [Verified native run](https://github.com/KingIronMan2011/restreamer-core/actions/runs/45775682315) · [Rust branch CI](https://github.com/KingIronMan2011/restreamer-core/actions/workflows/rust.yml?query=branch%3Arust).

`scripts/differential-media.py` passed ten comparisons against a locally running copy of the pinned Go reference. It covers storage discovery, uploads, listing, HEAD, ranges, replacement, alias rule management and missing-file error structure. Legacy public alias authorization can differ when Go IAM controls are enabled. RustSec reported no vulnerable dependencies and two unmaintained transitive crates (`chrono` and `humantime`) without fixed upgrades.

For browser acceptance with each pinned UI in [Bootstrap decisions](BOOTSTRAP.md), connect the unchanged UI to a disposable Core and sign in. Note any existing Core version gate. Create an FFmpeg source with a local fixture, check source and player configuration, then try HLS and DASH playback with a clean browser profile. Publish RTMP and encrypted SRT from a local FFmpeg process and try native playback as well as the HTTP-FLV view. Read channel and session statistics during simultaneous viewing, disconnect a publisher and reconnect. Inspect storage browsing, uploads, bounded reads, expired access, alias changes, process cleanup and unauthorized requests. Record exact UI/Core versions and browser behavior. Browser acceptance is unverified.

Phase 4 adds local IAM and HTTPS certificate automation. Distributed coordination, cluster failover, extended FFmpeg playout, reference-specific SRT telemetry and manual UI acceptance remain open.
