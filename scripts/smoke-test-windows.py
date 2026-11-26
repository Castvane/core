"""Run a standalone Windows executable and exercise its real FFmpeg Job Object."""
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import time
from urllib.request import Request, urlopen


def wait(check, timeout=20):
    until = time.monotonic() + timeout
    while time.monotonic() < until:
        if result := check():
            return result
        time.sleep(.1)
    raise AssertionError(f"timed out waiting for {check.__name__}")


def running(pid):
    result = subprocess.check_output(
        ["tasklist.exe", "/NH", "/FO", "CSV", "/FI", f"PID eq {pid}"], text=True,
    )
    return f'"{pid}"' in result


def exercise(binary, work):
    db = work / "core.sqlite3"
    log = work / "server.log"
    env = {**os.environ, "CORE_BIND_ADDR": "127.0.0.1:0",
           "CORE_DATABASE_PATH": str(db), "CORE_STORAGE_DISK_DIR": str(work / "media"),
           "RUST_LOG": "info"}
    with log.open("wb") as output, subprocess.Popen([binary], cwd=work, env=env, stdout=output,
                                                   stderr=subprocess.STDOUT) as core:
        try:
            def address():
                if core.poll() is not None:
                    raise RuntimeError("Core exited during startup")
                match = re.search(r"address=(127\.0\.0\.1:\d+)", log.read_text(errors="replace"))
                return match.group(1) if match else None
            origin = f"http://{wait(address)}"

            def call(path, method="GET", payload=None):
                data = json.dumps(payload).encode() if payload is not None else None
                request = Request(origin + path, data, {"Content-Type":"application/json"}, method=method)
                with urlopen(request, timeout=4) as response:
                    return response.read()

            assert call("/ping") == b"pong"
            assert json.loads(call("/api"))["version"]["arch"] == "windows/amd64"
            call("/api/v3/process", "POST", {
                "id": "fixture", "autostart": True,
                "input": [{"id": "source", "address": "testsrc2=size=64x64:rate=12",
                           "options": ["-re", "-f", "lavfi"]}],
                "output": [{"id": "discard", "address": "-", "options": ["-f", "null"]}],
            })

            def ffmpeg_active():
                state = json.loads(call("/api/v3/process/fixture/state"))
                return state["pid"] if state["exec"] == "running" and state["progress"]["frame"] else None

            pid = wait(ffmpeg_active)
            assert running(pid)
            call("/api/v3/process/fixture/command", "PUT", {"command": "stop"})
            wait(lambda: not running(pid))
            call("/api/v3/process/fixture/command", "PUT", {"command": "start"})
            restarted = wait(ffmpeg_active)
            assert running(restarted)
            core.kill()
            core.wait(timeout=10)
            wait(lambda: not running(restarted))
        except BaseException:
            print(log.read_text(errors="replace"), file=sys.stderr)
            raise
        finally:
            if core.poll() is None:
                core.kill()
                core.wait(timeout=10)
    subprocess.run([binary, "database-check"], cwd=work, env=env, check=True)
    print("Native Windows smoke passed: HTTP, FFmpeg progress, API stop and restart, crash cleanup, and database integrity.")


if __name__ == "__main__":
    with tempfile.TemporaryDirectory(prefix="castvane-native-") as directory:
        exercise(Path(sys.argv[1]).resolve(strict=True), Path(directory))
