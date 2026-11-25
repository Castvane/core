use std::{
    collections::HashMap,
    net::IpAddr,
    sync::Mutex,
    time::{Duration, Instant},
};

use argon2::{
    Argon2, PasswordHash, PasswordHasher, PasswordVerifier,
    password_hash::{SaltString, rand_core::OsRng},
};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use uuid::Uuid;
use zeroize::Zeroizing;

const ISSUER: &str = "datarhei-core";
pub const ACCESS_TTL: i64 = 600;
pub const REFRESH_TTL: i64 = 86_400;
const WINDOW: Duration = Duration::from_secs(60);
const PEER_LIMIT: usize = 1024;

#[derive(Clone)]
pub enum Identity {
    Administrator,
    TrustedLocalClient,
    Anonymous,
}

#[derive(Clone, Copy)]
pub enum Permission {
    ReadConfig,
    WriteConfig,
    ReadDiscovery,
}

impl Identity {
    pub fn permits(&self, _permission: Permission) -> bool {
        matches!(self, Self::Administrator | Self::TrustedLocalClient)
    }
}

#[derive(Clone, Copy, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TokenUse {
    Access,
    Refresh,
}

#[derive(Serialize, Deserialize)]
struct Claims {
    iss: String,
    sub: String,
    usefor: TokenUse,
    iat: i64,
    exp: i64,
    exi: i64,
    jti: String,
    generation: String,
}

pub enum TokenError {
    Invalid,
    WrongUse,
}

pub struct Authentication {
    key: Zeroizing<Vec<u8>>,
    dummy_hash: String,
    attempts: Mutex<Attempts>,
    password_jobs: std::sync::Arc<Semaphore>,
}

struct Attempts {
    peers: HashMap<IpAddr, (Instant, u32)>,
    global: (Instant, u32),
}

impl Authentication {
    pub async fn new(key: Zeroizing<Vec<u8>>) -> Result<Self, &'static str> {
        let dummy_hash = hash_password(Zeroizing::new(Uuid::new_v4().to_string())).await?;
        Ok(Self {
            key,
            dummy_hash,
            attempts: Mutex::new(Attempts {
                peers: HashMap::new(),
                global: (Instant::now(), 0),
            }),
            password_jobs: std::sync::Arc::new(Semaphore::new(2)),
        })
    }

    pub fn allow_attempt(&self, peer: IpAddr) -> bool {
        let Ok(mut attempts) = self.attempts.lock() else {
            return false;
        };
        let now = Instant::now();
        if now.duration_since(attempts.global.0) >= WINDOW {
            attempts.global = (now, 0);
            attempts
                .peers
                .retain(|_, (started, _)| now.duration_since(*started) < WINDOW);
        }
        if attempts.global.1 >= 60 || attempts.peers.len() >= PEER_LIMIT {
            return false;
        }
        let entry = attempts.peers.entry(peer).or_insert((now, 0));
        if now.duration_since(entry.0) >= WINDOW {
            *entry = (now, 0);
        }
        if entry.1 >= 8 {
            return false;
        }
        entry.1 += 1;
        attempts.global.1 += 1;
        true
    }

    pub async fn verify(
        &self,
        password: Zeroizing<String>,
        hash: Option<&str>,
    ) -> Result<bool, &'static str> {
        let permit = self
            .password_jobs
            .clone()
            .try_acquire_owned()
            .map_err(|_| "password verifier busy")?;
        let hash = hash.unwrap_or(&self.dummy_hash).to_owned();
        tokio::task::spawn_blocking(move || {
            let _permit = permit;
            verify_password(&password, &hash)
        })
        .await
        .map_err(|_| "password verifier failed")
    }

    pub fn issue(
        &self,
        subject: &str,
        generation: &str,
        usefor: TokenUse,
    ) -> Result<String, jsonwebtoken::errors::Error> {
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        self.issue_at(subject, generation, usefor, now)
    }

    fn issue_at(
        &self,
        subject: &str,
        generation: &str,
        usefor: TokenUse,
        now: i64,
    ) -> Result<String, jsonwebtoken::errors::Error> {
        let ttl = match usefor {
            TokenUse::Access => ACCESS_TTL,
            TokenUse::Refresh => REFRESH_TTL,
        };
        let claims = Claims {
            iss: ISSUER.into(),
            sub: subject.into(),
            usefor,
            iat: now,
            exp: now + ttl,
            exi: ttl,
            jti: Uuid::new_v4().to_string(),
            generation: generation.into(),
        };
        jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claims,
            &EncodingKey::from_secret(&self.key),
        )
    }

    pub fn validate(
        &self,
        token: &str,
        subject: &str,
        generation: Option<&str>,
        purpose: TokenUse,
    ) -> Result<Identity, TokenError> {
        if token.len() > 4096 {
            return Err(TokenError::Invalid);
        }
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 0;
        validation.set_issuer(&[ISSUER]);
        validation.sub = Some(subject.into());
        validation.set_required_spec_claims(&["exp", "iss", "sub"]);
        let claims = jsonwebtoken::decode::<Claims>(
            token,
            &DecodingKey::from_secret(&self.key),
            &validation,
        )
        .map_err(|_| TokenError::Invalid)?
        .claims;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        if generation != Some(claims.generation.as_str()) || claims.iat > now || claims.exp <= now {
            return Err(TokenError::Invalid);
        }
        if claims.usefor != purpose {
            return Err(TokenError::WrongUse);
        }
        Ok(Identity::Administrator)
    }
}

pub async fn hash_password(password: Zeroizing<String>) -> Result<String, &'static str> {
    tokio::task::spawn_blocking(move || {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|_| "password hashing failed")
    })
    .await
    .map_err(|_| "password hashing worker failed")?
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash).is_ok_and(|hash| {
        Argon2::default()
            .verify_password(password.as_bytes(), &hash)
            .is_ok()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn tokens_enforce_expiration_purpose_subject_and_generation() {
        let auth = Authentication::new(Zeroizing::new(vec![42; 32]))
            .await
            .unwrap();
        let expired = auth
            .issue_at("admin", "current", TokenUse::Access, 1)
            .unwrap();
        assert!(matches!(
            auth.validate(&expired, "admin", Some("current"), TokenUse::Access),
            Err(TokenError::Invalid)
        ));
        let token = auth.issue("admin", "current", TokenUse::Access).unwrap();
        assert!(
            auth.validate(&token, "admin", Some("current"), TokenUse::Access)
                .is_ok()
        );
        assert!(matches!(
            auth.validate(&token, "admin", Some("current"), TokenUse::Refresh),
            Err(TokenError::WrongUse)
        ));
        assert!(
            auth.validate(&token, "other", Some("current"), TokenUse::Access)
                .is_err()
        );
        assert!(
            auth.validate(&token, "admin", Some("old"), TokenUse::Access)
                .is_err()
        );
    }
}
