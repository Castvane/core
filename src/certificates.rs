use std::{
    collections::BTreeMap,
    fmt,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicI64, Ordering},
    },
    time::Duration,
};

use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, LetsEncrypt,
    NewAccount, NewOrder, OrderStatus, RetryPolicy,
};
use rustls::{
    crypto::CryptoProvider,
    pki_types::{CertificateDer, PrivateKeyDer, pem::PemObject},
    server::{ClientHello, ResolvesServerCert},
    sign::CertifiedKey,
};
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;
use zeroize::Zeroizing;

use crate::StartupError;

pub struct Certificates {
    provider: Arc<CryptoProvider>,
    key: RwLock<Option<Arc<CertifiedKey>>>,
    expiry: AtomicI64,
    challenges: Mutex<BTreeMap<String, String>>,
}

impl fmt::Debug for Certificates {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Certificates")
            .field("expires_at", &self.expiry())
            .finish()
    }
}

impl Certificates {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            provider: Arc::new(rustls::crypto::aws_lc_rs::default_provider()),
            key: RwLock::new(None),
            expiry: AtomicI64::new(0),
            challenges: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn provider(&self) -> Arc<CryptoProvider> {
        self.provider.clone()
    }

    pub fn expiry(&self) -> Option<i64> {
        let value = self.expiry.load(Ordering::Relaxed);
        (value > 0).then_some(value)
    }

    pub fn challenge(&self, token: &str) -> Option<String> {
        if token.len() > 128 {
            return None;
        }
        self.challenges.lock().ok()?.get(token).cloned()
    }

    fn validate(&self, chain: &[u8], key: &[u8]) -> Result<(Arc<CertifiedKey>, i64), StartupError> {
        let chain = CertificateDer::pem_slice_iter(chain).collect::<Result<Vec<_>, _>>()?;
        if chain.is_empty() || chain.len() > 16 {
            return Err("invalid TLS certificate chain".into());
        }
        let (_, certificate) = x509_parser::parse_x509_certificate(chain[0].as_ref())
            .map_err(|_| "invalid X.509 certificate")?;
        let validity = certificate.validity();
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        let expiry = validity.not_after.timestamp();
        if validity.not_before.timestamp() > now || expiry <= now {
            return Err("TLS certificate is outside its validity period".into());
        }
        let key = PrivateKeyDer::from_pem_slice(key)?;
        let certified = CertifiedKey::from_der(chain, key, &self.provider)?;
        certified.keys_match()?;
        Ok((Arc::new(certified), expiry))
    }

    fn load(&self, chain: &[u8], key: &[u8]) -> Result<(), StartupError> {
        self.install(self.validate(chain, key)?)
    }

    fn install(&self, (certified, expiry): (Arc<CertifiedKey>, i64)) -> Result<(), StartupError> {
        *self
            .key
            .write()
            .map_err(|_| "TLS certificate resolver unavailable")? = Some(certified);
        self.expiry.store(expiry, Ordering::Relaxed);
        Ok(())
    }

    async fn load_files(&self, chain: &Path, key: &Path) -> Result<(), StartupError> {
        let chain = read(chain, false).await?;
        let key = Zeroizing::new(read(key, true).await?);
        self.load(&chain, &key)
    }

    pub async fn manual(
        self: &Arc<Self>,
        chain: PathBuf,
        key: PathBuf,
        stop: CancellationToken,
    ) -> Result<(), StartupError> {
        let mut current = fingerprint(&chain, &key).await;
        self.load_files(&chain, &key).await?;
        let resolver = self.clone();
        tokio::spawn(async move {
            let mut failed = false;
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_secs(3)) => {}
                }
                let candidate = fingerprint(&chain, &key).await;
                if candidate != current {
                    if resolver.load_files(&chain, &key).await.is_ok() {
                        current = candidate;
                        failed = false;
                        tracing::info!("TLS certificate replaced");
                    } else if !failed {
                        failed = true;
                        tracing::warn!("TLS certificate replacement rejected");
                    }
                }
            }
        });
        Ok(())
    }

    pub async fn automatic(
        self: &Arc<Self>,
        options: Acme,
        stop: CancellationToken,
    ) -> Result<(), StartupError> {
        options.prepare().await?;
        let path = options.folder.join("certificate.json");
        if let Ok(data) = read(&path, true).await {
            let data = Zeroizing::new(data);
            if let Ok(bundle) = serde_json::from_slice::<Bundle>(&data)
                && bundle.directory == options.directory
                && bundle.names == options.names
            {
                let _ = self.load(bundle.chain.as_bytes(), bundle.key.as_bytes());
            }
        }
        let resolver = self.clone();
        tokio::spawn(async move {
            loop {
                let now = time::OffsetDateTime::now_utc().unix_timestamp();
                let renew = renewal_due(resolver.expiry(), now, options.renew_before);
                if renew {
                    let renewal = tokio::select! {
                        _ = stop.cancelled() => break,
                        result = tokio::time::timeout(Duration::from_secs(160), options.issue(&resolver, &path)) => result,
                    };
                    if let Ok(mut challenges) = resolver.challenges.lock() {
                        challenges.clear();
                    }
                    if renewal.is_ok_and(|outcome| outcome.is_ok()) {
                        tracing::info!("ACME certificate installed");
                    } else {
                        tracing::warn!("ACME issuance or renewal failed");
                    }
                }
                tokio::select! {
                    _ = stop.cancelled() => break,
                    _ = tokio::time::sleep(if renew {Duration::from_secs(900)} else {Duration::from_secs(3600)}) => {},
                }
            }
        });
        Ok(())
    }
}

