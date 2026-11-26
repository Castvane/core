pub mod logs;
pub mod resources;

use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use serde::Serialize;
use tokio::sync::{Semaphore, broadcast};
use tokio_util::sync::CancellationToken;

use crate::{engine::Engine, media::Media};

#[derive(Clone, Serialize)]
pub struct Event {
    pub ts: i64,
    pub category: &'static str,
    pub action: &'static str,
    pub name: String,
    pub domain: String,
    pub message: Option<String>,
    pub level: &'static str,
}

#[derive(Serialize)]
pub struct Metrics {
    pub ts: i64,
    pub values: BTreeMap<&'static str, f64>,
}

pub const METRICS: &[(&str, &str)] = &[
    (
        "cpu_ncpu",
        "Effective logical CPUs available to the process",
    ),
    ("cpu_idle", "Percentage of idle host CPU"),
    (
        "cpu_limit",
        "Effective CPU capacity as a percentage of one core",
    ),
    ("mem_total", "Total host memory in bytes"),
    ("mem_free", "Host memory available to start applications"),
    ("mem_limit", "Cgroup memory limit in bytes"),
    ("cgroup_memory_bytes", "Cgroup memory currently in use"),
    ("disk_total", "Capacity in bytes of the media filesystem"),
    ("disk_usage", "Used bytes on the media filesystem"),
    (
        "gpu_ngpu",
        "GPU and hardware codec devices visible to the node",
    ),
    (
        "linux_process_count",
        "Processes visible in the PID namespace",
    ),
    ("net_rx", "Received network bytes in the current namespace"),
    ("net_tx", "Sent network bytes in the current namespace"),
    ("uptime_uptime", "Application uptime in seconds"),
    ("linux_uptime_seconds", "System uptime in seconds"),
    ("process_running", "FFmpeg processes recorded as running"),
    (
        "process_cpu_percent",
        "Sum of sampled FFmpeg CPU percentages",
    ),
    (
        "process_memory_bytes",
        "Sum of sampled FFmpeg resident memory",
    ),
    (
        "process_failed_total",
        "Process executions that failed since startup",
    ),
    ("process_restarts", "Current jobs’ summed restart counts"),
    ("cache_used_bytes", "Resident media cache bytes"),
    (
        "storage_memory_used_bytes",
        "Resident bytes in the in-memory media filesystem",
    ),
    (
        "storage_memory_capacity_bytes",
        "Configured in-memory media capacity",
    ),
    (
        "storage_http_failures_total",
        "Failed filesystem HTTP operations",
    ),
    (
        "storage_http_duration_seconds_sum",
        "Cumulative filesystem HTTP handler duration",
    ),
    (
        "media_http_viewers",
        "Active HTTP and HTTP-FLV viewer sessions",
    ),
    (
        "media_http_bytes_total",
        "HTTP media bytes published to response bodies",
    ),
    ("media_publishers", "Active native stream publishers"),
    (
        "media_rtmp_connections",
        "Active RTMP and RTMPS connections",
    ),
    ("media_srt_connections", "Active SRT connections"),
    (
        "media_native_bytes_per_second",
        "Native transport sampled bandwidth",
    ),
];

pub struct Operations {
    since: Instant,
    pub stop: CancellationToken,
    pub streams: Arc<Semaphore>,
    pub bus: broadcast::Sender<Event>,
    records: Mutex<VecDeque<Event>>,
    log_lines: usize,
    measurements: Mutex<VecDeque<Metrics>>,
    host: Mutex<Arc<resources::Snapshot>>,
    pub interval: Duration,
    pub range: u64,
    counts: [AtomicU64; 500],
    request_nanos: AtomicU64,
    in_flight: AtomicU64,
    connections: AtomicU64,
    process_failures: AtomicU64,
    publishers: Mutex<std::collections::BTreeSet<(&'static str, String)>>,
    storage_failures: AtomicU64,
    storage_nanos: AtomicU64,
    ffmpeg_ready: AtomicBool,
    storage_ready: AtomicBool,
    memory_pressure: AtomicBool,
}

pub struct RequestGuard<'a> {
    ops: &'a Operations,
    start: Instant,
    storage: bool,
}

impl RequestGuard<'_> {
    pub fn status(&self, status: u16) {
        if self.storage && status >= 400 {
            self.ops.storage_failures.fetch_add(1, Ordering::Relaxed);
        }
        if let Some(count) = status
            .checked_sub(100)
            .and_then(|code| self.ops.counts.get(code as usize))
        {
            count.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        if self.storage {
            self.ops.storage_nanos.fetch_add(
                self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
        }
        self.ops.in_flight.fetch_sub(1, Ordering::Relaxed);
        self.ops.request_nanos.fetch_add(
            self.start.elapsed().as_nanos().min(u64::MAX as u128) as u64,
            Ordering::Relaxed,
        );
    }
}

pub(crate) struct ConnectionGuard<'a>(&'a AtomicU64);

