// OpenRaft defines the error types returned by its storage and transport traits.
#![allow(clippy::result_large_err)]

use std::{
    fmt::Debug,
    io::{self, Cursor},
    ops::{Bound, RangeBounds},
};

use futures_util::TryStreamExt;
use openraft::{
    EntryPayload, OptionalSend, RaftLogReader, RaftSnapshotBuilder, Snapshot, SnapshotMeta,
    StoredMembership,
    alias::{EntryOf, LogIdOf, SnapshotMetaOf, SnapshotOf, StoredMembershipOf, VoteOf},
    storage::{EntryResponder, IOFlushed, LogState, RaftLogStorage, RaftStateMachine},
};
use openraft_legacy::network_v1::SnapshotReceiverFactory;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sqlx::{Sqlite, SqlitePool, Transaction};

use super::{
    Types,
    model::{Applied, Data, MAX_STATE},
};

type Result<T> = io::Result<T>;

#[derive(Clone)]
pub struct SqliteStore(pub SqlitePool);

#[derive(Default, Serialize, Deserialize)]
pub struct Machine {
    pub last: Option<LogIdOf<Types>>,
    pub membership: StoredMembershipOf<Types>,
    pub data: Data,
}

#[derive(Serialize, Deserialize)]
struct SavedSnapshot {
    meta: SnapshotMetaOf<Types>,
    data: Vec<u8>,
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T> {
    serde_json::from_slice(bytes).map_err(io::Error::other)
}

fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    serde_json::to_vec(value).map_err(io::Error::other)
}

fn index(value: u64) -> Result<i64> {
    i64::try_from(value).map_err(io::Error::other)
}

async fn set(tx: &mut Transaction<'_, Sqlite>, key: &str, value: &impl Serialize) -> Result<()> {
    sqlx::query("INSERT INTO cluster_raft_state(key,data) VALUES(?,?) ON CONFLICT(key) DO UPDATE SET data=excluded.data")
        .bind(key).bind(encode(value)?).execute(&mut **tx).await.map_err(io::Error::other)?;
    Ok(())
}

impl SqliteStore {
    async fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let row: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT data FROM cluster_raft_state WHERE key=?")
                .bind(key)
                .fetch_optional(&self.0)
                .await
                .map_err(io::Error::other)?;
        row.as_deref().map(decode).transpose()
    }

    async fn save(&self, key: &str, value: &impl Serialize) -> Result<()> {
        let mut tx = self.0.begin().await.map_err(io::Error::other)?;
        set(&mut tx, key, value).await?;
        tx.commit().await.map_err(io::Error::other)
    }

    pub async fn machine(&self) -> Result<Machine> {
        Ok(self.get("machine").await?.unwrap_or_default())
    }
}

impl RaftLogReader<Types> for SqliteStore {
    async fn read_vote(&mut self) -> Result<Option<VoteOf<Types>>> {
        self.get("vote").await
    }

    async fn try_get_log_entries<R: RangeBounds<u64> + Clone + Debug + Send>(
        &mut self,
        range: R,
    ) -> Result<Vec<EntryOf<Types>>> {
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
        .map_err(io::Error::other)?;
        rows.iter().map(|row| decode(row)).collect()
    }
}

impl RaftSnapshotBuilder<Types> for SqliteStore {
    type SnapshotData = Cursor<Vec<u8>>;
    async fn build_snapshot(&mut self) -> Result<SnapshotOf<Types, Cursor<Vec<u8>>>> {
        let machine = self.machine().await?;
        let meta = SnapshotMeta {
            last_log_id: machine.last,
            last_membership: machine.membership.clone(),
        };
        let data = encode(&machine)?;
        let saved = SavedSnapshot {
            meta: meta.clone(),
            data,
        };
        self.save("snapshot", &saved).await?;
        Ok(Snapshot {
            meta,
            snapshot: Cursor::new(saved.data),
        })
    }
}

impl RaftLogStorage<Types> for SqliteStore {
    type LogReader = Self;

    async fn save_vote(&mut self, vote: &VoteOf<Types>) -> Result<()> {
        self.save("vote", vote).await
    }

