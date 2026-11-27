# Phase 2 status

Phase 2 implementation and automated verification are complete. The single-node Rust Core can create, probe, run, stop, restart, and delete real FFmpeg jobs through API v3. It persists desired state, configuration, metadata and completed reports. Browser acceptance is tracked separately.

## Architecture and persistence

- The v3 adapter translates legacy configuration, state, probe and report DTOs. Requests continue through the Phase 1 JWT, network policy and read-only checks.
- The manager serializes graph mutations and publishes committed changes to per-process supervisors. Each supervisor owns at most one child, waits on a coalescing intent channel, and reaps the old child before starting its replacement. A bounded event channel publishes state transitions. Process mutations keep their queue slot through completion even if the HTTP client disconnects.
- The command builder resolves references and placeholders at launch, checks access policies, and passes argument vectors to an administrator-configured executable. Probing and capability discovery use separate bounded subprocess captures.
- Two independent readers drain FFmpeg progress and stderr. Reports keep a bounded ring of recent stderr lines, log-pattern counters, exit reason, final progress and sampled resources.
- Migration `0002_process_engine.sql` adds jobs, per-process and core metadata, and indexed run histories. Jobs have stable internal IDs and unique `(domain, id)` keys. Rename preserves the internal ID, metadata and history. Foreign keys remove process-owned rows on deletion. Config updates and embedded metadata commit together. Startup applies SQLx migrations, loads the graph, and starts workers from persisted order without restoring stale PIDs or runtime measurements. Use one standalone Core instance per database.

