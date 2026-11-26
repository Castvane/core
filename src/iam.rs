use std::sync::Arc;

use glob::{MatchOptions, Pattern};
use serde::{Deserialize, Serialize};
use sqlx::{FromRow, Sqlite, SqlitePool, Transaction};
use uuid::Uuid;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Rule {
    pub domain: String,
    pub types: Vec<String>,
    pub resource: String,
    pub actions: Vec<String>,
    #[serde(default, skip_serializing_if = "is_allow")]
    pub effect: Effect,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Effect {
    #[default]
    Allow,
    Deny,
}

fn is_allow(effect: &Effect) -> bool {
    *effect == Effect::Allow
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'@'))
}

impl Rule {
    pub fn normalize(&mut self) -> Result<(), &'static str> {
        if self.domain.is_empty() {
            self.domain = "$none".into();
        }
        if self.domain.len() > 128
            || self.domain.chars().any(char::is_control)
            || self.resource.is_empty()
            || self.resource.len() > 512
            || self.resource.chars().any(char::is_control)
            || self.types.is_empty()
            || self.types.len() > 32
            || self.actions.is_empty()
            || self.actions.len() > 32
            || Pattern::new(&self.resource).is_err()
        {
            return Err("invalid policy");
        }
        for field in self.types.iter_mut().chain(&mut self.actions) {
            field.make_ascii_lowercase();
            if !valid_name(field) {
                return Err("invalid policy action or resource type");
            }
        }
        for values in [&mut self.types, &mut self.actions] {
            values.sort_unstable();
            values.dedup();
        }
        Ok(())
    }
}

#[derive(Clone)]
pub struct Principal {
    pub name: String,
    pub administrator: bool,
    pub generation: String,
    rules: Arc<Vec<(Rule, Pattern)>>,
}

impl Principal {
    pub fn from_account(account: Account, rules: Vec<Rule>) -> Result<Option<Self>, sqlx::Error> {
        if !account.active {
            return Ok(None);
        }
        let rules = rules
            .into_iter()
            .map(|rule| {
                let pattern = Pattern::new(&rule.resource)
                    .map_err(|error| sqlx::Error::Decode(Box::new(error)))?;
                Ok((rule, pattern))
            })
            .collect::<Result<_, sqlx::Error>>()?;
        Ok(Some(Self {
            name: account.name,
            administrator: account.administrator,
            generation: account.generation,
            rules: Arc::new(rules),
        }))
    }

    pub fn permits(&self, domain: &str, kind: &str, resource: &str, action: &str) -> bool {
        if self.administrator {
            return true;
        }
        let domain = if domain.is_empty() { "$none" } else { domain };
        let options = MatchOptions {
            require_literal_separator: matches!(kind, "api" | "fs" | "rtmp" | "srt"),
            ..Default::default()
        };
        let mut allowed = false;
        for (rule, pattern) in self.rules.iter() {
            if (rule.domain == "*" || rule.domain == domain)
                && rule.types.iter().any(|value| value == kind)
                && (rule.actions.iter().any(|value| value == action)
                    || rule.actions.as_slice() == ["any"])
                && pattern.matches_with(resource, options)
            {
                if rule.effect == Effect::Deny {
                    return false;
                }
                allowed = true;
            }
        }
        allowed
    }
}

#[derive(FromRow, Deserialize, Serialize)]
pub struct Account {
    pub name: String,
    pub alias: String,
    pub password_hash: String,
    pub administrator: bool,
    pub active: bool,
    pub generation: String,
    pub created_at: i64,
    pub updated_at: i64,
}

pub struct Iam(pub(crate) SqlitePool);

const GET: &str = "SELECT name, alias, password_hash, administrator, active, generation, created_at, updated_at FROM iam_identity WHERE name = ?";
const ALL: &str = "SELECT name, alias, password_hash, administrator, active, generation, created_at, updated_at FROM iam_identity ORDER BY name";

impl Iam {
    pub async fn get(&self, name: &str) -> Result<Option<Account>, sqlx::Error> {
        sqlx::query_as(GET).bind(name).fetch_optional(&self.0).await
    }

    pub async fn all(&self) -> Result<Vec<Account>, sqlx::Error> {
        sqlx::query_as(ALL).fetch_all(&self.0).await
    }

    pub async fn rules(&self, name: &str) -> Result<Vec<Rule>, sqlx::Error> {
        rules(&mut *self.0.acquire().await?, name).await
    }

