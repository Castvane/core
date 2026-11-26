#!/usr/bin/env python3
"""Run a disposable four-node mTLS cluster with real FFmpeg workloads."""

import argparse
from contextlib import ExitStack
from http.client import HTTPConnection
import json
import os
import select
from pathlib import Path
import socket
import ssl
import subprocess
import tempfile
import threading
import time


PASSWORD = "disposable-cluster-password"


def eventually(check, description, seconds=30):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            if value := check():
                return value
        except OSError:
            pass
        time.sleep(0.1)
    raise AssertionError(description)


def ports(count):
    with ExitStack() as stack:
        reserved = [stack.enter_context(socket.socket()) for _ in range(count)]
        for listener in reserved:
            listener.bind(("127.0.0.1", 0))
        return [listener.getsockname()[1] for listener in reserved]


def openssl(*args):
    subprocess.run(["openssl", *args], check=True, stdout=subprocess.DEVNULL,
                   stderr=subprocess.DEVNULL, timeout=10)


def certificates(root, ids):
    ca = root / "ca.pem"
    ca_key = root / "ca.key"
    openssl("req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
            "-noenc", "-subj", "/CN=Cluster fixture", "-days", "1", "-keyout", ca_key, "-out", ca)
    extensions = root / "extension"
    extensions.write_text("subjectAltName=IP:127.0.0.1\nextendedKeyUsage=serverAuth,clientAuth\n")
    for node in ids:
        name = root / str(node)
        openssl("req", "-new", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
                "-noenc", "-subj", f"/CN=cluster-{node}", "-keyout", name.with_suffix(".key"),
                "-out", name.with_suffix(".csr"))
        openssl("x509", "-req", "-in", name.with_suffix(".csr"), "-CA", ca, "-CAkey", ca_key,
                "-CAcreateserial", "-days", "1", "-extfile", extensions, "-out", name.with_suffix(".pem"))


class Link:
    """A loopback-only TCP relay that can sever existing and future traffic."""

    def __init__(self, target):
        self.target = target
        self.up = threading.Event()
        self.up.set()
        self.closed = threading.Event()
        self.listener = socket.socket()
        self.listener.bind(("127.0.0.1", 0))
        self.listener.listen(16)
        self.listener.settimeout(0.2)
        self.port = self.listener.getsockname()[1]
        self.thread = threading.Thread(target=self.accept, daemon=True)
        self.thread.start()

    def accept(self):
        while not self.closed.is_set():
            try:
                client, _ = self.listener.accept()
            except socket.timeout:
                continue
            except OSError:
                return
            if not self.up.is_set():
                client.close()
                continue
            threading.Thread(target=self.relay, args=(client,), daemon=True).start()

    def relay(self, client):
        try:
            with client, socket.create_connection(("127.0.0.1", self.target), timeout=1) as server:
                sides = {client: server, server: client}
                while self.up.is_set() and not self.closed.is_set():
                    readable, _, _ = select.select(list(sides), [], [], 0.1)
                    for origin in readable:
                        data = origin.recv(65536)
                        if not data:
                            return
                        sides[origin].sendall(data)
        except OSError:
            return

    def close(self):
        self.closed.set()
        self.listener.close()
        self.thread.join(timeout=1)


class Node:
    def __init__(self, binary, root, node, api, options):
        self.id, self.api, self.binary = node, api, binary
        self.directory = root / f"node-{node}"
        self.directory.mkdir()
        self.options = self.directory / "cluster.json"
        self.options.write_text(json.dumps(options))
        self.process = None
        self.token = None

    def start(self):
        environment = {
            "PATH": os.environ["PATH"],
            "CORE_BIND_ADDR": f"127.0.0.1:{self.api}",
            "CORE_DATABASE_PATH": str(self.directory / "core.sqlite"),
            "CORE_STORAGE_DISK_DIR": str(self.directory / "media"),
            "CORE_API_AUTH_PASSWORD": PASSWORD,
            "CORE_CLUSTER_ENABLE": "true",
            "CORE_CLUSTER_CONFIG": str(self.options),
            "CORE_HOST_AUTO": "false",
            "RUST_LOG": "warn,openraft=error",
        }
        with (self.directory / "output.log").open("ab") as log:
            self.process = subprocess.Popen([self.binary], cwd=self.directory, env=environment,
                                            stdout=log, stderr=log)
        def online():
            if self.process.poll() is not None:
                raise RuntimeError(f"node {self.id} failed to start")
            return self.request("GET", "/ping", auth=False)[0] == 200
        eventually(online, f"node {self.id} HTTP listener did not start")
        code, tokens = self.request("POST", "/api/login", {"username": "admin", "password": PASSWORD}, auth=False)
        assert code == 200
        self.token = tokens["access_token"]

    def stop(self, *, crash=False):
        if self.process is not None and self.process.poll() is None:
            if crash:
                self.process.kill()
            else:
                self.process.terminate()
            try:
                self.process.wait(timeout=12)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
                raise

    def request(self, method, path, payload=None, *, auth=True):
        connection = HTTPConnection("127.0.0.1", self.api, timeout=6)
        headers = {}
        if auth and self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        if payload is not None:
            headers["Content-Type"] = "application/json"
            payload = json.dumps(payload)
        try:
            connection.request(method, path, body=payload, headers=headers)
            result = connection.getresponse()
            data = result.read(262144)
            if data and result.headers.get_content_type() == "application/json":
                data = json.loads(data)
            return result.status, data
        finally:
            connection.close()

    def call(self, method, path, payload=None, *, expected=200):
        status, value = self.request(method, f"/api/v3/cluster{path}", payload)
        if status != expected:
            raise AssertionError(f"node {self.id} {method} {path} returned {status} instead of {expected}: {value}")
        return value

    def leader(self):
        return self.process.poll() is None and self.call("GET", "/healthy") is True


