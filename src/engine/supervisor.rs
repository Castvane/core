use std::{
    process::ExitStatus,
    sync::{Arc, Mutex, Weak},
    time::{Duration, Instant},
};

use nix::sys::signal::Signal;
use regex::Regex;
use tokio::{
    process::Child,
    sync::{Semaphore, broadcast, watch},
    task::JoinHandle,
};

use super::{
    Entry, Error, Event, Execution, Graph, Intent, Job, Options, Repository, Run, Runtime,
    command::CommandLine,
    executable::{Group, launch},
    progress::{Heartbeat, collect, collect_logs, lock},
    resources::{Cgroup, Monitor},
    scheduler::Schedule,
    unix_now,
};

pub struct Supervisor {
    pub graph: Weak<Graph>,
    pub live: Weak<super::LiveOutputs>,
    pub repository: Repository,
    pub options: watch::Receiver<Arc<Options>>,
    pub shutdown: watch::Receiver<bool>,
    pub slots: Arc<Semaphore>,
    pub budget: Arc<super::Budget>,
    pub events: broadcast::Sender<Event>,
}

impl Supervisor {
    pub fn spawn(self, entry: &Arc<Entry>, updates: watch::Receiver<Intent>) -> JoinHandle<()> {
        let state = entry.runtime.clone();
        tokio::spawn(self.run(updates, state))
    }

    async fn run(mut self, mut updates: watch::Receiver<Intent>, state: Arc<Mutex<Runtime>>) {
        loop {
            let intent = updates.borrow_and_update().clone();
            if *self.shutdown.borrow() || intent.removed {
                break;
            }
            if intent.job.desired {
                self.cycle(&intent, &mut updates, &state).await;
            } else {
                let mut state = lock(&state);
                state.reconnect = None;
                if state.execution == Execution::Starting {
                    state.execution = Execution::Finished;
                }
            }
            if *self.shutdown.borrow() {
                break;
            }
            match updates.has_changed() {
                Ok(true) => continue,
                Err(_) => break,
                Ok(false) => {}
            }
            if interrupted(&mut updates, &mut self.shutdown).await {
                break;
            }
        }
    }

