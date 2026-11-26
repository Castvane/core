use std::{fs, path::Path, time::Instant};

#[cfg(target_os = "linux")]
use std::path::{Component, PathBuf};

use serde::Serialize;

#[derive(Debug, Default, Serialize)]
pub struct Snapshot {
    pub logical_cpus: usize,
    pub physical_cores: Option<usize>,
    pub effective_cpus: Option<f64>,
    pub host_cpu_percent: Option<f64>,
    pub cgroup_cpu_cores: Option<f64>,
    pub total_memory: Option<u64>,
    pub available_memory: Option<u64>,
    pub cgroup_memory_limit: Option<u64>,
    pub cgroup_memory_used: Option<u64>,
    pub network_received: Option<u64>,
    pub network_sent: Option<u64>,
    pub network_received_per_second: Option<f64>,
    pub network_sent_per_second: Option<f64>,
    pub process_count: Option<usize>,
    pub host_uptime: Option<f64>,
    pub disk_bytes: Option<u64>,
    pub disk_available: Option<u64>,
    pub gpus: Vec<Gpu>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Gpu {
    pub vendor: &'static str,
    pub model: String,
    pub identifier: String,
    pub device: Option<String>,
    pub memory_bytes: Option<u64>,
    pub memory_used_bytes: Option<u64>,
    pub utilization: Option<f64>,
    pub encoder: Option<f64>,
    pub decoder: Option<f64>,
}

#[cfg(target_os = "linux")]
#[derive(Default)]
pub struct Sampler {
    prior: Option<Counters>,
}

#[cfg(target_os = "linux")]
struct Counters {
    at: Instant,
    cpu: Option<(u64, u64)>,
    group_cpu: Option<u64>,
    network: Option<(u64, u64)>,
}

fn raw(path: impl AsRef<Path>) -> Option<String> {
    fs::read_to_string(path).ok()
}

fn number(path: impl AsRef<Path>) -> Option<u64> {
    raw(path)?.trim().parse().ok()
}

#[cfg(target_os = "linux")]
pub fn cgroup_path(root: &Path, membership: &str) -> Option<PathBuf> {
    let relative = membership
        .lines()
        .find_map(|line| line.strip_prefix("0::/"))?;
    let relative = Path::new(relative);
    if relative
        .components()
        .all(|part| matches!(part, Component::Normal(_)))
    {
        Some(root.join(relative))
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
fn ancestors_limit<T: Copy + PartialOrd>(
    root: &Path,
    group: &Path,
    sample: impl Fn(&Path) -> Option<T>,
) -> Option<T> {
    group
        .ancestors()
        .take_while(|path| path.starts_with(root))
        .filter_map(sample)
        .reduce(|min, value| if value < min { value } else { min })
}

#[cfg(target_os = "linux")]
fn cpu_quota(group: &Path) -> Option<f64> {
    let text = raw(group.join("cpu.max"))?;
    let mut fields = text.split_whitespace().map(str::parse::<u64>);
    let quota = fields.next()?.ok()?;
    let period = fields.next()?.ok()?;
    (quota > 0 && period > 0).then_some(quota as f64 / period as f64)
}

#[cfg(target_os = "linux")]
fn counter(text: &str, field: &str) -> Option<u64> {
    text.lines()
        .filter_map(|line| line.split_once(char::is_whitespace))
        .find_map(|(key, value)| (key == field).then_some(value.trim()))
        .and_then(|value| value.parse().ok())
}

#[cfg(target_os = "linux")]
fn host_cpu() -> Option<(u64, u64)> {
    let text = raw("/proc/stat")?;
    let line = text.lines().next()?.strip_prefix("cpu ")?;
    let values: Vec<u64> = line
        .split_whitespace()
        .take(8)
        .map(str::parse)
        .collect::<Result<_, _>>()
        .ok()?;
    let idle = values
        .get(3)?
        .saturating_add(values.get(4).copied().unwrap_or_default());
    Some((values.iter().copied().fold(0, u64::saturating_add), idle))
}

#[cfg(target_os = "linux")]
fn network() -> Option<(u64, u64)> {
    let text = raw("/proc/net/dev")?;
    let mut totals = (0u64, 0u64);
    for line in text.lines().skip(2) {
        let (name, counters) = line.split_once(':')?;
        if name.trim() == "lo" {
            continue;
        }
        let mut fields = counters.split_whitespace();
        totals.0 = totals.0.saturating_add(fields.next()?.parse().ok()?);
        totals.1 = totals.1.saturating_add(fields.nth(7)?.parse().ok()?);
    }
    Some(totals)
}

#[cfg(target_os = "linux")]
fn cpu_topology() -> Option<usize> {
    let text = raw("/proc/cpuinfo")?;
    let mut cores = std::collections::BTreeSet::new();
    for entry in text.split("\n\n") {
        let field = |key| {
            entry
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find_map(|(name, val)| (name.trim() == key).then_some(val.trim()))
        };
        if let (Some(package), Some(core)) = (field("physical id"), field("core id")) {
            cores.insert((package, core));
        }
    }
    (!cores.is_empty()).then_some(cores.len())
}

#[cfg(target_os = "linux")]
fn memory() -> (Option<u64>, Option<u64>) {
    let Some(text) = raw("/proc/meminfo") else {
        return (None, None);
    };
    let field = |key: &str| {
        text.lines()
            .filter_map(|line| line.split_once(':'))
            .find_map(|(name, value)| (name == key).then_some(value))
            .and_then(|value| value.split_whitespace().next()?.parse::<u64>().ok())
            .map(|value| value.saturating_mul(1024))
    };
    (field("MemTotal"), field("MemAvailable"))
}

#[cfg(target_os = "linux")]
impl Sampler {
    pub fn sample(&mut self, disk: &Path, gpus: Vec<Gpu>) -> Snapshot {
        let root = Path::new("/sys/fs/cgroup");
        let group = raw("/proc/self/cgroup").and_then(|text| cgroup_path(root, &text));
        let local_limit = group
            .as_deref()
            .and_then(|group| ancestors_limit(root, group, cpu_quota));
        let memory_limit = group
            .as_deref()
            .and_then(|group| ancestors_limit(root, group, |path| number(path.join("memory.max"))));
        let group_cpu = group
            .as_deref()
            .and_then(|path| counter(&raw(path.join("cpu.stat"))?, "usage_usec"));
        let group_memory = group
            .as_deref()
            .and_then(|path| number(path.join("memory.current")));
        let next = Counters {
            at: Instant::now(),
            cpu: host_cpu(),
            group_cpu,
            network: network(),
        };
        let delta = self
            .prior
            .as_ref()
            .map(|prior| next.at.duration_since(prior.at).as_secs_f64())
            .filter(|seconds| *seconds > 0.0);
        let cpu = self.prior.as_ref().and_then(|prior| {
            let (count, idle) = next.cpu?;
            let (previous, previous_idle) = prior.cpu?;
            let total = count.checked_sub(previous)?;
            let idle = idle.checked_sub(previous_idle)?;
            (total > 0).then_some(100.0 * total.saturating_sub(idle) as f64 / total as f64)
        });
        let group_cores = self.prior.as_ref().and_then(|prior| {
            Some(next.group_cpu?.checked_sub(prior.group_cpu?)? as f64 / (delta? * 1_000_000.0))
        });
        let speed = self.prior.as_ref().and_then(|prior| {
            let (rx, tx) = next.network?;
            let (px, py) = prior.network?;
            Some((
                rx.checked_sub(px)? as f64 / delta?,
                tx.checked_sub(py)? as f64 / delta?,
            ))
        });
        let logical = std::thread::available_parallelism().map_or(1, usize::from);
        let filesystem = nix::sys::statvfs::statvfs(disk).ok();
        let (total_memory, available_memory) = memory();
        let result = Snapshot {
            logical_cpus: logical,
            physical_cores: cpu_topology(),
            effective_cpus: Some(
                local_limit.map_or(logical as f64, |limit| limit.min(logical as f64)),
            ),
            host_cpu_percent: cpu,
            cgroup_cpu_cores: group_cores,
            total_memory,
            available_memory,
            cgroup_memory_limit: memory_limit,
            cgroup_memory_used: group_memory,
            network_received: next.network.map(|value| value.0),
            network_sent: next.network.map(|value| value.1),
            network_received_per_second: speed.map(|value| value.0),
            network_sent_per_second: speed.map(|value| value.1),
            process_count: fs::read_dir("/proc").ok().map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.parse::<u32>().is_ok())
                    })
                    .count()
            }),
            host_uptime: raw("/proc/uptime")
                .and_then(|value| value.split_whitespace().next()?.parse().ok()),
            disk_bytes: filesystem
                .as_ref()
                .map(|fs| fs.blocks().saturating_mul(fs.fragment_size())),
            disk_available: filesystem
                .as_ref()
                .map(|fs| fs.blocks_available().saturating_mul(fs.fragment_size())),
            gpus,
        };
        self.prior = Some(next);
        result
    }
}

