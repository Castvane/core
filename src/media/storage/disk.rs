use std::{
    ops::Range,
    path::{Path, PathBuf},
    sync::Arc,
    time::UNIX_EPOCH,
};

use async_trait::async_trait;
use cap_std::{
    ambient_authority,
    fs::{Dir, Metadata, MetadataExt, OpenOptions, OpenOptionsExt},
};
use futures_util::{StreamExt, TryStreamExt};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio_util::io::ReaderStream;

use super::{Error, Key, MAX_LIST, MAX_UPLOAD, Meta, Storage, Stream, Usage, bounded};

pub struct Disk {
    root: Arc<Dir>,
    limit: Option<u64>,
    publish: Arc<tokio::sync::Mutex<()>>,
}

impl Disk {
    pub async fn open(path: PathBuf, limit: Option<u64>) -> Result<Self, Error> {
        tokio::fs::create_dir_all(&path).await?;
        let root = io(move || Ok(Dir::open_ambient_dir(path, ambient_authority())?)).await?;
        Ok(Self {
            root: Arc::new(root),
            limit,
            publish: Arc::new(tokio::sync::Mutex::new(())),
        })
    }

    async fn remove(&self, key: &Key, tag: Option<String>) -> Result<(), Error> {
        let guard = self.publish.clone().lock_owned().await;
        let root = self.root.clone();
        let key = key.clone();
        io(move || {
            let _guard = guard;
            if let Some(tag) = tag
                && meta(&key, &root.metadata(key.as_str())?)?.tag != tag
            {
                return Err(Error::Changed);
            }
            root.remove_file(key.as_str())?;
            Ok(())
        })
        .await
    }
}

async fn io<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Error> + Send + 'static,
) -> Result<T, Error> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|_| Error::Unavailable)?
}

fn meta(key: &Key, metadata: &Metadata) -> Result<Meta, Error> {
    if !metadata.is_file() {
        return Err(Error::Missing);
    }
    let modified = metadata
        .modified()
        .map(|t| t.into_std())
        .unwrap_or(UNIX_EPOCH);
    #[cfg(unix)]
    let tag = format!(
        "\"{:x}-{:x}-{:x}-{:x}\"",
        metadata.ino(),
        metadata.len(),
        metadata.mtime(),
        metadata.mtime_nsec()
    );
    #[cfg(windows)]
    let tag = format!(
        "W/\"{:x}-{:x}-{:x}\"",
        metadata.creation_time(),
        metadata.last_write_time(),
        metadata.len()
    );
    Ok(Meta {
        key: key.clone(),
        size: metadata.len(),
        modified,
        tag,
    })
}

#[async_trait]
impl Storage for Disk {
    async fn health(&self) -> Result<(), Error> {
        let root = self.root.clone();
        io(move || root.dir_metadata().map(|_| ()).map_err(Error::from)).await
    }

    async fn stat(&self, key: &Key) -> Result<Meta, Error> {
        let root = self.root.clone();
        let key = key.clone();
        io(move || meta(&key, &root.metadata(key.as_str())?)).await
    }

    async fn read(&self, expected: &Meta, range: Range<u64>) -> Result<Stream, Error> {
        let root = self.root.clone();
        let expected = expected.clone();
        let file = io(move || {
            let mut opts = OpenOptions::new();
            opts.read(true);
            #[cfg(unix)]
            opts.custom_flags(nix::libc::O_NONBLOCK);
            #[cfg(windows)]
            opts.share_mode(0x1 | 0x2 | 0x4); // FILE_SHARE_READ, WRITE and DELETE.
            let file = root.open_with(expected.key.as_str(), &opts)?;
            if meta(&expected.key, &file.metadata()?)?.tag != expected.tag {
                return Err(Error::Changed);
            }
            Ok(file.into_std())
        })
        .await?;
        let mut file = tokio::fs::File::from_std(file);
        file.seek(std::io::SeekFrom::Start(range.start)).await?;
        Ok(Box::pin(
            ReaderStream::with_capacity(file.take(range.end - range.start), 64 * 1024)
                .map_err(Error::from),
        ))
    }

    async fn put(&self, key: &Key, stream: Stream) -> Result<bool, Error> {
        let root = self.root.clone();
        let key = key.clone();
        let (temp, file) = io(move || {
            let target = Path::new(key.as_str());
            if let Some(parent) = target
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                root.create_dir_all(parent)?;
            }
            let path = target.with_file_name(format!(".core-upload-{}", uuid::Uuid::new_v4()));
            let mut opts = OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            opts.mode(0o600);
            let file = root.open_with(&path, &opts)?;
            Ok((Temp { root, path, key }, file.into_std()))
        })
        .await?;
        let mut stream = bounded(stream, self.limit.unwrap_or(MAX_UPLOAD).min(MAX_UPLOAD));
        let mut file = tokio::fs::File::from_std(file);
        let mut size = 0u64;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            size += chunk.len() as u64;
            file.write_all(&chunk).await?;
        }
        file.sync_all().await?;
        drop(file);
        let guard = self.publish.clone().lock_owned().await;
        let prior = match self.stat(&temp.key).await {
            Ok(meta) => Some(meta.size),
            Err(Error::Missing) => None,
            Err(error) => return Err(error),
        };
        if let Some(limit) = self.limit {
            let used = self.list().await?.iter().map(|meta| meta.size).sum::<u64>();
            if used
                .saturating_sub(prior.unwrap_or_default())
                .saturating_add(size)
                > limit
            {
                return Err(Error::Full);
            }
        }
        io(move || {
            let _guard = guard;
            temp.root
                .rename(&temp.path, &temp.root, temp.key.as_str())?;
            Ok(prior.is_none())
        })
        .await
    }

    async fn delete(&self, key: &Key) -> Result<(), Error> {
        self.remove(key, None).await
    }

    async fn delete_unchanged(&self, expected: &Meta) -> Result<(), Error> {
        self.remove(&expected.key, Some(expected.tag.clone())).await
    }

    async fn list(&self) -> Result<Vec<Meta>, Error> {
        let root = self.root.clone();
        io(move || {
            let mut result = Vec::new();
            let mut pending = vec![PathBuf::from(".")];
            let mut seen = 0;
            while let Some(parent) = pending.pop() {
                for entry in root.read_dir(&parent)? {
                    seen += 1;
                    if seen > MAX_LIST {
                        return Err(Error::Full);
                    }
                    let entry = entry?;
                    let local = parent.join(entry.file_name());
                    let Some(path) = local.strip_prefix(".").ok().and_then(Path::to_str) else {
                        continue;
                    };
                    #[cfg(windows)]
                    let path = &path.replace('\\', "/");
                    let Ok(key) = Key::parse(path) else { continue };
                    let kind = entry.file_type()?;
                    if kind.is_dir() {
                        pending.push(local);
                    } else if kind.is_file() {
                        match entry.metadata() {
                            Ok(data) => result.push(meta(&key, &data)?),
                            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
                            Err(err) => return Err(err.into()),
                        }
                    }
                }
            }
            Ok(result)
        })
        .await
    }

    async fn usage(&self) -> Result<Usage, Error> {
        let files = self.list().await?;
        let bytes = files.iter().map(|meta| meta.size).sum();
        Ok(Usage {
            bytes,
            resident_bytes: bytes,
            objects: files.len(),
            capacity_bytes: self.limit,
        })
    }
}

struct Temp {
    root: Arc<Dir>,
    path: PathBuf,
    key: Key,
}

impl Drop for Temp {
    fn drop(&mut self) {
        let _ = self.root.remove_file(&self.path);
    }
}
