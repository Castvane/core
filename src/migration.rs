//! Offline import of versioned datarhei JSON exports into a new Castvane database.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

use serde::Deserialize;
use serde_json::{Value, json};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use uuid::Uuid;

use crate::{
    StartupError,
    api::process_dto::Configuration,
    auth::hash_password,
    config::{Environment, SCHEMA},
    engine::{self, Job, Metadata},
    iam::{Account, Effect, Iam, Rule as Policy},
    maintenance,
    media::alias::{Aliases, Rule},
    persistence::Store,
    state::{take_secret, validate, validate_password},
    zone::{self, Resources},
};

const MIB: u64 = 1024 * 1024;

#[derive(Default, Deserialize)]
#[serde(default)]
struct Inventory {
    version: u64,
    domain: BTreeMap<String, Domain>,
    process: BTreeMap<String, LegacyJob>,
    metadata: LegacyMetadata,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct LegacyMetadata {
    #[serde(deserialize_with = "nullable_map")]
    system: Metadata,
    #[serde(deserialize_with = "nullable_map")]
    process: BTreeMap<String, Option<Metadata>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Domain {
    #[serde(deserialize_with = "nullable_map")]
    process: BTreeMap<String, LegacyJob>,
    #[serde(deserialize_with = "nullable_map")]
    metadata: BTreeMap<String, Option<Metadata>>,
}

#[derive(Deserialize)]
struct LegacyJob {
    id: String,
    #[serde(default)]
    domain: String,
    #[serde(default)]
    owner: String,
    config: Value,
    created_at: i64,
    updated_at: i64,
    order: String,
}

#[derive(Deserialize)]
struct ZoneExport {
    version: u64,
    zones: Vec<LegacyZone>,
}
#[derive(Deserialize)]
struct LegacyZone {
    id: String,
    limits: Value,
}
#[derive(Deserialize)]
struct AliasExport {
    version: u64,
    rules: Vec<Rule>,
}

#[derive(Deserialize)]
struct LegacyUser {
    name: String,
    alias: String,
    superuser: bool,
    created_at: String,
    updated_at: String,
}

#[derive(Deserialize)]
struct PolicyDomain {
    name: String,
    policies: Option<Vec<LegacyPolicy>>,
}

#[derive(Deserialize)]
struct LegacyPolicy {
    username: String,
    resource: String,
    actions: String,
}

pub struct Source<'a> {
    pub config: &'a Path,
    pub processes: &'a Path,
    pub zones: Option<&'a Path>,
    pub aliases: Option<&'a Path>,
    pub users: Option<&'a Path>,
    pub policies: Option<&'a Path>,
}

fn nullable_map<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, T>, D::Error> {
    Ok(Option::deserialize(deserializer)?.unwrap_or_default())
}

/// Dry runs prepare and validate a disposable database. Imports publish a new database atomically.
pub async fn migrate(
    source: Source<'_>,
    output: Option<&Path>,
    env: &Environment,
) -> Result<Value, StartupError> {
    let mut configuration =
        maintenance::merge_configuration(maintenance::read_json(source.config, 4 * MIB).await?)?;
    let mut changes =
        vec!["Review imported process arguments, paths and credentials before starting any job."];
    if configuration["id"] == "" {
        configuration["id"] = json!(Uuid::new_v4().to_string());
    }
    if configuration["cluster"]["enable"] == true {
        changes.push("Go cluster membership and consensus state were omitted. Provision a new Castvane cluster.");
        configuration["cluster"] = crate::config::defaults()?["cluster"].clone();
    }
    // Never import the old deployment's plaintext administrator password or signing key.
    take_secret(&mut configuration, "/api/auth/password");
    take_secret(&mut configuration, "/api/auth/jwt/secret");
    if configuration["api"]["auth"]["auth0"]["enable"] == true {
        return Err(
            "Auth0 federation requires an explicit replacement configuration before import".into(),
        );
    }
    if let Some(password) = &env.password {
        validate_password(password)
            .map_err(|_| "new administrator password must be 12–1024 bytes")?;
        configuration["api"]["auth"]["enable"] = json!(true);
        if configuration["api"]["auth"]["username"] == "" {
            configuration["api"]["auth"]["username"] = json!("admin");
        }
    }
    let effective = env.apply(configuration.clone());
    let settings = validate(&effective, env.password.is_some())
        .map_err(|field| format!("invalid import setting: {field}; use a new CORE_API_AUTH_PASSWORD for authenticated setups"))?;
    if !crate::config::address(&settings.address)?
        .ip()
        .is_loopback()
        && !settings.api.auth.enable
        && !env.allow_insecure
    {
        return Err("the imported external listener requires authentication".into());
    }

    let mut inventory: Inventory =
        serde_json::from_value(maintenance::read_json(source.processes, 32 * MIB).await?)?;
    if inventory.version != 4 || inventory.domain.contains_key("") {
        return Err("expected a version 4 datarhei process store with named domain entries".into());
    }
    inventory.domain.insert(
        String::new(),
        Domain {
            process: inventory.process,
            metadata: inventory.metadata.process,
        },
    );
    let mut jobs = Vec::new();
    let mut graph = BTreeMap::new();
    let options = engine::Options::from_document(&effective, &env.engine)?;
    for (domain, records) in inventory.domain {
        let mut metadata = records.metadata;
        for (id, record) in records.process {
            if jobs.len() == engine::MAX_JOBS {
                return Err("process count exceeds Castvane capacity".into());
            }
            if record.id != id || record.domain != domain || id.is_empty() {
                return Err("process key, ID or domain does not match its enclosing record".into());
            }
            let mut config = record.config;
            if !config.is_object()
                || config["id"] != id
                || (!domain.is_empty() && config["domain"] != domain)
            {
                return Err(
                    "process configuration ID or domain does not match its enclosing record".into(),
                );
            }
            config["domain"] = json!(domain);
            let was_autostart = config["autostart"] == true;
            config["autostart"] = json!(false);
            config["metadata"] = json!({});
            config["runtime_duration_seconds"] = config["timeout"].clone();
            config["limits"] = json!({
                "cpu_usage":config["limit_cpu_usage"], "memory_mbytes":mb(&config["limit_memory_bytes"])?,
                "gpu_usage":config["limit_gpu_usage"]["usage"],
                "gpu_encoder":config["limit_gpu_usage"]["encoder"],
                "gpu_decoder":config["limit_gpu_usage"]["decoder"],
                "gpu_memory_mbytes":mb(&config["limit_gpu_usage"]["memory"])?,
                "waitfor_seconds":config["limit_waitfor_seconds"], "log_event_rate":config["limit_log_rate"],
            });
            let (config, _) =
                Configuration::from_patch(None, config, &record.owner, false)?.into_config();
            options.validate(&config)?;
            let job = Arc::new(Job {
                uid: Uuid::new_v4().to_string(),
                config,
                desired: false,
                created: record.created_at,
                updated: record.updated_at,
            });
            graph.insert((domain.clone(), id.clone()), job.clone());
            jobs.push((
                job,
                metadata.remove(&id).flatten().unwrap_or_default(),
                was_autostart || record.order == "start",
            ));
        }
        if !metadata.is_empty() {
            return Err("migration source has metadata for a missing process".into());
        }
    }
    for (job, _, _) in &jobs {
        engine::validate_references(&job.config, &graph)?;
    }

    let zones = read_zones(source.zones).await?;
    let rules = read_aliases(source.aliases).await?;
    let (users, policies) =
        read_identities(source.users, source.policies, &settings.api.auth.username).await?;
    if !users.is_empty() {
        if env.password.is_none() {
            return Err("set a new CORE_API_AUTH_PASSWORD to manage imported identities".into());
        }
        changes.push("Imported users are inactive. Review policy scope and set a new password before activation.");
    }
    if output.is_none() {
        changes.push("Dry run completed without creating a target installation.");
    }
    changes.push("Service credentials, certificates, media files and execution reports require separate migration.");

    let temp = tempfile::tempdir()?;
    let staged = temp.path().join("migration.sqlite3");
    let store = Store::open(&staged).await?;
    let mut tx = store.write().await?;
    let now = OffsetDateTime::now_utc().format(&Rfc3339)?;
    Store::save(&mut tx, SCHEMA, &configuration, &now, &now).await?;
    if let Some(password) = &env.password {
        Store::save_password(
            &mut tx,
            &hash_password(password.clone()).await?,
            &Uuid::new_v4().to_string(),
        )
        .await?;
    }

    for user in &users {
        if !Iam::save(&mut tx, user, None).await? {
            return Err("duplicate identity in migration source".into());
        }
        if let Some(rules) = policies.get(&user.name) {
            Iam::replace_rules(&mut tx, &user.name, rules).await?;
        }
    }
    for (id, limits) in &zones {
        sqlx::query("INSERT INTO zone VALUES (?, ?) ON CONFLICT(id) DO UPDATE SET document = excluded.document")
            .bind(id).bind(serde_json::to_string(limits)?).execute(&mut *tx).await?;
    }
    for (position, rule) in rules.iter().enumerate() {
        sqlx::query("INSERT INTO media_aliases VALUES (?, ?, ?)")
            .bind(position as i64)
            .bind(&rule.pattern)
            .bind(&rule.target)
            .execute(&mut *tx)
            .await?;
    }
    for (job, metadata, _) in &jobs {
        zone::validate_claim(&mut tx, job).await?;
        sqlx::query("INSERT INTO process_jobs VALUES (?, ?, ?, ?, 0, ?, ?)")
            .bind(&job.uid)
            .bind(&job.config.domain)
            .bind(&job.config.id)
            .bind(serde_json::to_string(&job.config)?)
            .bind(job.created)
            .bind(job.updated)
            .execute(&mut *tx)
            .await?;
        for (key, value) in metadata {
            engine::write_metadata(&mut tx, Some(&job.uid), key, value).await?;
        }
    }
    for (key, value) in &inventory.metadata.system {
        engine::write_metadata(&mut tx, None, key, value).await?;
    }
    tx.commit().await?;
    store.close().await;
    let checksum = if let Some(output) = output {
        Some(maintenance::backup(&staged, output).await?)
    } else {
        maintenance::database_check(&staged).await?;
        None
    };
    Ok(json!({
        "dry_run":output.is_none(), "processes":jobs.len(),
        "processes_previously_enabled":jobs.iter().filter(|(_, _, enabled)| *enabled).count(),
        "zones":zones.len(), "aliases":rules.len(), "users":users.len(),
        "policies":policies.values().map(Vec::len).sum::<usize>(), "sha256":checksum, "notes":changes,
    }))
}

fn mb(value: &Value) -> Result<u64, StartupError> {
    match value {
        Value::Null => Ok(0),
        value => Ok(value
            .as_u64()
            .ok_or("expected an unsigned byte limit")?
            .div_ceil(MIB)),
    }
}

async fn read_zones(path: Option<&Path>) -> Result<BTreeMap<String, Resources>, StartupError> {
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    let export: ZoneExport = serde_json::from_value(maintenance::read_json(path, MIB).await?)?;
    if export.version != 1 || export.zones.len() > 128 {
        return Err("unsupported zone export version or size".into());
    }
    let mut zones = BTreeMap::new();
    for mut zone in export.zones {
        if !zone.limits.is_object() || !zone::valid_id(&zone.id) {
            return Err("invalid zone in migration source".into());
        }
        zone.limits["memory_mbytes"] = json!(mb(&zone.limits["memory_bytes"])?);
        zone.limits["gpu_memory_mbytes"] = json!(mb(&zone.limits["gpu_memory_bytes"])?);
        let limits: Resources = serde_json::from_value(zone.limits)?;
        if !limits.valid() || zones.insert(zone.id, limits).is_some() {
            return Err("invalid or duplicate zone in migration source".into());
        }
    }
    Ok(zones)
}

async fn read_aliases(path: Option<&Path>) -> Result<Vec<Rule>, StartupError> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let export: AliasExport = serde_json::from_value(maintenance::read_json(path, MIB).await?)?;
    let mut patterns = BTreeSet::new();
    if export.version != 1
        || !export
            .rules
            .iter()
            .all(|rule| patterns.insert(&rule.pattern))
    {
        return Err("unsupported alias export version or duplicate alias".into());
    }
    Aliases::validate_rules(export.rules.clone())
        .map_err(|_| "invalid alias in migration source")?;
    Ok(export.rules)
}

