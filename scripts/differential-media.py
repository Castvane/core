#!/usr/bin/env python3
"""Compare storage and alias contracts on disposable, authenticated loopback cores."""

import argparse
import json
import os
import urllib.error
import urllib.parse
import urllib.request
import uuid


def snapshot(origin, username, password, name):
    token = None

    def call(method, path, expected=200, data=None, binary=False, extra=None):
        headers = dict(extra or {})
        if token:
            headers["Authorization"] = f"Bearer {token}"
        if data is not None:
            headers["Content-Type"] = "application/octet-stream" if binary else "application/json"
        if data is not None and not binary:
            data = json.dumps(data).encode()
        req = urllib.request.Request(origin + path, data=data, headers=headers, method=method)
        try:
            result = urllib.request.urlopen(req, timeout=10)
        except urllib.error.HTTPError as err:
            result = err
        with result:
            if result.status != expected:
                raise RuntimeError(f"{method} {path}: {result.status}, expected {expected}")
            return result.headers, result.read()

    def as_json(*args):
        return json.loads(call(*args)[1])

    token = as_json("POST", "/api/login", 200, {
        "username": username, "password": password
    })["access_token"]
    root = "/api/v3"
    target = f"/{name}.ts"
    fspath = f"{root}/fs/disk{target}"
    alias = f"/{name}-alias.ts"
    rule = {"pattern": f"^{alias}$", "target": target}
    lookup = f"{root}/alias/rule?" + urllib.parse.urlencode({"pattern": rule["pattern"]})
    summary = {}
    summary["filesystem keys"] = sorted({key for item in as_json("GET", f"{root}/fs")
                                            for key in item.keys()})
    call("PUT", fspath, 201, b"0123456789", True)
    try:
        listing = as_json("GET", f"{root}/fs/disk?glob={target}")
        summary["listing"] = [{k: v for k, v in item.items() if k != "last_modified"}
                              for item in listing]
        headers, _ = call("HEAD", fspath)
        summary["file length"] = headers.get("Content-Length")
        headers, body = call("GET", fspath, 206, extra={"Range": "bytes=2-5"})
        summary["range"] = (body, headers.get("Content-Range"))
        call("PUT", fspath, 204, b"replacement", True)
        summary["replacement"] = call("GET", fspath)[1]
        summary["alias create"] = as_json("POST", f"{root}/alias", 200, rule)
        try:
            summary["alias get"] = as_json("GET", lookup)
        finally:
            summary["alias remove"] = as_json("DELETE", lookup)
    finally:
        summary["remove"] = call("DELETE", fspath)[1]
    summary["missing error"] = sorted(as_json("GET", fspath, 404).keys())
    return summary


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("go_url")
    parser.add_argument("rust_url")
    args = parser.parse_args()
    origins = [value.rstrip("/") for value in (args.go_url, args.rust_url)]
    for origin in origins:
        url = urllib.parse.urlsplit(origin)
        if (url.scheme != "http" or url.hostname not in ("127.0.0.1", "::1")
                or url.path or url.username or url.query or url.fragment):
            parser.error("Use disposable loopback instances")
    username = os.environ["PHASE1_TEST_USERNAME"]
    password = os.environ["PHASE1_TEST_PASSWORD"]
    name = "phase3-" + uuid.uuid4().hex
    go, rust = (snapshot(origin, username, password, name) for origin in origins)
    differences = [check for check in go if go[check] != rust[check]]
    for check in differences:
        print(f"{check}: Go={go[check]!r}, Rust={rust[check]!r}")
    if differences:
        raise SystemExit(1)
    print(f"{len(go)} media contract comparisons passed")


if __name__ == "__main__":
    main()