impl Drop for ConnectionGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Operations {
    pub fn new(doc: &serde_json::Value, initial: resources::Snapshot) -> Arc<Self> {
        let interval = doc["metrics"]["interval_sec"]
            .as_u64()
            .unwrap_or(2)
            .clamp(1, 60);
        let range = doc["metrics"]["range_sec"]
            .as_u64()
            .unwrap_or(300)
            .clamp(interval, interval * 512);
        Arc::new(Self {
            since: Instant::now(),
            stop: CancellationToken::new(),
            streams: Arc::new(Semaphore::new(32)),
            bus: broadcast::channel(256).0,
            records: Mutex::new(VecDeque::new()),
            log_lines: doc["log"]["max_lines"].as_u64().unwrap_or(1000).min(10_000) as usize,
            measurements: Mutex::new(VecDeque::new()),
            host: Mutex::new(Arc::new(initial)),
            range,
            interval: Duration::from_secs(interval),
            counts: std::array::from_fn(|_| AtomicU64::new(0)),
            request_nanos: AtomicU64::new(0),
            in_flight: AtomicU64::new(0),
            connections: AtomicU64::new(0),
            process_failures: AtomicU64::new(0),
            publishers: Mutex::default(),
            storage_failures: AtomicU64::new(0),
            storage_nanos: AtomicU64::new(0),
            ffmpeg_ready: AtomicBool::new(false),
            storage_ready: AtomicBool::new(false),
            memory_pressure: AtomicBool::new(false),
        })
    }

