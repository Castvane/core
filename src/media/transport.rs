use std::{
    collections::{BTreeSet, HashMap},
    net::{IpAddr, SocketAddr},
    path::Path,
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::Deserialize;
use serde_json::{Value, json};
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, process::Child};
use tokio_util::sync::CancellationToken;

use super::{
    sessions::Limits,
    storage::{Error, Key, now_secs},
};

struct Policy {
    rtmp_token: String,
    srt_token: String,
    srt_process_token: String,
    prefix: String,
    admin: String,
    admission: Mutex<HashMap<String, Instant>>,
    limits: Limits,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Authorization {
    action: String,
    protocol: String,
    path: String,
    password: String,
    user: String,
    query: String,
    id: Option<String>,
}

async fn auth(State(policy): State<Arc<Policy>>, Json(request): Json<Authorization>) -> StatusCode {
    if request.action == "api" {
        return if eq(&request.password, &policy.admin) {
            StatusCode::NO_CONTENT
        } else {
            StatusCode::FORBIDDEN
        };
    }
    if !matches!(request.action.as_str(), "publish" | "read") || Key::parse(&request.path).is_err()
    {
        return StatusCode::FORBIDDEN;
    }
    let expected = match request.protocol.as_str() {
        "rtmp" | "rtmps"
            if policy.prefix.is_empty()
                || request
                    .path
                    .strip_prefix(&policy.prefix)
                    .is_some_and(|tail| tail.starts_with('/')) =>
        {
            &policy.rtmp_token
        }
        "srt" => &policy.srt_token,
        _ => return StatusCode::FORBIDDEN,
    };
    let valid = expected.is_empty()
        || eq(expected, &request.password)
        || (request.protocol == "srt"
            && request.user == "core"
            && eq(&request.password, &policy.srt_process_token))
        || url::form_urlencoded::parse(request.query.as_bytes())
            .any(|(key, value)| key == "token" && eq(expected, &value));
    if !valid {
        return StatusCode::FORBIDDEN;
    }
    let Some(id) = request.id.filter(|id| uuid::Uuid::parse_str(id).is_ok()) else {
        return StatusCode::FORBIDDEN;
    };
    let Ok(mut admission) = policy.admission.lock() else {
        return StatusCode::SERVICE_UNAVAILABLE;
    };
    admission.retain(|_, time| time.elapsed() < Duration::from_secs(15));
    if !admission.contains_key(&id) && admission.len() >= policy.limits.total {
        return StatusCode::TOO_MANY_REQUESTS;
    }
    admission.insert(id, Instant::now());
    StatusCode::NO_CONTENT
}

pub fn eq(left: &str, right: &str) -> bool {
    bool::from(left.as_bytes().ct_eq(right.as_bytes()))
}

#[derive(Clone, Default)]
pub struct TransportMetrics {
    pub publishers: usize,
    pub rtmp: usize,
    pub srt: usize,
    pub bandwidth_bytes_per_second: f64,
    pub publisher_names: BTreeSet<(&'static str, String)>,
    pub available: bool,
}

pub struct Transport {
    metrics: Arc<Mutex<TransportMetrics>>,
    endpoint: String,
    client: reqwest::Client,
    rtmp: Option<(SocketAddr, bool)>,
    srt: Option<SocketAddr>,
    rtmp_secure: bool,
    tls_trust: Option<String>,
    policy: Arc<Policy>,
    srt_passphrase: String,
    flv: Option<String>,
    child: tokio::sync::Mutex<Child>,
    _dir: tempfile::TempDir,
    stop: CancellationToken,
}

impl Transport {
    pub async fn open(
        doc: &Value,
        limits: &Limits,
        launcher: Option<&Path>,
    ) -> Result<Option<Self>, Error> {
        let r = &doc["rtmp"];
        let s = &doc["srt"];
        let tls = r["enable_tls"].as_bool().unwrap_or(false);
        let address = |value: &Value| {
            crate::config::address(value.as_str().ok_or(Error::Invalid)?)
                .map_err(|_| Error::Invalid)
                .and_then(|address| {
                    if address.port() == 0 {
                        Err(Error::Invalid)
                    } else {
                        Ok(address)
                    }
                })
        };
        let plain = r["enable"].as_bool() == Some(true) && (!tls || has_tls_files(doc));
        let secure = tls && has_tls_files(doc);
        if tls && !secure {
            tracing::warn!("RTMPS disabled because its certificate or private key is missing");
        }
        let plain = plain.then(|| address(&r["address"])).transpose()?;
        let secure = secure.then(|| address(&r["address_tls"])).transpose()?;
        let rtmp = plain
            .map(|address| (address, false))
            .or_else(|| secure.map(|address| (address, true)));
        let srt = if s["enable"].as_bool() == Some(true) {
            Some(address(&s["address"])?)
        } else {
            None
        };
        if rtmp.is_none() && srt.is_none() {
            return Ok(None);
        }
        let prefix = r["app"]
            .as_str()
            .unwrap_or("/")
            .trim_matches('/')
            .to_owned();
        if !prefix.is_empty() {
            Key::parse(&prefix)?;
        }
        let policy = Arc::new(Policy {
            admin: uuid::Uuid::new_v4().simple().to_string(),
            admission: Default::default(),
            limits: limits.clone(),
            prefix,
            rtmp_token: r["token"].as_str().unwrap_or_default().into(),
            srt_token: s["token"].as_str().unwrap_or_default().into(),
            srt_process_token: URL_SAFE_NO_PAD.encode(s["token"].as_str().unwrap_or_default()),
        });
        let srt_passphrase = s["passphrase"].as_str().unwrap_or_default().to_owned();
        if !srt_passphrase.is_empty() && !(10..=79).contains(&srt_passphrase.len()) {
            return Err(Error::Invalid);
        }
        let flv = if rtmp.is_some() && r["httpflv"]["enable"].as_bool() == Some(true) {
            let mount = r["httpflv"]["mount"]
                .as_str()
                .ok_or(Error::Invalid)?
                .to_owned();
            super::valid_mount(&mount)?;
            Some(mount)
        } else {
            None
        };
        let callback = TcpListener::bind("127.0.0.1:0").await?;
        let reserved = TcpListener::bind("127.0.0.1:0").await?;
        let api = reserved.local_addr()?;
        let secret = uuid::Uuid::new_v4().simple().to_string();
        let endpoint = format!("http://{api}");
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(3))
            .build()
            .map_err(|_| Error::Unavailable)?;
        let working_directory = std::env::current_dir()?;
        let mut config = json!({
            "logLevel":"error","logDestinations":["stdout"],"api":true,"apiAddress":api.to_string(),
            "authMethod":"http","authHTTPAddress":format!("http://{}/{secret}",callback.local_addr()?),
            "authHTTPExclude":[],
            "rtsp":false,"hls":false,"webrtc":false,"playback":false,"metrics":false,"pprof":false,
            "rtmp":rtmp.is_some(),"srt":srt.is_some(),"readTimeout":"10s","writeTimeout":"5s",
            "writeQueueSize":128,
            "pathDefaults":{"maxReaders":limits.per_stream.min(limits.total),
                "overridePublisher":false,"srtReadPassphrase":srt_passphrase,"srtPublishPassphrase":srt_passphrase},
            "paths":{"all_others":{}},
        });
        if let Some(address) = plain {
            config["rtmpAddress"] = json!(address.to_string());
        }
        if let Some(address) = secure {
            config["rtmpsAddress"] = json!(address.to_string());
            config["rtmpEncryption"] = json!(if plain.is_some() {
                "optional"
            } else {
                "strict"
            });
            for (field, option) in [
                ("cert_file", "rtmpServerCert"),
                ("key_file", "rtmpServerKey"),
            ] {
                let file = doc["tls"][field].as_str().ok_or(Error::Invalid)?;
                config[option] = json!(working_directory.join(file));
            }
        }
        if let Some(address) = srt {
            config["srtAddress"] = json!(address.to_string());
        }
        let dir = tempfile::Builder::new()
            .prefix("restreamer-transport-")
            .tempdir()?;
        let path = dir.path().join("mediamtx.json");
        tokio::fs::write(&path, config.to_string()).await?;
        let program = Path::new(
            doc["rtmp"]["transport_binary"]
                .as_str()
                .unwrap_or("mediamtx"),
        );
        let program = if program
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty())
        {
            working_directory.join(program)
        } else {
            program.to_owned()
        };
        let launcher = launcher.map(|path| working_directory.join(path));
        let mut command = tokio::process::Command::new(launcher.as_deref().unwrap_or(&program));
        if launcher.is_some() {
            command
                .arg("__media_child")
                .arg(std::process::id().to_string())
                .arg(program);
        }
        command
            .arg(path)
            .current_dir(dir.path())
            .env_clear()
            .env("PATH", "/usr/local/bin:/usr/bin:/bin")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .process_group(0);
        let stop = CancellationToken::new();
        let shutdown = stop.clone().drop_guard();
        let callback_app = Router::new()
            .route(&format!("/{secret}"), post(auth))
            .with_state(policy.clone());
        let cancel = stop.clone();
        tokio::spawn(async move {
            let _ = axum::serve(callback, callback_app)
                .with_graceful_shutdown(cancel.cancelled_owned())
                .await;
        });
        drop(reserved);
        let mut child = command.spawn().map_err(|_| Error::Unavailable)?;
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut started = false;
        while Instant::now() < deadline {
            if let Some(status) = child.try_wait()? {
                tracing::warn!(
                    code = status.code(),
                    "media companion exited during startup"
                );
                break;
            }
            if client
                .get(format!("{endpoint}/v3/config/global/get"))
                .basic_auth("core", Some(&policy.admin))
                .send()
                .await
                .is_ok_and(|r| r.status().is_success())
            {
                started = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
        if !started {
            return Err(Error::Unavailable);
        }
        let metrics = Arc::new(Mutex::new(TransportMetrics::default()));
        let supervisor = Monitor {
            metrics: metrics.clone(),
            client: client.clone(),
            endpoint: endpoint.clone(),
            policy: policy.clone(),
            protocols: [
                plain.map(|_| "rtmp"),
                secure.map(|_| "rtmps"),
                srt.map(|_| "srt"),
            ],
            stop: stop.clone(),
        };
        tokio::spawn(supervisor.run());
        shutdown.disarm();
        Ok(Some(Self {
            metrics,
            endpoint,
            client,
            rtmp,
            rtmp_secure: secure.is_some(),
            tls_trust: doc["tls"]["trust_file"]
                .as_str()
                .filter(|path| !path.is_empty())
                .map(str::to_owned),
            srt,
            policy,
            srt_passphrase,
            flv,
            child: tokio::sync::Mutex::new(child),
            _dir: dir,
            stop,
        }))
    }

    pub fn metrics(&self) -> TransportMetrics {
        self.metrics
            .lock()
            .map(|stats| stats.clone())
            .unwrap_or_default()
    }

    pub fn enabled(&self, protocol: &str) -> bool {
        if protocol == "rtmp" {
            self.rtmp.is_some()
        } else {
            self.srt.is_some()
        }
    }

    pub fn flv_mount(&self) -> Option<&str> {
        self.flv.as_deref()
    }

    pub fn allow_rtmp(&self, token: Option<&str>) -> bool {
        self.policy.rtmp_token.is_empty()
            || token.is_some_and(|token| eq(token, &self.policy.rtmp_token))
    }

    pub fn rtmp_url(&self, path: &str) -> Result<String, Error> {
        let (address, tls) = self.rtmp.ok_or(Error::Unavailable)?;
        Key::parse(path)?;
        let mut url = url::Url::parse(&format!(
            "{}://{}",
            if tls { "rtmps" } else { "rtmp" },
            local(address)
        ))
        .map_err(|_| Error::Invalid)?;
        url.set_path(&format!("/{}", path.trim_start_matches('/')));
        if !self.policy.rtmp_token.is_empty() {
            url.query_pairs_mut()
                .append_pair("token", &self.policy.rtmp_token);
        }
        if tls {
            url.query_pairs_mut().append_pair("tls_verify", "1");
        }
        Ok(url.into())
    }

    pub fn rtmp_tls_trust(&self) -> Option<&str> {
        self.rtmp
            .is_some_and(|(_, tls)| tls)
            .then_some(self.tls_trust.as_deref())
            .flatten()
    }

    pub fn base_rtmp(&self) -> Option<String> {
        let (address, tls) = self.rtmp?;
        Some(format!(
            "{}://{}/{}",
            if tls { "rtmps" } else { "rtmp" },
            local(address),
            self.policy.prefix
        ))
    }

    pub fn rtmp_token(&self) -> &str {
        &self.policy.rtmp_token
    }

    pub fn srt_url(&self, path: &str, publish: bool) -> Result<String, Error> {
        let address = self.srt.ok_or(Error::Unavailable)?;
        Key::parse(path)?;
        if path.contains([':', ',', '&', '=']) {
            return Err(Error::Invalid);
        }
        let mut url =
            url::Url::parse(&format!("srt://{}", local(address))).map_err(|_| Error::Invalid)?;
        let action = if publish { "publish" } else { "read" };
        // FFmpeg 6 forwards the Stream ID verbatim. Keep native separators
        // literal so FFmpeg releases with and without URL decoding agree.
        let path = path.trim_start_matches('/');
        let token = &self.policy.srt_process_token;
        url.set_query(Some(&format!(
            "mode=caller&streamid={action}:{path}:core:{token}"
        )));
        if !self.srt_passphrase.is_empty() {
            url.query_pairs_mut()
                .append_pair("passphrase", &self.srt_passphrase)
                .append_pair("pbkeylen", "16");
        }
        Ok(url.into())
    }

    fn wire_protocols<'a>(&self, protocol: &'a str) -> [Option<&'a str>; 2] {
        if protocol != "rtmp" {
            return [Some(protocol), None];
        }
        [
            self.rtmp.is_some_and(|(_, tls)| !tls).then_some("rtmp"),
            self.rtmp_secure.then_some("rtmps"),
        ]
    }