async fn read_identities(
    users: Option<&Path>,
    policies: Option<&Path>,
    bootstrap: &str,
) -> Result<(Vec<Account>, BTreeMap<String, Vec<Policy>>), StartupError> {
    let Some(users) = users else {
        if policies.is_some() {
            return Err("a policy export requires the corresponding user export".into());
        }
        return Ok((Vec::new(), BTreeMap::new()));
    };
    let users: Vec<LegacyUser> =
        serde_json::from_value(maintenance::read_json(users, 4 * MIB).await?)?;
    if users.len() > 256 {
        return Err("too many identities in the migration source".into());
    }
    let random = zeroize::Zeroizing::new(format!("{}{}", Uuid::new_v4(), Uuid::new_v4()));
    let disabled_hash = hash_password(random).await?;
    let mut accounts = Vec::with_capacity(users.len());
    let mut names = BTreeSet::new();
    for user in users {
        if user.name == bootstrap {
            if !user.superuser {
                return Err(
                    "a non-administrator identity conflicts with the bootstrap username".into(),
                );
            }
            // The explicitly reprovisioned bootstrap administrator supersedes the Go root record.
            continue;
        }
        if !crate::iam::valid_name(&user.name)
            || user.alias.len() > 128
            || user.alias.chars().any(char::is_control)
            || !names.insert(user.name.clone())
        {
            return Err("invalid or duplicate identity in migration source".into());
        }
        accounts.push(Account {
            name: user.name,
            alias: user.alias,
            active: false,
            administrator: user.superuser,
            password_hash: disabled_hash.clone(),
            generation: Uuid::new_v4().to_string(),
            created_at: OffsetDateTime::parse(&user.created_at, &Rfc3339)?.unix_timestamp(),
            updated_at: OffsetDateTime::parse(&user.updated_at, &Rfc3339)?.unix_timestamp(),
        });
    }
    let Some(path) = policies else {
        return Ok((accounts, BTreeMap::new()));
    };
    let domains: Vec<PolicyDomain> =
        serde_json::from_value(maintenance::read_json(path, 4 * MIB).await?)?;
    let mut imported = BTreeMap::<String, Vec<Policy>>::new();
    for domain in domains {
        for rule in domain.policies.into_iter().flatten() {
            if rule.username == bootstrap {
                continue;
            }
            if !names.contains(&rule.username) {
                return Err("policy references an unknown identity".into());
            }
            let (types, pattern) = rule
                .resource
                .split_once(':')
                .ok_or("untyped policy requires manual conversion")?;
            // Brace expansion and some bracket/escape rules differ between the two glob engines.
            if pattern.contains(['{', '}', '[', ']', '\\']) || domain.name == "*" {
                return Err("policy pattern requires manual conversion before import".into());
            }
            let mut policy = Policy {
                domain: domain.name.clone(),
                resource: pattern.to_owned(),
                types: types.split('|').map(str::to_owned).collect(),
                actions: rule.actions.split('|').map(str::to_owned).collect(),
                effect: Effect::Allow,
            };
            policy.normalize()?;
            let records = imported.entry(rule.username).or_default();
            if records.len() == 64 {
                return Err("identity exceeds the Castvane policy limit".into());
            }
            records.push(policy);
        }
    }
    Ok((accounts, imported))
}
