use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::StatusCode,
    routing::{get, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;
use zeroize::Zeroizing;

use super::{ApiError, read_settings};
use crate::{
    App,
    auth::{Identity, hash_password},
    iam::{Account, Iam, Rule, valid_name},
    state::validate_password,
};

type Reply = Result<Json<Value>, ApiError>;

pub fn routes() -> Router<Arc<App>> {
    Router::new()
        .route("/iam/user", get(list).post(add))
        .route("/iam/user/{name}", get(get_user).put(update).delete(remove))
        .route("/iam/user/{name}/policy", put(policies))
        .route("/iam/user/{name}/revoke", put(revoke))
        .route("/iam/signing-key/rotate", put(rotate))
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Change {
    name: Option<String>,
    alias: Option<String>,
    superuser: Option<bool>,
    active: Option<bool>,
    auth: InputAuth,
    policies: Option<Vec<Rule>>,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct InputAuth {
    api: InputApi,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct InputApi {
    userpass: String,
}

fn parse<T>(value: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    value
        .map(|Json(value)| value)
        .map_err(|_| ApiError::bad_request("invalid JSON"))
}

fn scrubbed(name: &str, account: Option<&Account>, rules: Vec<Rule>, bootstrap: bool) -> Value {
    json!({
        "name":name, "alias": account.map_or("", |row| row.alias.as_str()),
        "created_at":account.map_or(0, |row| row.created_at), "updated_at":account.map_or(0, |row| row.updated_at),
        "superuser":bootstrap || account.is_some_and(|row| row.administrator),
        "active":bootstrap || account.is_some_and(|row| row.active),
        "auth":{"api":{"userpass":"","auth0":{"user":"","tenant":{"domain":"","audience":"","client_id":""}}},
                "services":{"basic":[],"token":[],"session":[]}},
        "policies":rules
    })
}

async fn list(State(app): State<Arc<App>>, Extension(actor): Extension<Identity>) -> Reply {
    let (record, settings) = read_settings(&app).await?;
    let mut entries = Vec::new();
    if record.password_hash.is_some()
        && actor.allows("$none", "iam", &settings.api.auth.username, "read")
    {
        entries.push(scrubbed(
            &settings.api.auth.username,
            None,
            Vec::new(),
            true,
        ));
    }
    for account in app.iam.all().await? {
        if actor.allows("$none", "iam", &account.name, "read") {
            entries.push(scrubbed(
                &account.name,
                Some(&account),
                app.iam.rules(&account.name).await?,
                false,
            ));
        }
    }
    if actor.administrator() {
        entries.push(scrubbed("$anon", None, Vec::new(), false));
    }
    Ok(Json(json!(entries)))
}

async fn get_user(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    Path(name): Path<String>,
) -> Reply {
    if !actor.allows("$none", "iam", &name, "read") {
        return Err(ApiError::forbidden("access denied"));
    }
    let (record, settings) = read_settings(&app).await?;
    if name == settings.api.auth.username && record.password_hash.is_some() {
        return Ok(Json(scrubbed(&name, None, Vec::new(), true)));
    }
    if name == "$anon" && actor.administrator() {
        return Ok(Json(scrubbed(&name, None, Vec::new(), false)));
    }
    let account = app.iam.get(&name).await?.ok_or_else(ApiError::not_found)?;
    Ok(Json(scrubbed(
        &name,
        Some(&account),
        app.iam.rules(&name).await?,
        false,
    )))
}

fn require_admin(actor: &Identity) -> Result<(), ApiError> {
    if actor.administrator() {
        Ok(())
    } else {
        Err(ApiError::forbidden("administrator access required"))
    }
}

fn check_rules(rules: &mut [Rule]) -> Result<(), ApiError> {
    if rules.len() > 64 {
        return Err(ApiError::bad_request("too many policies"));
    }
    for rule in rules {
        rule.normalize().map_err(ApiError::bad_request)?;
    }
    Ok(())
}

async fn add(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    input: Result<Json<Change>, JsonRejection>,
) -> Reply {
    change(&app, &actor, None, parse(input)?).await
}

async fn update(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    Path(name): Path<String>,
    input: Result<Json<Change>, JsonRejection>,
) -> Reply {
    change(&app, &actor, Some(name), parse(input)?).await
}

async fn change(
    app: &App,
    actor: &Identity,
    previous: Option<String>,
    mut change: Change,
) -> Reply {
    require_admin(actor)?;
    let name = change
        .name
        .take()
        .or_else(|| previous.clone())
        .unwrap_or_default();
    let settings = read_settings(app).await?.1;
    if !valid_name(&name) {
        return Err(ApiError::bad_request("invalid identity name"));
    }
    if name == settings.api.auth.username
        || previous.as_deref() == Some(&settings.api.auth.username)
    {
        return Err(ApiError::forbidden(
            "bootstrap administrator is managed through configuration",
        ));
    }
    if change
        .alias
        .as_ref()
        .is_some_and(|alias| alias.len() > 128 || alias.chars().any(char::is_control))
    {
        return Err(ApiError::bad_request("invalid identity alias"));
    }
    let password = Zeroizing::new(change.auth.api.userpass);
    if !password.is_empty() || previous.is_none() {
        validate_password(&password)
            .map_err(|_| ApiError::bad_request("password requires 12–1024 bytes"))?;
    }
    let now = crate::engine::unix_now();
    let old = if let Some(previous) = previous.as_ref() {
        Some(
            app.iam
                .get(previous)
                .await?
                .ok_or_else(ApiError::not_found)?,
        )
    } else {
        None
    };
    let mut rules = if let Some(rules) = change.policies {
        rules
    } else if let Some(previous) = previous.as_ref() {
        app.iam.rules(previous).await?
    } else {
        Vec::new()
    };
    check_rules(&mut rules)?;
    let previous_generation = old.as_ref().map(|row| row.generation.clone());
    let invalidate = !password.is_empty()
        || previous.as_ref().is_none_or(|previous| previous != &name)
        || change
            .active
            .zip(old.as_ref())
            .is_some_and(|(active, old)| active != old.active)
        || change
            .superuser
            .zip(old.as_ref())
            .is_some_and(|(admin, old)| admin != old.administrator);
    let hash = if password.is_empty() {
        old.as_ref()
            .ok_or_else(ApiError::not_found)?
            .password_hash
            .clone()
    } else {
        hash_password(password).await.map_err(ApiError::Internal)?
    };
    let account = Account {
        name: name.clone(),
        alias: change
            .alias
            .or_else(|| old.as_ref().map(|row| row.alias.clone()))
            .unwrap_or_default(),
        administrator: change
            .superuser
            .or_else(|| old.as_ref().map(|row| row.administrator))
            .unwrap_or(false),
        active: change
            .active
            .or_else(|| old.as_ref().map(|row| row.active))
            .unwrap_or(true),
        password_hash: hash,
        generation: previous_generation
            .filter(|_| !invalidate)
            .unwrap_or_else(|| Uuid::new_v4().to_string()),
        created_at: old.map_or(now, |row| row.created_at),
        updated_at: now,
    };
    let mut tx = app.store.write().await?;
    if let Some(previous) = previous.as_deref()
        && !(account.administrator && account.active)
    {
        ensure_administrator(&mut tx, previous).await?;
    }
    let existing: i64 = sqlx::query_scalar("SELECT count(*) FROM iam_identity WHERE name = ?")
        .bind(&name)
        .fetch_one(&mut *tx)
        .await?;
    let replacing = previous.as_deref() == Some(name.as_str());
    if (!replacing && existing != 0) || !Iam::save(&mut tx, &account, previous.as_deref()).await? {
        return Err(ApiError::Http(
            StatusCode::CONFLICT,
            "identity already exists or changed",
        ));
    }
    if previous.is_none() {
        let count: i64 = sqlx::query_scalar("SELECT count(*) FROM iam_identity")
            .fetch_one(&mut *tx)
            .await?;
        if count > 1024 {
            return Err(ApiError::Http(
                StatusCode::SERVICE_UNAVAILABLE,
                "identity capacity reached",
            ));
        }
    }
    Iam::replace_rules(&mut tx, &name, &rules).await?;
    audit(
        &mut tx,
        actor,
        if previous.is_none() {
            "create"
        } else {
            "update"
        },
        &name,
    )
    .await?;
    tx.commit().await?;
    app.ops.emit(
        "iam",
        if previous.is_none() {
            "create"
        } else {
            "update"
        },
        &name,
        "",
    );
    Ok(Json(scrubbed(&name, Some(&account), rules, false)))
}

async fn remove(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    Path(name): Path<String>,
) -> Reply {
    require_admin(&actor)?;
    if name == read_settings(&app).await?.1.api.auth.username {
        return Err(ApiError::forbidden(
            "bootstrap administrator cannot be removed",
        ));
    }
    let mut tx = app.store.write().await?;
    ensure_administrator(&mut tx, &name).await?;
    if sqlx::query("DELETE FROM iam_identity WHERE name = ?")
        .bind(&name)
        .execute(&mut *tx)
        .await?
        .rows_affected()
        != 1
    {
        return Err(ApiError::not_found());
    }
    audit(&mut tx, &actor, "delete", &name).await?;
    tx.commit().await?;
    app.ops.emit("iam", "delete", &name, "");
    Ok(Json(json!("OK")))
}

async fn revoke(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    Path(name): Path<String>,
) -> Reply {
    require_admin(&actor)?;
    let (record, settings) = read_settings(&app).await?;
    let mut tx = app.store.write().await?;
    if name == settings.api.auth.username {
        crate::persistence::Store::save_password(
            &mut tx,
            record
                .password_hash
                .as_deref()
                .ok_or_else(ApiError::not_found)?,
            &Uuid::new_v4().to_string(),
        )
        .await?;
    } else if !Iam::revoke(&mut tx, &name).await? {
        return Err(ApiError::not_found());
    }
    audit(&mut tx, &actor, "revoke", &name).await?;
    tx.commit().await?;
    app.ops.emit("iam", "revoke", &name, "");
    Ok(Json(json!("OK")))
}

async fn policies(
    State(app): State<Arc<App>>,
    Extension(actor): Extension<Identity>,
    Path(name): Path<String>,
    input: Result<Json<Vec<Rule>>, JsonRejection>,
) -> Reply {
    require_admin(&actor)?;
    let mut rules = parse(input)?;
    check_rules(&mut rules)?;
    let mut tx = app.store.write().await?;
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM iam_identity WHERE name = ?)")
            .bind(&name)
            .fetch_one(&mut *tx)
            .await?;
    if !exists {
        return Err(ApiError::not_found());
    }
    Iam::replace_rules(&mut tx, &name, &rules).await?;
    audit(&mut tx, &actor, "policy", &name).await?;
    tx.commit().await?;
    app.ops.emit("iam", "policy", &name, "");
    Ok(Json(json!(rules)))
}

async fn ensure_administrator(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    name: &str,
) -> Result<(), ApiError> {
    let last: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM iam_identity WHERE name=? AND active AND administrator)
        AND NOT EXISTS(SELECT 1 FROM administrator)
        AND NOT EXISTS(SELECT 1 FROM iam_identity WHERE name<>? AND active AND administrator)",
    )
    .bind(name)
    .bind(name)
    .fetch_one(&mut **tx)
    .await?;
    if last {
        return Err(ApiError::forbidden(
            "final active administrator cannot be removed or deactivated",
        ));
    }
    Ok(())
}

async fn rotate(State(app): State<Arc<App>>, Extension(actor): Extension<Identity>) -> Reply {
    require_admin(&actor)?;
    if app.environment.signing_key_is_managed() {
        return Err(ApiError::validation(
            "api.auth.jwt.secret",
            "managed by environment",
        ));
    }
    let mut key = Zeroizing::new(vec![0u8; 32]);
    getrandom::fill(&mut key).map_err(|_| ApiError::Internal("signing key generation failed"))?;
    let mut tx = app.store.write().await?;
    sqlx::query("UPDATE signing_key SET secret = ? WHERE singleton = 1")
        .bind(key.as_slice())
        .execute(&mut *tx)
        .await?;
    audit(&mut tx, &actor, "rotate", "").await?;
    tx.commit().await?;
    app.auth.rotate(key.clone()).map_err(ApiError::Internal)?;
    app.media.sessions.rotate(key)?;
    app.ops.emit("iam", "rotate", "", "");
    Ok(Json(json!("OK")))
}

async fn audit(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    actor: &Identity,
    action: &str,
    resource: &str,
) -> Result<(), ApiError> {
    super::operations::record(
        tx,
        "iam",
        action,
        Some(actor.name().unwrap_or("$root")),
        Some(resource),
    )
    .await
}
