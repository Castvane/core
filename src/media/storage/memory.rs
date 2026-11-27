use std::{
    collections::BTreeMap,
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::SystemTime,
};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::{StreamExt, stream};

use super::{Error, Key, MAX_LIST, Meta, Storage, Stream, Usage, bounded};

#[derive(Clone)]
struct Entry {
    meta: Meta,
    bytes: Bytes,
}

pub struct Memory {
    entries: tokio::sync::Mutex<BTreeMap<Key, Entry>>,
    bytes: Arc<AtomicU64>,
    limit: u64,
    evict: bool,
}

impl Memory {
    pub fn new(limit: u64, evict: bool) -> Self {
        Self {
            entries: Default::default(),
            bytes: Default::default(),
            limit,
            evict,
        }
    }

    async fn reserve(&self, key: &Key, count: u64) -> Result<(), Error> {
        loop {
            if self
                .bytes
                .try_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                    used.checked_add(count).filter(|next| *next <= self.limit)
                })
                .is_ok()
            {
                return Ok(());
            }
            if !self.evict {
                return Err(Error::Full);
            }
            let mut entries = self.entries.lock().await;
            let oldest = entries
                .iter()
                .filter(|(candidate, entry)| *candidate != key && entry.meta.size > 0)
                .min_by_key(|(_, entry)| entry.meta.modified)
                .map(|(candidate, _)| candidate.clone());
            if oldest.and_then(|oldest| entries.remove(&oldest)).is_none() {
                return Err(Error::Full);
            }
        }
    }
}

struct Allocation {
    bytes: Vec<u8>,
    charged: u64,
    owner: Arc<AtomicU64>,
}

impl AsRef<[u8]> for Allocation {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for Allocation {
    fn drop(&mut self) {
        self.owner.fetch_sub(self.charged, Ordering::AcqRel);
    }
}

#[async_trait]
impl Storage for Memory {
    async fn stat(&self, key: &Key) -> Result<Meta, Error> {
        self.entries
            .lock()
            .await
            .get(key)
            .map(|entry| entry.meta.clone())
            .ok_or(Error::Missing)
    }

    async fn read(&self, meta: &Meta, range: Range<u64>) -> Result<Stream, Error> {
        let entries = self.entries.lock().await;
        let entry = entries.get(&meta.key).ok_or(Error::Missing)?;
        if entry.meta.tag != meta.tag {
            return Err(Error::Changed);
        }
        let start = usize::try_from(range.start).map_err(|_| Error::Invalid)?;
        let end = usize::try_from(range.end).map_err(|_| Error::Invalid)?;
        if start > end || end > entry.bytes.len() {
            return Err(Error::Invalid);
        }
        let bytes = entry.bytes.slice(start..end);
        Ok(Box::pin(stream::unfold(bytes, |mut bytes| async move {
            if bytes.is_empty() {
                return None;
            }
            let prefix = bytes.split_to(bytes.len().min(64 * 1024));
            Some((Ok(prefix), bytes))
        })))
    }

    async fn put(&self, key: &Key, stream: Stream) -> Result<bool, Error> {
        let mut allocation = Allocation {
            bytes: Vec::new(),
            charged: 0,
            owner: self.bytes.clone(),
        };
        let mut stream = bounded(stream, self.limit);
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            let wanted = allocation
                .bytes
                .len()
                .checked_add(chunk.len())
                .ok_or(Error::Full)?;
            if wanted > allocation.bytes.capacity() {
                let desired = wanted
                    .max(allocation.bytes.capacity().saturating_mul(2))
                    .min(self.limit as usize);
                let charge = (desired - allocation.bytes.capacity()) as u64;
                self.reserve(key, charge).await?;
                allocation.charged += charge;
                allocation
                    .bytes
                    .try_reserve_exact(desired - allocation.bytes.len())
                    .map_err(|_| Error::Full)?;
            }
            allocation.bytes.extend_from_slice(&chunk);
        }
        let meta = Meta {
            key: key.clone(),
            size: allocation.bytes.len() as u64,
            modified: SystemTime::now(),
            tag: format!("\"{}\"", uuid::Uuid::new_v4()),
        };
        let mut entries = self.entries.lock().await;
        if entries.len() >= MAX_LIST && !entries.contains_key(key) {
            return Err(Error::Full);
        }
        Ok(entries
            .insert(
                key.clone(),
                Entry {
                    meta,
                    bytes: Bytes::from_owner(allocation),
                },
            )
            .is_none())
    }

    async fn delete(&self, key: &Key) -> Result<(), Error> {
        self.entries
            .lock()
            .await
            .remove(key)
            .map(|_| ())
            .ok_or(Error::Missing)
    }

    async fn delete_unchanged(&self, expected: &Meta) -> Result<(), Error> {
        let mut entries = self.entries.lock().await;
        let entry = entries.get(&expected.key).ok_or(Error::Missing)?;
        if entry.meta.tag != expected.tag {
            return Err(Error::Changed);
        }
        entries.remove(&expected.key);
        Ok(())
    }

    async fn list(&self) -> Result<Vec<Meta>, Error> {
        Ok(self
            .entries
            .lock()
            .await
            .values()
            .map(|entry| entry.meta.clone())
            .collect())
    }

    async fn usage(&self) -> Result<Usage, Error> {
        let entries = self.entries.lock().await;
        Ok(Usage {
            bytes: entries.values().map(|entry| entry.meta.size).sum(),
            resident_bytes: self.bytes.load(Ordering::Acquire),
            objects: entries.len(),
            capacity_bytes: Some(self.limit),
        })
    }
}
