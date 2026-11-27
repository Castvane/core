use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Mutex, RwLock},
    time::{Duration, Instant},
};

use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use tokio::sync::Semaphore;
use uuid::Uuid;
use zeroize::Zeroizing;

const ISSUER: &str = "datarhei-core";
#[cfg(test)]
const ACCESS_TTL: i64 = 600;
#[cfg(test)]
const REFRESH_TTL: i64 = 86_400;
const WINDOW: Duration = Duration::from_secs(60);
const PEER_LIMIT: usize = 1024;

#[derive(Clone)]
pub enum Identity {
    Administrator,
    Managed(crate::iam::Principal),
    TrustedLocalClient,
    Anonymous,
}

#[derive(Clone, Copy)]
pub enum Permission {
    ReadConfig,
    WriteConfig,
    ReadDiscovery,
    ReadProcess,
    WriteProcess,
}

impl Identity {
    pub fn administrator(&self) -> bool {
        matches!(self, Self::Administrator | Self::TrustedLocalClient)
            || matches!(self, Self::Managed(identity) if identity.administrator)
    }

    pub fn permits(&self, permission: Permission) -> bool {
        self.administrator()
            || matches!(
                (self, permission),
                (Self::Managed(_), Permission::ReadDiscovery)
            )
    }

    pub fn allows(&self, domain: &str, kind: &str, resource: &str, action: &str) -> bool {
        self.administrator()
            || matches!(self, Self::Managed(identity) if identity.permits(domain, kind, resource, action))
    }

    pub fn name(&self) -> Option<&str> {
        match self {
            Self::Managed(identity) => Some(&identity.name),
            _ => None,
        }
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
    key: RwLock<Zeroizing<Vec<u8>>>,
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
            key: RwLock::new(key),
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

    #[cfg(test)]
    pub fn issue(
        &self,
        subject: &str,
        generation: &str,
        usefor: TokenUse,
    ) -> Result<String, &'static str> {
        self.issue_with_ttl(
            subject,
            generation,
            usefor,
            match usefor {
                TokenUse::Access => ACCESS_TTL,
                TokenUse::Refresh => REFRESH_TTL,
            },
        )
    }

    pub fn rotate(&self, key: Zeroizing<Vec<u8>>) -> Result<(), &'static str> {
        *self.key.write().map_err(|_| "signing key unavailable")? = key;
        Ok(())
    }

    pub fn issue_with_ttl(
        &self,
        subject: &str,
        generation: &str,
        usefor: TokenUse,
        ttl: i64,
    ) -> Result<String, &'static str> {
        self.issue_at(
            subject,
            generation,
            usefor,
            time::OffsetDateTime::now_utc().unix_timestamp(),
            ttl,
        )
    }

    fn issue_at(
        &self,
        subject: &str,
        generation: &str,
        usefor: TokenUse,
        now: i64,
        ttl: i64,
    ) -> Result<String, &'static str> {
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
        let key = self.key.read().map_err(|_| "signing key unavailable")?;
        let key = EncodingKey::from_secret(&key).map_err(|_| "invalid signing key")?;
        jsonwebtoken::encode(&Header::new(Algorithm::HS256), &claims, &key)
            .map_err(|_| "signing failed")
    }

    #[cfg(test)]
    pub fn validate(
        &self,
        token: &str,
        subject: &str,
        generation: Option<&str>,
        purpose: TokenUse,
    ) -> Result<Identity, TokenError> {
        let (token_subject, token_generation) = self.authenticate(token, purpose)?;
        if token_subject != subject || generation != Some(token_generation.as_str()) {
            return Err(TokenError::Invalid);
        }
        Ok(Identity::Administrator)
    }

    pub fn authenticate(
        &self,
        token: &str,
        purpose: TokenUse,
    ) -> Result<(String, String), TokenError> {
        if token.len() > 4096 {
            return Err(TokenError::Invalid);
        }
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 0;
        validation.set_issuer(&[ISSUER]);
        validation.set_required_spec_claims(&["exp", "iss", "sub"]);
        let key = self.key.read().map_err(|_| TokenError::Invalid)?;
        let key = DecodingKey::from_secret(&key).map_err(|_| TokenError::Invalid)?;
        let claims = jsonwebtoken::decode::<Claims>(token, &key, &validation)
            .map_err(|_| TokenError::Invalid)?
            .claims;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        if claims.iat > now || claims.exp <= now {
            return Err(TokenError::Invalid);
        }
        if claims.usefor != purpose {
            return Err(TokenError::WrongUse);
        }
        Ok((claims.sub, claims.generation))
    }
}

pub async fn hash_password(password: Zeroizing<String>) -> Result<String, &'static str> {
    tokio::task::spawn_blocking(move || {
        Argon2::default()
            .hash_password(password.as_bytes())
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
            .issue_at("admin", "current", TokenUse::Access, 1, ACCESS_TTL)
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