#[cfg(windows)]
#[derive(Default)]
pub struct Sampler {
    system: Option<sysinfo::System>,
    networks: Option<sysinfo::Networks>,
    prior: Option<Instant>,
}

#[cfg(windows)]
impl Sampler {
    pub fn sample(&mut self, disk: &Path, gpus: Vec<Gpu>) -> Snapshot {
        use sysinfo::{Disks, Networks, ProcessRefreshKind, ProcessesToUpdate, System};

        let logical = std::thread::available_parallelism().map_or(1, usize::from);
        let mut snapshot = Snapshot {
            logical_cpus: logical,
            effective_cpus: Some(logical as f64),
            physical_cores: System::physical_core_count().ok(),
            host_uptime: System::uptime().ok().map(|seconds| seconds as f64),
            gpus,
            ..Snapshot::default()
        };
        if self.system.is_none() {
            self.system = System::new().ok();
        }
        if let Some(system) = &mut self.system {
            system.refresh_cpu_usage();
            system.refresh_memory();
            system.refresh_processes_specifics(
                ProcessesToUpdate::All,
                true,
                ProcessRefreshKind::nothing(),
            );
            snapshot.host_cpu_percent = self.prior.map(|_| f64::from(system.global_cpu_usage()));
            snapshot.total_memory = Some(system.total_memory());
            snapshot.available_memory = Some(system.available_memory());
            snapshot.process_count = Some(system.processes().len());
        }
        if self.networks.is_none() {
            self.networks = Networks::new_with_refreshed_list().ok();
        }
        if let Some(networks) = &mut self.networks {
            networks.refresh(true);
            let totals = networks
                .values()
                .fold((0u64, 0u64, 0u64, 0u64), |counts, net| {
                    (
                        counts.0.saturating_add(net.total_received()),
                        counts.1.saturating_add(net.total_transmitted()),
                        counts.2.saturating_add(net.received()),
                        counts.3.saturating_add(net.transmitted()),
                    )
                });
            snapshot.network_received = Some(totals.0);
            snapshot.network_sent = Some(totals.1);
            if let Some(prior) = self.prior {
                let seconds = prior.elapsed().as_secs_f64().max(0.001);
                snapshot.network_received_per_second = Some(totals.2 as f64 / seconds);
                snapshot.network_sent_per_second = Some(totals.3 as f64 / seconds);
            }
        }
        if let (Ok(path), Ok(disks)) = (fs::canonicalize(disk), Disks::new_with_refreshed_list()) {
            let path = path.to_string_lossy();
            let path = path.trim_start_matches(r"\\?\").to_ascii_lowercase();
            if let Some(volume) = disks
                .iter()
                .filter(|volume| {
                    path.starts_with(&volume.mount_point().to_string_lossy().to_ascii_lowercase())
                })
                .max_by_key(|volume| volume.mount_point().as_os_str().len())
            {
                snapshot.disk_bytes = Some(volume.total_space());
                snapshot.disk_available = Some(volume.available_space());
            }
        }
        self.prior = Some(Instant::now());
        snapshot
    }
}

pub fn discover_gpus(drm: &Path, devices: &Path) -> Vec<Gpu> {
    let mut gpu = Vec::new();
    if let Ok(entries) = fs::read_dir(drm) {
        for entry in entries.filter_map(Result::ok).take(128) {
            let name = entry.file_name();
            let Some(name) = name.to_str().filter(|name| {
                name.strip_prefix("card")
                    .is_some_and(|id| id.parse::<u16>().is_ok())
            }) else {
                continue;
            };
            let device = entry.path().join("device");
            let vendor = match raw(device.join("vendor")).as_deref().map(str::trim) {
                Some("0x10de") => "nvidia",
                Some("0x8086") => "intel",
                Some("0x1002") => "amd",
                Some("0x14e4") => "broadcom",
                _ => "unknown",
            };
            let driver = fs::read_link(device.join("driver")).ok().and_then(|path| {
                path.file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            });
            let model = raw(device.join("product_name"))
                .or_else(|| raw(device.join("device")))
                .map(|value| value.trim().to_owned())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| vendor.to_owned());
            let identifier = fs::canonicalize(&device)
                .ok()
                .and_then(|path| {
                    path.file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                })
                .unwrap_or_else(|| name.into());
            gpu.push(Gpu {
                vendor,
                model,
                identifier,
                device: driver,
                memory_bytes: number(device.join("mem_info_vram_total")),
                memory_used_bytes: number(device.join("mem_info_vram_used")),
                utilization: number(device.join("gpu_busy_percent"))
                    .filter(|value| *value <= 100)
                    .map(|v| v as f64),
                encoder: None,
                decoder: None,
            });
        }
    }
    if let Ok(entries) = fs::read_dir(devices) {
        for entry in entries.filter_map(Result::ok).take(64) {
            let sysname = raw(entry.path().join("name")).unwrap_or_default();
            if ["bcm2835-codec", "rpivid", "bcm2712-codec"]
                .iter()
                .any(|name| sysname.contains(name))
            {
                gpu.push(Gpu {
                    vendor: "broadcom",
                    model: sysname.trim().into(),
                    identifier: entry.file_name().to_string_lossy().into_owned(),
                    device: Some("v4l2".into()),
                    memory_bytes: None,
                    memory_used_bytes: None,
                    utilization: None,
                    encoder: None,
                    decoder: None,
                });
            }
        }
    }
    gpu.sort_unstable_by(|a, b| (&a.vendor, &a.identifier).cmp(&(&b.vendor, &b.identifier)));
    gpu
}

