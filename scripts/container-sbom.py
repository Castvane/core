#!/usr/bin/env python3
"""Produce a CycloneDX inventory of native Core and installed Ubuntu packages."""
import hashlib
import json
from pathlib import Path
import sys
from urllib.parse import quote


def main(output, version):
    core_path = output / "core.bom.cdx.json"
    core = json.loads(core_path.read_text()) if core_path.exists() else None
    info = json.loads((output / "container-image.json").read_text())[0]
    with (output / "castvane-core-container.tar.gz").open("rb") as handle:
        archive_hash = hashlib.file_digest(handle, "sha256").hexdigest()
    image = {"type": "container", "bom-ref": info["Id"], "name": "castvane-core", "version": version,
             "hashes": [{"alg": "SHA-256", "content": archive_hash}],
             "properties": [{"name": "castvane:archive:format", "value": "docker-save"}]}
    components = []
    dependencies = []
    if core:
        components = [core["metadata"]["component"], *core["components"]]
        dependencies = core["dependencies"]
    direct = [components[0]["bom-ref"]] if components else []
    for line in (output / "runtime-packages.tsv").read_text().splitlines():
        name, version, source, source_version, arch = line.split("\t")
        name = name.split(":", 1)[0]
        purl = f"pkg:deb/ubuntu/{quote(name)}@{quote(version)}?arch={quote(arch)}&distro=ubuntu-24.04"
        components.append({
            "type": "library", "bom-ref": purl, "name": name, "version": version, "purl": purl,
            "properties": [{"name": "castvane:ubuntu:source", "value": source},
                           {"name": "castvane:ubuntu:source_version", "value": source_version}],
        })
        direct.append(purl)
    bom = {"bomFormat": "CycloneDX", "specVersion": "1.6", "version": 1,
           "metadata": {"component": image}, "components": components,
           "dependencies": [{"ref": image["bom-ref"], "dependsOn": direct}, *dependencies]}
    (output / "container.bom.cdx.json").write_text(json.dumps(bom, indent=2) + "\n")


if __name__ == "__main__":
    main(Path(sys.argv[1]), sys.argv[2])