    pub fn connection(&self) -> ConnectionGuard<'_> {
        self.connections.fetch_add(1, Ordering::Relaxed);
        ConnectionGuard(&self.connections)
    }

    pub fn request(&self, storage: bool) -> RequestGuard<'_> {
        self.in_flight.fetch_add(1, Ordering::Relaxed);
        RequestGuard {
            ops: self,
            start: Instant::now(),
            storage,
        }
    }

    pub fn emit(&self, category: &'static str, action: &'static str, name: &str, domain: &str) {
        let event = Event {
            ts: time::OffsetDateTime::now_utc()
                .unix_timestamp_nanos()
                .div_euclid(1_000_000) as i64,
            category,
            action,
            name: crate::engine::sanitize(name),
            domain: crate::engine::sanitize(domain),
            message: None,
            level: "info",
        };
        self.publish(event);
    }

    pub fn publish(&self, event: Event) {
        if self.log_lines != 0
            && let Ok(mut records) = self.records.lock()
        {
            if records.len() == self.log_lines {
                records.pop_front();
            }
            records.push_back(event.clone());
        }
        let _ = self.bus.send(event);
    }

    pub fn records(&self) -> Vec<Event> {
        self.records
            .lock()
            .map(|records| records.iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn host(&self) -> Arc<resources::Snapshot> {
        self.host
            .lock()
            .map(|host| host.clone())
            .unwrap_or_default()
    }

    pub fn components(&self) -> (bool, bool) {
        (
            self.ffmpeg_ready.load(Ordering::Relaxed),
            self.storage_ready.load(Ordering::Relaxed),
        )
    }

    async fn check_components(&self, engine: &Engine, media: &Media) {
        self.ffmpeg_ready
            .store(engine.executable_ready().await, Ordering::Relaxed);
        let mut ready = true;
        for mount in media.mounts.values().filter(|mount| mount.required) {
            ready &= tokio::time::timeout(Duration::from_secs(2), mount.backend.health())
                .await
                .is_ok_and(|result| result.is_ok());
        }
        self.storage_ready.store(ready, Ordering::Relaxed);
    }

    pub fn start(self: &Arc<Self>, engine: &Arc<Engine>, media: &Arc<Media>, disk: PathBuf) {
        let ops = Arc::downgrade(self);
        let mut process = engine.subscribe();
        let engine = Arc::downgrade(engine);
        let media = Arc::downgrade(media);
        let stop = self.stop.clone();
        let mut interval = tokio::time::interval(self.interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let sampler = Arc::new(Mutex::new(resources::Sampler::default()));
        {
            let ops = ops.clone();
            let media = media.clone();
            let engine = engine.clone();
            let stop = stop.clone();
            tokio::spawn(async move {
                let mut period = tokio::time::interval(Duration::from_secs(15));
                period.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! { _ = stop.cancelled()=>break, _ = period.tick()=>{} }
                    let (Some(ops), Some(media), Some(engine)) =
                        (ops.upgrade(), media.upgrade(), engine.upgrade())
                    else {
                        break;
                    };
                    tokio::select! { _ = stop.cancelled()=>break, () = ops.check_components(&engine, &media)=>{} }
                }
            });
        }
        tokio::spawn(async move {
            let mut next_nvidia = Instant::now();
            let mut nvidia = Vec::new();
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    event = process.recv() => match event {
                        Ok(event) => {
                            let Some(ops) = ops.upgrade() else { break; };
                            if matches!(event.execution, crate::engine::Execution::Failed | crate::engine::Execution::Killed) {
                                ops.process_failures.fetch_add(1, Ordering::Relaxed);
                            }
                            ops.emit("process", event.execution.wire(), &event.process, &event.domain);
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                        Err(broadcast::error::RecvError::Lagged(_)) => {},
                    },
                    _ = interval.tick() => {
                        let (Some(ops), Some(engine), Some(media)) = (ops.upgrade(), engine.upgrade(), media.upgrade()) else { break };
                        let disk = disk.clone();
                        let sampler = sampler.clone();
                        if let Ok(Some(mut snapshot)) = tokio::task::spawn_blocking(move || {
                            let gpus = resources::discover_gpus(Path::new("/sys/class/drm"), Path::new("/sys/class/video4linux"));
                            Some(sampler.lock().ok()?.sample(&disk, gpus))
                        }).await {
                            if Instant::now() >= next_nvidia {
                                nvidia = resources::nvidia().await;
                                next_nvidia = Instant::now() + Duration::from_secs(15);
                            }
                            if !nvidia.is_empty() {
                                snapshot.gpus.retain(|gpu| gpu.vendor != "nvidia");
                                snapshot.gpus.extend(nvidia.iter().cloned());
                            }
                            ops.sample(snapshot, &engine, &media).await;
                        }
                    },
                }
            }
        });
    }

    async fn sample(&self, host: resources::Snapshot, engine: &Engine, media: &Media) {
        let memory_pressure = host
            .cgroup_memory_used
            .zip(host.cgroup_memory_limit)
            .is_some_and(|(usage, limit)| limit > 0 && usage as f64 / limit as f64 >= 0.90);
        if self
            .memory_pressure
            .swap(memory_pressure, Ordering::Relaxed)
            != memory_pressure
        {
            self.emit(
                "resources",
                if memory_pressure {
                    "memory_warning"
                } else {
                    "memory_recovered"
                },
                "",
                "",
            );
        }
        let mut values = BTreeMap::new();
        let mut add = |name, value: Option<f64>| {
            if let Some(value) = value.filter(|value| value.is_finite()) {
                values.insert(name, value);
            }
        };
        add("cpu_ncpu", Some(host.logical_cpus as f64));
        add("cpu_idle", host.host_cpu_percent.map(|usage| 100.0 - usage));
        add("cpu_limit", host.effective_cpus.map(|limit| limit * 100.0));
        add("mem_total", host.total_memory.map(|bytes| bytes as f64));
        add("mem_free", host.available_memory.map(|bytes| bytes as f64));
        add(
            "mem_limit",
            host.cgroup_memory_limit.map(|bytes| bytes as f64),
        );
        add(
            "cgroup_memory_bytes",
            host.cgroup_memory_used.map(|bytes| bytes as f64),
        );
        add("disk_total", host.disk_bytes.map(|bytes| bytes as f64));
        add(
            "disk_usage",
            host.disk_bytes
                .zip(host.disk_available)
                .map(|(total, free)| total.saturating_sub(free) as f64),
        );
        add("gpu_ngpu", Some(host.gpus.len() as f64));
        add(
            "linux_process_count",
            host.process_count.map(|count| count as f64),
        );
        add("linux_uptime_seconds", host.host_uptime);
        add("net_rx", host.network_received.map(|bytes| bytes as f64));
        add("net_tx", host.network_sent.map(|bytes| bytes as f64));
        add("uptime_uptime", Some(self.since.elapsed().as_secs_f64()));
        add(
            "process_failed_total",
            Some(self.process_failures.load(Ordering::Relaxed) as f64),
        );
        let mut running = 0.0;
        let mut cpu = 0.0;
        let mut memory = 0u64;
        let mut restarts = 0u64;
        for job in engine.list().await {
            let state = job.snapshot();
            restarts = restarts.saturating_add(u64::from(state.restart_count));
            if state.pid.is_some() {
                running += 1.0;
                cpu += state.usage.cpu;
                memory = memory.saturating_add(state.usage.memory);
            }
        }
        add("process_running", Some(running));
        add("process_cpu_percent", Some(cpu));
        add("process_memory_bytes", Some(memory as f64));
        add("process_restarts", Some(restarts as f64));
        add(
            "storage_http_failures_total",
            Some(self.storage_failures.load(Ordering::Relaxed) as f64),
        );
        add(
            "storage_http_duration_seconds_sum",
            Some(self.storage_nanos.load(Ordering::Relaxed) as f64 / 1e9),
        );
        if let Ok(fs) = media.get("mem")
            && let Ok(usage) = fs.backend.usage().await
        {
            add(
                "storage_memory_used_bytes",
                Some(usage.resident_bytes as f64),
            );
            add(
                "storage_memory_capacity_bytes",
                usage.capacity_bytes.map(|bytes| bytes as f64),
            );
        }
        let (viewers, bytes) = media.sessions.metrics();
        add("media_http_viewers", Some(viewers as f64));
        add("media_http_bytes_total", Some(bytes as f64));
        if let Some(transport) = &media.transport {
            let telemetry = transport.metrics();
            if telemetry.available {
                add("media_publishers", Some(telemetry.publishers as f64));
                add("media_rtmp_connections", Some(telemetry.rtmp as f64));
                add("media_srt_connections", Some(telemetry.srt as f64));
                add(
                    "media_native_bytes_per_second",
                    Some(telemetry.bandwidth_bytes_per_second),
                );
                if let Ok(mut previous) = self.publishers.lock() {
                    for (protocol, channel) in telemetry.publisher_names.difference(&previous) {
                        self.emit("media", "add", channel, protocol);
                    }
                    for (protocol, channel) in previous.difference(&telemetry.publisher_names) {
                        self.emit("media", "remove", channel, protocol);
                    }
                    *previous = telemetry.publisher_names;
                }
            }
        }
        if let Some(cache) = &media.cache {
            use crate::media::storage::Storage;
            add(
                "cache_used_bytes",
                cache
                    .data
                    .usage()
                    .await
                    .ok()
                    .map(|data| data.resident_bytes as f64),
            );
        }
        if let Ok(mut snapshot) = self.host.lock() {
            *snapshot = Arc::new(host);
        }
        let ts = time::OffsetDateTime::now_utc().unix_timestamp();
        if let Ok(mut history) = self.measurements.lock() {
            while history
                .front()
                .is_some_and(|value| ts.saturating_sub(value.ts) > self.range as i64)
                || history.len() >= 512
            {
                history.pop_front();
            }
            history.push_back(Metrics { ts, values });
        }
    }

    pub fn measure<R>(&self, request: impl FnOnce(&VecDeque<Metrics>) -> R) -> Option<R> {
        self.measurements
            .lock()
            .ok()
            .map(|measurements| request(&measurements))
    }

    pub fn prometheus(&self) -> String {
        use std::fmt::Write;
        let mut out = String::with_capacity(4096);
        out.push_str("# TYPE castvane_http_requests_total counter\n");
        let mut requests = 0u64;
        for (index, count) in self.counts.iter().enumerate() {
            let count = count.load(Ordering::Relaxed);
            if count != 0 {
                let _ = writeln!(
                    out,
                    "castvane_http_requests_total{{status=\"{}\"}} {count}",
                    index + 100
                );
                requests = requests.saturating_add(count);
            }
        }
        let _ = writeln!(
            out,
            "# TYPE castvane_http_requests_in_flight gauge\ncastvane_http_requests_in_flight {}",
            self.in_flight.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "# TYPE castvane_http_active_connections gauge\ncastvane_http_active_connections {}",
            self.connections.load(Ordering::Relaxed)
        );
        let _ = writeln!(
            out,
            "# TYPE castvane_http_request_duration_seconds summary\ncastvane_http_request_duration_seconds_sum {}\ncastvane_http_request_duration_seconds_count {requests}",
            self.request_nanos.load(Ordering::Relaxed) as f64 / 1e9
        );
        self.measure(|measurements| {
            if let Some(latest) = measurements.back() {
                for (name, desc) in METRICS {
                    if let Some(value) = latest.values.get(name) {
                        let kind = if matches!(*name, "process_failed_total" | "media_http_bytes_total" | "storage_http_failures_total" | "storage_http_duration_seconds_sum") { "counter" } else { "gauge" };
                        let _ = writeln!(out,"# HELP castvane_{name} {desc}\n# TYPE castvane_{name} {kind}\ncastvane_{name} {value}");
                    }
                }
            }
        });
        out
    }
}

impl Drop for Operations {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}
