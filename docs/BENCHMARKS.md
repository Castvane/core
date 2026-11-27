# Castvane candidate benchmarks

## Standalone management comparison

Three repetitions per build on Linux AMD64, kernel 6.18.57, three assigned AMD EPYC 9V74 CPUs. Castvane used the Rust 1.100.0 locked release build at `04da01a`. The Go executable was built from pinned reference `452e9429` with Go 1.27.4. Each test used a separate empty database and media directory, loopback HTTP, no jobs, no external traffic and the same eight persistent HTTP clients. Each endpoint ran for two seconds per trial. Startup probes used 10 ms polling. RSS is the Core process's resident memory sampled after half a second of idle time.

| Measurement | Castvane median (range) | Go median (range) |
| --- | --- | --- |
| Launch to successful `/ping` | 94.50 ms (83.96–97.64) | 520.48 ms (510.94–643.06) |
| Idle RSS | 19,912 KiB (19,424–20,340) | 31,980 KiB (31,644–32,116) |
| Idle open file descriptors | 16 (14–18) | 10 (7–10) |
| CPU time across both HTTP workloads | 2.58 s (2.57–2.62) | 3.13 s (3.11–3.22) |
| `/ping` throughput | 10,650 requests/s (10,279–11,143) | 8,645 requests/s (8,233–9,367) |
| `/ping` p95 latency | 1.75 ms (1.65–1.81) | 2.26 ms (2.06–2.38) |
| `/api` discovery throughput | 7,358 requests/s (7,130–7,706) | 8,465 requests/s (8,252–8,948) |
| `/api` discovery p95 latency | 2.25 ms (2.23–2.37) | 2.35 ms (2.23–2.43) |
| Binary bytes | 43,597,488 | 58,547,705 |

Castvane started about 5.5 times as quickly, used about 38% less idle RSS, and served about 23% more liveness requests per second in this fixture. Go served about 15% more discovery requests per second. Response bodies and CPU work differ across implementations, even when the route and load generator match. Binary build settings differ. These short measurements cover the management layer. They provide no evidence of faster FFmpeg transcoding, physical hardware behavior, or performance for real deployments.

```sh
python3 scripts/benchmark-management.py target/release/restreamer-core \
  --reference /path/to/pinned-go-core --repetitions 3 --clients 8 --seconds 2
```

## Dependency-upgrade rerun

The locked release build at `22122f4` and the same pinned Go executable ran the three-trial management fixture on Linux AMD64 with three assigned CPUs. Each run used eight clients and two seconds per endpoint.

| Measurement | Castvane median (range) | Go median (range) |
| --- | --- | --- |
| Launch to successful `/ping` | 114.58 ms (105.41–120.12) | 445.51 ms (441.75–560.48) |
| Idle RSS | 20,632 KiB (20,468–20,648) | 31,324 KiB (31,208–31,712) |
| `/ping` throughput | 10,366 requests/s (10,272–10,827) | 9,468 requests/s (9,220–9,645) |
| `/api` discovery throughput | 7,028 requests/s (6,842–7,930) | 9,653 requests/s (8,681–9,873) |
| Binary bytes | 44,700,880 | 58,547,705 |

Startup and request timings vary between runs. These measurements cover synthetic loopback management traffic.

## Candidate-only workloads

The release binary completed the [60-second recovery and four-node cluster measurements](PHASE_6_STATUS.md#automated-verification) with FFmpeg 7.1.5. Cluster times include lease and scheduling settings. No Go cluster workload was run.

Selected Rust test-profile fixture results on the same host were 254 MiB/s disk and 409 MiB/s memory full-file loopback delivery with eight viewers. Memory-backed ranges reached 330 MiB/s. HLS playlist rewriting handled 171 requests/s. Median 4 KiB publication was 1.44 ms on disk and 0.66 ms in memory. A normal 4 MiB download completed in 97 ms with four concurrent paced viewers. Four separate FFmpeg jobs generated 96 frames in 1.80 s. With 12 IAM rules the policy fixture evaluated about 305,000 decisions/s. Host sampling averaged 0.256 ms. Observed throughput varied with short-run scheduling and synthetic payloads.

The operations fixture measured 848 configuration reads/s and 1,028 metric scrapes/s with eight clients. Debug Argon2id login took about 448 ms including API and SQLite. The bounded log/event fixture captured around 181,000 events/s. Media throughput in the no-scrape and scrape trials was 217 and 238 MiB/s respectively. The single sample per mode cannot establish the cost of monitoring.

```sh
cargo test --locked --test media benchmark_http_media -- --ignored --nocapture
cargo test --locked --test operations benchmark_parallel_management_and_metrics -- --ignored --nocapture
cargo test --locked --test process_engine four_concurrent_ffmpeg_jobs -- --ignored --nocapture
cargo test --locked --lib benchmark_ -- --ignored --nocapture
python3 scripts/test-recovery.py target/release/restreamer-core --seconds 60 --clients 8
python3 scripts/test-cluster.py target/release/restreamer-core
```

The MediaMTX interoperability fixture observed publisher launch to channel discovery at 1.87 s for RTMP, 1.84 s for RTMPS and 0.27 s for SRT. Playback plus a six-frame decode took 6.05 s, 6.14 s and 6.08 s respectively. The fixture uses local generated streams and includes FFmpeg launch, buffering and decode time. S3 multipart and range functionality passed on isolated services. No comparative S3 latency or saturation measurements were taken.

No 24-hour or 72-hour runs, physical GPU runs, Go media throughput comparison, or large-topology cluster capacity claims are included.