    async fn cycle(
        &mut self,
        intent: &Intent,
        updates: &mut watch::Receiver<Intent>,
        state: &Arc<Mutex<Runtime>>,
    ) {
        let job = &intent.job;
        let Ok(schedule) = Schedule::parse(&job.config.schedule) else {
            return;
        };
        let scheduled = schedule
            .as_ref()
            .and_then(|item| item.next(jiff::Timestamp::now()));
        let mut delay = if schedule.is_some() && !intent.immediate {
            let Some(delay) = scheduled else {
                if lock(state).execution == Execution::Starting {
                    lock(state).execution = Execution::Finished;
                }
                return;
            };
            delay
        } else {
            Duration::ZERO
        };
        let local_storage = job
            .config
            .inputs
            .iter()
            .chain(&job.config.outputs)
            .any(|endpoint| {
                endpoint.address.contains("{memfs}")
                    || endpoint
                        .address
                        .match_indices("{fs:")
                        .any(|(index, prefix)| {
                            !endpoint.address[index + prefix.len()..].starts_with("disk}")
                        })
            });
        let mut retries = 0u32;
        loop {
            if !delay.is_zero() {
                lock(state).reconnect = Some(Instant::now() + delay);
                tokio::select! {
                    biased;
                    _ = wake(updates, &mut self.shutdown) => { lock(state).reconnect = None; return; }
                    _ = tokio::time::sleep(delay) => {}
                }
            }
            let options = loop {
                let current = self.options.borrow_and_update().clone();
                if !local_storage || current.local_media_ready() {
                    break current;
                }
                tokio::select! {
                    biased;
                    _ = wake(updates, &mut self.shutdown) => return,
                    changed = self.options.changed() => if changed.is_err() { return; },
                }
            };
            {
                let mut runtime = lock(state);
                runtime.reconnect = None;
                runtime.execution = Execution::Starting;
            }
            let permit = tokio::select! {
                biased;
                _ = wake(updates, &mut self.shutdown) => return,
                permit = self.slots.clone().acquire_owned() => match permit { Ok(permit) => permit, Err(_) => return },
            };
            // The configured limit may be tighter than the absolute concurrency bound.
            let budget = 128 - options.max_children;
            if self.slots.available_permits() < budget {
                drop(permit);
                // A small scoped retry leaves capacity available to already queued workers.
                delay = Duration::from_millis(250);
                continue;
            }
            let fits = self.budget.validate(&job.config.limits);
            let mut reservation = None;
            if fits.is_ok() {
                let Some(claim) = self.budget.claim(&job.config.limits) else {
                    drop(permit);
                    delay = Duration::from_millis(250);
                    continue;
                };
                reservation = Some(claim);
            }
            let reason = if let Err(error) = fits {
                Err(error)
            } else if let (Some(graph), Some(live)) = (self.graph.upgrade(), self.live.upgrade()) {
                let graph = graph.read().await;
                options.build(&job.config, &graph, &lock(&live))
            } else {
                Err(Error::Internal)
            };
            let exit = match reason {
                Ok(command) => self.execute(job, command, &options, state, updates).await,
                Err(error) => {
                    let created = unix_now();
                    let message = error.public();
                    lock(state).active = Run {
                        created,
                        log: [[created.to_string(), message.into()]].into(),
                        ..Run::default()
                    };
                    Exit {
                        state: Execution::Failed,
                        reason: message.into(),
                        code: None,
                        interrupted: false,
                    }
                }
            };
            drop(reservation);
            drop(permit);
            let run = {
                let mut runtime = lock(state);
                runtime.execution = exit.state;
                runtime.pid = None;
                runtime.launched = None;
                runtime.last_exit = Some((exit.code, exit.reason.to_string()));
                let mut run = runtime.active.clone();
                run.exited = unix_now();
                run.state = exit.state.wire().into();
                run.reason = exit.reason.to_string();
                run.code = exit.code;
                run.restart_count = runtime.restart_count;
                run.progress = Some(runtime.progress.clone());
                run.usage = Some(runtime.usage.clone());
                runtime.active = Run::default();
                runtime.log_bytes = 0;
                runtime.progress = Default::default();
                runtime.usage = Default::default();
                runtime.inputs.clear();
                runtime.outputs.clear();
                run
            };
            if let Err(error) = self
                .repository
                .record(&job.uid, &run, options.history)
                .await
            {
                tracing::warn!(
                    kind = error.public(),
                    "process report could not be persisted"
                );
            }
            let _ = self.events.send(Event {
                process: job.config.id.clone(),
                domain: job.config.domain.clone(),
                execution: exit.state,
            });
            if !*self.shutdown.borrow()
                && !updates.has_changed().unwrap_or(true)
                && let Some(graph) = self.graph.upgrade()
            {
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    super::command::clean_outputs(&options, &job.config, false, &graph),
                )
                .await;
            }
            if exit.interrupted || *self.shutdown.borrow() || updates.has_changed().unwrap_or(true)
            {
                return;
            }
            let next = schedule
                .as_ref()
                .and_then(|item| item.next(jiff::Timestamp::now()));
            let limit = job.config.retries.unwrap_or(options.retry_limit);
            let backoff = retry_delay(job.config.delay, retries);
            if job.config.reconnect
                && retries < limit
                && (exit.state != Execution::Finished || schedule.is_none())
                && next.is_none_or(|next| backoff < next)
            {
                retries += 1;
                let mut runtime = lock(state);
                runtime.restart_count = runtime.restart_count.saturating_add(1);
                delay = backoff;
            } else if let Some(next) = next {
                delay = next;
                retries = 0;
            } else {
                return;
            }
        }
    }

    async fn execute(
        &mut self,
        job: &Job,
        command: CommandLine,
        options: &Options,
        state: &Arc<Mutex<Runtime>>,
        updates: &mut watch::Receiver<Intent>,
    ) -> Exit {
        let now = Instant::now();
        {
            let mut runtime = lock(state);
            runtime.command = command.display;
            let describe = |io: &super::command::Resolved| {
                (io.id.clone(), command.redactor.clean(&io.address))
            };
            runtime.inputs = command.inputs.iter().map(describe).collect();
            runtime.outputs = command.outputs.iter().map(describe).collect();

            runtime.active = Run {
                created: unix_now(),
                ..Run::default()
            };
            runtime.log_bytes = 0;
            runtime.progress = Default::default();
            runtime.usage = Default::default();
            runtime.launched = Some(now);
        }
        let Ok((mut child, group)) =
            launch(&options.ffmpeg, &command.args, options.launcher.as_deref())
        else {
            let mut runtime = lock(state);
            let timestamp = runtime.active.created.to_string();
            runtime.active.log.push_back([
                timestamp,
                "configured FFmpeg executable could not start".into(),
            ]);
            return Exit::failed("configured FFmpeg executable could not start");
        };
        let Some(pid) = child.id() else {
            return Exit::failed("FFmpeg child has no PID");
        };
        if let (Some(graph), Some(live)) = (self.graph.upgrade(), self.live.upgrade()) {
            let graph = graph.read().await;
            let current = graph.get(&super::key(&job.config));
            if current.is_some_and(|current| std::ptr::eq(current.as_ref(), job)) {
                lock(&live).insert(
                    job.uid.clone(),
                    command
                        .outputs
                        .iter()
                        .map(|io| (io.id.clone(), io.address.clone()))
                        .collect(),
                );
            }
        }
        let cgroup = Cgroup::attach(options.cgroup.as_deref(), pid, &job.config.limits).await;
        let mut usage = super::Usage {
            enforced: cgroup.is_some(),
            soft: cgroup.is_none()
                && (job.config.limits.cpu_usage > 0.0 || job.config.limits.memory_mbytes > 0),
            ..Default::default()
        };
        {
            let mut runtime = lock(state);
            runtime.execution = Execution::Running;
            runtime.pid = Some(pid);
            runtime.usage = usage.clone();
        }
        let _ = self.events.send(Event {
            process: job.config.id.clone(),
            domain: job.config.domain.clone(),
            execution: Execution::Running,
        });
        let heartbeat = Arc::new(Mutex::new(Heartbeat::default()));
        let Some(stdout) = child.stdout.take() else {
            return Exit::failed("FFmpeg progress pipe unavailable");
        };
        let Some(stderr) = child.stderr.take() else {
            return Exit::failed("FFmpeg log pipe unavailable");
        };
        let patterns = job
            .config
            .patterns
            .iter()
            .filter_map(|pattern| Regex::new(pattern).ok())
            .collect();
        let progress = tokio::spawn(collect(stdout, state.clone(), heartbeat.clone()));
        let logs = tokio::spawn(collect_logs(
            stderr,
            state.clone(),
            Arc::new(command.redactor),
            patterns,
            options.log_lines,
        ));
        let mut monitor = Monitor::default();
        let mut sample = tokio::time::interval(Duration::from_millis(500));
        sample.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let config = &job.config;

        let mut cleanup_ticks = 0u32;
        let mut exit = loop {
            tokio::select! {
                biased;
                _ = wake(updates, &mut self.shutdown) => {
                    let reason = if *self.shutdown.borrow() { "application shutdown".into() } else { updates.borrow().reason.clone() };
                    break terminate(state, &mut child, &group, options.stop_timeout, reason, true).await;
                }
                status = child.wait() => break Exit::natural(status),
                _ = sample.tick() => {
                    cleanup_ticks += 1;
                    if cleanup_ticks >= 30 && let Some(graph) = self.graph.upgrade() {
                        cleanup_ticks = 0;
                        let _ = tokio::time::timeout(Duration::from_millis(200),
                            super::command::clean_outputs(options,config,false,&graph)).await;
                    }
                    let cpu_sampled = monitor.sample(pid, &mut usage).await;
                    lock(state).usage = usage.clone();
                    let idle = lock(&heartbeat).0.unwrap_or(now).elapsed().as_secs();
                    let reason = if config.duration > 0 && now.elapsed().as_secs() >= config.duration {
                        Some(("runtime limit reached", true))
                    } else if config.stale > 0 && idle >= config.stale {
                        Some(("stale progress", false))
                    } else if usage.soft && monitor.exceeds(&config.limits, &usage, cpu_sampled) {
                        Some(("sampled resource limit exceeded", false))
                    } else { None };
                    if let Some((reason, normal)) = reason {
                        let mut exit = terminate(state, &mut child, &group, options.stop_timeout, reason.into(), false).await;
                        if !normal && exit.state == Execution::Finished { exit.state = Execution::Failed; }
                        break exit;
                    }
                }
            }
        };
        group.signal(Signal::SIGKILL);
        if options.launcher.is_some() && exit.code == Some(126) && !exit.interrupted {
            exit.reason = "configured FFmpeg executable could not start".into();
        }
        if let Some(cgroup) = cgroup
            && cgroup.cleanup().await
            && !exit.interrupted
        {
            exit.reason = "cgroup memory limit exceeded".into();
            exit.state = Execution::Killed;
        }
        let mut readers = [progress, logs];
        for reader in &mut readers {
            if tokio::time::timeout(Duration::from_millis(400), &mut *reader)
                .await
                .is_err()
            {
                reader.abort();
                let _ = reader.await;
            }
        }
        exit
    }
}