def media_running(node, owner=None):
    status, value = node.request("GET", "/api/v3/cluster/process/video/state")
    return (status == 200 and value["exec"] == "running"
            and value["progress"]["frame"] > 0
            and (owner is None or value["assignment"]["node"] == owner) and value)


def exercise(binary, root):
    ids = (1, 2, 3, 4)
    certificates(root, ids)
    assigned_ports = ports(8)
    api_ports, raft_ports = assigned_ports[:4], assigned_ports[4:]
    links = {(source, destination): Link(port)
             for source in ids for destination, port in zip(ids, raft_ports)}

    def peer_list(source):
        return {
            node: {"address": f"https://127.0.0.1:{links[source, node].port}",
                   "certificate": str(root / f"{node}.pem"),
                   "node": {"name": f"node-{node}", "role": {1: "control", 4: "worker"}.get(node, "combined")}}
            for node in ids
        }
    cohort = {
        node: Node(binary, root, node, api, {
            "cluster": "fixture", "id": node, "bind": f"127.0.0.1:{raft}", "lease_ms": 8000,
            "peers": peer_list(node), "bootstrap": [1, 2, 3] if node == 1 else [],
            "certificate": str(root / f"{node}.pem"), "private_key": str(root / f"{node}.key"),
            "ca": str(root / "ca.pem"),
        })
        for node, api, raft in zip(ids, api_ports, raft_ports)
    }

    def leader():
        return next((node for node in cohort.values() if node.leader()), None)

    try:
        for node in cohort.values():
            node.start()
        leader_node = eventually(leader, "Raft did not elect a quorum-backed leader")
        assert leader_node.request("GET", "/api/v3/cluster", auth=False)[0] in (401, 403)

        context = ssl.create_default_context(cafile=str(root / "ca.pem"))
        try:
            with socket.create_connection(("127.0.0.1", raft_ports[0]), timeout=3) as raw:
                with context.wrap_socket(raw, server_hostname="127.0.0.1") as peer:
                    peer.sendall(b"POST /internal/cluster/v1/telemetry HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n")
                    assert b"200 OK" not in peer.recv(1024)
        except (OSError, ssl.SSLError):
            pass

        started = time.monotonic()
        first = leader_node.call("PUT", "/db/kv/fixture", {"value": "initial", "expected": 0})
        leader_node.call("PUT", "/db/kv/fixture", {"value": "stale", "expected": 0}, expected=409)
        assert leader_node.call("GET", "/db/kv/fixture")["revision"] == first["revision"]
        lock = leader_node.call("PUT", "/db/locks/job", {"owner": 1, "ttl_ms": 10000})
        leader_node.call("PUT", "/db/locks/job", {"owner": 2, "ttl_ms": 10000}, expected=409)
        leader_node.call("DELETE", f"/db/locks/job?owner=1&token={lock['token']}")
        for idx in range(72):
            leader_node.call("PUT", f"/db/kv/checkpoint-{idx}", {"value": idx, "expected": 0})
        print(f"72 durable writes plus CAS and lock checks {time.monotonic() - started:.3f}s", flush=True)
        eventually(lambda: leader_node.call("GET", "/snapshot")["snapshot_index"], "snapshot did not compact log")

        rule = {"domain": "$none", "types": ["api"], "resource": "/api/v3/config", "actions": ["read"]}
        created = leader_node.call("POST", "/iam/user", {
            "name": "fixture-user", "auth": {"api": {"userpass": PASSWORD}}, "policies": [rule],
        })
        assert created["auth"]["api"]["userpass"] == ""
        leader_node.call("POST", "/zone", {"id": "test-zone", "limits": {"memory_mbytes": 64}})
        leader_node.call("POST", "/alias", {"pattern": "^/test-alias$", "target": "/fixture.txt"})

        started = time.monotonic()
        leader_node.call("POST", "/node/4")
        assert any(node["id"] == "4" and not node["voter"] for node in leader_node.call("GET", "/node"))
        print(f"learner enrollment {time.monotonic() - started:.3f}s", flush=True)

        # The newly enrolled worker must receive and install a snapshot or catch up before auth succeeds.
        worker = cohort[4]
        def managed_login():
            status, data = worker.request("POST", "/api/login", {"username": "fixture-user", "password": PASSWORD}, auth=False)
            return data if status == 200 else None
        managed = eventually(managed_login, "replicated identity was unavailable on learner")
        root_token = worker.token
        worker.token = managed["access_token"]
        assert worker.request("GET", "/api/v3/config")[0] == 200
        leader_node.call("PUT", "/iam/user/fixture-user", {"active": False})
        eventually(lambda: worker.request("GET", "/api/v3/config")[0] in (401, 403), "revocation did not reach learner")
        worker.token = root_token
        (worker.directory / "media" / "fixture.txt").write_text("fixture")
        eventually(lambda: worker.request("GET", "/test-alias", auth=False) == (200, b"fixture"),
                   "alias did not reach worker")

        assert {"name": "/fixture.txt", "size_bytes": 7}.items() <= leader_node.call(
            "GET", "/node/4/fs/disk?glob=fixture.txt")[0].items()
        assert leader_node.call("HEAD", "/node/4/fs/disk/fixture.txt") == b""
        assert leader_node.call("GET", "/node/4/fs/disk/fixture.txt") == b"fixture"
        assert leader_node.call("PUT", "/node/4/fs/disk/remote.json", {"value": 42}, expected=201) == b""
        assert leader_node.call("GET", "/node/4/fs/disk/remote.json") == {"value": 42}
        leader_node.call("DELETE", "/node/4/fs/disk/remote.json")

        started = time.monotonic()
        leader_node.call("POST", "/process", {
            "id": "video", "autostart": True, "placement": {"preferred": 4},
            "input": [{"id": "input", "address": "testsrc2=size=64x64:rate=10", "options": ["-re", "-f", "lavfi"]}],
            "output": [{"id": "output", "address": "-", "options": ["-threads", "1", "-f", "null"]}],
        })
        original = eventually(lambda: media_running(leader_node, 4), "learner never ran FFmpeg")
        print(f"remote deployment {time.monotonic() - started:.3f}s", flush=True)
        started = time.monotonic()
        leader_node.call("PUT", "/reallocation", {"id": "video", "node": 2})
        moved = eventually(lambda: media_running(leader_node, 2), "explicit relocation did not converge")
        assert moved["assignment"]["epoch"] > original["assignment"]["epoch"]
        print(f"relocation {time.monotonic() - started:.3f}s", flush=True)
        isolated = leader_node
        partition = [link for (source, destination), link in links.items()
                     if isolated.id in (source, destination) and source != destination]
        start_partition = time.monotonic()
        for link in partition:
            link.up.clear()
        isolated.call("PUT", "/db/kv/isolated", {"value": True}, expected=503)
        leader_node = eventually(lambda: next((peer for peer in cohort.values()
                                               if peer.id != isolated.id and peer.leader()), None),
                                 "majority did not elect a successor during network partition")
        leader_node.call("PUT", "/db/kv/majority", {"value": "still authoritative"})
        for link in partition:
            link.up.set()
        print(f"partition and majority write {time.monotonic() - start_partition:.3f}s", flush=True)
        leader_node.call("PUT", "/reallocation", {"id": "video", "node": None})

        started = time.monotonic()
        cohort[2].stop(crash=True)
        replacement_leader = eventually(leader, "quorum did not survive voter termination")
        moved = eventually(lambda: media_running(replacement_leader, 4), "worker failover did not converge")
        print(f"worker failover {time.monotonic() - started:.3f}s", flush=True)

        remaining_voter = next(node for node in (1, 3) if node != replacement_leader.id)
        cohort[remaining_voter].stop(crash=True)
        replacement_leader.call("PUT", "/db/kv/blocked", {"value": True}, expected=503)
        cohort[remaining_voter].start()
        recovered = eventually(leader, "restarted voter did not restore quorum")
        assert recovered.call("GET", "/db/kv/checkpoint-71")["value"] == 71
        recovered.call("PUT", "/process/video/command", {"command": "stop"})
        eventually(lambda: recovered.call("GET", "/process/video/state")["assignment"] is None,
                   "process assignment was not released")
        recovered.call("DELETE", "/process/video")
        print("Quorum loss, restored voting state, and lease cleanup passed.", flush=True)
    except BaseException:
        for node in cohort.values():
            log = node.directory / "output.log"
            if log.is_file():
                print(f"Node {node.id} log tail:\n{log.read_text(errors='replace')[-4000:]}")
        raise
    finally:
        for node in cohort.values():
            node.stop()
        for link in links.values():
            link.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="castvane-cluster-") as work:
        exercise(args.binary.resolve(strict=True), Path(work))
