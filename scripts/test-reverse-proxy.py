#!/usr/bin/env python3
"""Exercise Core behind an HTTPS reverse proxy in an isolated loopback fixture."""

import argparse
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import socket
import ssl
import subprocess
import tempfile
from threading import Thread
import time
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen


def exercise(binary, work):
    cert = work / "proxy.crt"
    key = work / "proxy.key"
    subprocess.run([
        "openssl", "req", "-x509", "-newkey", "ec", "-pkeyopt", "ec_paramgen_curve:P-256",
        "-noenc", "-days", "1", "-subj", "/CN=localhost", "-addext",
        "subjectAltName=IP:127.0.0.1", "-keyout", str(key), "-out", str(cert),
    ], check=True, capture_output=True)
    with socket.socket() as reservation:
        reservation.bind(("127.0.0.1", 0))
        port = reservation.getsockname()[1]

    class Proxy(BaseHTTPRequestHandler):
        def relay(self):
            length = int(self.headers.get("Content-Length", "0"))
            headers = {
                key: value for key, value in self.headers.items()
                if key.lower() in {"authorization", "content-type", "content-length"}
            }
            headers.update({
                "X-Forwarded-For": self.client_address[0],
                "X-Forwarded-Proto": "https",
            })
            upstream = HTTPConnection(
                "127.0.0.1", port, timeout=3, source_address=("127.0.0.2", 0),
            )
            try:
                upstream.request(self.command, self.path, self.rfile.read(length), headers)
                response = upstream.getresponse()
                body = response.read()
                self.send_response(response.status)
                self.send_header("Content-Type", response.getheader("Content-Type"))
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)
            finally:
                upstream.close()

        do_GET = relay
        do_POST = relay

    server_tls = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    server_tls.load_cert_chain(cert, key)
    client_tls = ssl.create_default_context(cafile=str(cert))
    password = "reverse-proxy-fixture-password"
    settings = {
        "CORE_BIND_ADDR": f"127.0.0.1:{port}",
        "CORE_DATABASE_PATH": str(work / "core.sqlite"),
        "CORE_STORAGE_DISK_DIR": str(work / "media"),
        "CORE_API_AUTH_ENABLE": "true",
        "CORE_API_AUTH_USERNAME": "proxy-fixture",
        "CORE_API_AUTH_PASSWORD": password,
        "CORE_API_ACCESS_TRUSTED_PROXIES": "127.0.0.2/32",
        "CORE_API_ACCESS_HTTP_ALLOW": "192.0.2.0/24",
        "CORE_API_ACCESS_HTTPS_ALLOW": "127.0.0.1/32",
        "CORE_TLS_ENABLE": "false",
    }
    with subprocess.Popen([binary], cwd=work, env=settings) as node:
        try:
            for _ in range(100):
                try:
                    with urlopen(f"http://127.0.0.1:{port}/ping", timeout=1) as response:
                        assert response.read() == b"pong"
                    break
                except URLError:
                    if node.poll() is not None:
                        raise RuntimeError("Core exited before the proxy test")
                    time.sleep(0.05)
            else:
                raise RuntimeError("Core did not become ready")

            try:
                urlopen(Request(f"http://127.0.0.1:{port}/api", headers={
                    "X-Forwarded-For": "192.0.2.1", "X-Forwarded-Proto": "https",
                }), timeout=3).close()
            except HTTPError as error:
                assert error.code == 403
            else:
                raise AssertionError("direct client bypassed the network policy")

            with HTTPServer(("127.0.0.1", 0), Proxy) as proxy:
                proxy.socket = server_tls.wrap_socket(proxy.socket, server_side=True)
                worker = Thread(target=proxy.serve_forever)
                worker.start()
                try:
                    origin = f"https://127.0.0.1:{proxy.server_port}"
                    with urlopen(Request(origin + "/api/login", headers={
                        "Content-Type": "application/json",
                        "X-Forwarded-For": "192.0.2.1",
                    }, data=json.dumps({
                        "username": "proxy-fixture", "password": password,
                    }).encode()), timeout=4, context=client_tls) as response:
                        tokens = json.load(response)
                    with urlopen(Request(origin + "/api/v3/config", headers={
                        "Authorization": "Bearer " + tokens["access_token"],
                    }), timeout=4, context=client_tls) as response:
                        assert json.load(response)["config"]["version"] == 3
                finally:
                    proxy.shutdown()
                    worker.join()
        finally:
            node.terminate()
            node.wait(timeout=10)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="castvane-proxy-") as temporary:
        exercise(args.binary.resolve(strict=True), Path(temporary))
    print("HTTPS proxy, trusted forwarding, authentication and direct HTTP denial passed.")
