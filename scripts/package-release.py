#!/usr/bin/env python3
"""Assemble an unsigned native candidate and its Rust dependency inventory."""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import platform
import subprocess
import tarfile
import tempfile
from urllib.parse import quote


ROOT = Path(__file__).resolve().parents[1]
TARGETS = {"x86_64": ("amd64", "x86_64"), "aarch64": ("arm64", "aarch64")}


def metadata(target):
    return json.loads(subprocess.check_output(
        ["cargo", "metadata", "--locked", "--format-version", "1", "--filter-platform", target],
        cwd=ROOT, text=True,
    ))


def packages_for_build(data):
    packages = {pkg["id"]: pkg for pkg in data["packages"]}
    nodes = {node["id"]: node["deps"] for node in data["resolve"]["nodes"]}
    root = data["resolve"]["root"]
    dependencies = {}
    pending = [root]
    while pending:
        package = pending.pop()
        if package in dependencies:
            continue
        dependencies[package] = sorted({
            entry["pkg"] for entry in nodes[package]
            if any(kind["kind"] != "dev" for kind in entry["dep_kinds"])
        })
        pending.extend(dependencies[package])
    return root, {key: packages[key] for key in dependencies}, dependencies


def file_hash(path):
    with path.open("rb") as file:
        return hashlib.file_digest(file, "sha256").hexdigest()


def component(package):
    result = {"type": "library", "name": package["name"], "version": package["version"],
              "bom-ref": package["id"]}
    if (package["source"] or "").startswith("registry+https://github.com/rust-lang/crates.io-index"):
        result["purl"] = f"pkg:cargo/{quote(package['name'])}@{quote(package['version'])}"
    if package["license"]:
        result["licenses"] = [{"expression": package["license"]}]
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", type=Path)
    parser.add_argument("output", type=Path)
    args = parser.parse_args()
    binary = args.binary.resolve(strict=True)
    arch, rust_arch = TARGETS[platform.machine()]
    target = f"{rust_arch}-unknown-linux-gnu"
    data = metadata(target)
    root, packages, dependencies = packages_for_build(data)
    version = packages[root]["version"]
    timestamp = int(os.environ.get("SOURCE_DATE_EPOCH") or subprocess.check_output(
        ["git", "log", "-1", "--format=%ct"], cwd=ROOT, text=True,
    ).strip())
    app = component(packages[root])
    app.update(type="application", name="Castvane Core",
               hashes=[{"alg": "SHA-256", "content": file_hash(binary)}])
    bom = {
        "bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1,
        "metadata": {"timestamp": datetime.datetime.fromtimestamp(
            timestamp, tz=datetime.UTC).isoformat(), "component": app},
        "components": [component(packages[key]) for key in sorted(packages) if key != root],
        "dependencies": [{"ref": key, "dependsOn": dependencies[key]} for key in sorted(dependencies)],
    }
    licenses = ["package\tversion\tdeclared_license\tsource"]
    for package in sorted(packages.values(), key=lambda item: (item["name"], item["version"])):
        licenses.append("\t".join((package["name"], package["version"],
                                  package["license"] or package["license_file"] or "UNDECLARED",
                                  package["source"] or "workspace")))

    args.output.mkdir(parents=True, exist_ok=True)
    name = f"castvane-core-{version}-linux-{arch}"
    archive = args.output / f"{name}.tar.gz"
    artifact_paths = {
        "restreamer-core": binary, "LICENSE": ROOT / "LICENSE",
        "README.md": ROOT / "README.md", "IMPLEMENTATION_PLAN.md": ROOT / "IMPLEMENTATION_PLAN.md",
        "examples/castvane.env": ROOT / "examples/castvane.env",
    }
    for filename in ("THIRD_PARTY_NOTICES.md", "CHANGELOG.md"):
        artifact_paths[filename] = ROOT / filename
    for document in (ROOT / "docs").glob("*.md"):
        artifact_paths[f"docs/{document.name}"] = document
    for key, package in packages.items():
        if key == root:
            continue
        source = Path(package["manifest_path"]).parent
        notices = {path for path in source.iterdir() if path.is_file() and
                   path.name.upper().startswith(("LICENSE", "LICENCE", "NOTICE", "COPYING"))}
        if package["license_file"]:
            notices.add(source / package["license_file"])
        for notice in notices:
            if notice.is_file():
                archive_path = f"third-party/{package['name']}-{package['version']}/{notice.name}"
                artifact_paths[archive_path] = notice

    with tempfile.TemporaryDirectory(dir=args.output) as work:
        work = Path(work)
        (work / "bom.cdx.json").write_text(json.dumps(bom, indent=2) + "\n")
        (work / "DEPENDENCIES.tsv").write_text("\n".join(licenses) + "\n")
        artifact_paths.update({key: work / key for key in ("bom.cdx.json", "DEPENDENCIES.tsv")})
        tar_path = work / f"{name}.tar"
        with tarfile.open(tar_path, "w", format=tarfile.USTAR_FORMAT) as tar:
            for entry, source in sorted(artifact_paths.items()):
                with source.open("rb") as file:
                    info = tarfile.TarInfo(f"{name}/{entry}")
                    info.size = source.stat().st_size
                    info.mtime = timestamp
                    info.mode = 0o755 if entry == "restreamer-core" else 0o644
                    tar.addfile(info, file)
        with (work / archive.name).open("wb") as output:
            subprocess.run(["gzip", "-n", "-c", tar_path], stdout=output, check=True)
        (work / archive.name).replace(archive)
    (args.output / f"{archive.name}.sha256").write_text(f"{file_hash(archive)}  {archive.name}\n")
    (args.output / "core.bom.cdx.json").write_text(json.dumps(bom, indent=2) + "\n")
    (args.output / "native-build.json").write_text(json.dumps({
        "version": version, "target": target, "commit": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "archive": archive.name, "sha256": file_hash(archive), "binary_sha256": file_hash(binary),
    }, indent=2) + "\n")
    print(archive)


if __name__ == "__main__":
    main()