#[cfg(any(target_os = "linux", test))]
pub fn parse_nvidia_csv(csv: &str) -> Vec<Gpu> {
    csv.lines()
        .take(64)
        .filter_map(|line| {
            let columns: Vec<_> = line.rsplitn(8, ',').map(str::trim).collect();
            let [
                decoder,
                encoder,
                utilization,
                used,
                total,
                model,
                bus,
                unique,
            ] = columns.as_slice()
            else {
                return None;
            };
            if !unique.starts_with("GPU-") || unique.len() > 72 || model.len() > 256 {
                return None;
            }
            let percent = |value: &str| {
                value
                    .parse::<f64>()
                    .ok()
                    .filter(|number| (0.0..=100.0).contains(number))
            };
            let bytes = |value: &str| {
                value
                    .parse::<u64>()
                    .ok()
                    .and_then(|mib| mib.checked_mul(1024 * 1024))
            };
            Some(Gpu {
                vendor: "nvidia",
                model: crate::engine::sanitize(model),
                identifier: (*unique).into(),
                device: Some((*bus).into()),
                memory_bytes: bytes(total),
                memory_used_bytes: bytes(used),
                utilization: percent(utilization),
                encoder: percent(encoder),
                decoder: percent(decoder),
            })
        })
        .collect()
}