    pub async fn principal(&self, name: &str) -> Result<Option<Principal>, sqlx::Error> {
        let mut tx = self.0.begin().await?;
        let account: Option<Account> = sqlx::query_as(GET)
            .bind(name)
            .fetch_optional(&mut *tx)
            .await?;
        let Some(account) = account.filter(|account| account.active) else {
            return Ok(None);
        };
        let rules = rules(&mut tx, name).await?;
        tx.commit().await?;
        Principal::from_account(account, rules)
    }

    pub async fn save(
        tx: &mut Transaction<'_, Sqlite>,
        account: &Account,
        previous: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let updated = if let Some(previous) = previous {
            sqlx::query("UPDATE iam_identity SET name=?, alias=?, password_hash=?, administrator=?, active=?, generation=?, updated_at=? WHERE name=?")
                .bind(&account.name).bind(&account.alias).bind(&account.password_hash)
                .bind(account.administrator).bind(account.active).bind(&account.generation).bind(account.updated_at)
                .bind(previous).execute(&mut **tx).await?
        } else {
            sqlx::query(
                "INSERT INTO iam_identity VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(name) DO NOTHING",
            )
            .bind(&account.name)
            .bind(&account.alias)
            .bind(&account.password_hash)
            .bind(account.administrator)
            .bind(account.active)
            .bind(&account.generation)
            .bind(account.created_at)
            .bind(account.updated_at)
            .execute(&mut **tx)
            .await?
        };
        Ok(updated.rows_affected() == 1)
    }

    pub async fn replace_rules(
        tx: &mut Transaction<'_, Sqlite>,
        name: &str,
        rules: &[Rule],
    ) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM iam_policy WHERE identity = ?")
            .bind(name)
            .execute(&mut **tx)
            .await?;
        for (position, rule) in rules.iter().enumerate() {
            let data = serde_json::to_string(rule)
                .map_err(|error| sqlx::Error::Encode(Box::new(error)))?;
            sqlx::query("INSERT INTO iam_policy VALUES (?, ?, ?)")
                .bind(name)
                .bind(position as i64)
                .bind(data)
                .execute(&mut **tx)
                .await?;
        }
        Ok(())
    }

    pub async fn revoke(tx: &mut Transaction<'_, Sqlite>, name: &str) -> Result<bool, sqlx::Error> {
        let result =
            sqlx::query("UPDATE iam_identity SET generation = ?, updated_at = ? WHERE name = ?")
                .bind(Uuid::new_v4().to_string())
                .bind(crate::engine::unix_now())
                .bind(name)
                .execute(&mut **tx)
                .await?;
        Ok(result.rows_affected() == 1)
    }
}

async fn rules(conn: &mut sqlx::SqliteConnection, name: &str) -> Result<Vec<Rule>, sqlx::Error> {
    let rows: Vec<String> =
        sqlx::query_scalar("SELECT document FROM iam_policy WHERE identity = ? ORDER BY position")
            .bind(name)
            .fetch_all(conn)
            .await?;
    rows.iter()
        .map(|data| {
            serde_json::from_str(data).map_err(|error| sqlx::Error::Decode(Box::new(error)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "repeatable policy benchmark"]
    fn benchmark_policy_evaluation() {
        let rules: Vec<_> = (0..12)
            .map(|index| {
                let rule = Rule {
                    domain: "tenant".into(),
                    types: vec!["fs".into()],
                    resource: format!("/mem/live{index}/**"),
                    actions: vec!["read".into()],
                    effect: if index % 2 == 0 {
                        Effect::Allow
                    } else {
                        Effect::Deny
                    },
                };
                let pattern = Pattern::new(&rule.resource).unwrap();
                (rule, pattern)
            })
            .collect();
        let principal = Principal {
            name: "bench".into(),
            administrator: false,
            generation: String::new(),
            rules: Arc::new(rules),
        };
        let start = std::time::Instant::now();
        for tick in 0..100_000usize {
            let path = if tick % 2 == 0 {
                "/mem/live10/a.ts"
            } else {
                "/mem/live11/b.ts"
            };
            assert_eq!(
                std::hint::black_box(principal.permits("tenant", "fs", path, "read")),
                tick % 2 == 0
            );
        }
        println!(
            "Policy decisions per second with twelve rules: {:.0}",
            100000.0 / start.elapsed().as_secs_f64()
        );
    }
}
