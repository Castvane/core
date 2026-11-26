// OpenRaft defines the error types returned by its storage and transport traits.
#![allow(clippy::result_large_err)]

use std::{
    fmt::Debug,
    io::Cursor,
    ops::{Bound, RangeBounds},
};

use openraft::{
    Entry, EntryPayload, LogId, RaftLogReader, RaftSnapshotBuilder, RaftStorage, Snapshot,
    SnapshotMeta, StorageError, StorageIOError, StoredMembership, Vote, storage::LogState,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlx::{Sqlite, SqlitePool, Transaction};

use super::{
    Types,
    model::{Applied, Data, MAX_STATE},
};

type Result<T> = std::result::Result<T, StorageError<u64>>;

#[derive(Clone)]
pub struct SqliteStore(pub SqlitePool);

#[derive(Default, Serialize, Deserialize)]
pub struct Machine {
    pub last: Option<LogId<u64>>,
    pub membership: StoredMembership<u64, openraft::BasicNode>,
    pub data: Data,
}

#[derive(Serialize, Deserialize)]
struct SavedSnapshot {
    meta: SnapshotMeta<u64, openraft::BasicNode>,
    data: Vec<u8>,
}

fn read(error: &(impl std::error::Error + 'static)) -> StorageError<u64> {
    StorageIOError::read(openraft::AnyError::new(error)).into()
}

fn write(error: &(impl std::error::Error + 'static)) -> StorageError<u64> {
    StorageIOError::write(openraft::AnyError::new(error)).into()
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(|error| read(&error))
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(|error| write(&error))
}

fn index(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(|error| read(&error))
}

async fn set(tx: &mut Transaction<'_, Sqlite>, key: &str, value: &impl Serialize) -> Result<()> {
    sqlx::query("INSERT INTO cluster_raft_state(key,data) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET data=excluded.data")
        .bind(key).bind(encode(value)?).execute(&mut **tx).await.map_err(|error| write(&error))?;
    Ok(())
}

impl SqliteStore {
    async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let row: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT data FROM cluster_raft_state WHERE key=?")
                .bind(key)
                .fetch_optional(&self.0)
                .await
                .map_err(|error| read(&error))?;
        row.as_deref().map(decode).transpose()
    }

    async fn save(&self, key: &str, value: &impl Serialize) -> Result<()> {
        let mut tx = self.0.begin().await.map_err(|error| write(&error))?;
        set(&mut tx, key, value).await?;
        tx.commit().await.map_err(|error| write(&error))
    }

    pub async fn machine(&self) -> Result<Machine> {
        Ok(self.get("machine").await?.unwrap_or_default())
    }
}

impl RaftLogReader<Types> for SqliteStore {
    async fn try_get_log_entries<R: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: R,
    ) -> Result<Vec<Entry<Types>>> {
        let first = match range.start_bound() {
            Bound::Included(value) => *value,
            Bound::Excluded(value) => match value.checked_add(1) {
                Some(next) => next,
                None => return Ok(Vec::new()),
            },
            Bound::Unbounded => 0,
        };
        let last = match range.end_bound() {
            Bound::Included(value) => *value,
            Bound::Excluded(value) => match value.checked_sub(1) {
                Some(previous) => previous,
                None => return Ok(Vec::new()),
            },
            Bound::Unbounded => i64::MAX as u64,
        };
        if first > last || first > i64::MAX as u64 {
            return Ok(Vec::new());
        }
        let rows: Vec<Vec<u8>> = sqlx::query_scalar(
            "SELECT entry FROM cluster_raft_log WHERE idx BETWEEN ? AND ? ORDER BY idx",
        )
        .bind(index(first)?)
        .bind(index(last.min(i64::MAX as u64))?)
        .fetch_all(&self.0)
        .await
        .map_err(|error| read(&error))?;
        rows.iter().map(|row| decode(row)).collect()
    }
}

impl RaftSnapshotBuilder<Types> for SqliteStore {
    async fn build_snapshot(&mut self) -> Result<Snapshot<Types>> {
        let machine = self.machine().await?;
        let meta = SnapshotMeta {
            last_log_id: machine.last,
            last_membership: machine.membership.clone(),
            snapshot_id: uuid::Uuid::new_v4().to_string(),
        };
        let data = encode(&machine)?;
        let saved = SavedSnapshot {
            meta: meta.clone(),
            data,
        };
        self.save("snapshot", &saved).await?;
        Ok(Snapshot {
            meta,
            snapshot: Box::new(Cursor::new(saved.data)),
        })
    }
}

impl RaftStorage<Types> for SqliteStore {
    type LogReader = Self;
    type SnapshotBuilder = Self;

    async fn save_vote(&mut self, vote: &Vote<u64>) -> Result<()> {
        self.save("vote", vote).await
    }

    async fn read_vote(&mut self) -> Result<Option<Vote<u64>>> {
        self.get("vote").await
    }

    async fn get_log_state(&mut self) -> Result<LogState<Types>> {
        let last_purged_log_id = self.get("purged").await?;
        let row: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT entry FROM cluster_raft_log ORDER BY idx DESC LIMIT 1")
                .fetch_optional(&self.0)
                .await
                .map_err(|error| read(&error))?;
        let last_log_id = row
            .as_deref()
            .map(decode::<Entry<Types>>)
            .transpose()?
            .map(|entry| entry.log_id)
            .or(last_purged_log_id);
        Ok(LogState {
            last_purged_log_id,
            last_log_id,
        })
    }

    async fn get_log_reader(&mut self) -> Self {
        self.clone()
    }

    async fn append_to_log<I>(&mut self, entries: I) -> Result<()>
    where
        I: IntoIterator<Item = Entry<Types>> + Send,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        let mut tx = self.0.begin().await.map_err(|error| write(&error))?;
        for entry in entries {
            sqlx::query("INSERT INTO cluster_raft_log(idx,entry) VALUES(?,?) ON CONFLICT(idx) DO UPDATE SET entry=excluded.entry")
                .bind(index(entry.log_id.index)?).bind(encode(&entry)?)
                .execute(&mut *tx).await.map_err(|error| write(&error))?;
        }
        tx.commit().await.map_err(|error| write(&error))
    }

    async fn delete_conflict_logs_since(&mut self, log_id: LogId<u64>) -> Result<()> {
        sqlx::query("DELETE FROM cluster_raft_log WHERE idx>=?")
            .bind(index(log_id.index)?)
            .execute(&self.0)
            .await
            .map_err(|error| write(&error))?;
        Ok(())
    }

    async fn purge_logs_upto(&mut self, log_id: LogId<u64>) -> Result<()> {
        let mut tx = self.0.begin().await.map_err(|error| write(&error))?;
        set(&mut tx, "purged", &log_id).await?;
        sqlx::query("DELETE FROM cluster_raft_log WHERE idx<=?")
            .bind(index(log_id.index)?)
            .execute(&mut *tx)
            .await
            .map_err(|error| write(&error))?;
        tx.commit().await.map_err(|error| write(&error))
    }

    async fn last_applied_state(
        &mut self,
    ) -> Result<(
        Option<LogId<u64>>,
        StoredMembership<u64, openraft::BasicNode>,
    )> {
        let machine = self.machine().await?;
        Ok((machine.last, machine.membership))
    }

    async fn apply_to_state_machine(&mut self, entries: &[Entry<Types>]) -> Result<Vec<Applied>> {
        let mut machine = self.machine().await?;
        let mut responses = Vec::with_capacity(entries.len());
        for entry in entries {
            machine.last = Some(entry.log_id);
            let applied = match &entry.payload {
                EntryPayload::Normal(cmd) => machine.data.apply(entry.log_id.index, cmd),
                EntryPayload::Membership(membership) => {
                    machine.membership = StoredMembership::new(machine.last, membership.clone());
                    Applied::Updated(entry.log_id.index)
                }
                EntryPayload::Blank => Applied::Updated(entry.log_id.index),
            };
            responses.push(applied);
        }
        self.save("machine", &machine).await?;
        Ok(responses)
    }

    async fn get_snapshot_builder(&mut self) -> Self {
        self.clone()
    }

    async fn begin_receiving_snapshot(&mut self) -> Result<Box<Cursor<Vec<u8>>>> {
        Ok(Box::new(Cursor::new(Vec::new())))
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMeta<u64, openraft::BasicNode>,
        snapshot: Box<Cursor<Vec<u8>>>,
    ) -> Result<()> {
        let data = snapshot.into_inner();
        let machine: Machine = decode(&data)?;
        if data.len() > MAX_STATE + 64 * 1024
            || machine.last != meta.last_log_id
            || machine.membership != meta.last_membership
        {
            return Err(write(&std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "snapshot metadata mismatch",
            )));
        }
        let mut tx = self.0.begin().await.map_err(|error| write(&error))?;
        set(&mut tx, "machine", &machine).await?;
        set(
            &mut tx,
            "snapshot",
            &SavedSnapshot {
                meta: meta.clone(),
                data,
            },
        )
        .await?;
        tx.commit().await.map_err(|error| write(&error))
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<Snapshot<Types>>> {
        Ok(self
            .get::<SavedSnapshot>("snapshot")
            .await?
            .map(|saved| Snapshot {
                meta: saved.meta,
                snapshot: Box::new(Cursor::new(saved.data)),
            }))
    }
}