    async fn get_log_state(&mut self) -> Result<LogState<Types>> {
        let last_purged_log_id = self.get("purged").await?;
        let row: Option<Vec<u8>> =
            sqlx::query_scalar("SELECT entry FROM cluster_raft_log ORDER BY idx DESC LIMIT 1")
                .fetch_optional(&self.0)
                .await
                .map_err(io::Error::other)?;
        let last_log_id = row
            .as_deref()
            .map(decode::<EntryOf<Types>>)
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

    async fn append<I>(&mut self, entries: I, flushed: IOFlushed<Types>) -> Result<()>
    where
        I: IntoIterator<Item = EntryOf<Types>> + Send,
    {
        let entries: Vec<_> = entries.into_iter().collect();
        let mut tx = self.0.begin().await.map_err(io::Error::other)?;
        for entry in entries {
            sqlx::query("INSERT INTO cluster_raft_log(idx,entry) VALUES(?,?) ON CONFLICT(idx) DO UPDATE SET entry=excluded.entry")
                .bind(index(entry.log_id.index)?).bind(encode(&entry)?)
                .execute(&mut *tx).await.map_err(io::Error::other)?;
        }
        tx.commit().await.map_err(io::Error::other)?;
        flushed.io_completed(Ok(()));
        Ok(())
    }

    async fn truncate_after(&mut self, last_log_id: Option<LogIdOf<Types>>) -> Result<()> {
        sqlx::query("DELETE FROM cluster_raft_log WHERE idx>?")
            .bind(
                last_log_id
                    .map(|id| index(id.index))
                    .transpose()?
                    .unwrap_or(-1),
            )
            .execute(&self.0)
            .await
            .map_err(io::Error::other)?;
        Ok(())
    }

    async fn purge(&mut self, log_id: LogIdOf<Types>) -> Result<()> {
        let mut tx = self.0.begin().await.map_err(io::Error::other)?;
        set(&mut tx, "purged", &log_id).await?;
        sqlx::query("DELETE FROM cluster_raft_log WHERE idx<=?")
            .bind(index(log_id.index)?)
            .execute(&mut *tx)
            .await
            .map_err(io::Error::other)?;
        tx.commit().await.map_err(io::Error::other)
    }
}

impl RaftStateMachine<Types> for SqliteStore {
    type SnapshotData = Cursor<Vec<u8>>;
    type SnapshotBuilder = Self;

    async fn applied_state(
        &mut self,
    ) -> Result<(Option<LogIdOf<Types>>, StoredMembershipOf<Types>)> {
        let machine = self.machine().await?;
        Ok((machine.last, machine.membership))
    }

    async fn apply<S>(&mut self, entries: S) -> Result<()>
    where
        S: futures_util::Stream<Item = Result<EntryResponder<Types>>> + Unpin + OptionalSend,
    {
        let mut entries = entries.try_ready_chunks(64);
        let mut machine = self.machine().await?;
        while let Some(batch) = entries.try_next().await.map_err(|error| error.1)? {
            let mut responses = Vec::with_capacity(batch.len());
            for (entry, responder) in batch {
                machine.last = Some(entry.log_id);
                let applied = match entry.payload {
                    EntryPayload::Normal(cmd) => machine.data.apply(entry.log_id.index, &cmd),
                    EntryPayload::Membership(membership) => {
                        machine.membership = StoredMembership::new(machine.last, membership);
                        Applied::Updated(entry.log_id.index)
                    }
                    EntryPayload::Blank => Applied::Updated(entry.log_id.index),
                };
                if let Some(responder) = responder {
                    responses.push((responder, applied));
                }
            }
            self.save("machine", &machine).await?;
            for (responder, applied) in responses {
                responder.send(applied);
            }
        }
        Ok(())
    }

    async fn get_snapshot_builder(&mut self) -> Self {
        self.clone()
    }

    async fn install_snapshot(
        &mut self,
        meta: &SnapshotMetaOf<Types>,
        snapshot: Cursor<Vec<u8>>,
    ) -> Result<()> {
        let data = snapshot.into_inner();
        let machine: Machine = decode(&data)?;
        if data.len() > MAX_STATE + 64 * 1024
            || machine.last != meta.last_log_id
            || machine.membership != meta.last_membership
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "snapshot metadata mismatch",
            ));
        }
        let mut tx = self.0.begin().await.map_err(io::Error::other)?;
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
        tx.commit().await.map_err(io::Error::other)
    }

    async fn get_current_snapshot(&mut self) -> Result<Option<SnapshotOf<Types, Cursor<Vec<u8>>>>> {
        Ok(self
            .get::<SavedSnapshot>("snapshot")
            .await?
            .map(|saved| Snapshot {
                meta: saved.meta,
                snapshot: Cursor::new(saved.data),
            }))
    }
}

impl SnapshotReceiverFactory<Types> for SqliteStore {
    type SnapshotReceiver = Cursor<Vec<u8>>;

    async fn begin_receiving_snapshot(&mut self) -> Result<Self::SnapshotReceiver> {
        Ok(Cursor::new(Vec::new()))
    }
}