    pub async fn connections(&self, protocol: &str) -> Result<Vec<Connection>, Error> {
        if !self.enabled(protocol) {
            return Err(Error::Missing);
        }
        let mut all = Vec::new();
        for wire in self.wire_protocols(protocol).into_iter().flatten() {
            all.extend(connections(&self.client, &self.endpoint, &self.policy.admin, wire).await?);
        }
        Ok(all)
    }

    pub async fn disconnect(&self, protocol: &str, path: &str) -> Result<(), Error> {
        if !self.enabled(protocol) {
            return Err(Error::Missing);
        }
        for protocol in self.wire_protocols(protocol).into_iter().flatten() {
            for connection in
                connections(&self.client, &self.endpoint, &self.policy.admin, protocol).await?
            {
                if path != "*" && path.trim_start_matches('/') != connection.path {
                    continue;
                }
                let response = self
                    .client
                    .post(format!(
                        "{}/v3/{protocol}conns/kick/{}",
                        self.endpoint, connection.id
                    ))
                    .basic_auth("core", Some(&self.policy.admin))
                    .send()
                    .await
                    .map_err(|_| Error::Unavailable)?;
                if !response.status().is_success() && response.status() != StatusCode::NOT_FOUND {
                    return Err(Error::Unavailable);
                }
            }
        }
        Ok(())
    }

