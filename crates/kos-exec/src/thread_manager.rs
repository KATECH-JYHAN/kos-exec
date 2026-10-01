// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::comm::Transport;
use crate::config::ThreadConfigToml;
use crate::error::{KosError, Result};
use crate::thread_context::ThreadContext;

#[derive(Debug, Clone, PartialEq)]
pub enum Trigger {
    Periodic(Duration),
    Event(String),
}

pub type Priority = kos_safety::Priority;

pub const RT_PRIO_CRITICAL: i32 = 80;
pub const RT_PRIO_HIGH: i32 = 60;
pub const NICE_LOW: i32 = 10;

#[derive(Debug, Clone)]
pub struct ThreadConfig {
    pub name: String,
    pub trigger: Trigger,
    pub priority: Priority,
    pub cpu_affinity: Option<usize>,
    pub subs: Vec<String>,
    pub pubs: Vec<String>,
    pub threshold: Option<u32>,
}

impl ThreadConfig {
    pub fn periodic(name: &str, period: Duration) -> Self {
        Self {
            name: name.to_string(),
            trigger: Trigger::Periodic(period),
            priority: Priority::Normal,
            cpu_affinity: None,
            subs: Vec::new(),
            pubs: Vec::new(),
            threshold: None,
        }
    }

    pub fn event(name: &str, event: &str) -> Self {
        Self {
            name: name.to_string(),
            trigger: Trigger::Event(event.to_string()),
            priority: Priority::Normal,
            cpu_affinity: None,
            subs: Vec::new(),
            pubs: Vec::new(),
            threshold: None,
        }
    }

    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_cpu_affinity(mut self, core: usize) -> Self {
        self.cpu_affinity = Some(core);
        self
    }

