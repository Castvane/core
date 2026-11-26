#!/usr/bin/env python3
"""Exercise concurrent media/API traffic, crash reconciliation and a restored SQLite backup."""

import argparse
from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
from http.client import HTTPConnection
import json
from pathlib import Path
import os
import socket
import subprocess
import tempfile
import time


PASSWORD = "fixture-recovery-password"
MEDIA = bytes(range(256)) * 1024


def request(port, method, path, expected=200, *, data=None, headers=None, return_headers=False):
    connection = HTTPConnection("127.0.0.1", port, timeout=4)
    try:
        connection.request(method, path, body=data, headers=headers or {})
        response = connection.getresponse()
        body = response.read(1024 * 1024)
        if response.status != expected:
            raise AssertionError(f"{method} {path}: {response.status}, expected {expected}")
        return (body, response.headers) if return_headers else body
    finally:
        connection.close()


def wait_until(check, seconds=12):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        if result := check():
            return result
        time.sleep(0.1)
    raise TimeoutError("fixture state did not become ready")


@contextmanager
def run_core(binary, directory, bootstrap=False):
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]
    environment = {
        "PATH": os.environ["PATH"], "CORE_BIND_ADDR": f"127.0.0.1:{port}",
        "CORE_DATABASE_PATH": str(directory / "core.sqlite"),
        "CORE_STORAGE_DISK_DIR": str(directory / "media"),
        "CORE_API_AUTH_ENABLE": "true", "CORE_API_AUTH_DISABLE_LOCALHOST": "false",
        "CORE_HOST_AUTO": "false", "CORE_UPDATE_CHECK": "false", "RUST_LOG": "warn",
    }
    if bootstrap:
        environment["CORE_API_AUTH_PASSWORD"] = PASSWORD
    with (directory / "core.log").open("ab") as log:
        process = subprocess.Popen([binary], cwd=directory, env=environment, stdout=log, stderr=log)
        try:
            def ping():
                if process.poll() is not None:
                    raise RuntimeError(f"Castvane exited during startup with status {process.returncode}")
                try:
                    return request(port, "GET", "/ping") == b"pong"
                except OSError:
                    return False
            wait_until(ping)
            yield port, process
        finally:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
                    raise


def credentials(port):
    body = json.dumps({"username": "admin", "password": PASSWORD})
    tokens = json.loads(request(port, "POST", "/api/login", data=body,
                                headers={"Content-Type": "application/json"}))
    return {"Authorization": f"Bearer {tokens['access_token']}", "Content-Type": "application/json"}


def running(port, headers):
    state = json.loads(request(port, "GET", "/api/v3/process/fixture/state", headers=headers))
    return state["pid"] if state["exec"] == "running" and state["progress"]["frame"] else None


def dead(pid):
    status = Path(f"/proc/{pid}/stat")
    try:
        return status.read_text().rsplit(")", 1)[1].strip().startswith("Z ")
    except FileNotFoundError:
        return True


def peak_rss(pid):
    status = Path(f"/proc/{pid}/status").read_text()
    return next(int(line.split()[1]) for line in status.splitlines() if line.startswith("VmHWM:"))


def backup(binary, source, snapshot, target):
    def environment(database):
        return {"PATH": os.environ["PATH"], "CORE_DATABASE_PATH": str(database)}
    checksum = subprocess.check_output([binary, "backup", snapshot], env=environment(source), text=True).strip()
    subprocess.run([binary, "restore", snapshot, "--sha256", checksum], env=environment(target), check=True)
    subprocess.run([binary, "database-check"], env=environment(target), check=True)


def load(port, headers, seconds, clients):
    deadline = time.monotonic() + seconds

    def worker():
        _, manifest_headers = request(port, "GET", "/fixture.mpd", return_headers=True)
        cookie = manifest_headers["Set-Cookie"].split(";", 1)[0]
        assert cookie.startswith("core_dash=")
        media_headers = {"Range": "bytes=17-8192", "Cookie": cookie}
        count = 0
        while time.monotonic() < deadline:
            segment = request(port, "GET", "/fixture.m4s", 206, headers=media_headers)
            assert segment == MEDIA[17:8193]
            request(port, "GET", "/api/v3/process/fixture/state", headers=headers)
            count += 2
        return count

    with ThreadPoolExecutor(max_workers=clients) as workers:
        return sum(workers.map(lambda _: worker(), range(clients)))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("--seconds", type=int, default=15)
    parser.add_argument("--clients", type=int, default=4)
    args = parser.parse_args()
    if not 1 <= args.seconds <= 259200 or not 1 <= args.clients <= 64:
        parser.error("choose 1–259200 seconds and 1–64 clients")
    binary = args.binary.resolve(strict=True)
    with tempfile.TemporaryDirectory(prefix="castvane-recovery-") as work:
        primary = Path(work) / "primary"
        restore = Path(work) / "restore"
        primary.mkdir()
        restore.mkdir()
        config = {
            "id": "fixture", "autostart": True,
            "input": [{"id": "in", "address": "testsrc2=size=64x64:rate=5",
                       "options": ["-re", "-f", "lavfi"]}],
            "output": [{"id": "out", "address": "-", "options": ["-threads", "1", "-f", "null"]}],
        }
        with run_core(binary, primary, bootstrap=True) as (port, process):
            headers = credentials(port)
            request(port, "PUT", "/api/v3/fs/disk/fixture.m4s", 201, data=MEDIA, headers=headers)
            request(port, "PUT", "/api/v3/fs/disk/fixture.mpd", 201,
                    data=b'<MPD xmlns="urn:mpeg:dash:schema:mpd:2011"/>', headers=headers)
            request(port, "POST", "/api/v3/process", data=json.dumps(config), headers=headers)
            request(port, "PUT", "/api/v3/metadata/recovery", data='{"checkpoint":1}', headers=headers)
            pid = wait_until(lambda: running(port, headers))
            before_load = peak_rss(process.pid)
            requests = load(port, headers, args.seconds, args.clients)
            after_load = peak_rss(process.pid)
            backup(binary, primary / "core.sqlite", Path(work) / "backup.sqlite", restore / "core.sqlite")
            process.kill()
            process.wait(timeout=5)
        wait_until(lambda: dead(pid))
        (restore / "media").mkdir()
        (restore / "media" / "fixture.m4s").write_bytes((primary / "media" / "fixture.m4s").read_bytes())
        for directory in (primary, restore):
            with run_core(binary, directory) as (port, _):
                headers = credentials(port)
                wait_until(lambda: running(port, headers))
                assert json.loads(request(port, "GET", "/api/v3/metadata/recovery", headers=headers)) == {"checkpoint": 1}
                assert request(port, "GET", "/fixture.m4s") == MEDIA
        print(f"{requests} requests with {args.clients} clients over {args.seconds}s; "
              f"Core peak RSS {before_load} → {after_load} KiB; "
              "crash cleanup, restart, and online backup restore passed")


if __name__ == "__main__":
    main()