    pub async fn close(&self) {
        self.stop.cancel();
        let mut child = self.child.lock().await;
        if let Some(id) = child.id().and_then(|id| i32::try_from(id).ok()) {
            let _ = nix::sys::signal::killpg(
                nix::unistd::Pid::from_raw(id),
                nix::sys::signal::Signal::SIGTERM,
            );
        }
        if tokio::time::timeout(Duration::from_secs(3), child.wait())
            .await
            .is_err()
        {
            let _ = child.kill().await;
        }
    }
}

impl Drop for Transport {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

#[derive(Clone)]
pub struct Connection {
    pub id: String,
    pub path: String,
    pub publisher: bool,
    pub created: u64,
    pub rx: u64,
    pub tx: u64,
    pub remote: String,
    pub stats: Value,
}

impl Connection {
    fn parse(value: &Value) -> Option<Self> {
        let id = value["id"].as_str()?;
        uuid::Uuid::parse_str(id).ok()?;
        let path = value["path"].as_str()?.trim_start_matches('/');
        if path.is_empty() {
            return None;
        }
        let publisher = match value["state"].as_str()? {
            "publish" => true,
            "read" => false,
            _ => return None,
        };
        Some(Self {
            id: id.into(),
            path: path.into(),
            publisher,
            created: value["created"]
                .as_str()
                .and_then(|date| date.parse::<jiff::Timestamp>().ok())
                .map_or_else(
                    || now_secs(std::time::SystemTime::now()),
                    |date| date.as_second().max(0) as u64,
                ),
            rx: value["bytesReceived"].as_u64().unwrap_or(0),
            tx: value["bytesSent"].as_u64().unwrap_or(0),
            remote: value["remoteAddr"].as_str().unwrap_or_default().into(),
            stats: srt_stats(value),
        })
    }