    pub fn with_subs(mut self, subs: Vec<&str>) -> Self {
        self.subs = subs.into_iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_pubs(mut self, pubs: Vec<&str>) -> Self {
        self.pubs = pubs.into_iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_threshold(mut self, threshold: u32) -> Self {
        self.threshold = Some(threshold);
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    Created,
    Running,
    Stopped,
    Failed,
}

impl std::fmt::Display for ThreadState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ThreadState::Created => write!(f, "Created"),
            ThreadState::Running => write!(f, "Running"),
            ThreadState::Stopped => write!(f, "Stopped"),
            ThreadState::Failed => write!(f, "Failed"),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct ThreadStats {
    pub run_count: u64,
    pub error_count: u64,
    pub last_run_duration: Option<Duration>,
    pub max_run_duration: Duration,
    pub last_wake_latency: Option<Duration>,
    pub max_wake_latency: Duration,
    pub overrun_count: u64,
    pub skipped_periods: u64,
}

#[derive(Debug)]
pub struct ThreadHandle {
    pub name: String,
    pub config: ThreadConfig,
    state: Arc<Mutex<ThreadState>>,
    stats: Arc<Mutex<ThreadStats>>,
    progress: Arc<AtomicU64>,
    join_handle: Option<JoinHandle<()>>,
}

impl ThreadHandle {
    pub fn state(&self) -> ThreadState {
        *self.state.lock().unwrap()
    }

    pub fn stats(&self) -> ThreadStats {
        self.stats.lock().unwrap().clone()
    }
}

pub type TaskFn = Box<dyn Fn(&ThreadContext) + Send + Sync + 'static>;

pub struct ThreadCallbacks {
    pub on_init: Option<Box<dyn Fn(&mut ThreadContext) -> Result<()> + Send + Sync + 'static>>,
    pub on_run: TaskFn,
    pub on_shutdown: Option<Box<dyn Fn(&mut ThreadContext) + Send + Sync + 'static>>,
}

impl ThreadCallbacks {
    pub fn new<F>(on_run: F) -> Self
    where
        F: Fn(&ThreadContext) + Send + Sync + 'static,
    {
        Self {
            on_init: None,
            on_run: Box::new(on_run),
            on_shutdown: None,
        }
    }

    pub fn with_init<F>(mut self, on_init: F) -> Self
    where
        F: Fn(&mut ThreadContext) -> Result<()> + Send + Sync + 'static,
    {
        self.on_init = Some(Box::new(on_init));
        self
    }

    pub fn with_shutdown<F>(mut self, on_shutdown: F) -> Self
    where
        F: Fn(&mut ThreadContext) + Send + Sync + 'static,
    {
        self.on_shutdown = Some(Box::new(on_shutdown));
        self
    }
}

pub struct ThreadManager {
    threads: HashMap<String, ThreadHandle>,
    callbacks: HashMap<String, Arc<ThreadCallbacks>>,
    shutdown: Arc<Mutex<bool>>,
    transport: Option<Arc<dyn Transport>>,
    overrides: Vec<ThreadConfigToml>,
    liveness: Arc<Mutex<Vec<Liveness>>>,
    watchdog: Option<JoinHandle<()>>,
}

struct Liveness {
    progress: Arc<AtomicU64>,
    state: Arc<Mutex<ThreadState>>,
    slack_ms: u64,
}

impl ThreadManager {
    pub fn new() -> Self {
        Self {
            threads: HashMap::new(),
            callbacks: HashMap::new(),
            shutdown: Arc::new(Mutex::new(false)),
            transport: None,
            overrides: crate::launch_env::LaunchEnv::current().threads.clone(),
            liveness: Arc::new(Mutex::new(Vec::new())),
            watchdog: None,
        }
    }

    pub fn set_thread_overrides(&mut self, overrides: Vec<ThreadConfigToml>) {
        self.overrides = overrides;
    }

    pub fn set_transport(&mut self, transport: Arc<dyn Transport>) {
        self.transport = Some(transport);
    }

    pub fn register(
        &mut self,
        config: ThreadConfig,
        callbacks: ThreadCallbacks,
    ) -> Result<()> {
        let config = match self.overrides.iter().find(|t| t.name == config.name) {
            Some(o) => apply_override(config, o),
            None => config,
        };
        let name = config.name.clone();
        if self.threads.contains_key(&name) {
            return Err(KosError::AlreadyExists(format!("thread '{name}'")));
        }

        let slack_ms = match &config.trigger {
            Trigger::Periodic(period) => period.as_millis() as u64,
            Trigger::Event(_) => 10,
        };
        let handle = ThreadHandle {
            name: name.clone(),
            config,
            state: Arc::new(Mutex::new(ThreadState::Created)),
            stats: Arc::new(Mutex::new(ThreadStats::default())),
            progress: Arc::new(AtomicU64::new(0)),
            join_handle: None,
        };
        self.liveness.lock().unwrap().push(Liveness {
            progress: handle.progress.clone(),
            state: handle.state.clone(),
            slack_ms,
        });

        self.threads.insert(name.clone(), handle);
        self.callbacks.insert(name, Arc::new(callbacks));
        Ok(())
    }

    pub fn start_all(&mut self) -> Result<()> {
        let names: Vec<String> = self.threads.keys().cloned().collect();
        for name in names {
            self.start_thread(&name)?;
        }
        Ok(())
    }

    pub fn start_thread(&mut self, name: &str) -> Result<()> {
        let handle = self.threads.get_mut(name)
            .ok_or_else(|| KosError::NotFound(format!("thread '{name}'")))?;

        if handle.state() == ThreadState::Running {
            return Ok(());
        }

        let callbacks = self.callbacks.get(name)
            .ok_or_else(|| KosError::NotFound(format!("callbacks for thread '{name}'")))?
            .clone();
        let config = handle.config.clone();
        let state = handle.state.clone();
        let stats = handle.stats.clone();
        let progress = handle.progress.clone();
        progress.store(0, AtomicOrdering::Release);
        let shutdown = self.shutdown.clone();
        let transport = self.transport.clone();

        let started = Arc::new(AtomicBool::new(false));
        let started_clone = started.clone();

        *state.lock().unwrap() = ThreadState::Running;

        let state_for_thread = state.clone();
        let join_handle = match thread::Builder::new()
            .name(config.name.clone())
            .spawn(move || {
                started_clone.store(true, AtomicOrdering::Release);
                thread_loop(config, callbacks, state_for_thread, stats, progress, shutdown, transport);
            }) {
            Ok(h) => h,
            Err(e) => {
                *state.lock().unwrap() = ThreadState::Failed;
                return Err(KosError::InvalidConfig(format!("failed to spawn thread '{name}': {e}")));
            }
        };

        while !started.load(AtomicOrdering::Acquire) {
            thread::yield_now();
        }

        handle.join_handle = Some(join_handle);
        self.ensure_watchdog();
        Ok(())
    }

    fn ensure_watchdog(&mut self) {
        let Some(timeout) = crate::heartbeat::timeout() else {
            return;
        };
        if self.watchdog.is_some() {
            return;
        }
        let liveness = self.liveness.clone();
        let shutdown = self.shutdown.clone();
        let timeout_ms = timeout.as_millis() as u64;
        let interval = Duration::from_millis((timeout_ms / 4).clamp(5, 250));
        self.watchdog = thread::Builder::new()
            .name("kos-watchdog".into())
            .spawn(move || loop {
                if *shutdown.lock().unwrap() {
                    break;
                }
                let now = mono_now_ms();
                let all_alive = liveness.lock().unwrap().iter().all(|l| {
                    let last = l.progress.load(AtomicOrdering::Acquire);
                    last == 0
                        || *l.state.lock().unwrap() != ThreadState::Running
                        || now.saturating_sub(last) <= timeout_ms + l.slack_ms
                });
                if all_alive {
                    crate::heartbeat::beat();
                }
                thread::sleep(interval);
            })
            .ok();
    }

    pub fn shutdown_all(&mut self) {
        *self.shutdown.lock().unwrap() = true;
        if let Some(w) = self.watchdog.take() {
            let _ = w.join();
        }

        for (_, handle) in self.threads.iter_mut() {
            if let Some(jh) = handle.join_handle.take() {
                let _ = jh.join();
            }
            *handle.state.lock().unwrap() = ThreadState::Stopped;
        }
    }

    pub fn thread_state(&self, name: &str) -> Option<ThreadState> {
        self.threads.get(name).map(|h| h.state())
    }

    pub fn thread_stats(&self, name: &str) -> Option<ThreadStats> {
        self.threads.get(name).map(|h| h.stats())
    }

    pub fn check_health(&mut self) -> Vec<String> {
        let mut failed = Vec::new();
        for (name, handle) in &mut self.threads {
            if let Some(jh) = &handle.join_handle {
                if jh.is_finished() {
                    if let Some(jh) = handle.join_handle.take() {
                        let panicked = jh.join().is_err();
                        let mut state = handle.state.lock().unwrap();
                        if panicked || *state == ThreadState::Failed {
                            *state = ThreadState::Failed;
                            failed.push(name.clone());
                        } else {
                            *state = ThreadState::Stopped;
                        }
                    }
                }
            }
        }
        failed
    }

    pub fn restart_thread(&mut self, name: &str) -> Result<()> {
        let handle = self.threads.get(name)
            .ok_or_else(|| KosError::NotFound(format!("thread '{name}'")))?;

        match handle.state() {
            ThreadState::Failed | ThreadState::Stopped => {}
            state => {
                return Err(KosError::InvalidTransition(
                    format!("cannot restart thread '{name}' in state {state}"),
                ));
            }
        }

        self.start_thread(name)
    }

    pub fn thread_names(&self) -> Vec<String> {
        self.threads.keys().cloned().collect()
    }

    pub fn thread_count(&self) -> usize {
        self.threads.len()
    }
}

impl Default for ThreadManager {
    fn default() -> Self {
        Self::new()
    }
}

fn apply_override(mut config: ThreadConfig, o: &ThreadConfigToml) -> ThreadConfig {
    match o.trigger.as_str() {
        "event" => {
            if let Some(topic) = &o.event_topic {
                config.trigger = Trigger::Event(topic.clone());
            }
        }
        _ => {
            if let Some(ms) = o.period_ms {
                config.trigger = Trigger::Periodic(Duration::from_millis(ms as u64));
            }
        }
    }
    config.priority = Priority::from_str_lossy(&o.priority);
    if let Some(core) = o.cpu_affinity {
        config.cpu_affinity = Some(core as usize);
    }
    if !o.subs.is_empty() {
        config.subs = o.subs.clone();
    }
    if !o.pubs.is_empty() {
        config.pubs = o.pubs.clone();
    }
    eprintln!(
        "[kos-exec] thread '{}': launch config applied ({:?}, {:?})",
        config.name, config.trigger, config.priority
    );
    config
}

fn thread_loop(
    config: ThreadConfig,
    callbacks: Arc<ThreadCallbacks>,
    state: Arc<Mutex<ThreadState>>,
    stats: Arc<Mutex<ThreadStats>>,
    progress: Arc<AtomicU64>,
    shutdown: Arc<Mutex<bool>>,
    transport: Option<Arc<dyn Transport>>,
) {
    let mut subs = config.subs.clone();
    if let Trigger::Event(topic) = &config.trigger {
        if !subs.contains(topic) {
            subs.push(topic.clone());
        }
    }
    let mut ctx = ThreadContext::new(&config.name, subs, config.pubs.clone());

    if let Some(t) = &transport {
        if let Err(e) = ctx.attach(t.as_ref()) {
            eprintln!(
                "[kos-exec] thread '{}': transport '{}' attach failed: {e}",
                config.name,
                t.name()
            );
            *state.lock().unwrap() = ThreadState::Failed;
            return;
        }
    }

    if let Some(ref on_init) = callbacks.on_init {
        if let Err(_e) = on_init(&mut ctx) {
            *state.lock().unwrap() = ThreadState::Failed;
            return;
        }
    }

    apply_thread_scheduling(&config);

    match &config.trigger {
        Trigger::Periodic(period) => {
            let period_ns = period.as_nanos() as u64;
            let mut next_wake = mono_now_ns();
            loop {
                progress.store(mono_now_ms().max(1), AtomicOrdering::Release);
                if *shutdown.lock().unwrap() {
                    break;
                }

                let start = Instant::now();
                let wake_latency = Duration::from_nanos(mono_now_ns().saturating_sub(next_wake));

                ctx.lock_topics();

                (callbacks.on_run)(&ctx);

                ctx.unlock_topics();

                let elapsed = start.elapsed();

                next_wake += period_ns;
                let now = mono_now_ns();
                let mut skipped = 0u64;
                if period_ns > 0 && now > next_wake {
                    skipped = (now - next_wake) / period_ns + 1;
                    next_wake += skipped * period_ns;
                }

                {
                    let mut s = stats.lock().unwrap();
                    s.run_count += 1;
                    s.last_run_duration = Some(elapsed);
                    if elapsed > s.max_run_duration {
                        s.max_run_duration = elapsed;
                    }
                    s.last_wake_latency = Some(wake_latency);
                    if wake_latency > s.max_wake_latency {
                        s.max_wake_latency = wake_latency;
                    }
                    if skipped > 0 {
                        s.overrun_count += 1;
                        s.skipped_periods += skipped;
                    }
                }

                sleep_until_ns(next_wake);
            }
        }
        Trigger::Event(event_topic) => {
            loop {
                progress.store(mono_now_ms().max(1), AtomicOrdering::Release);
                if *shutdown.lock().unwrap() {
                    break;
                }

                if ctx.is_attached() {
                    if !ctx.wait_topic(event_topic, Duration::from_millis(10)) {
                        continue;
                    }
                } else {
                    thread::sleep(Duration::from_millis(10));
                }

                let start = Instant::now();
                let event_fresh = ctx.is_fresh(event_topic);
                ctx.lock_topics();
                if event_fresh {
                    ctx.mark_fresh(event_topic);
                }
                (callbacks.on_run)(&ctx);
                ctx.unlock_topics();

                let elapsed = start.elapsed();
                {
                    let mut s = stats.lock().unwrap();
                    s.run_count += 1;
                    s.last_run_duration = Some(elapsed);
                    if elapsed > s.max_run_duration {
                        s.max_run_duration = elapsed;
                    }
                }
            }
        }
    }

    if let Some(ref on_shutdown) = callbacks.on_shutdown {
        on_shutdown(&mut ctx);
    }

    *state.lock().unwrap() = ThreadState::Stopped;
}

fn apply_thread_scheduling(config: &ThreadConfig) {
    unsafe {
        libc::prctl(libc::PR_SET_TIMERSLACK, 1 as libc::c_ulong, 0, 0, 0);
    }

    if let Some(core) = config.cpu_affinity {
        use nix::sched::{sched_setaffinity, CpuSet};
        use nix::unistd::Pid;
        let mut cpuset = CpuSet::new();
        let res = cpuset.set(core).and_then(|_| sched_setaffinity(Pid::from_raw(0), &cpuset));
        if let Err(e) = res {
            eprintln!(
                "[kos-exec] WARNING: thread '{}': failed to set CPU affinity {core}: {e}",
                config.name
            );
        }
    }

    match config.priority {
        Priority::Critical => set_thread_fifo(&config.name, RT_PRIO_CRITICAL),
        Priority::High => set_thread_fifo(&config.name, RT_PRIO_HIGH),
        Priority::Normal => {}
        Priority::Low => set_thread_nice(&config.name, NICE_LOW),
    }
}

fn set_thread_fifo(name: &str, priority: i32) {
    let param = libc::sched_param { sched_priority: priority };
    let ret = unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        eprintln!(
            "[kos-exec] WARNING: thread '{name}': failed to set SCHED_FIFO {priority}: {err}. \
             Continuing with CFS. (Requires CAP_SYS_NICE.)"
        );
    }
}

fn set_thread_nice(name: &str, nice: i32) {
    let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::id_t;
    let ret = unsafe { libc::setpriority(libc::PRIO_PROCESS, tid, nice) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("[kos-exec] WARNING: thread '{name}': failed to set nice {nice}: {err}");
    }
}

fn mono_now_ms() -> u64 {
    mono_now_ns() / 1_000_000
}

fn mono_now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn sleep_until_ns(target_ns: u64) {
    let ts = libc::timespec {
        tv_sec: (target_ns / 1_000_000_000) as libc::time_t,
        tv_nsec: (target_ns % 1_000_000_000) as libc::c_long,
    };
    loop {
        let ret = unsafe {
            libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &ts, std::ptr::null_mut())
        };
        if ret != libc::EINTR {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    #[test]
    fn thread_config_builder() {
        let config = ThreadConfig::periodic("sensor", Duration::from_millis(10))
            .with_priority(Priority::Critical)
            .with_cpu_affinity(0)
            .with_subs(vec!["raw_data"])
            .with_pubs(vec!["processed"]);

        assert_eq!(config.name, "sensor");
        assert_eq!(config.trigger, Trigger::Periodic(Duration::from_millis(10)));
        assert_eq!(config.priority, Priority::Critical);
        assert_eq!(config.cpu_affinity, Some(0));
        assert_eq!(config.subs, vec!["raw_data"]);
        assert_eq!(config.pubs, vec!["processed"]);
    }

    #[test]
    fn thread_config_event() {
        let config = ThreadConfig::event("diag", "diag_req")
            .with_priority(Priority::Low);

        assert_eq!(config.name, "diag");
        assert_eq!(
            config.trigger,
            Trigger::Event("diag_req".to_string())
        );
        assert_eq!(config.priority, Priority::Low);
    }

    #[test]
    fn register_and_start_thread() {
        let mut mgr = ThreadManager::new();
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let config = ThreadConfig::periodic("worker", Duration::from_millis(5));
        let callbacks = ThreadCallbacks::new(move |_ctx| {
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        mgr.register(config, callbacks).unwrap();
        mgr.start_all().unwrap();

        thread::sleep(Duration::from_millis(30));

        mgr.shutdown_all();

        assert!(counter.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn duplicate_register_rejected() {
        let mut mgr = ThreadManager::new();

        let config = ThreadConfig::periodic("worker", Duration::from_millis(10));
        let callbacks = ThreadCallbacks::new(|_ctx| {});
        mgr.register(config, callbacks).unwrap();

        let config2 = ThreadConfig::periodic("worker", Duration::from_millis(20));
        let callbacks2 = ThreadCallbacks::new(|_ctx| {});
        let err = mgr.register(config2, callbacks2).unwrap_err();
        assert!(matches!(err, KosError::AlreadyExists(_)));
    }

    #[test]
    fn on_init_called_before_run() {
        let mut mgr = ThreadManager::new();
        let init_flag = Arc::new(AtomicU32::new(0));
        let run_flag = Arc::new(AtomicU32::new(0));
        let init_clone = init_flag.clone();
        let run_clone = run_flag.clone();

        let config = ThreadConfig::periodic("test", Duration::from_millis(5));
        let callbacks = ThreadCallbacks::new(move |_ctx| {
            assert!(init_clone.load(Ordering::Relaxed) > 0);
            run_clone.fetch_add(1, Ordering::Relaxed);
        })
        .with_init({
            let init_flag = init_flag.clone();
            move |_ctx| {
                init_flag.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
        });

        mgr.register(config, callbacks).unwrap();
        mgr.start_all().unwrap();

        thread::sleep(Duration::from_millis(30));
        mgr.shutdown_all();

        assert_eq!(init_flag.load(Ordering::Relaxed), 1);
        assert!(run_flag.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn on_shutdown_called_on_stop() {
        let mut mgr = ThreadManager::new();
        let shutdown_flag = Arc::new(AtomicU32::new(0));
        let shutdown_clone = shutdown_flag.clone();

        let config = ThreadConfig::periodic("test", Duration::from_millis(5));
        let callbacks = ThreadCallbacks::new(|_ctx| {})
            .with_shutdown(move |_ctx| {
                shutdown_clone.fetch_add(1, Ordering::Relaxed);
            });

        mgr.register(config, callbacks).unwrap();
        mgr.start_all().unwrap();

        thread::sleep(Duration::from_millis(20));
        mgr.shutdown_all();

        thread::sleep(Duration::from_millis(10));
        assert_eq!(shutdown_flag.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn thread_stats_updated() {
        let mut mgr = ThreadManager::new();
        let config = ThreadConfig::periodic("stats_test", Duration::from_millis(5));
        let callbacks = ThreadCallbacks::new(|_ctx| {
            thread::sleep(Duration::from_micros(100));
        });

        mgr.register(config, callbacks).unwrap();
        mgr.start_all().unwrap();

        thread::sleep(Duration::from_millis(30));
        mgr.shutdown_all();

        let stats = mgr.thread_stats("stats_test").unwrap();
        assert!(stats.run_count > 0);
        assert!(stats.last_run_duration.is_some());
        assert!(stats.max_run_duration > Duration::ZERO);
    }

    #[test]
    fn periodic_does_not_drift() {
        let period = Duration::from_millis(5);
        let mut mgr = ThreadManager::new();
        let starts = Arc::new(Mutex::new(Vec::<Instant>::new()));
        let st = starts.clone();
        let config = ThreadConfig::periodic("nodrift", period);
        mgr.register(config, ThreadCallbacks::new(move |_ctx| {
            st.lock().unwrap().push(Instant::now());
        })).unwrap();
        mgr.start_all().unwrap();
        thread::sleep(Duration::from_millis(560));
        mgr.shutdown_all();

        let starts = starts.lock().unwrap();
        assert!(starts.len() > 100, "too few runs: {}", starts.len());
        let drift = (95..=100)
            .map(|k| starts[k].saturating_duration_since(starts[0] + period * k as u32))
            .min()
            .unwrap();
        assert!(drift < Duration::from_millis(2), "drift after ~100 periods: {drift:?}");
    }

    #[test]
    fn overrun_skips_missed_periods() {
        let mut mgr = ThreadManager::new();
        let config = ThreadConfig::periodic("overrun", Duration::from_millis(5));
        mgr.register(config, ThreadCallbacks::new(|_ctx| {
            thread::sleep(Duration::from_millis(12));
        })).unwrap();
        mgr.start_all().unwrap();
        thread::sleep(Duration::from_millis(100));
        mgr.shutdown_all();

        let stats = mgr.thread_stats("overrun").unwrap();
        assert!(stats.run_count > 0);
        assert!(stats.overrun_count > 0);
        assert!(stats.skipped_periods >= stats.overrun_count);
        assert!(stats.run_count <= 10, "run_count {} suggests burst catch-up", stats.run_count);
    }

    #[test]
    fn wake_latency_recorded() {
        let mut mgr = ThreadManager::new();
        let config = ThreadConfig::periodic("lat", Duration::from_millis(5));
        mgr.register(config, ThreadCallbacks::new(|_ctx| {})).unwrap();
        mgr.start_all().unwrap();
        thread::sleep(Duration::from_millis(30));
        mgr.shutdown_all();

        let stats = mgr.thread_stats("lat").unwrap();
        assert!(stats.last_wake_latency.is_some());
    }

    #[test]
    fn low_priority_applies_thread_nice() {
        let mut mgr = ThreadManager::new();
        let observed = Arc::new(Mutex::new(None::<i32>));
        let o = observed.clone();
        let config = ThreadConfig::periodic("low", Duration::from_millis(5)).with_priority(Priority::Low);
        mgr.register(config, ThreadCallbacks::new(move |_ctx| {
            let tid = unsafe { libc::syscall(libc::SYS_gettid) } as libc::id_t;
            let nice = unsafe { libc::getpriority(libc::PRIO_PROCESS, tid) };
            *o.lock().unwrap() = Some(nice);
        })).unwrap();
        mgr.start_all().unwrap();
        thread::sleep(Duration::from_millis(20));
        mgr.shutdown_all();

        let nice = observed.lock().unwrap().expect("on_run not called");
        assert!(nice >= NICE_LOW, "expected nice >= {NICE_LOW}, got {nice}");
    }

    #[test]
    fn cpu_affinity_applied() {
        let mut mgr = ThreadManager::new();
        let observed = Arc::new(Mutex::new(None::<i32>));
        let o = observed.clone();
        let config = ThreadConfig::periodic("pinned", Duration::from_millis(5)).with_cpu_affinity(0);
        mgr.register(config, ThreadCallbacks::new(move |_ctx| {
            *o.lock().unwrap() = Some(unsafe { libc::sched_getcpu() });
        })).unwrap();
        mgr.start_all().unwrap();
        thread::sleep(Duration::from_millis(20));
        mgr.shutdown_all();

        assert_eq!(*observed.lock().unwrap(), Some(0));
    }

    fn wait_until(timeout_ms: u64, mut cond: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms);
        while Instant::now() < deadline {
            if cond() {
                return true;
            }
            thread::sleep(Duration::from_millis(2));
        }
        cond()
    }

    #[test]
    fn periodic_threads_exchange_data_via_transport() {
        use crate::comm::MockTransport;
        let mut mgr = ThreadManager::new();
        mgr.set_transport(Arc::new(MockTransport::new()));

        let counter = Arc::new(AtomicU32::new(0));
        let c = counter.clone();
        mgr.register(
            ThreadConfig::periodic("producer", Duration::from_millis(2)).with_pubs(vec!["t/value"]),
            ThreadCallbacks::new(move |ctx| {
                let v = c.fetch_add(1, Ordering::Relaxed) + 1;
                ctx.write("t/value", &v);
            }),
        ).unwrap();

        let seen = Arc::new(Mutex::new(Vec::<u32>::new()));
        let s2 = seen.clone();
        mgr.register(
            ThreadConfig::periodic("consumer", Duration::from_millis(2)).with_subs(vec!["t/value"]),
            ThreadCallbacks::new(move |ctx| {
                if ctx.is_fresh("t/value") {
                    s2.lock().unwrap().push(ctx.read::<u32>("t/value"));
                }
            }),
        ).unwrap();

        mgr.start_all().unwrap();
        assert!(wait_until(1000, || seen.lock().unwrap().len() >= 5));
        mgr.shutdown_all();

        let seen = seen.lock().unwrap();
        assert!(seen.windows(2).all(|w| w[0] < w[1]), "values must increase: {seen:?}");
    }

    #[test]
    fn event_thread_runs_on_new_data_only() {
        use crate::comm::{MockTransport, Transport};
        let transport = Arc::new(MockTransport::new());
        let mut mgr = ThreadManager::new();
        mgr.set_transport(transport.clone());

        let runs = Arc::new(Mutex::new(Vec::<u32>::new()));
        let r = runs.clone();
        mgr.register(
            ThreadConfig::event("on_cmd", "t/cmd"),
            ThreadCallbacks::new(move |ctx| {
                assert!(ctx.is_fresh("t/cmd"));
                r.lock().unwrap().push(ctx.read::<u32>("t/cmd"));
            }),
        ).unwrap();
        mgr.start_all().unwrap();

        thread::sleep(Duration::from_millis(50));
        assert!(runs.lock().unwrap().is_empty(), "must not run without data");

        let mut p = Transport::publisher(transport.as_ref(), "t/cmd").unwrap();
        p.publish(&7u32.to_ne_bytes()).unwrap();
        assert!(wait_until(500, || runs.lock().unwrap().len() == 1));
        thread::sleep(Duration::from_millis(50));
        mgr.shutdown_all();
        assert_eq!(*runs.lock().unwrap(), vec![7]);
    }

    #[test]
    fn checked_transport_blocks_low_asil_publisher() {
        use crate::comm::{CheckedTransport, MockTransport};
        use crate::domain::{AsilLevel, DomainConfig, DomainController};
        let dc = |id: &str, asil| DomainConfig {
            id: id.into(), asil, cores: vec![0], rt_priority: None,
            memory_limit_mb: None, max_pids: None, cpu_quota: None,
        };
        let mut domains = DomainController::from_config(&[dc("adas", AsilLevel::AsilD), dc("ivi", AsilLevel::QM)]).unwrap();
        domains.assign_app("ivi.app", "ivi").unwrap();
        let transport = CheckedTransport::new(MockTransport::new(), "ivi.app", Arc::new(domains));

        let mut mgr = ThreadManager::new();
        mgr.set_transport(Arc::new(transport));
        mgr.register(
            ThreadConfig::periodic("bad", Duration::from_millis(5)).with_pubs(vec!["adas/brake"]),
            ThreadCallbacks::new(|_ctx| {}),
        ).unwrap();
        mgr.start_all().unwrap();
        assert!(wait_until(2000, || mgr.thread_state("bad") == Some(ThreadState::Failed)));
        mgr.shutdown_all();
    }

    #[test]
    fn launch_thread_config_overrides_code_values() {
        let mut mgr = ThreadManager::new();
        mgr.set_thread_overrides(vec![ThreadConfigToml {
            name: "control".into(),
            trigger: "periodic".into(),
            period_ms: Some(5),
            event_topic: None,
            priority: "low".into(),
            cpu_affinity: Some(0),
            subs: vec!["t/in".into()],
            pubs: vec![],
        }]);
        mgr.register(
            ThreadConfig::periodic("control", Duration::from_millis(100)).with_pubs(vec!["t/out"]),
            ThreadCallbacks::new(|_ctx| {}),
        ).unwrap();
        mgr.register(ThreadConfig::periodic("other", Duration::from_millis(100)), ThreadCallbacks::new(|_ctx| {})).unwrap();

        let c = &mgr.threads["control"].config;
        assert_eq!(c.trigger, Trigger::Periodic(Duration::from_millis(5)));
        assert_eq!(c.priority, Priority::Low);
        assert_eq!(c.cpu_affinity, Some(0));
        assert_eq!(c.subs, vec!["t/in"]);
        assert_eq!(c.pubs, vec!["t/out"]);
        assert_eq!(mgr.threads["other"].config.trigger, Trigger::Periodic(Duration::from_millis(100)));
    }

    #[test]
    fn thread_count_and_names() {
        let mut mgr = ThreadManager::new();

        let config1 = ThreadConfig::periodic("a", Duration::from_millis(10));
        mgr.register(config1, ThreadCallbacks::new(|_| {})).unwrap();

        let config2 = ThreadConfig::periodic("b", Duration::from_millis(10));
        mgr.register(config2, ThreadCallbacks::new(|_| {})).unwrap();

        assert_eq!(mgr.thread_count(), 2);
        let mut names = mgr.thread_names();
        names.sort();
        assert_eq!(names, vec!["a", "b"]);
    }
}
