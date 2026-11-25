# Restreamer Core — Rust implementation

An independently implemented FFmpeg orchestration backend intended to be compatible with existing Restreamer user interfaces. This is an early development branch, not yet a replacement for the Go core.

## Development

Install stable Rust (edition 2024 or newer), then run:

```sh
cargo run
curl http://127.0.0.1:8080/ping
cargo test
```

The starter exposes only the existing plain-text liveness endpoint (`pong`). It binds to loopback by default; `CORE_BIND_ADDR` overrides the address. No media processing, authentication, or UI compatibility is implemented yet.

Read [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md) for the roadmap and compatibility contract. The Go reference remains on `vod` at commit `452e9429`.

## Licensing

The license for newly authored Rust code remains to be selected by the owner before distribution. No permission to redistribute the new code is implied by this draft. The reference Go project and its historical commits remain licensed under their original terms. Do not copy upstream implementation, documentation, or generated schemas into this tree without an explicit license review. Dependencies and FFmpeg retain their respective licenses.
