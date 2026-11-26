use std::{
    collections::HashMap,
    net::IpAddr,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime},
};

use jsonwebtoken::{Algorithm, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use zeroize::Zeroizing;

use super::storage::{Error, now_secs};

#[derive(Clone)]
pub struct Limits {
    pub enabled: bool,
    pub total: usize,
    pub per_stream: usize,
    pub idle: Duration,
    pub total_rate: u64,
    pub stream_rate: u64,
    pub session_rate: u64,
}

impl Limits {
    pub fn from_document(doc: &Value) -> Result<Self, Error> {
        let max =
            |field: &str, default: u64| doc[field].as_u64().filter(|v| *v > 0).unwrap_or(default);
        if max("max_sessions", 1024) > 20_000 || max("max_viewers_per_stream", 256) > 20_000 {
            return Err(Error::Invalid);
        }
        Ok(Self {
            enabled: doc["enable"].as_bool().unwrap_or(true),
            total: max("max_sessions", 1024) as usize,
            per_stream: max("max_viewers_per_stream", 256) as usize,
            idle: Duration::from_secs(max("session_timeout_sec", 30).clamp(2, 3600)),
            total_rate: doc["max_bitrate_mbit"]
                .as_u64()
                .unwrap_or(0)
                .saturating_mul(125_000),
            stream_rate: doc["stream_bitrate_kbit"]
                .as_u64()
                .unwrap_or(0)
                .saturating_mul(125),
            session_rate: doc["session_bitrate_kbit"]
                .as_u64()
                .unwrap_or(0)
                .saturating_mul(125),
        })
    }
}

#[derive(Default)]
struct Active {
    data: HashMap<String, Session>,
    totals: HashMap<String, (u64, u64)>,
    deadline: Option<Instant>,
    streams: HashMap<String, Instant>,
}

struct Session {
    created: u64,
    touched: Instant,
    protocol: &'static str,
    reference: String,
    tx: u64,
    requests: u64,
    deadline: Option<Instant>,
    transfers: u32,
}

pub struct Sessions {
    limits: Limits,
    active: Mutex<Active>,
    key: std::sync::RwLock<Zeroizing<Vec<u8>>>,
    transmitted: std::sync::atomic::AtomicU64,
}

#[derive(Deserialize, Serialize)]
struct Token {
    iss: String,
    sub: String,
    exp: u64,
    scope: String,
    remote: Vec<ipnet::IpNet>,
}

#[derive(Deserialize, Serialize)]
pub struct TokenRequest {
    #[serde(default, rename = "match")]
    pub scope: String,
    #[serde(default)]
    pub remote: Vec<String>,
    #[serde(default)]
    pub extra: Value,
    pub ttl_sec: u64,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub token: String,
}

impl Sessions {
    pub fn new(limits: Limits, key: Zeroizing<Vec<u8>>) -> Self {
        Self {
            limits,
            active: Default::default(),
            key: std::sync::RwLock::new(key),
            transmitted: Default::default(),
        }
    }

    pub(crate) fn metrics(&self) -> (usize, u64) {
        (
            self.state().data.len(),
            self.transmitted.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    pub fn rotate(&self, key: Zeroizing<Vec<u8>>) -> Result<(), Error> {
        *self.key.write().map_err(|_| Error::Unavailable)? = key;
        Ok(())
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    pub fn issue(&self, user: &str, request: &mut TokenRequest) -> Result<(), Error> {
        if request.ttl_sec == 0 || request.ttl_sec > 604_800 || request.scope.len() > 1024 {
            return Err(Error::Invalid);
        }
        glob::Pattern::new(&request.scope).map_err(|_| Error::Invalid)?;
        let remote = request
            .remote
            .iter()
            .map(|value| {
                value
                    .parse::<ipnet::IpNet>()
                    .or_else(|_| value.parse::<IpAddr>().map(ipnet::IpNet::from))
                    .map_err(|_| Error::Invalid)
            })
            .collect::<Result<_, _>>()?;
        let claim = Token {
            iss: "datarhei-core-media".into(),
            sub: user.into(),
            exp: now_secs(SystemTime::now()) + request.ttl_sec,
            scope: request.scope.clone(),
            remote,
        };
        request.token = jsonwebtoken::encode(
            &Header::new(Algorithm::HS256),
            &claim,
            &EncodingKey::from_secret(&self.key.read().map_err(|_| Error::Unavailable)?),
        )
        .map_err(|_| Error::Unavailable)?;
        Ok(())
    }

    pub fn validate_token(&self, token: &str, path: &str, peer: Option<IpAddr>) -> bool {
        if token.len() > 4096 {
            return false;
        }
        let mut validation = Validation::new(Algorithm::HS256);
        validation.leeway = 0;
        validation.set_issuer(&["datarhei-core-media"]);
        let Ok(key) = self.key.read() else {
            return false;
        };
        let Ok(token) =
            jsonwebtoken::decode::<Token>(token, &DecodingKey::from_secret(&key), &validation)
        else {
            return false;
        };
        let claim = token.claims;
        now_secs(SystemTime::now()) < claim.exp
            && (claim.scope.is_empty()
                || glob::Pattern::new(&claim.scope).is_ok_and(|pattern| pattern.matches(path)))
            && (claim.remote.is_empty()
                || peer.is_some_and(|peer| claim.remote.iter().any(|net| net.contains(&peer))))
    }

    fn state(&self) -> std::sync::MutexGuard<'_, Active> {
        let mut active = self
            .active
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let idle = self.limits.idle;
        active
            .data
            .retain(|_, item| item.transfers > 0 || item.touched.elapsed() < idle);
        active
    }

    pub fn open(
        self: &Arc<Self>,
        requested: Option<&str>,
        protocol: &'static str,
        reference: &str,
    ) -> Result<Transfer, Error> {
        let mut active = self.state();
        let id = if let Some(id) = requested.filter(|id| {
            active.data.get(*id).is_some_and(|s| {
                s.protocol == protocol && (protocol == "hls" || s.reference == reference)
            })
        }) {
            id.to_owned()
        } else {
            if active.data.len() >= self.limits.total
                || active
                    .data
                    .values()
                    .filter(|item| item.reference == reference && item.protocol == protocol)
                    .count()
                    >= self.limits.per_stream
            {
                return Err(Error::Full);
            }
            let id = uuid::Uuid::new_v4().simple().to_string();
            active.data.insert(
                id.clone(),
                Session {
                    created: now_secs(SystemTime::now()),
                    touched: Instant::now(),
                    protocol,
                    reference: reference.to_owned(),
                    tx: 0,
                    requests: 0,
                    deadline: None,
                    transfers: 0,
                },
            );
            if active.totals.len() < 8192 || active.totals.contains_key(reference) {
                let value = active.totals.entry(reference.to_owned()).or_default();
                value.0 = value.0.saturating_add(1);
            }
            id
        };
        let session = active.data.get_mut(&id).ok_or(Error::Unavailable)?;
        if session.transfers >= 16 {
            return Err(Error::Full);
        }
        session.touched = Instant::now();
        session.requests += 1;
        session.transfers += 1;
        Ok(Transfer {
            owner: self.clone(),
            id,
        })
    }

    pub fn view(&self, collectors: &str, summary: bool) -> Value {
        let state = self.state();
        let selection: Vec<_> = if collectors.is_empty() {
            vec!["hls", "http", "rtmp", "srt", "flv", "dash"]
        } else {
            collectors.split(',').take(32).collect()
        };
        let mut result = json!({});
        for protocol in selection {
            if !["hls", "http", "rtmp", "srt", "flv", "dash"].contains(&protocol) {
                continue;
            }
            let mut total_rate = 0.0;
            let list:Vec<_>=state.data.iter().filter(|(_,s)|s.protocol==protocol && self.limits.enabled)
                .map(|(id,s)| {
                    let elapsed=now_secs(SystemTime::now()).saturating_sub(s.created).max(1);
                    let rate=s.tx as f64 * 0.008 / elapsed as f64;
                    total_rate+=rate;
                    let mut item=json!({"id":id,"reference":s.reference,"created_at":s.created,"local":"","remote":"","extra":{},
                        "bytes_rx":0,"bytes_tx":s.tx,"bandwidth_rx_kbit":0,"bandwidth_tx_kbit":rate});
                    if protocol=="hls" {item["hls"]=json!({"hls_variants":[],"hls_segments":{"requests":s.requests},
                        "http_status":{},"bandwidth_tx_bits":{"min":0,"max":0,"avg":rate*1000.0}});}
                    item
                }).collect();
            if summary {
                let count = list.len();
                let references: serde_json::Map<_, _> = if protocol == "hls" {
                    state.totals.iter().map(|(key,(count,bytes))|
                    (key.clone(),json!({"sessions":count,"traffic_rx_mb":0,"traffic_tx_mb":bytes/1_000_000}))).collect()
                } else {
                    Default::default()
                };
                result[protocol] = json!({"active":{"list":list,"sessions":count,"bandwidth_rx_mbit":0,
                    "bandwidth_tx_mbit":total_rate/1000.0,"max_sessions":self.limits.total,
                    "max_bandwidth_rx_mbit":0,"max_bandwidth_tx_mbit":self.limits.total_rate/125_000},
                    "summary":{"remote":{},"local":{},"reference":references}});
            } else {
                result[protocol] = json!(list);
            }
        }
        result
    }

    pub fn counts(&self, reference: &str) -> (usize, u64) {
        let state = self.state();
        if !self.limits.enabled {
            return (0, 0);
        }
        (
            state
                .data
                .values()
                .filter(|s| s.protocol == "hls" && s.reference.starts_with(reference))
                .count(),
            state
                .totals
                .iter()
                .filter(|(key, _)| key.starts_with(reference))
                .map(|(_, v)| v.0)
                .sum(),
        )
    }
}

pub struct Transfer {
    owner: Arc<Sessions>,
    pub id: String,
}

impl Transfer {
    pub async fn account(&self, bytes: usize) -> Result<(), Error> {
        let at = {
            let mut state = self.owner.state();
            let session = state.data.get_mut(&self.id).ok_or(Error::Unavailable)?;
            session.touched = Instant::now();
            session.tx = session.tx.saturating_add(bytes as u64);
            let now = Instant::now();
            let mut at = pace(
                &mut session.deadline,
                self.owner.limits.session_rate,
                bytes,
                now,
            )?;
            let reference = session.reference.clone();
            if let Some(value) = state.totals.get_mut(&reference) {
                value.1 = value.1.saturating_add(bytes as u64);
            }
            at = at.max(pace(
                &mut state.deadline,
                self.owner.limits.total_rate,
                bytes,
                now,
            )?);
            if self.owner.limits.stream_rate > 0 {
                if state.streams.len() >= 4096 {
                    state.streams.retain(|_, deadline| *deadline > now);
                }
                if state.streams.len() >= 4096 && !state.streams.contains_key(&reference) {
                    return Err(Error::Full);
                }
                let mut previous = state.streams.get(&reference).copied();
                at = at.max(pace(
                    &mut previous,
                    self.owner.limits.stream_rate,
                    bytes,
                    now,
                )?);
                if let Some(deadline) = previous {
                    state.streams.insert(reference, deadline);
                }
            }
            at
        };
        tokio::time::sleep_until(at.into()).await;
        self.owner
            .transmitted
            .fetch_add(bytes as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }
}

impl Drop for Transfer {
    fn drop(&mut self) {
        if let Some(session) = self.owner.state().data.get_mut(&self.id) {
            session.transfers = session.transfers.saturating_sub(1);
            session.touched = Instant::now();
        }
    }
}

fn pace(
    deadline: &mut Option<Instant>,
    rate: u64,
    bytes: usize,
    now: Instant,
) -> Result<Instant, Error> {
    if rate == 0 {
        return Ok(now);
    }
    let at = deadline.unwrap_or(now).max(now);
    let next = at + Duration::from_secs_f64(bytes as f64 / rate as f64);
    if next.duration_since(now) > Duration::from_secs(5) {
        return Err(Error::Full);
    }
    *deadline = Some(next);
    Ok(next)
}
