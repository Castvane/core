# Running Castvane natively on Windows

Native Windows AMD64 builds are intended for development. Use Linux or a Linux container for production.

Use 64-bit Windows, the Microsoft Visual C++ x64 Redistributable, and a native x64 FFmpeg and FFprobe pair. These executables are supplied by the operator. Windows CI builds with the MSVC Rust toolchain and provides a portable `restreamer-core.exe` archive with a SHA-256 checksum. The candidate workflow packages Windows alongside both Linux targets with a CycloneDX SBOM, license inventory and GitHub artifact attestations. See the [release guide](RELEASING.md).

To build from source, install Rust through rustup and Visual Studio Build Tools with **Desktop development with C++** and a Windows SDK. Open a developer PowerShell and run `cargo build --locked --release`. Use `target\release\restreamer-core.exe` or extract the CI archive.

Start in a directory controlled by your Windows account. Keep the SQLite database, backups, TLS keys and ACME state in folders whose NTFS permissions grant access to the service account and trusted administrators only. Castvane inherits Windows access-control lists. Check the parent directory before starting with `icacls`.

```powershell
$state = Join-Path $env:LOCALAPPDATA 'Castvane'
New-Item -ItemType Directory -Force $state | Out-Null
$env:CORE_DATABASE_PATH = Join-Path $state 'core.sqlite3'
$env:CORE_STORAGE_DISK_DIR = Join-Path $state 'media'
$env:CORE_FFMPEG_BINARY = 'C:\Tools\ffmpeg\bin\ffmpeg.exe'
$env:CORE_FFPROBE_BINARY = 'C:\Tools\ffmpeg\bin\ffprobe.exe'
$env:CORE_BIND_ADDR = '127.0.0.1:8080'
.\restreamer-core.exe ffmpeg-check
.\restreamer-core.exe serve
```

From another terminal, run `Invoke-WebRequest http://127.0.0.1:8080/ping`. Use Ctrl+C to shut down from the console. Set a new bootstrap password through your secret store before enabling an external listener. Configure Windows Firewall and TLS to match that deployment. To use RTMP, RTMPS, SRT or HTTP-FLV, supply a compatible MediaMTX executable with `CORE_MEDIA_TRANSPORT_BINARY`.

Castvane uses Windows Job Objects for child cleanup. A normal FFmpeg stop requests a quit through its private input pipe and terminates the Job Object if FFmpeg does not exit in time. CPU and memory monitoring and sampled limit checks are available. Linux cgroup hard limits and sysfs GPU telemetry are Linux-specific. FFmpeg capability discovery still reports encoders and acceleration compiled into the installed executable. Physical GPU availability and multi-host Windows clustering need deployment validation. Native Windows ARM64 is outside the checked build matrix.

### SRT interoperability

**Encrypted SRT playback is unsupported on native Windows.** It is outside the Windows release acceptance criteria. Windows CI continues to check publishing, playback and publisher disconnection for RTMP, RTMPS and clear SRT, plus HTTP-FLV. Encrypted SRT playback with MediaMTX v1.21.4 produced packet decryption failures with Gyan FFmpeg 9.1 and the BtbN FFmpeg 9.0.2 build dated 2026-11-26. A standalone MediaMTX instance on Linux reproduced the BtbN failure on the same generated stream that distribution FFmpeg 7.1.5 decoded successfully. The Linux distribution FFmpeg and MediaMTX fixtures still exercise encrypted playback as a required check. The server continues to require its configured passphrase when SRT encryption is enabled. [Windows test evidence](https://github.com/Castvane/core/actions/runs/45965677713) · [Media configuration](PHASE_3_STATUS.md)
