#!/usr/bin/env python3
"""Compare the shared v3 operations contract on disposable local cores."""

import argparse
import json
import os
import urllib.error
import urllib.parse
import urllib.request
import uuid


def compare_shape(base, fixture, username, password):
    token = None

    def call(method, path, expected=200, payload=None):
        headers = {}
        if token:
            headers["Authorization"] = f"Bearer {token}"
        if payload is not None:
            data = json.dumps(payload).encode()
            headers["Content-Type"] = "application/json"
        else:
            data = None
        req = urllib.request.Request(base + path, headers=headers, method=method, data=data)
        try:
            result = urllib.request.urlopen(req, timeout=10)
        except urllib.error.HTTPError as error:
            result = error
        with result:
            if result.status != expected:
                raise RuntimeError(f"{method} {path} returned {result.status}, expected {expected}")
            return json.load(result)

    token = call("POST", "/api/login", payload={"username": username, "password": password})["access_token"]
    results = {}
    iam = "/api/v3/iam/user"
    identity = {"name": fixture, "alias": "Contract identity", "auth": {"api": {"userpass": uuid.uuid4().hex}}}
    call("POST", iam, payload=identity)
    try:
        result = call("GET", f"{iam}/{fixture}")
        results["identity"] = {key: result[key] for key in ("name", "alias", "superuser")}
        results["identity listed"] = fixture in [account["name"] for account in call("GET", iam)]
        policies = [{"domain": "$none", "resource": fixture, "types": ["process"], "actions": ["read"]}]
        rules = call("PUT", f"{iam}/{fixture}/policy", payload=policies)
        results["policies"] = [{key: rule[key] for key in policies[0]} for rule in rules]
    finally:
        results["identity deleted"] = call("DELETE", f"{iam}/{fixture}") == "OK"

    zone = "/api/v3/zone"
    limits = {"cpu_usage": 25, "memory_mbytes": 32}
    result = call("POST", zone, payload={"id": fixture, "limits": limits})
    results["zone created"] = {key: result["limits"][key] for key in limits}
    try:
        current = call("GET", f"{zone}/{fixture}")
        results["zone state"] = (current["id"], current["processes"], sorted(current["usage"].keys()))
        call("PUT", f"{zone}/{fixture}", payload={"limits": {"memory_mbytes": 48}})
        results["zone updated"] = call("GET", f"{zone}/{fixture}")["limits"]["memory_mbytes"]
        results["zone conflict"] = call("POST", zone, expected=409, payload={"id": fixture})["code"]
    finally:
        results["zone deleted"] = call("DELETE", f"{zone}/{fixture}") == "OK"
    results["unknown zone error"] = sorted(call("GET", f"{zone}/{fixture}", expected=404))
    results["log raw array"] = isinstance(call("GET", "/api/v3/log?format=raw"), list)
    for path, pattern in (
        ("log", {"event": "core", "message": "["}),
        ("process", {"type": "["}),
    ):
        route = f"/api/v3/events/{path}"
        results[f"invalid {path} regex"] = sorted(
            call("POST", route, 400, {"filters": [pattern]})
        )

    def heartbeat(path, accept):
        req = urllib.request.Request(
            base + path, method="POST", data=b"{}",
            headers={"Authorization": f"Bearer {token}", "Content-Type": "application/json",
                     "Accept": accept},
        )
        with urllib.request.urlopen(req, timeout=8) as response:
            first = response.readline(4096).decode().strip()
            return response.headers.get_content_type(), json.loads(first) if first.startswith("{") else first

    results["log SSE heartbeat"] = heartbeat("/api/v3/events/log", "text/event-stream")
    results["log JSON heartbeat"] = heartbeat("/api/v3/events/log", "application/x-json-stream")
    results["process JSON heartbeat"] = heartbeat("/api/v3/events/process", "application/x-json-stream")
    metric = next(row for row in call("GET", "/api/v3/metrics") if row["name"] == "restream_state")
    results["process state metric descriptor"] = metric["labels"]
    return results


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("reference")
    parser.add_argument("implementation")
    args = parser.parse_args()
    urls = [value.rstrip("/") for value in (args.reference, args.implementation)]
    for value in urls:
        url = urllib.parse.urlsplit(value)
        if url.scheme != "http" or url.hostname not in ("127.0.0.1", "::1") or url.path or url.username or url.query or url.fragment:
            parser.error("Use disposable loopback instances")
    fixture = "phase4-" + uuid.uuid4().hex
    results = [compare_shape(url, fixture, os.environ["PHASE1_TEST_USERNAME"], os.environ["PHASE1_TEST_PASSWORD"]) for url in urls]
    differences = [name for name, value in results[0].items() if results[1].get(name) != value]
    for name in differences:
        print(f"{name}: Go={results[0][name]!r}, Castvane={results[1][name]!r}")
    if differences:
        raise SystemExit(1)
    print(f"{len(results[0])} operations contract comparisons passed")


if __name__ == "__main__":
    main()
