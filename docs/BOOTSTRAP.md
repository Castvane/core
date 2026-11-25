# Bootstrap decisions

## Release and API versions

The Rust package version starts at `0.1.0`. It is independent of the legacy public API version (`/api/v3`) and the reference Go release (`16.20.0`). Phase 1 will determine what version metadata existing UIs expect during discovery; do not claim a compatible core version until the corresponding behavior works. Future breaking public API changes belong under `/api/v4`, not in v3.

## Toolchain and delivery targets

- Rust `1.100.0`, edition 2024, with rustfmt and Clippy. The exact toolchain is set in `rust-toolchain.toml`; dependency resolution is committed in `Cargo.lock`. Update them through reviewed changes.
- Primary target: Linux `x86_64-unknown-linux-gnu` (amd64). Secondary target: Linux `aarch64-unknown-linux-gnu` (arm64), including 64-bit Raspberry Pi installations when their OS and FFmpeg provide the required capabilities.
- CI builds, checks, and exercises the executable on native amd64 and arm64 Ubuntu 24.04 runners. These are bootstrap targets, not a declaration of tested media or hardware acceleration support.
- Initial delivery will support standalone Linux binaries and OCI/Docker containers. Windows, macOS, 32-bit ARM, and musl are not release targets initially. Expand only after platform-specific process management and media tests exist.
- The bootstrap server binds to `127.0.0.1:8080` by default; Docker deployment will explicitly opt into an external bind address when ready. SIGINT/SIGTERM request graceful shutdown.

## FFmpeg distribution

- Keep FFmpeg and FFprobe as separate executables; do not statically or dynamically link libav* into the MIT core. Do not download executable code at runtime.
- For standalone installations, the administrator supplies patched/supported FFmpeg and FFprobe through the OS package manager or a trusted custom build. Discover executable paths using compatible configuration (including legacy `CORE_FFMPEG_BINARY`) in Phase 2. Treat FFprobe as a matching companion executable; verify features, not only version strings.
- Start development with the distribution's maintained FFmpeg 7.1.x build as a reference. Compatibility with other versions, custom FFmpeg extensions, RTMP/SRT, playout, and GPU codecs is a Phase 2–4 validation requirement, not a promise of universal support.
- No FFmpeg binaries are included in the Rust source tree or bootstrap release. A future container image may include distribution-packaged FFmpeg only after recording exact versions/build configuration, licenses, copyright notices, corresponding source availability, and SBOM. Do not distribute non-redistributable FFmpeg builds; evaluate codec patents and jurisdiction-specific obligations before commercial distribution.
- The resulting binaries and all dependencies keep their own licenses. See [FFmpeg licensing guidance](https://ffmpeg.org/legal.html). MIT applies to independently authored Rust code only.

## Pinned compatibility references

Use the Go reference repository without importing its implementation into the new Rust tree. The reference is `KingIronMan2011/restreamer-core`, `vod`, commit `452e9429c1fd7e1a90d75ed12883171ecabd80eb`. Keep it separately runnable in an isolated test environment; do not modify it during comparison.

Public UI releases from [Restreamer UI](https://github.com/datarhei/restreamer-ui) and the owner's fork have matching tag refs at the time of this inventory. The initial target is compatibility across the available v1 release series, with the following pinned representative targets for the first browser regression suite:

| UI | Ref | Commit | Purpose |
| --- | --- | --- | --- |
| Upstream UI | v1.0.0 | `07996860851d6906711260cb077cb3f2f03158ff` | Oldest released baseline |
| Upstream UI | v1.8.0 | `3d9d663e35005b9aa2a5811fadb577eb70181100` | Intermediate release |
| Upstream UI | v1.14.0 | `c1a9c715a4a2947fe84f789e9126e02955b0fcd7` | Latest tagged release in this inventory |
| Upstream UI | default HEAD | `7164b38cb3a8655f7cd896e941093a1ca166b19b` | Current upstream development snapshot |
| KingIronMan2011/restreamer-ui | main | `69feb33cedc8a93754cc0c8d7895221cfe16803f` | Owner's current UI snapshot |

Phase 1 must inventory which API contracts these versions actually use. Expand the release regression matrix to all published v1 tags (v1.0.0–v1.14.0, including v1.5.1) before claiming support for that range. Older UI versions must not be modified to make tests pass. Build historical releases in isolated test environments, using their own locked dependencies; do not expose those reference installations publicly.

Test discovery/login/refresh first; add process creation, playback, configuration and administration as each phase lands. If a historical UI cannot build or run, record the exact reason and use an independently authored API fixture until that UI can be exercised. That is not equivalent to a passing browser test.

## Bootstrap exit criteria

- [x] MIT license and Cargo package metadata.
- [x] Minimal Axum/Tokio server, legacy plain-text `/ping`, unknown routes do not return fake success, graceful shutdown.
- [x] Pinned Rust toolchain and committed dependency lockfile.
- [x] Pinned CI action and locked lint/test/release build commands on both target architectures.
- [x] Supported platform, FFmpeg distribution and reference UI decisions recorded.
- [x] Local format, Clippy, test, release build and HTTP/SIGTERM smoke verification on Linux amd64 (2 unit/contract tests; real HTTP smoke test).
- [x] Remote CI verification on amd64 and arm64: [run 45606969113](https://github.com/KingIronMan2011/restreamer-core/actions/runs/45606969113), both jobs passed for commit `47e4cae7`.

CI runner labels are documented in [GitHub-hosted runners](https://docs.github.com/en/actions/reference/runners/github-hosted-runners). Phase 0 is complete. See [Phase 1 status](PHASE_1_STATUS.md) for the v3 foundation and its verification.
