use std::sync::Arc;

use regex::Regex;
use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::storage::{Error, Key};

#[derive(Clone, Deserialize, Serialize)]
pub struct Rule {
    pub pattern: String,
    pub target: String,
}

struct Compiled {
    rule: Rule,
    expression: Regex,
}

pub struct Aliases {
    store: SqlitePool,
    rules: tokio::sync::RwLock<Arc<Vec<Compiled>>>,
    writes: tokio::sync::Mutex<()>,
}

impl Aliases {
    pub async fn open(store: SqlitePool) -> Result<Self, Error> {
        let rows: Vec<(String, String)> =
            sqlx::query_as("SELECT pattern,target FROM media_aliases ORDER BY position")
                .fetch_all(&store)
                .await
                .map_err(|_| Error::Unavailable)?;
        let rules = compile(
            rows.into_iter()
                .map(|(pattern, target)| Rule { pattern, target })
                .collect(),
        )?;
        Ok(Self {
            store,
            rules: tokio::sync::RwLock::new(Arc::new(rules)),
            writes: Default::default(),
        })
    }

    pub async fn list(&self) -> Vec<Rule> {
        self.rules
            .read()
            .await
            .iter()
            .map(|item| item.rule.clone())
            .collect()
    }

    pub async fn edit<T>(
        &self,
        change: impl FnOnce(&mut Vec<Rule>) -> Result<T, Error>,
    ) -> Result<T, Error> {
        let _guard = self.writes.lock().await;
        let mut rules = self.list().await;
        let result = change(&mut rules)?;
        let compiled = compile(rules)?;
        let mut tx = self.store.begin().await.map_err(|_| Error::Unavailable)?;
        sqlx::query("DELETE FROM media_aliases")
            .execute(&mut *tx)
            .await
            .map_err(|_| Error::Unavailable)?;
        for (position, rule) in compiled.iter().enumerate() {
            sqlx::query("INSERT INTO media_aliases (position,pattern,target) VALUES (?,?,?)")
                .bind(position as i64)
                .bind(&rule.rule.pattern)
                .bind(&rule.rule.target)
                .execute(&mut *tx)
                .await
                .map_err(|_| Error::Unavailable)?;
        }
        tx.commit().await.map_err(|_| Error::Unavailable)?;
        *self.rules.write().await = Arc::new(compiled);
        Ok(result)
    }

    pub async fn resolve(&self, url: &str) -> Result<String, Error> {
        let rules = self.rules.read().await.clone();
        let mut path = url.to_owned();
        let mut used = std::collections::HashSet::new();
        for _ in 0..8 {
            if !used.insert(path.clone()) {
                return Err(Error::Invalid);
            }
            let Some(rule) = rules.iter().find(|rule| rule.expression.is_match(&path)) else {
                return Ok(path);
            };
            let next = rule
                .expression
                .replace(&path, rule.rule.target.as_str())
                .into_owned();
            validate_target(&next)?;
            path = next;
        }
        Err(Error::Invalid)
    }
}

fn compile(rules: Vec<Rule>) -> Result<Vec<Compiled>, Error> {
    if rules.len() > 128 {
        return Err(Error::Full);
    }
    let mut patterns = std::collections::HashSet::new();
    rules
        .into_iter()
        .map(|rule| {
            if rule.pattern.is_empty()
                || rule.pattern.len() > 512
                || !patterns.insert(rule.pattern.clone())
            {
                return Err(Error::Invalid);
            }
            validate_target(&rule.target)?;
            let expression = regex::RegexBuilder::new(&rule.pattern)
                .size_limit(128 * 1024)
                .build()
                .map_err(|_| Error::Invalid)?;
            Ok(Compiled { rule, expression })
        })
        .collect()
}

fn validate_target(target: &str) -> Result<(), Error> {
    if !target.starts_with('/')
        || target.starts_with("//")
        || target.contains(['?', '#', '%'])
        || ["/api", "/ping"].iter().any(|prefix| {
            target == *prefix
                || target
                    .strip_prefix(prefix)
                    .is_some_and(|tail| tail.starts_with('/'))
        })
    {
        return Err(Error::Invalid);
    }
    Key::parse(target)?;
    Ok(())
}