#[cfg(target_os = "linux")]
pub async fn nvidia() -> Vec<Gpu> {
    use tokio::io::AsyncReadExt;
    async fn query() -> Option<Vec<Gpu>> {
        let mut child = tokio::process::Command::new("/usr/bin/nvidia-smi")
            .args(["--query-gpu=uuid,pci.bus_id,name,memory.total,memory.used,utilization.gpu,utilization.encoder,utilization.decoder",
                "--format=csv,noheader,nounits"])
            .env_clear().env("PATH", "/usr/bin:/bin").env("LC_ALL","C")
            .kill_on_drop(true).stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null()).spawn().ok()?;
        let mut result = Vec::new();
        child
            .stdout
            .take()?
            .take(65_537)
            .read_to_end(&mut result)
            .await
            .ok()?;
        if result.len() > 65_536 || !child.wait().await.ok()?.success() {
            return None;
        }
        Some(parse_nvidia_csv(std::str::from_utf8(&result).ok()?))
    }
    tokio::time::timeout(std::time::Duration::from_millis(1000), query())
        .await
        .ok()
        .flatten()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    #[test]
    fn cgroup_membership_is_relative_and_ancestor_limits_are_effective() {
        let root = tempfile::TempDir::new().unwrap();
        let child = root.path().join("tenant/job");
        fs::create_dir_all(&child).unwrap();
        fs::write(root.path().join("cpu.max"), "40000 10000").unwrap();
        fs::write(child.join("cpu.max"), "max 10000").unwrap();
        fs::write(child.parent().unwrap().join("cpu.max"), "25000 10000").unwrap();
        let resolved = cgroup_path(root.path(), "0::/tenant/job\n").unwrap();
        assert_eq!(ancestor_limit(root.path(), &resolved), Some(2.5));
        assert!(cgroup_path(root.path(), "0::/../outside").is_none());
        assert_eq!(
            cgroup_path(root.path(), "0::/").as_deref(),
            Some(root.path())
        );
    }

    #[cfg(target_os = "linux")]
    fn ancestor_limit(root: &Path, group: &Path) -> Option<f64> {
        ancestors_limit(root, group, cpu_quota)
    }

    #[test]
    fn gpu_counters_preserve_unavailable_values() {
        let data = parse_nvidia_csv(
            "GPU-test, 0000:01:00.0, Fixture, 4096, 1024, 60, [N/A], 3\nnot-a-gpu",
        );
        assert_eq!(data.len(), 1);
        assert_eq!(data[0].memory_bytes, Some(4 * 1024 * 1024 * 1024));
        assert_eq!(data[0].encoder, None);
        assert_eq!(data[0].decoder, Some(3.0));
        let dir = tempfile::TempDir::new().unwrap();
        let device = dir.path().join("card0/device");
        fs::create_dir_all(&device).unwrap();
        fs::write(device.join("vendor"), "0x1002\n").unwrap();
        fs::write(device.join("gpu_busy_percent"), "47\n").unwrap();
        let data = discover_gpus(dir.path(), &dir.path().join("unavailable"));
        assert_eq!(data[0].vendor, "amd");
        assert_eq!(data[0].utilization, Some(47.0));
    }
    #[test]
    #[ignore = "repeatable resource sampling benchmark"]
    fn benchmark_resource_collection() {
        let mut sampler = Sampler::default();
        let start = std::time::Instant::now();
        for _ in 0..100 {
            std::hint::black_box(sampler.sample(Path::new("/"), Vec::new()));
        }
        println!(
            "Host snapshot mean: {:.3} ms",
            start.elapsed().as_secs_f64() * 10.0
        );
    }
}

#[cfg(windows)]
pub async fn nvidia() -> Vec<Gpu> {
    Vec::new()
}
