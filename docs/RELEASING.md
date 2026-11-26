# Release policy and verification

`main` is the current development branch. The first production branch and its support window will be established when a stable version is accepted. Use semantic versioning for the application and exact `X.Y.Z-rc.N` versions for candidates. The candidate is `0.2.0-rc.1`. Public REST stays on v3 while additive changes are compatible. Breaking public API changes belong under v4. Deprecation requires documented impact, a replacement and a published transition period.

| Contract | Current version |
| --- | --- |
| Public REST | v3 with legacy unversioned discovery, login, docs and GraphQL |
| Cluster RPC | `/internal/cluster/v1` |
| Configuration document | 3 |
| SQLite | Migration files 1 through 5 |
| Consensus command and state format | Castvane cluster v1 |

Do not assume that the same REST version implies persistent database or Raft compatibility. Use the matching application version when restoring a captured database. Run `config-check` and `database-check` before upgrading. Stop standalone nodes before replacing their binary. Save a backup first, keep the old binary and settings, and test startup on isolated restored data. If a migration changed the database, restore the old database alongside the old binary for rollback. An interrupted change must be investigated before restarting production jobs.

For rolling cluster upgrades, only use a version pair with an explicit successful upgrade rehearsal and unchanged persisted command/RPC semantics. Back up each voting member and capture membership, the node certificate bindings and deployment configuration independently. Keep quorum throughout and replace one member at a time, starting with followers. Confirm readiness and committed reads before the next member. Stop the upgrade if state or membership disagrees. Rehearse the intended binaries locally with `python3 scripts/test-cluster.py NEW_BINARY --upgrade-from OLD_BINARY`. Never downgrade a live voter to a stale backup as an ordinary rollback. Restore into an isolated drill before attempting environment recovery. [Cluster guide](CLUSTER.md)

## Candidate workflow

Dispatch **Candidate artifacts** on `main`. The two native Linux jobs build against the committed lockfile, smoke-test Core, validate a separate FFmpeg-bundled image, and package native and Core-only Docker archives. Each archive has a SHA-256 checksum and provenance. The native package includes the CycloneDX Rust dependency SBOM and license inventory. The image package includes the installed Ubuntu package/source inventory, image configuration and a CycloneDX inventory of OS and Rust components. Docker archive checksums are distinct from registry manifest digests.

```sh
sha256sum -c castvane-core-0.2.0-rc.1-linux-amd64.tar.gz.sha256
gh attestation verify castvane-core-0.2.0-rc.1-linux-amd64.tar.gz \
  --repo Castvane/core --signer-workflow Castvane/core/.github/workflows/candidates.yml
```

Use **Verify candidate artifacts** to confirm both native and container archive checksums, version labels, build provenance and SBOM attestations from the selected build run without publishing.

Configure a protected GitHub environment named `release-candidate` with required reviewers before using **Promote release candidate**. Provide the successful build run ID and exact version. Promotion verifies the identity, checksum and provenance and prepares an unpublished GitHub prerelease draft. The optional GHCR step publishes the tested Core-only images with an exact candidate tag and records the multi-architecture digest. The temporary manifest source tags include the run and attempt ID. The optional Docker Hub publisher reads `DOCKERHUB_USERNAME` and `DOCKERHUB_REPOSITORY` from protected environment variables and `DOCKERHUB_TOKEN` from its secrets. Set the repository to `namespace/repo` and use a scoped token. Both registry paths check the architecture and version of each archive and assemble by digest. Stable publishing remains gated.

Stable publication requires the mandatory [owner acceptance](RELEASE_ACCEPTANCE.md) and explicit approval. Only a stable workflow should advance `latest` and supported major/minor aliases. Development builds use commit identifiers. Never repoint a released exact version. No bit-for-bit reproducibility claim is made.