fn retry_delay(delay: u64, retries: u32) -> Duration {
    let base = delay.max(1);
    Duration::from_secs(
        base.saturating_mul(2u64.pow(retries.min(20)))
            .min(base.max(300)),
    )
}

async fn wake(updates: &mut watch::Receiver<Intent>, shutdown: &mut watch::Receiver<bool>) {
    if *shutdown.borrow()
        || shutdown.has_changed().is_err()
        || updates.has_changed().unwrap_or(true)
    {
        return;
    }
    // wait_for avoids consuming a notification before the outer cycle snapshots the new intent.
    let mut changes = updates.clone();
    tokio::select! { _ = changes.changed() => {}, _ = shutdown.changed() => {} }
}

async fn interrupted(
    updates: &mut watch::Receiver<Intent>,
    shutdown: &mut watch::Receiver<bool>,
) -> bool {
    wake(updates, shutdown).await;
    *shutdown.borrow() || shutdown.has_changed().is_err() || updates.has_changed().is_err()
}

struct Exit {
    state: Execution,
    reason: Arc<str>,
    code: Option<i32>,
    interrupted: bool,
}

impl Exit {
    fn failed(reason: impl Into<Arc<str>>) -> Self {
        Self {
            state: Execution::Failed,
            reason: reason.into(),
            code: None,
            interrupted: false,
        }
    }