    pub fn wire(&self) -> Value {
        json!({"remote":self.remote,"created_at":self.created,"rx_bytes":self.rx,"tx_bytes":self.tx})
    }
}

/// Map only counters the companion actually reports. Credentials and connection
/// query strings are discarded before the inventory reaches the control API.
fn srt_stats(connection: &Value) -> Value {
    json!(
        [
            ("sent_pkt", "packetsSent"),
            ("recv_pkt", "packetsReceived"),
            ("sent_unique_pkt", "packetsSentUnique"),
            ("recv_unique_pkt", "packetsReceivedUnique"),
            ("send_loss_pkt", "packetsSendLoss"),
            ("recv_loss_pkt", "packetsReceivedLoss"),
            ("sent_retrans_pkt", "packetsRetrans"),
            ("recv_retran_pkts", "packetsReceivedRetrans"),
            ("sent_ack_pkt", "packetsSentACK"),
            ("recv_ack_pkt", "packetsReceivedACK"),
            ("sent_nak_pkt", "packetsSentNAK"),
            ("recv_nak_pkt", "packetsReceivedNAK"),
            ("send_km_pkt", "packetsSentKM"),
            ("recv_km_pkt", "packetsReceivedKM"),
            ("send_duration_us", "usSndDuration"),
            ("send_drop_pkt", "packetsSendDrop"),
            ("recv_drop_pkt", "packetsReceivedDrop"),
            ("recv_undecrypt_pkt", "packetsReceivedUndecrypt"),
            ("sent_bytes", "bytesSent"),
            ("recv_bytes", "bytesReceived"),
            ("sent_unique_bytes", "bytesSentUnique"),
            ("recv_unique_bytes", "bytesReceivedUnique"),
            ("recv_loss_bytes", "bytesReceivedLoss"),
            ("sent_retrans_bytes", "bytesRetrans"),
            ("send_drop_bytes", "bytesSendDrop"),
            ("recv_drop_bytes", "bytesReceivedDrop"),
            ("recv_undecrypt_bytes", "bytesReceivedUndecrypt"),
            ("pkt_send_period_us", "usPacketsSendPeriod"),
            ("flow_window_pkt", "packetsFlowWindow"),
            ("flight_size_pkt", "packetsFlightSize"),
            ("rtt_ms", "msRTT"),
            ("bandwidth_mbit", "mbpsLinkCapacity"),
            ("avail_send_buf_bytes", "bytesAvailSendBuf"),
            ("avail_recv_buf_bytes", "bytesAvailReceiveBuf"),
            ("max_bandwidth_mbit", "mbpsMaxBW"),
            ("mss_bytes", "byteMSS"),
            ("send_buf_pkt", "packetsSendBuf"),
            ("send_buf_bytes", "bytesSendBuf"),
            ("send_buf_ms", "msSendBuf"),
            ("send_tsbpd_delay_ms", "msSendTsbPdDelay"),
            ("recv_buf_pkt", "packetsReceiveBuf"),
            ("recv_buf_bytes", "bytesReceiveBuf"),
            ("recv_buf_ms", "msReceiveBuf"),
            ("recv_tsbpd_delay_ms", "msReceiveTsbPdDelay"),
            ("reorder_tolerance_pkt", "packetsReorderTolerance"),
            (
                "pkt_recv_avg_belated_time_ms",
                "packetsReceivedAvgBelatedTime"
            ),
        ]
        .into_iter()
        .filter_map(|(legacy, provider)| connection
            .get(provider)
            .filter(|value| value.is_number())
            .map(|value| (legacy.to_owned(), value.clone())))
        .collect::<serde_json::Map<String, Value>>()
    )
}

async fn connections(
    client: &reqwest::Client,
    endpoint: &str,
    password: &str,
    protocol: &str,
) -> Result<Vec<Connection>, Error> {
    let mut result = Vec::new();
    let mut page = 0;
    while page < 32 {
        let response = client
            .get(format!(
                "{}/v3/{protocol}conns/list?page={page}&itemsPerPage=100",
                endpoint
            ))
            .basic_auth("core", Some(password))
            .send()
            .await
            .map_err(|_| Error::Unavailable)?
            .error_for_status()
            .map_err(|_| Error::Unavailable)?;
        let data: Value = response.json().await.map_err(|_| Error::Unavailable)?;
        result.extend(
            data["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Connection::parse),
        );
        page += 1;
        if page >= data["pageCount"].as_u64().unwrap_or(0) {
            return Ok(result);
        }
    }
    Err(Error::Full)
}

struct Monitor {
    metrics: Arc<Mutex<TransportMetrics>>,
    client: reqwest::Client,
    endpoint: String,
    policy: Arc<Policy>,
    protocols: [Option<&'static str>; 3],
    stop: CancellationToken,
}

impl Monitor {
    async fn run(self) {
        let mut ticker = tokio::time::interval(Duration::from_secs(1));
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut previous = HashMap::<String, (u64, u64)>::new();
        let mut last = Instant::now();
        loop {
            tokio::select! {_=self.stop.cancelled()=>return,_=ticker.tick()=>{}}
            let mut current = Vec::new();
            let mut available = true;
            for protocol in self.protocols.into_iter().flatten() {
                if let Ok(list) =
                    connections(&self.client, &self.endpoint, &self.policy.admin, protocol).await
                {
                    current.extend(list.into_iter().map(|conn| (protocol, conn)));
                } else {
                    available = false;
                }
            }
            if let Ok(mut admission) = self.policy.admission.lock() {
                for (_, connection) in &current {
                    admission.insert(connection.id.clone(), Instant::now());
                }
            }
            let seconds = last.elapsed().as_secs_f64().max(0.1);
            last = Instant::now();
            let mut rates = HashMap::<_, f64>::new();
            let mut usage = Vec::new();
            let mut total = 0.0;
            let mut next = HashMap::new();
            for (protocol, connection) in &current {
                next.insert(connection.id.clone(), (connection.rx, connection.tx));
                let Some((rx, tx)) = previous.get(&connection.id) else {
                    continue;
                };
                let rate = (connection.rx.saturating_sub(*rx) + connection.tx.saturating_sub(*tx))
                    as f64
                    / seconds;
                total += rate;
                *rates
                    .entry((*protocol, connection.path.as_str()))
                    .or_default() += rate;
                usage.push((*protocol, connection, rate));
            }
            if let Ok(mut stats) = self.metrics.lock() {
                *stats = TransportMetrics {
                    publishers: current.iter().filter(|(_, c)| c.publisher).count(),
                    rtmp: current
                        .iter()
                        .filter(|(protocol, _)| *protocol != "srt")
                        .count(),
                    srt: current
                        .iter()
                        .filter(|(protocol, _)| *protocol == "srt")
                        .count(),
                    bandwidth_bytes_per_second: total,
                    publisher_names: current
                        .iter()
                        .filter(|(_, c)| c.publisher)
                        .map(|(protocol, c)| {
                            (
                                if *protocol == "srt" { "srt" } else { "rtmp" },
                                format!("/{}", c.path),
                            )
                        })
                        .collect(),
                    available,
                };
            }
            previous = next;
            usage.sort_by(|a, b| b.2.total_cmp(&a.2));
            let exceeded = |value: f64, limit: u64| limit > 0 && value > limit as f64;
            for (protocol, connection, rate) in usage.into_iter().take(128) {
                let stream = rates
                    .entry((protocol, connection.path.as_str()))
                    .or_default();
                if exceeded(rate, self.policy.limits.session_rate)
                    || exceeded(*stream, self.policy.limits.stream_rate)
                    || exceeded(total, self.policy.limits.total_rate)
                {
                    let request = self
                        .client
                        .post(format!(
                            "{}/v3/{protocol}conns/kick/{}",
                            self.endpoint, connection.id
                        ))
                        .basic_auth("core", Some(&self.policy.admin));
                    tokio::select! {_=self.stop.cancelled()=>return,_=request.send()=>{}}
                    total -= rate;
                    *stream -= rate;
                }
            }
        }
    }
}

fn has_tls_files(doc: &Value) -> bool {
    ["cert_file", "key_file"].iter().all(|key| {
        doc["tls"][key]
            .as_str()
            .is_some_and(|value| !value.is_empty())
    })
}

fn local(address: SocketAddr) -> SocketAddr {
    if !address.ip().is_unspecified() {
        return address;
    }
    SocketAddr::new(
        if address.is_ipv4() {
            IpAddr::V4(std::net::Ipv4Addr::LOCALHOST)
        } else {
            IpAddr::V6(std::net::Ipv6Addr::LOCALHOST)
        },
        address.port(),
    )
}
