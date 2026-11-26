#!/usr/bin/env python3
"""Create a real pinned-Go JSON store and rehearse the offline import."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import socket
import sqlite3
import subprocess
import tempfile
import time
import urllib.error
import urllib.request


def send(base, path, data=None, token=None):
    headers = {"Content-Type": "application/json"}
    if token:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(base + path, headers=headers,
                                     data=json.dumps(data).encode() if data is not None else None)
    return urllib.request.urlopen(request, timeout=5)


def exercise(go, castvane, directory):
    source = directory / "go"
    target = directory / "rust"
    source.mkdir()
    target.mkdir()
    (source / "config").mkdir()
    (source / "media").mkdir()
    with socket.socket() as reserve:
        reserve.bind(("127.0.0.1", 0))
        address = f"127.0.0.1:{reserve.getsockname()[1]}"
    base = f"http://{address}"
    env = {"PATH": os.environ["PATH"], "HOME": str(source), "CORE_ADDRESS": address,
           "CORE_DB_DIR": str(source / "config"), "CORE_STORAGE_DISK_DIR": str(source / "media"),
           "CORE_API_AUTH_ENABLE": "true", "CORE_API_AUTH_DISABLE_LOCALHOST": "false",
           "CORE_API_AUTH_USERNAME": "fixture", "CORE_API_AUTH_PASSWORD": "legacy-fixture-password",
           "CORE_HOST_AUTO": "false", "CORE_UPDATE_CHECK": "false", "NO_PROXY": "127.0.0.1"}
    with (directory / "go.log").open("w") as log, subprocess.Popen([go], cwd=source, env=env, stdout=log, stderr=log) as node:
        try:
            for _ in range(100):
                if node.poll() is not None:
                    raise RuntimeError("Go fixture exited before starting")
                try:
                    with send(base, "/ping") as response:
                        assert response.read() == b"pong"
                    break
                except OSError:
                    time.sleep(.05)
            else:
                raise RuntimeError("Go fixture failed to start")
            with send(base, "/api/login", {"username": "fixture", "password": "legacy-fixture-password"}) as response:
                token = json.load(response)["access_token"]
            with send(base, "/api/v3/process", {"id": "migration-fixture", "type": "ffmpeg",
                      "autostart": False, "reference": "pinned-go",
                      "input": [{"id": "in", "address": "testsrc=size=64x64", "options": ["-f", "lavfi"]}],
                      "output": [{"id": "out", "address": "-", "options": ["-f", "null"]}]}, token) as response:
                assert response.status == 200
        finally:
            node.terminate()
            node.wait(timeout=10)
    files = source / "config"
    config = files / "config.json"
    database = files / "db.json"
    exports = [config, database]
    arguments = [castvane, "migrate", "--config", config, "--processes", database]
    for flag, name in [("zones", "zones.json"), ("aliases", "aliases.json"),
                       ("users", "users.json"), ("policies", "policy.json")]:
        path = files / name
        if path.exists():
            exports.append(path)
            arguments += [f"--{flag}", path]
    before = {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in exports}
    environment = {"PATH": os.environ["PATH"], "CORE_API_AUTH_USERNAME": "fixture",
                   "CORE_API_AUTH_PASSWORD": "replacement-fixture-password",
                   "CORE_STORAGE_DISK_DIR": str(target / "media")}
    outfile = target / "core.db"
    dry = json.loads(subprocess.check_output(arguments + ["--dry-run"], env=environment))
    assert dry["dry_run"] is True and dry["processes"] == 1 and not outfile.exists()
    result = json.loads(subprocess.check_output(arguments + ["--output", outfile], env=environment))
    assert result["processes"] == 1
    assert before == {path: hashlib.sha256(path.read_bytes()).hexdigest() for path in exports}
    with sqlite3.connect(f"{outfile.as_uri()}?mode=ro", uri=True) as connection:
        row = connection.execute("SELECT desired, configuration FROM process_jobs").fetchone()
        assert row[0] == 0 and json.loads(row[1])["reference"] == "pinned-go"
    print("Pinned Go process store imported, source checksums unchanged.")


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("go", type=Path)
    parser.add_argument("castvane", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="castvane-go-migration-") as work:
        exercise(args.go.resolve(strict=True), args.castvane.resolve(strict=True), Path(work))
