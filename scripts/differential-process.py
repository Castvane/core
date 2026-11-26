#!/usr/bin/env python3
"""Compare the shared Phase 2 API surface on disposable loopback cores."""

import argparse
import json
import os
import time
import urllib.error
import urllib.request
import uuid
from urllib.parse import urlsplit


def snapshot(origin, username, password, identifier):
    token = None

    def request(path, method="GET", data=None, expected=200):
        url = origin + "/api" + path
        headers = {"Content-Type": "application/json"}
        if token:
            headers["Authorization"] = "Bearer " + token
        body = json.dumps(data).encode() if data is not None else None
        outgoing = urllib.request.Request(url, headers=headers, data=body, method=method)
        try:
            response = urllib.request.urlopen(outgoing, timeout=15)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            if response.status != expected:
                # Avoid dumping credentials or process configurations into diagnostic logs.
                raise RuntimeError(f"{method} {path} returned {response.status}, expected {expected}")
            return json.load(response)

    token = request("/login", "POST", {"username": username, "password": password})["access_token"]
    key = "/v3/process/" + identifier
    cfg = {
        "id": identifier,
        "reference": "phase2",
        "input": [{"id": "input", "address": "testsrc2=size=64x64:rate=10", "options": ["-f", "lavfi"]}],
        "output": [{"id": "output", "address": "-", "options": ["-t", "1", "-f", "null"]}],
        "autostart": False,
    }
    checked = {}
    skill = request("/v3/skills")
    checked["skills shape"] = sorted(set(skill) & {"ffmpeg", "filter", "hwaccels", "codecs", "devices", "formats", "protocols"})
    checked["validate"] = request("/v3/process/validate", "POST", cfg)
    checked["create"] = request("/v3/process", "POST", cfg)
    try:
        checked["conflict"] = request("/v3/process", "POST", cfg, 409)["code"]
        checked["config"] = request(key + "/config")
        checked["initial report"] = request(key + "/report")
        checked["initial state"] = {k: request(key + "/state")[k] for k in ("order", "exec", "command", "pid")}
        checked["filtered list"] = [sorted(r) for r in request(f"/v3/process?id={identifier}&filter=config")]
        checked["detail"] = sorted(request(key + "?filter=state"))
        checked["metadata empty"] = request(key + "/metadata")
        checked["metadata missing"] = request(key + "/metadata/probe", expected=404)
        checked["metadata write"] = request(key + "/metadata/probe", "PUT", {"field": [1, 2]})
        checked["metadata read"] = request(key + "/metadata/probe")
        checked["metadata all"] = request(key + "/metadata")
        checked["update"] = request(key, "PUT", {"reference": "updated"})
        for label, probed in (
            ("instance probe", request(key + "/probe")),
            ("config probe", request("/v3/process/probe", "POST", cfg)),
        ):
            checked[label] = [{k: stream[k] for k in ("type", "width", "height", "fps")} for stream in probed["streams"]]
            if not checked[label]:
                raise RuntimeError(f"{label} returned no streams")
        checked["start"] = request(key + "/command", "PUT", {"command": "start"})
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            report = request(key + "/report")
            if report["history"]:
                checked["execution result"] = report["history"][0]["exit_state"]
                break
            time.sleep(0.1)
        else:
            raise RuntimeError("Expected finite local FFmpeg process to finish")
        checked["stop"] = request(key + "/command", "PUT", {"command": "stop"})
        checked["history search"] = [r["exit_state"] for r in request(f"/v3/report/process?idpattern={identifier}")]
        checked["report import"] = request(key + "/report", "PUT", {"history": []})
    finally:
        checked["delete"] = request(key, "DELETE")
    checked["deleted"] = request(key + "/config", expected=404)["code"]
    return checked


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("go_url")
    parser.add_argument("rust_url")
    args = parser.parse_args()
    urls = [value.rstrip("/") for value in (args.go_url, args.rust_url)]
    for url in urls:
        parsed = urlsplit(url)
        if parsed.scheme != "http" or parsed.hostname not in ("127.0.0.1", "::1") or parsed.path:
            parser.error("Use disposable loopback instances")
    fixture = "phase2-" + uuid.uuid4().hex
    username, password = os.environ["PHASE1_TEST_USERNAME"], os.environ["PHASE1_TEST_PASSWORD"]
    reference, implementation = (snapshot(url, username, password, fixture) for url in urls)
    differences = [key for key in reference if reference[key] != implementation[key]]
    if differences:
        parser.exit(1, "Contract mismatches: " + ", ".join(differences) + "\n")
    print(f"Matched {len(reference)} Go/Rust process contract checks")


if __name__ == "__main__":
    main()
