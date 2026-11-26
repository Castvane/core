use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use crate::engine::{Limits, Usage};

pub struct Cgroup(PathBuf);

impl Cgroup {
    pub async fn attach(root: Option<&Path>, pid: u32, limits: &Limits) -> Option<Self> {
        let root = root?;
        let check = root.to_owned();
        if !tokio::task::spawn_blocking(move || {
            nix::sys::statfs::statfs(&check)
                .is_ok_and(|stat| stat.filesystem_type() == nix::sys::statfs::CGROUP2_SUPER_MAGIC)
        })
        .await
        .ok()?
        {
            return None;
        }
        if limits.cpu_usage == 0.0 && limits.memory_mbytes == 0 {
            return None;
        }
        let path = root.join(format!("restreamer-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir(&path).await.ok()?;
        let result = async {
            if limits.cpu_usage > 0.0 {
                let quota = (limits.cpu_usage * 1000.0).round().max(1000.0) as u64;
                tokio::fs::write(path.join("cpu.max"), format!("{quota} 100000")).await?;
            }
            if limits.memory_mbytes > 0 {
                tokio::fs::write(
                    path.join("memory.max"),
                    (limits.memory_mbytes * 1024 * 1024).to_string(),
                )
                .await?;
            }
            tokio::fs::write(path.join("cgroup.procs"), pid.to_string()).await
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_dir(&path).await;
            return None;
        }
        Some(Self(path))
    }

    pub async fn cleanup(self) -> bool {
        let events = tokio::fs::read_to_string(self.0.join("memory.events"))
            .await
            .unwrap_or_default();
        let oom = events
            .lines()
            .find_map(|line| line.strip_prefix("oom_kill "))
            .and_then(|count| count.parse::<u64>().ok())
            .is_some_and(|count| count > 0);
        let _ = tokio::fs::write(self.0.join("cgroup.kill"), b"1").await;
        for _ in 0..3 {
            if tokio::fs::remove_dir(&self.0).await.is_ok() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        oom
    }
}

#[derive(Default)]
pub struct Monitor {
    previous: Option<(u64, Instant)>,
    exceeded_since: Option<Instant>,
}

impl Monitor {
    pub async fn sample(&mut self, pid: u32, usage: &mut Usage) -> bool {
        let stat = tokio::fs::read_to_string(format!("/proc/{pid}/stat")).await;
        let status = tokio::fs::read_to_string(format!("/proc/{pid}/status")).await;
        let (Ok(stat), Ok(status)) = (stat, status) else {
            return false;
        };
        let Some((_, fields)) = stat.rsplit_once(')') else {
            return false;
        };
        let mut words = fields.split_whitespace();
        let Some(user) = words.nth(11).and_then(|word| word.parse::<u64>().ok()) else {
            return false;
        };
        let Some(system) = words.next().and_then(|word| word.parse::<u64>().ok()) else {
            return false;
        };
        let total = user.saturating_add(system);
        let now = Instant::now();
        let clock = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
            .ok()
            .flatten()
            .unwrap_or(100);
        let cpu_sampled = self.previous.is_some();
        if let Some((prev, then)) = self.previous {
            usage.cpu = (total.saturating_sub(prev) as f64 * 100.0)
                / (clock as f64 * now.duration_since(then).as_secs_f64().max(0.001));
            usage.cpu_peak = usage.cpu_peak.max(usage.cpu);
        }
        let memory = status
            .lines()
            .find_map(|line| line.strip_prefix("VmRSS:"))
            .and_then(|rest| rest.split_whitespace().next())
            .and_then(|value| value.parse::<u64>().ok())
            .unwrap_or(0)
            .saturating_mul(1024);
        usage.memory = memory;
        usage.memory_peak = usage.memory_peak.max(memory);
        usage.samples += 1;
        let count = usage.samples as f64;
        usage.cpu_average += (usage.cpu - usage.cpu_average) / count;
        usage.memory_average += (memory as f64 - usage.memory_average) / count;
        self.previous = Some((total, now));
        cpu_sampled
    }

    pub fn exceeds(&mut self, limits: &Limits, usage: &Usage, cpu_sampled: bool) -> bool {
        let exceeds = (cpu_sampled && limits.cpu_usage > 0.0 && usage.cpu > limits.cpu_usage)
            || (limits.memory_mbytes > 0 && usage.memory > limits.memory_mbytes * 1024 * 1024);
        if exceeds {
            self.exceeded_since
                .get_or_insert_with(Instant::now)
                .elapsed()
                .as_secs()
                >= limits.waitfor_seconds
        } else {
            self.exceeded_since = None;
            false
        }
    }
}