    fn natural(status: std::io::Result<ExitStatus>) -> Self {
        let Ok(status) = status else {
            return Self::failed("FFmpeg exit status unavailable");
        };
        if status.success() {
            Self {
                state: Execution::Finished,
                reason: "FFmpeg completed".into(),
                code: status.code(),
                interrupted: false,
            }
        } else {
            use std::os::unix::process::ExitStatusExt;
            let state = if status.signal().is_some() {
                Execution::Killed
            } else {
                Execution::Failed
            };
            Self {
                state,
                reason: "FFmpeg exited unexpectedly".into(),
                code: status.code(),
                interrupted: false,
            }
        }
    }
}

async fn terminate(
    state: &Arc<Mutex<Runtime>>,
    child: &mut Child,
    group: &Group,
    timeout: u64,
    reason: Arc<str>,
    interrupted: bool,
) -> Exit {
    lock(state).execution = Execution::Finishing;
    group.signal(Signal::SIGTERM);
    match tokio::time::timeout(Duration::from_secs(timeout), child.wait()).await {
        Ok(Ok(status)) => Exit {
            state: Execution::Finished,
            reason,
            code: status.code(),
            interrupted,
        },
        Ok(Err(_)) => Exit {
            interrupted,
            ..Exit::failed(reason)
        },
        Err(_) => {
            group.signal(Signal::SIGKILL);
            let _ = child.wait().await;
            Exit {
                state: Execution::Killed,
                reason,
                code: None,
                interrupted,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn retry_is_exponential_and_bounded() {
        assert_eq!(retry_delay(0, 0).as_secs(), 1);
        assert_eq!(retry_delay(3, 2).as_secs(), 12);
        assert_eq!(retry_delay(3, 100).as_secs(), 300);
    }
}
