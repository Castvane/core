# Third-party notices

Castvane Core is an independent MIT-licensed project. It implements a compatibility API for the datarhei Restreamer ecosystem. Castvane is unaffiliated with datarhei. The Go behavioral reference at commit `452e9429` is Apache-2.0-licensed and remains in its repository.

## Rust components

A native candidate includes a CycloneDX SBOM, declared package licenses in `DEPENDENCIES.tsv`, and the packaged notices and license files collected from crate source distributions. Run `cargo deny --locked check` against [deny.toml](deny.toml) to review the two Linux build graphs. Embedded SQLite is built by SQLx using the bundled SQLite source. OpenRaft and rustls are available under MIT or Apache-2.0. Async-GraphQL uses MIT or Apache-2.0. The AWS-LC family includes the licenses and notices named in its crate distributions. The full lockfile also resolves packages for platforms outside the supported builds.

## External media executables

Native Castvane packages ship the Core executable alone. FFmpeg and FFprobe execute as separate processes. They have their own licenses. An FFmpeg build may be LGPL or GPL depending on configuration. A build with `--enable-nonfree` is unsuitable for redistribution. Determine codec and patent obligations independently of copyright licensing. Consult [FFmpeg's legal page](https://ffmpeg.org/legal.html).

The Dockerfile can install Ubuntu's FFmpeg package during an operator build and preserves Ubuntu runtime copyright notices. It writes the FFmpeg configure flags and exact package/source versions under `/usr/share/doc/castvane-core/`. A public FFmpeg-bundled release additionally requires corresponding source preservation and an applicable distribution review. The candidate workflow exports a Core-only image while separately testing the bundled Docker build. Ubuntu packages in either image retain their Debian copyright files.

MediaMTX is an optional external MIT-licensed RTMP, RTMPS and SRT service. It is not included in the exported candidates. Integration fixtures provision MediaMTX, SeaweedFS, MinIO and Pebble as separate local test programs. See the scripts for versions and checksums. Project notices and separate dependency licenses remain applicable.
