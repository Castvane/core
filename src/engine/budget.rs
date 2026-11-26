use std::sync::{Arc, Mutex};

use super::{Error, Limits};

#[derive(Clone, Copy, Default)]
struct Claim {
    cpu: f64,
    bytes: u64,
}

impl Claim {
    fn of(limits: &Limits) -> Self {
        Self {
            cpu: limits.cpu_usage,
            bytes: limits.memory_mbytes.saturating_mul(1024 * 1024),
        }
    }

    fn sum(self, other: Self) -> Self {
        Self {
            cpu: self.cpu + other.cpu,
            bytes: self.bytes.saturating_add(other.bytes),
        }
    }
}

pub struct Budget {
    cpu: Option<f64>,
    memory: Option<u64>,
    used: Mutex<Claim>,
}

pub struct Reservation {
    budget: Arc<Budget>,
    claim: Claim,
}

impl Budget {
    pub fn from_host(
        doc: &serde_json::Value,
        host: &crate::ops::resources::Snapshot,
    ) -> Result<Arc<Self>, Error> {
        let requested = |key: &str| doc["resources"][key].as_f64().filter(|value| *value > 0.0);
        let cpu = requested("max_cpu_usage")
            .map(|percent| {
                host.effective_cpus
                    .map(|count| count * percent)
                    .ok_or(Error::Unavailable("effective CPU capacity is unavailable"))
            })
            .transpose()?;
        let memory = requested("max_memory_usage")
            .map(|percent| {
                let bytes = match (host.total_memory, host.cgroup_memory_limit) {
                    (Some(a), Some(b)) => Some(a.min(b)),
                    (a, b) => a.or(b),
                };
                bytes
                    .map(|bytes| (bytes as f64 * percent / 100.0) as u64)
                    .ok_or(Error::Unavailable(
                        "effective memory capacity is unavailable",
                    ))
            })
            .transpose()?;
        Ok(Arc::new(Self::new(cpu, memory)))
    }

    fn new(cpu: Option<f64>, memory: Option<u64>) -> Self {
        Self {
            cpu,
            memory,
            used: Mutex::default(),
        }
    }

    fn fits(&self, claim: Claim) -> bool {
        self.cpu.is_none_or(|cap| claim.cpu <= cap)
            && self.memory.is_none_or(|cap| claim.bytes <= cap)
    }

    pub fn validate(&self, limits: &Limits) -> Result<(), Error> {
        if self.fits(Claim::of(limits)) {
            Ok(())
        } else {
            Err(Error::Invalid("process reservation exceeds node capacity"))
        }
    }

    pub fn claim(self: &Arc<Self>, limits: &Limits) -> Option<Reservation> {
        let claim = Claim::of(limits);
        let mut used = self.used.lock().ok()?;
        let combined = used.sum(claim);
        if !self.fits(combined) {
            return None;
        }
        *used = combined;
        Some(Reservation {
            budget: self.clone(),
            claim,
        })
    }

    pub fn diagnostics(&self) -> serde_json::Value {
        let used = self.used.lock().map(|held| *held).unwrap_or_default();
        serde_json::json!({"cpu_capacity_percent_of_one_core":self.cpu,"memory_capacity_bytes":self.memory,
            "cpu_reserved_percent_of_one_core":used.cpu,"memory_reserved_bytes":used.bytes})
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        if let Ok(mut used) = self.budget.used.lock() {
            used.cpu = (used.cpu - self.claim.cpu).max(0.0);
            used.bytes = used.bytes.saturating_sub(self.claim.bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_reservations_release_on_drop_and_oversized_claims_fail() {
        let memory = 20 * 1024 * 1024;
        let budget = Arc::new(Budget::new(Some(100.0), Some(memory)));
        let limits = Limits {
            memory_mbytes: 12,
            cpu_usage: 60.0,
            ..Limits::default()
        };
        let first = budget.claim(&limits).unwrap();
        assert!(budget.claim(&limits).is_none());
        assert!(
            budget
                .validate(&Limits {
                    memory_mbytes: 21,
                    ..Limits::default()
                })
                .is_err()
        );
        drop(first);
        assert!(budget.claim(&limits).is_some());
    }
}
