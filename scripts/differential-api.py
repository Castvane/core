#!/usr/bin/env python3
"""Compare the Phase 1 wire contract on two disposable loopback instances."""

import argparse
import base64
import json
import os
import urllib.error
import urllib.request
from urllib.parse import urlsplit


def snapshot(base, username, password):
    def request(path, method="GET", data=None, token=None):
        headers = {"Content-Type": "application/json"} if data is not None else {}
        if token:
            headers["Authorization"] = "Bearer " + token
        call = urllib.request.Request(base + path, method=method, data=data, headers=headers)
        try:
            response = urllib.request.urlopen(call, timeout=20)
        except urllib.error.HTTPError as error:
            response = error
        with response:
            kind = response.headers.get_content_type()
            raw = response.read()
            return response.status, kind, json.loads(raw) if kind == "application/json" else raw

    def config(method="GET", data=None, token=None):
        return request("/api/v3/config", method, data, token)

    snap = {
        "ping": request("/ping"),
        "config_without_token": config(),
        "invalid_credentials": request("/api/login", "POST", b'{"username":"invalid","password":"invalid"}'),
        "malformed_login": request("/api/login", "POST", b"{"),
        "refresh_without_token": request("/api/login/refresh"),
        "anonymous_docs": request("/api/swagger/doc.json"),
    }
    status, kind, info = request("/api")
    snap["anonymous_discovery"] = (status, kind, info["app"], info["auths"], "id" in info)
    data = json.dumps({"username": username, "password": password}).encode()
    status, kind, tokens = request("/api/login", "POST", data)
    if status != 200:
        raise RuntimeError("Fixture login failed")
    snap["login"] = (status, kind, sorted(tokens))
    access, refresh = tokens["access_token"], tokens["refresh_token"]
    for label, token in (("access", access), ("refresh", refresh)):
        part = token.split(".")[1]
        claims = json.loads(base64.urlsafe_b64decode(part + "=" * (-len(part) % 4)))
        snap[label + "_claims"] = (
            {key: claims[key] for key in ("iss", "sub", "usefor", "exi")},
            claims["exp"] - claims["iat"],
            bool(claims["jti"]),
        )
    status, kind, info = request("/api", token=access)
    snap["authenticated_discovery"] = (
        status, kind, sorted(set(info) - {"capabilities"}), sorted(info["version"]),
        sorted(info["resources"]), info["app"], info["auths"],
    )
    snap["access_at_refresh"] = request("/api/login/refresh", token=access)
    snap["refresh_at_config"] = config(token=refresh)
    snap["invalid_access_token"] = config(token="invalid")[:2]
    status, kind, tokens = request("/api/login/refresh", token=refresh)
    snap["refresh"] = (status, kind, sorted(tokens))
    status, kind, settings = config(token=access)
    snap["configuration_shape"] = (
        status, kind, sorted(settings), sorted(settings["config"]),
        settings["config"]["version"], sorted(settings["overrides"]),
    )
    for version in (1, 2, 3):
        data = json.dumps({"version": version, "name": "phase-one-differential"}).encode()
        snap[f"patch_v{version}"] = config("PUT", data, access)
    snap["configured_name"] = config(token=access)[2]["config"]["name"]
    snap["malformed_config"] = config("PUT", b"{", access)[:2]
    snap["unsupported_schema"] = config("PUT", b'{"version":99}', access)[:2]
    snap["documentation"] = request("/api/swagger/doc.json", token=access)[:2]
    snap["reload"] = request("/api/v3/config/reload", token=access)
    return snap


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("go_url", help="Disposable pinned Go instance")
    parser.add_argument("rust_url", help="Disposable Rust instance")
    args = parser.parse_args()
    urls = [url.rstrip("/") for url in (args.go_url, args.rust_url)]
    for url in urls:
        parsed = urlsplit(url)
        if parsed.scheme != "http" or parsed.hostname not in ("127.0.0.1", "::1") or parsed.path:
            parser.error("Use disposable loopback HTTP instances")
    username = os.environ["PHASE1_TEST_USERNAME"]
    password = os.environ["PHASE1_TEST_PASSWORD"]
    expected, actual = (snapshot(url, username, password) for url in urls)
    differences = [key for key in expected if expected[key] != actual[key]]
    if differences:
        # Do not display response payloads, credentials, or tokens.
        parser.exit(1, "Contract mismatches: " + ", ".join(differences) + "\n")
    print(f"Matched {len(expected)} Go/Rust contract checks")


if __name__ == "__main__":
    main()
