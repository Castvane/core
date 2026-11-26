#!/usr/bin/env python3
"""Measure the same isolated loopback management workload on Castvane and optional Go."""
import argparse
from concurrent.futures import ThreadPoolExecutor
from http.client import HTTPConnection
import json
import os
from pathlib import Path
import platform
import socket
import statistics
import subprocess
import tempfile
import time


def usage(pid):
    proc = Path("/proc") / str(pid)
    fields = (proc / "stat").read_text().rsplit(")", 1)[1].split()
    memory = (proc / "status").read_text().splitlines()
    return {
        "cpu_seconds": (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK"),
        "rss_kib": int(next(line for line in memory if line.startswith("VmRSS:")).split()[1]),
        "fd_count": len(list((proc / "fd").iterdir())),
    }


def requests(port, path, clients, seconds):
    deadline = time.monotonic() + seconds

    def client():
        connection = HTTPConnection("127.0.0.1", port, timeout=3)
        samples = []
        try:
            while time.monotonic() < deadline:
                start = time.perf_counter_ns()
                connection.request("GET", path)
                response = connection.getresponse()
                if response.status != 200 or not response.read(131072):
                    raise RuntimeError(f"{path} returned {response.status}")
                if len(samples) >= 100000:
                    raise RuntimeError("per-client sample limit reached")
                samples.append(time.perf_counter_ns() - start)
        finally:
            connection.close()
        return samples

    with ThreadPoolExecutor(max_workers=clients) as threads:
        samples = sorted(item for batch in threads.map(lambda _: client(), range(clients)) for item in batch)
    return {"requests": len(samples), "requests_per_second": round(len(samples) / seconds, 2),
            "median_ms": round(statistics.median(samples) / 1e6, 4),
            "p95_ms": round(samples[int(0.95 * (len(samples) - 1))] / 1e6, 4)}


def sample(binary, repetitions, clients, seconds):
    samples = []
    for _ in range(repetitions):
        with tempfile.TemporaryDirectory(prefix="castvane-bench-") as work, socket.socket() as reserve:
            reserve.bind(("127.0.0.1", 0))
            port = reserve.getsockname()[1]
            reserve.close()
            env = {"PATH": os.environ["PATH"], "HOME": work, "CORE_ADDRESS": f"127.0.0.1:{port}",
                   "CORE_API_AUTH_ENABLE": "false", "CORE_HOST_AUTO": "false",
                   "CORE_UPDATE_CHECK": "false", "CORE_LOG_LEVEL": "error",
                   "CORE_STORAGE_DISK_DIR": str(Path(work) / "media")}
            with (Path(work) / "start.log").open("w") as log:
                start = time.monotonic()
                with subprocess.Popen([binary], cwd=work, env=env, stdout=log, stderr=log) as app:
                    try:
                        while time.monotonic() - start < 20 and app.poll() is None:
                            connection = HTTPConnection("127.0.0.1", port, timeout=1)
                            try:
                                connection.request("GET", "/ping")
                                if connection.getresponse().read() == b"pong":
                                    break
                            except OSError:
                                time.sleep(.01)
                            finally:
                                connection.close()
                        else:
                            raise RuntimeError("failed to start local process")
                        startup = (time.monotonic() - start) * 1000
                        time.sleep(.5)
                        idle = usage(app.pid)
                        ping = requests(port, "/ping", clients, seconds)
                        about = requests(port, "/api", clients, seconds)
                        samples.append({"startup_ms": round(startup, 2), "idle": idle,
                                        "after_load": usage(app.pid), "ping": ping, "discovery": about})
                    finally:
                        app.terminate()
                        try:
                            app.wait(timeout=10)
                        except subprocess.TimeoutExpired:
                            app.kill()
                            app.wait()
                            raise
    return {"binary_bytes": binary.stat().st_size, "samples": samples}


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("castvane", type=Path)
    parser.add_argument("--reference", type=Path)
    parser.add_argument("--repetitions", type=int, default=3)
    parser.add_argument("--clients", type=int, default=8)
    parser.add_argument("--seconds", type=int, default=2)
    args = parser.parse_args()
    if not (1 <= args.repetitions <= 10 and 1 <= args.clients <= 32 and 1 <= args.seconds <= 10):
        parser.error("use 1–10 repetitions, 1–32 clients and 1–10 seconds per endpoint")
    result = {"platform": platform.platform(), "cpus": len(os.sched_getaffinity(0)),
              "clients": args.clients, "seconds_per_endpoint": args.seconds,
              "castvane": sample(args.castvane.resolve(strict=True), args.repetitions, args.clients, args.seconds)}
    if args.reference:
        result["go"] = sample(args.reference.resolve(strict=True), args.repetitions, args.clients, args.seconds)
    print(json.dumps(result, indent=2))
