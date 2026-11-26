use std::{
    ops::Range,
    time::{Duration, SystemTime},
};

use async_trait::async_trait;
use futures_util::{StreamExt, TryStreamExt};
use object_store::{
    ClientOptions, GetOptions, GetRange, MultipartUpload, ObjectMeta, ObjectStore, ObjectStoreExt,
    RetryConfig,
    aws::{AmazonS3, AmazonS3Builder},
    path::Path,
};
use serde::Deserialize;

use super::{Error, Key, MAX_LIST, MAX_UPLOAD, Meta, Storage, Stream, Usage, bounded};

#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct S3Config {
    pub name: String,
    pub mountpoint: String,
    pub endpoint: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub bucket: String,
    pub region: String,
    pub use_ssl: bool,
    pub virtual_hosted_style: bool,
}

pub struct S3(AmazonS3);

impl S3 {
    pub fn open(config: &S3Config) -> Result<Self, Error> {
        if config.bucket.is_empty()
            || config.access_key_id.is_empty()
            || config.secret_access_key.is_empty()
        {
            return Err(Error::Invalid);
        }
        let mut builder = AmazonS3Builder::new()
            .with_bucket_name(&config.bucket)
            .with_region(if config.region.is_empty() {
                "us-east-1"
            } else {
                &config.region
            })
            .with_access_key_id(&config.access_key_id)
            .with_secret_access_key(&config.secret_access_key)
            .with_virtual_hosted_style_request(config.virtual_hosted_style)
            .with_retry(RetryConfig {
                max_retries: 3,
                retry_timeout: Duration::from_secs(20),
                ..Default::default()
            })
            .with_client_options(
                ClientOptions::new()
                    .with_connect_timeout(Duration::from_secs(4))
                    .with_timeout(Duration::from_secs(30)),
            );
        if !config.endpoint.is_empty() {
            let endpoint = if config.endpoint.contains("://") {
                config.endpoint.clone()
            } else {
                format!(
                    "{}://{}",
                    if config.use_ssl { "https" } else { "http" },
                    config.endpoint
                )
            };
            let url = url::Url::parse(&endpoint).map_err(|_| Error::Invalid)?;
            if url.host().is_none()
                || !url.username().is_empty()
                || url.password().is_some()
                || url.fragment().is_some()
                || url.query().is_some()
                || url.path() != "/"
                || !matches!(url.scheme(), "http" | "https")
                || (config.use_ssl && url.scheme() != "https")
            {
                return Err(Error::Invalid);
            }
            builder = builder
                .with_endpoint(endpoint)
                .with_allow_http(url.scheme() == "http" && !config.use_ssl);
        }
        Ok(Self(builder.build().map_err(map_error)?))
    }
}

fn map_error(error: object_store::Error) -> Error {
    match error {
        object_store::Error::NotFound { .. } => Error::Missing,
        object_store::Error::Precondition { .. } => Error::Changed,
        _ => Error::Unavailable,
    }
}

fn key_path(key: &Key) -> Path {
    Path::from_iter(key.as_str().split('/'))
}

fn meta(key: Key, object: ObjectMeta) -> Meta {
    let modified: SystemTime = object.last_modified.into();
    let tag = object
        .e_tag
        .unwrap_or_else(|| format!("\"{}-{}\"", object.size, super::now_secs(modified)));
    Meta {
        key,
        size: object.size,
        modified,
        tag,
    }
}

#[async_trait]
impl Storage for S3 {
    async fn health(&self) -> Result<(), Error> {
        self.0
            .list(None)
            .next()
            .await
            .transpose()
            .map_err(map_error)?;
        Ok(())
    }

    async fn stat(&self, key: &Key) -> Result<Meta, Error> {
        Ok(meta(
            key.clone(),
            self.0.head(&key_path(key)).await.map_err(map_error)?,
        ))
    }

    async fn read(&self, meta: &Meta, range: Range<u64>) -> Result<Stream, Error> {
        if range.is_empty() {
            return Ok(Box::pin(futures_util::stream::empty()));
        }
        let get = self
            .0
            .get_opts(
                &key_path(&meta.key),
                GetOptions {
                    range: Some(GetRange::Bounded(range)),
                    if_match: Some(meta.tag.clone()),
                    ..Default::default()
                },
            )
            .await
            .map_err(map_error)?;
        Ok(Box::pin(get.into_stream().map_err(map_error)))
    }

    async fn put(&self, key: &Key, stream: Stream) -> Result<bool, Error> {
        let path = key_path(key);
        let created = match self.stat(key).await {
            Ok(_) => false,
            Err(Error::Missing) => true,
            Err(error) => return Err(error),
        };
        let mut stream = bounded(stream, MAX_UPLOAD);
        let mut pending = Vec::new();
        let mut pending_bytes = 0;
        let mut multipart = Upload(None);
        const PART: usize = 8 * 1024 * 1024;
        while let Some(chunk) = stream.next().await {
            let mut chunk = chunk?;
            while !chunk.is_empty() {
                let part = chunk.split_to(chunk.len().min(PART - pending_bytes));
                pending_bytes += part.len();
                pending.push(part);
                if pending_bytes == PART {
                    if multipart.0.is_none() {
                        multipart.0 = Some(self.0.put_multipart(&path).await.map_err(map_error)?);
                    }
                    let upload = multipart.0.as_mut().ok_or(Error::Unavailable)?;
                    upload
                        .put_part(std::mem::take(&mut pending).into_iter().collect())
                        .await
                        .map_err(map_error)?;
                    pending_bytes = 0;
                }
            }
        }
        if let Some(upload) = &mut multipart.0 {
            if !pending.is_empty() {
                upload
                    .put_part(pending.into_iter().collect())
                    .await
                    .map_err(map_error)?;
            }
            upload.complete().await.map_err(map_error)?;
            multipart.0 = None;
        } else {
            self.0
                .put(&path, pending.into_iter().collect())
                .await
                .map_err(map_error)?;
        }
        Ok(created)
    }

    async fn delete(&self, key: &Key) -> Result<(), Error> {
        self.stat(key).await?;
        self.0.delete(&key_path(key)).await.map_err(map_error)
    }

    async fn list(&self) -> Result<Vec<Meta>, Error> {
        let mut objects = self.0.list(None);
        let mut result = Vec::new();
        let mut seen = 0;
        while let Some(item) = objects.next().await {
            seen += 1;
            if seen > MAX_LIST {
                return Err(Error::Full);
            }
            let item = item.map_err(map_error)?;
            let logical = item
                .location
                .parts()
                .map(|part| part.as_ref().to_owned())
                .collect::<Vec<_>>()
                .join("/");
            if let Ok(key) = Key::parse(&logical) {
                result.push(meta(key, item));
            }
        }
        Ok(result)
    }

    async fn usage(&self) -> Result<Usage, Error> {
        let files = self.list().await?;
        Ok(Usage {
            bytes: files.iter().map(|meta| meta.size).sum(),
            resident_bytes: 0,
            objects: files.len(),
            capacity_bytes: None,
        })
    }
}

struct Upload(Option<Box<dyn MultipartUpload>>);

impl Drop for Upload {
    fn drop(&mut self) {
        if let Some(mut upload) = self.0.take()
            && let Ok(runtime) = tokio::runtime::Handle::try_current()
        {
            runtime.spawn(async move {
                let _ = upload.abort().await;
            });
        }
    }
}
