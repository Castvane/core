# Migrate from datarhei Core

A standalone Restreamer migration uses copies of the JSON files already on disk. The procedure requires no export action in the Restreamer UI. Copy the files to a private staging directory during a maintenance window with the Go service stopped, or use a consistent filesystem snapshot. Keep a separate backup of the original installation, certificates, media volumes, object stores and deployment settings.

| Input | Where to find it in the pinned standalone Go version |
| --- | --- |
| Configuration | The path selected by `CORE_CONFIGFILE`. The reference Dockerfile sets `/core/config/config.json`. |
| Processes and metadata | `db.json` in the effective database directory (`CORE_DB_DIR` or `db.dir`). The reference Dockerfile sets `/core/config`. |
| Zones and aliases, if present | `zones.json` and `aliases.json` in the effective database directory. |
| Identities and policies, if present | `iam/users.json` and `iam/policy.json` under the effective database directory. |

For Docker, look in the bind mount or named volume backing these paths. The reference image keeps media separately at `/core/data`. Keep the staging copy local because the JSON may contain credentials or private stream URLs. [Reference Dockerfile](https://github.com/KingIronMan2011/restreamer-core/blob/452e9429c1fd7e1a90d75ed12883171ecabd80eb/Dockerfile) · [Reference state paths](https://github.com/KingIronMan2011/restreamer-core/blob/452e9429c1fd7e1a90d75ed12883171ecabd80eb/app/api/api.go)

Castvane reads configuration versions 1–3 and the pinned Go process store version 4. It imports the optional version 1 zone and alias files and the JSON identity and policy files in the same transaction. The source files are opened read-only. Reject the migration if any validation check fails. The report contains counts and actions for review without disclosing configuration values.

```sh
# Provision a fresh strong CORE_API_AUTH_PASSWORD through the site's secret manager.
# Prefer a private, empty staging directory on the destination filesystem.
restreamer-core migrate --dry-run --config /imports/config.json --processes /imports/db.json \
  --zones /imports/zones.json --aliases /imports/aliases.json \
  --users /imports/iam/users.json --policies /imports/iam/policy.json

restreamer-core migrate --config /imports/config.json --processes /imports/db.json \
  --zones /imports/zones.json --aliases /imports/aliases.json \
  --users /imports/iam/users.json --policies /imports/iam/policy.json \
  --output /var/lib/castvane/core.sqlite3

CORE_DATABASE_PATH=/var/lib/castvane/core.sqlite3 restreamer-core database-check
```

Omit the arguments for optional files your installation never created. Supply a new local bootstrap password for an authenticated installation and when importing additional users. A Go administrator matching the bootstrap username is replaced by that explicitly provisioned identity. Other users import inactive and need a new password. The old API password and JWT signing key are discarded. Inspect policy scopes before activation using the v3 IAM user and policy routes. The importer rejects policy patterns with ambiguous glob syntax, wildcard domains, and policies for absent users. Convert such rules in a staging copy or recreate them through the API. Auth0 needs a local replacement identity configuration.

The import preserves supported configuration fields, process references, scheduled expressions, reconnect settings, I/O options, metadata and limits. Byte-based limits round up to MiB. All jobs import stopped with autostart disabled to prevent duplicate publishers. Enable them explicitly after testing paths, protocol credentials and schedules. Configuration paths remain legacy values where supported. Set new paths for the disk media directory, executable selection and certificates before starting Castvane. Review S3 endpoints and credentials. A diagnostic run should use a separate bucket or prefix with isolated credentials.

Confirm that absolute media paths, stream aliases, configured zones, and all dependent jobs are correct. Recreate service identities or transport tokens as required. Historical execution reports, media bytes, ACME keys and Go consensus state require separate handling. Build a new Castvane cluster with its own CA and node identities, then apply reviewed definitions and placement. Keep the Go and Castvane endpoints isolated while testing. [Cluster installation](CLUSTER.md)

Retries are safe when the target does not exist. The import refuses to overwrite or merge an existing SQLite database. To resolve a duplicate or incompatible configuration, revise the staged copies and repeat the dry run. Keep secrets and the generated database private and retain an offline backup before cutover.