impl ResolvesServerCert for Certificates {
    fn resolve(&self, _: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        if self.expiry()? <= time::OffsetDateTime::now_utc().unix_timestamp() {
            return None;
        }
        self.key.read().ok()?.clone()
    }
}

pub struct Acme {
    directory: String,
    ca: Option<PathBuf>,
    folder: PathBuf,
    names: Vec<String>,
    email: String,
    renew_before: i64,
}

impl Acme {
    pub fn parse(doc: &serde_json::Value, db: &Path) -> Result<Option<Self>, &'static str> {
        let tls = &doc["tls"];
        if tls["enable"].as_bool() != Some(true) || tls["auto"].as_bool() != Some(true) {
            return Ok(None);
        }
        if tls["acme_agree"].as_bool() != Some(true) {
            return Err("tls.acme_agree");
        }
        let directory = tls["acme_directory"]
            .as_str()
            .filter(|v| !v.is_empty())
            .unwrap_or(if tls["staging"].as_bool() == Some(true) {
                LetsEncrypt::Staging.url()
            } else {
                LetsEncrypt::Production.url()
            });
        let url = url::Url::parse(directory).map_err(|_| "tls.acme_directory")?;
        if url.scheme() != "https"
            || url.host().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.fragment().is_some()
            || url.query().is_some()
        {
            return Err("tls.acme_directory");
        }
        let names = doc["host"]["name"].as_array().ok_or("host.name")?;
        if names.is_empty() || names.len() > 10 {
            return Err("host.name");
        }
        let mut names: Vec<_> = names
            .iter()
            .map(|name| {
                name.as_str()
                    .filter(|name| valid_hostname(name))
                    .map(str::to_ascii_lowercase)
                    .ok_or("host.name")
            })
            .collect::<Result<_, _>>()?;
        names.sort_unstable();
        names.dedup();
        let email = tls["email"]
            .as_str()
            .filter(|email| {
                email.len() <= 254 && email.contains('@') && !email.contains(char::is_whitespace)
            })
            .ok_or("tls.email")?
            .to_owned();
        let folder = tls["acme_storage"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| db.with_extension("acme"));
        let ca = tls["acme_ca_file"]
            .as_str()
            .filter(|v| !v.is_empty())
            .map(PathBuf::from);
        let renew_before = tls["acme_renew_before_seconds"]
            .as_i64()
            .filter(|seconds| (300..=5_184_000).contains(seconds))
            .ok_or("tls.acme_renew_before_seconds")?;
        Ok(Some(Self {
            directory: directory.into(),
            ca,
            folder,
            names,
            email,
            renew_before,
        }))
    }

    pub async fn check_storage(&self, media: &Path) -> Result<(), StartupError> {
        self.prepare().await?;
        if tokio::fs::canonicalize(&self.folder)
            .await?
            .starts_with(media)
        {
            return Err("ACME storage must be outside the public media directory".into());
        }
        Ok(())
    }

    async fn prepare(&self) -> Result<(), StartupError> {
        let mut builder = tokio::fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder.create(&self.folder).await?;
        let metadata = tokio::fs::symlink_metadata(&self.folder).await?;
        if !metadata.is_dir() || metadata.permissions().mode() & 0o077 != 0 {
            return Err("ACME storage must be a private directory (chmod 700)".into());
        }
        Ok(())
    }

    async fn account(&self) -> Result<Account, StartupError> {
        let builder = if let Some(ca) = &self.ca {
            Account::builder_with_root(ca)?
        } else {
            Account::builder()?
        };
        let path = self
            .folder
            .join(if self.directory == LetsEncrypt::Production.url() {
                "production.json"
            } else if self.directory == LetsEncrypt::Staging.url() {
                "staging.json"
            } else {
                "custom.json"
            });
        match read(&path, true).await {
            Ok(data) => {
                let data = Zeroizing::new(data);
                let saved: SavedAccount = serde_json::from_slice(&data)?;
                if self.directory == saved.directory {
                    return Ok(builder.from_credentials(saved.credentials).await?);
                }
            }
            Err(error)
                if error
                    .downcast_ref::<std::io::Error>()
                    .is_some_and(|error| error.kind() == std::io::ErrorKind::NotFound) => {}
            Err(error) => return Err(error),
        }
        let (account, credentials) = builder
            .create(
                &NewAccount {
                    contact: &[&format!("mailto:{}", self.email)],
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                self.directory.clone(),
                None,
            )
            .await?;
        let data = Zeroizing::new(serde_json::to_vec(&SavedAccount {
            directory: self.directory.clone(),
            credentials,
        })?);
        write_private(&path, &data).await?;
        Ok(account)
    }

    async fn issue(&self, resolver: &Certificates, path: &Path) -> Result<(), StartupError> {
        let account = self.account().await?;
        let identifiers: Vec<_> = self.names.iter().cloned().map(Identifier::Dns).collect();
        let mut order = account.new_order(&NewOrder::new(&identifiers)).await?;
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authorization = result?;
            match authorization.status {
                AuthorizationStatus::Valid => continue,
                AuthorizationStatus::Pending => {}
                _ => return Err("ACME authorization could not be started".into()),
            }
            let mut challenge = authorization
                .challenge(ChallengeType::Http01)
                .ok_or("ACME HTTP-01 challenge unavailable")?;
            let key_auth = challenge.key_authorization();
            resolver
                .challenges
                .lock()
                .map_err(|_| "ACME challenge storage unavailable")?
                .insert(challenge.token.clone(), key_auth.as_str().to_owned());
            challenge.set_ready().await?;
        }
        let retry = RetryPolicy::default().timeout(Duration::from_secs(60));
        if order.poll_ready(&retry).await? != OrderStatus::Ready {
            return Err("ACME order is not ready".into());
        }
        let key = Zeroizing::new(order.finalize().await?);
        let chain = order.poll_certificate(&retry).await?;
        let data = Zeroizing::new(serde_json::to_vec(&Bundle {
            directory: self.directory.clone(),
            names: self.names.clone(),
            chain: chain.clone(),
            key: key.to_string(),
        })?);
        let validated = resolver.validate(chain.as_bytes(), key.as_bytes())?;
        write_private(path, &data).await?;
        resolver.install(validated)
    }
}

