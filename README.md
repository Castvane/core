# Castvane Core

Castvane Core is an independent MIT-licensed Rust engine for FFmpeg orchestration and media delivery. It implements the legacy Restreamer v3 control API. The executable retains the `restreamer-core` name for deployment compatibility.

## Development

Install Rust using rustup (the repository pins the toolchain), then run:

```sh
cargo run --locked
curl http://127.0.0.1:8080/ping
cargo test --locked
```

The server binds to loopback by default. It exposes liveness, legacy discovery, local JWT login and refresh, persistent v3 configuration, and OpenAPI documentation. Set `CORE_API_AUTH_PASSWORD` to a strong password to provision the administrator and enable login. `CORE_API_AUTH_USERNAME` defaults to `admin` when provisioning. `CORE_DATABASE_PATH` selects the SQLite file and `CORE_BIND_ADDR` overrides the listener.

Install FFmpeg and FFprobe to run single-node processes. The v3 process API supports lifecycle commands, probing, progress, reports, schedules and restart reconciliation. Use `CORE_FFMPEG_BINARY` or `CORE_FFPROBE_BINARY` to select custom executable paths. See [Phase 2 status](docs/PHASE_2_STATUS.md) for the contracts and deployment settings.

The media service supports disk, memory and S3 mounts, HTTP byte ranges, HLS and DASH, viewer tokens and filesystem aliases. Install MediaMTX to enable RTMP, RTMPS, SRT and HTTP-FLV. See [Phase 3 status](docs/PHASE_3_STATUS.md) for deployment examples, test commands and compatibility limits.

The operations layer adds managed identities, per-resource rules, zone reservations, bounded logs, audit events, health checks, TLS certificate replacement, and optional ACME HTTP-01. See [Phase 4 status](docs/PHASE_4_STATUS.md) for configuration and compatibility coverage.

For opt-in multi-node operation, see [cluster installation](docs/CLUSTER.md) and [Phase 5 status](docs/PHASE_5_STATUS.md).

See [Phase 1 status](docs/PHASE_1_STATUS.md) for authentication, configuration and manual UI testing. The [compatibility matrix](docs/API_COMPATIBILITY.md) describes the wire contract and UI initialization limits.

Read [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) for the roadmap and compatibility contract. The Go reference remains on `vod` at commit `452e9429`.

See [bootstrap decisions](docs/BOOTSTRAP.md) for the Linux amd64/arm64 target policy, FFmpeg distribution model, pinned reference UI versions, and verification checklist.

For unsigned native and container candidates, backup, and rollback instructions, see [deployment and recovery](docs/DEPLOYMENT.md). The [browser regression report](docs/UI_REGRESSION.md) records the behavior of the unchanged Restreamer UI pins.

## Licensing

The independently authored Rust implementation is licensed under the [MIT License](LICENSE). The reference Go project and its historical commits remain licensed under their original terms (Apache 2.0). Do not copy upstream implementation, documentation, or generated schemas into this tree without an explicit license review. Dependencies and FFmpeg retain their respective licenses.