The pinned Go reference stores process configuration and order through its `restream/store` contract. Its task layer resolves addresses, separates process order from execution, and delegates to the FFmpeg and process packages. The Rust engine keeps that HTTP distinction while using SQLite, Tokio, FFprobe JSON and standard FFmpeg progress. The contract was checked against [Go commit 452e9429](https://github.com/KingIronMan2011/restreamer-core/tree/452e9429c1fd7e1a90d75ed12883171ecabd80eb).

## Lifecycle and recovery

| v3 field or state | Meaning |
| --- | --- |
| `order=start/stop` | Durable desired order. Create defaults to start unless `autostart=false`. Updates preserve order. |
| `finished` | Idle, a normal exit, or a graceful stop. A job waiting for its first schedule is idle with a nonnegative reconnect countdown. |
| `starting`, `running`, `finishing` | Waiting for capacity or starting, a live child, or graceful termination in progress. |
| `failed`, `killed` | Spawn error or unsuccessful exit, and signal exit or forced kill respectively. |
| `reconnect_seconds` | Seconds until a pending start or retry. `-1` indicates no retry timer. |
| `runtime_seconds`, `pid` | Elapsed child runtime and PID. Idle PID is `-1`. |
| `restart_count`, `exit_code`, `exit_reason` | Rust extensions for automatic retries since Core startup and the most recent exit. Finished-run details persist in history. |

Intentional stop cancels schedules and retries. Restart and reload commands replace an ordered job immediately. Repeated start on an active or waiting job is idempotent. Updates that change a running job replace it, and `force=restart` also replaces an unchanged ordered job. An unscheduled reconnect-enabled job retries unexpected exits and can loop a normally completed finite source. The default retry budget is ten with a one-second minimum delay, exponential growth, and a cap of 300 seconds or the configured initial delay, whichever is greater. Re-arming the job resets its retry budget. Recurring schedules re-arm on the next occurrence. Shutdown terminates the children and preserves their desired order for the next launch.

`scheduler` accepts an RFC 3339 timestamp or cron. Cron is evaluated in UTC. A missed one-shot or missed cron tick during downtime is skipped. An overlapping tick is skipped while its prior child runs. A one-shot can retry after failure if reconnection is enabled. Cancel by stopping the job or update the schedule through PUT.

## API contracts

All paths below are relative to `/api/v3`. Requests use Phase 1 authorization, with an access token when authentication is enabled. Read-only mode removes process and metadata mutation routes. JSON payloads use `application/json`. Errors use the legacy `code`, `message`, and `details` envelope. Missing jobs or keys are 404, duplicates are 409, malformed or invalid configuration is 400, a denied endpoint address is 403, and saturated capacity or unavailable media executables can return 503.

| Method and path | Behavior |
| --- | --- |
| GET `/skills`, GET `/skills/reload` | Cached executable capabilities or fresh scan. The result includes version, build compiler/configuration, linked libraries, codecs, encoder and decoder IDs, devices, formats, filters, protocol directions and advertised hardware accelerators. |
| GET `/process` | List jobs. Supports `id` and `reference` with ID taking precedence, plus `idpattern`, `refpattern`, `ownerpattern`, `domainpattern`, `zonepattern`. `filter=config,state,report,metadata` selects optional sections. Rust additionally accepts `offset` and `limit`. |
| POST `/process` | Creates a job and returns the normalized configuration. Blank endpoint IDs are assigned. Configuration properties include `type`, `input`, `output`, `options`, `reconnect`, `reconnect_delay_seconds`, `autostart`, `stale_timeout_seconds`, `runtime_duration_seconds`, `scheduler`, `log_patterns`, `limits`, and optional `metadata`. |
| GET, PUT, DELETE `/process/{id}` | Returns a filtered detail, applies a partial update, or deletes the job. `domain` scopes the lookup. Delete returns `"OK"` and accepts `purge` (default true). Purge applies eligible disk cleanup rules. |
| GET `/process/{id}/config`, GET `/process/{id}/state` | Configuration or order, execution, command, PID, progress, timers and resource usage. |
| PUT `/process/{id}/command` | JSON `command` is `start`, `stop`, `restart` or `reload`. An optional `reason` appears in the resulting report after sanitization. Returns `"OK"`. |
| POST `/process/probe`, GET `/process/{id}/probe` | Inspects input streams. Returns `streams` and `log` arrays with stream index, format, codec, dimensions, frame rate and audio properties where present. A probe payload can omit outputs. |
| POST `/process/validate` | Validates a process configuration without running it. Returns the accepted configuration and reports invalid references, unsupported placeholders and invalid limits. |
| GET, PUT `/process/{id}/report` | Current execution report and completed history. GET supports `created_at` and `exited_at` Unix times. A single value matches exactly, and both form a range. PUT replaces retained history and returns `"OK"`. |
| GET `/report/process` | Finds completed reports using `idpattern`, `refpattern`, `state` and inclusive Unix `from`/ `to`. Rust also accepts `offset` and `limit`. |
| GET `/process/{id}/metadata`, GET `/process/{id}/metadata/{key}`, PUT `/process/{id}/metadata/{key}` | Returns all metadata or a keyed value. PUT accepts arbitrary JSON and echoes the stored value. A null value removes the key. Empty metadata reads as null. |
| GET `/metadata`, GET `/metadata/{key}`, PUT `/metadata/{key}` | The same operations for Core-wide metadata. |

## Executables and configuration

Install FFmpeg and FFprobe from the operating system or an administrator-managed package. Core invokes both externally and never downloads them. Capability discovery identifies what the executable advertises. Availability of an accelerator depends on the device, drivers and container access. Native AMD64 and ARM64 CI use Ubuntu’s FFmpeg package.

| Environment setting | Default or effect |
| --- | --- |
| `CORE_FFMPEG_BINARY`, `CORE_FFPROBE_BINARY` | `ffmpeg` and its sibling `ffprobe`. Use absolute paths for custom builds. Executable names resolve against the child’s fixed system PATH. |
| `CORE_FFMPEG_PROBE_TIMEOUT_SECONDS` | 8 seconds across the inputs in one probe. Allowed range 1–12 seconds. |
| `CORE_FFMPEG_STOP_TIMEOUT_SECONDS` | 3 seconds before SIGKILL. Allowed range 1–10 seconds. |
| `CORE_FFMPEG_MAX_PROCESSES`, `CORE_FFMPEG_MAXPROCESSES` | Zero uses the default concurrency limit of 32. Capacity tops out at 128 jobs and 128 concurrent processes. |
| `CORE_FFMPEG_MAX_RETRIES` | 10 automatic retries per restart cycle, capped at 100. A process can override this with `reconnect_max_retries`. |
| `CORE_FFMPEG_LOG_MAX_LINES`, `CORE_FFMPEG_LOG_MAX_HISTORY` | Default 50 lines and 3 reports. The aliases without internal underscores also work. A run’s stderr has a 128 KiB budget. |
| `CORE_FFMPEG_ACCESS_INPUT_ALLOW`, `CORE_FFMPEG_ACCESS_INPUT_BLOCK`, `CORE_FFMPEG_ACCESS_OUTPUT_ALLOW`, `CORE_FFMPEG_ACCESS_OUTPUT_BLOCK` | Space-separated regular expressions. Block matches take precedence. Supplying an access policy restricts option-driven I/O. |
| `CORE_STORAGE_DISK_DIR` | Base for disk placeholders and eligible cleanup rules. Cleanup is bounded and uses regular files beneath the configured disk root. |
| `CORE_PROCESS_CGROUP_ROOT` | Optional writable, delegated cgroup v2 directory with CPU and memory controllers enabled. Configure via the host or container manager. |

The effective node policy comes from the Phase 1 configuration overlay. Config PUT validates it and config reload publishes it to future runs and probes. Running jobs finish with the policy, executable and limits selected at their last launch.

Use `#id:output=endpoint` to connect another job’s output, with optional `:domain=name` and `:source=protocol` selectors. A producer that has started publishes its expanded output, including a date-dependent filename. A consumer does its own retry if the producer’s data is unavailable. The runtime does not own, start or coordinate external storage listeners. The reference graph rejects self-links, cycles, missing jobs and missing outputs when validating a process. Deleting a producer can leave its consumers waiting for a valid source on their next run.

Placeholders include `{processid}`, `{domain}`, `{reference}`, `{inputid}`, `{outputid}`, `{diskfs}`, `{fs:disk}`, `{date}`, and `{date,format=%Y%m%d}`. `{env:NAME}` and `{secret:NAME}` resolve the dedicated `CORE_PROCESS_VALUE_NAME` and `CORE_PROCESS_SECRET_NAME` variables at process launch using the Core startup environment. The child receives a clean environment. Never put instance credentials or JWT keys into process variables.

## Isolation and compatibility limits

- Linux process groups receive SIGTERM and then SIGKILL when graceful exit expires. The standalone child launcher enables Linux parent-death signaling and prevents privilege elevation through executable metadata. The media leader also exits on abrupt Core death. A delegated cgroup is the containment boundary for descendants that leave the process group. Delegated cgroup v2 CPU and memory controls are used where available. Otherwise Core samples the leader from `/proc` and can stop a job after sustained excess. `limit_mode` and enforcement flags distinguish hard, sampled and unapplied limits. Sampled limits can miss short spikes and workload in descendants. GPU limits remain descriptive. The standard suite exercises the no-delegation path. On a Linux host with a disposable delegated subtree writable by the test user and both controllers enabled, run `bash scripts/test-cgroup.sh /sys/fs/cgroup/path/to/subtree`. The fixture reads the kernel CPU and memory limits, then verifies the process group is removed.
- FFmpeg is an external process with the service account’s permissions. Use a dedicated unprivileged account, narrow filesystem mounts and deployment network policy. Address ACLs apply to declared endpoints and resolved tee targets. Protocol negotiation, redirects and data-driven secondary requests remain within FFmpeg’s trust boundary. The engine restricts control flags and reserves its progress descriptors. Command state and reports sanitize credentials and URLs. Privileged configuration reads expose submitted arguments for round-trip editing. Inline arguments can also be visible to host process inspection. Use deployment permissions and secret placeholders.
- Standard FFmpeg progress provides aggregate frame count, FPS, output time, bitrate, speed and dropped or duplicated frames. Castvane's patched FFmpeg 9.1 emits JSON progress on stderr. Core also accepts that format and populates aggregate packet counts and numeric per-stream frame, keyframe, packet, size and frame-rate counters. Reports omit the raw telemetry records. Each record is limited to 32 KiB and Core retains up to 32 streams in each direction. FFprobe supplies codec and format details. Detailed mapping, AVstream and custom FIFO/tee telemetry remain unavailable through the progress adapter.
- Phase 3 provides memory, S3 and native media placeholders. Phase 4 provides IAM and GPU inventory. Hardware allocation and distributed scheduling remain future work. Live playout still requires the extended FFmpeg control interface.
- The [browser regression results](UI_REGRESSION.md) distinguish sign-in and refresh from the version-gated dashboard. Use the process HTTP tests to verify backend behavior independently.
- `/skills` includes the observed FFprobe version when the configured executable reports a parseable version. Standard builds lack the extended per-stream progress fields.

## Verification and performance

To repeat the API progress check with a patched Castvane FFmpeg executable, set its absolute path. The same test runs against a deterministic executable fixture when the variable is absent.

```sh
PHASE2_PATCHED_FFMPEG_BINARY=/path/to/ffmpeg cargo test --locked --test process_engine \
  patched_ffmpeg_progress_updates_state_without_logging_wire_records
```

Run the full Rust checks and the HTTP smoke test listed in the root workflow. Native CI installs FFmpeg before the process integration tests. The tests use generated local media and controlled executable fixtures. They cover inventory, FFprobe parsing, real encoding, scheduling and retries, stalled processes, group shutdown, metadata, access policy, capacity and restart reconciliation.

The optional differential runner uses disposable Go and Rust loopback servers and credentials provided through the same variables as the Phase 1 differential test. It checks shared response content after normalizing executable-specific probe codec names and generated runtime values.

```sh
python3 scripts/differential-process.py http://127.0.0.1:18880 http://127.0.0.1:18881
cargo test --locked --test process_engine four_concurrent_ffmpeg_jobs -- --ignored --nocapture
```

Local Linux AMD64 verification passed formatting, Clippy with warnings denied, 19 automated tests, the locked release build and the HTTP/SIGTERM smoke test. The differential script matched 24 process checks against Go 1.27.4 and FFmpeg 7.1.5. The Phase 1 differential suite also matched 24 checks on the release binary. A database created by the Phase 1 binary upgraded through migration 2 and passed SQLite integrity checks. Both native Linux jobs passed on implementation commit `f98b90f`. [AMD64 and ARM64 CI](https://github.com/KingIronMan2011/restreamer-core/actions/runs/45620579168)

The performance fixture launches four two-second, real-time 64×64 FFmpeg jobs at 12 FPS. It completed 96 output frames in 1.933 seconds from first submission to all reports on the local Linux AMD64 environment with FFmpeg 7.1.5. This is a local Rust Core baseline.

Cargo Audit 0.23.0 reported zero vulnerabilities after upgrading SQLx and bundled SQLite and switching the JWT crypto provider to AWS-LC. It reported an unmaintained warning for Croner’s optional `chrono` entry in the lockfile. The dependency is absent from the active graphs for both Linux targets, which use Jiff.

## Manual UI acceptance

1. Build each pinned UI listed in [BOOTSTRAP.md](BOOTSTRAP.md). Use the same-origin reverse proxy and test administrator setup in [PHASE_1_STATUS.md](PHASE_1_STATUS.md).
2. Verify discovery and login. Record the UI’s version checks and the observed request sequence for skills and process discovery.
3. Where the process UI is reachable, create a stream using a local test-media file and an FFmpeg file output. Confirm it appears in the process list and can be probed. Exercise start, stop, restart and configuration edits.
4. Watch live progress and the run report. Use a disposable invalid input with short reconnection delays to check failure and recovery. Then restore the valid input and verify the report history and metadata after restarting Core.
5. Record which actions are blocked by version checks, the FFmpeg requirement or deferred storage/media calls. Execute those process requests directly as an HTTP cross-check and record the browser outcome separately. Keep exported traces free of secrets.