#[derive(Serialize, Deserialize)]
struct SavedAccount {
    directory: String,
    credentials: AccountCredentials,
}

#[derive(Serialize, Deserialize)]
struct Bundle {
    directory: String,
    names: Vec<String>,
    chain: String,
    key: String,
}

impl Drop for Bundle {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.key.zeroize();
    }
}

fn valid_hostname(name: &str) -> bool {
    name.len() <= 253
        && name.contains('.')
        && name.parse::<std::net::IpAddr>().is_err()
        && name.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
}

type Revision = Option<(u64, i64, i64, u64)>;

async fn fingerprint(chain: &Path, key: &Path) -> (Revision, Revision) {
    async fn version(path: &Path) -> Revision {
        use std::os::unix::fs::MetadataExt;
        let metadata = tokio::fs::metadata(path).await.ok()?;
        Some((
            metadata.ino(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.len(),
        ))
    }
    (version(chain).await, version(key).await)
}

fn renewal_due(expiry: Option<i64>, now: i64, window: i64) -> bool {
    expiry.is_none_or(|expiry| expiry.saturating_sub(now) <= window)
}

async fn read(path: &Path, private: bool) -> Result<Vec<u8>, StartupError> {
    use std::os::unix::fs::OpenOptionsExt;
    // Opening the configured path with O_NOFOLLOW for secrets prevents accepting
    // a symlink to publicly readable PEM data.
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    if private {
        options.custom_flags(nix::libc::O_NOFOLLOW);
    }
    let path = path.to_path_buf();
    let file = tokio::task::spawn_blocking(move || options.open(path)).await??;
    let metadata = file.metadata()?;
    if !metadata.is_file() || (private && metadata.permissions().mode() & 0o077 != 0) {
        return Err("invalid TLS or ACME file permissions".into());
    }
    let mut data = Vec::new();
    tokio::fs::File::from_std(file)
        .take(131_073)
        .read_to_end(&mut data)
        .await?;
    if data.len() > 131_072 {
        return Err("TLS file exceeds size limit".into());
    }
    Ok(data)
}

async fn write_private(path: &Path, value: &[u8]) -> Result<(), StartupError> {
    let parent = path.parent().ok_or("invalid ACME storage path")?;
    let temp = tempfile::NamedTempFile::new_in(parent)?;
    let mut file = tokio::fs::File::from_std(temp.reopen()?);
    file.write_all(value).await?;
    file.sync_all().await?;
    temp.persist(path)?;
    tokio::fs::File::open(parent).await?.sync_all().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renewal_window_and_domain_validation() {
        assert!(renewal_due(None, 1000, 300));
        assert!(renewal_due(Some(1300), 1000, 300));
        assert!(!renewal_due(Some(1301), 1000, 300));
        assert!(valid_hostname("media.castvane.test"));
        for invalid in [
            "*.castvane.test",
            "media..test",
            "-bad.test",
            "localhost",
            "127.0.0.1",
            "castvane.test/path",
        ] {
            assert!(!valid_hostname(invalid));
        }
    }
}
