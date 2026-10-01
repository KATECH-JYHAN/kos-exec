// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;

use serde::Deserialize;

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use std::collections::HashSet;
use std::sync::Arc;

use crate::app_manager::{AppCrashEvent, AppManager, SpawnResources};
use crate::comm::{TopicSubscriber, Transport};
use crate::diag::DiagNotifier;
use crate::app_scheduler::{AppSchedule, AppScheduler, AppTrigger, VehicleState};
use crate::config::{AppConfig, RestartPolicy, RestartStrategy, ScheduleConfig, ThreadConfigToml};
use crate::dependency::DependencyResolver;
use crate::domain::{DomainConfig, DomainController};
use crate::error::{KosError, Result};
use crate::lifecycle::{AppState, Lifecycle};
use crate::task_scheduler::{OverrunPolicy, TaskPriority, TaskScheduler};

#[derive(Deserialize)]
struct TomlRoot {
    domain: Vec<TomlDomain>,
    app: Vec<TomlApp>,
}

#[derive(Deserialize)]
struct TomlDomain {
    id: String,
    asil: String,
    cores: Vec<u32>,
    #[serde(default)]
    rt_priority: Option<u8>,
    #[serde(default)]
    memory_limit_mb: Option<u64>,
    #[serde(default)]
    max_pids: Option<u64>,
    #[serde(default)]
    cpu_quota: Option<u8>,
}

#[derive(Deserialize)]
struct TomlApp {
    id: String,
    binary: String,
    #[serde(default)]
    args: Vec<String>,
    domain: String,
    #[serde(default)]
    depends_on: Vec<String>,
    #[serde(default)]
    priority: Option<String>,
    #[serde(default)]
    schedule: Option<TomlSchedule>,
    #[serde(default)]
    restart: Option<TomlRestart>,
    #[serde(default)]
    params: Option<HashMap<String, toml::Value>>,
    #[serde(default)]
    thread: Option<Vec<TomlThread>>,
}

#[derive(Deserialize)]
struct TomlThread {
    name: String,
    #[serde(default = "default_trigger")]
    trigger: String,
    #[serde(default)]
    period_ms: Option<u32>,
    #[serde(default)]
    event_topic: Option<String>,
    #[serde(default = "default_thread_priority")]
    priority: String,
    #[serde(default)]
    cpu_affinity: Option<u32>,
    #[serde(default)]
    subs: Vec<String>,
    #[serde(default)]
    pubs: Vec<String>,
}

fn default_trigger() -> String {
    "periodic".into()
}
fn default_thread_priority() -> String {
    "normal".into()
}

#[derive(Deserialize)]
struct TomlSchedule {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    period_ms: Option<u32>,
    #[serde(default)]
    trigger: Option<String>,
    #[serde(default)]
    expr: Option<String>,
}

#[derive(Deserialize)]
struct TomlRestart {
    #[serde(default = "default_strategy")]
    strategy: String,
    #[serde(default = "default_max_retries")]
    max_retries: u32,
    backoff_ms: Option<u64>,
    backoff_max_ms: Option<u64>,
    watchdog_ms: Option<u64>,
}

fn default_strategy() -> String {
    "cold".into()
}
fn default_max_retries() -> u32 {
    3
}

fn parse_asil(s: &str) -> Result<kos_safety::AsilLevel> {
    use kos_safety::AsilLevel;
    match s.to_uppercase().as_str() {
        "QM" => Ok(AsilLevel::QM),
        "A" | "ASILA" => Ok(AsilLevel::AsilA),
        "B" | "ASILB" => Ok(AsilLevel::AsilB),
        "C" | "ASILC" => Ok(AsilLevel::AsilC),
        "D" | "ASILD" => Ok(AsilLevel::AsilD),
        _ => Err(KosError::InvalidConfig(format!("unknown ASIL level: {s}"))),
    }
}

fn parse_priority(s: &str) -> TaskPriority {
    TaskPriority::from_str_lossy(s)
}

fn parse_schedule(sched: &Option<TomlSchedule>) -> Result<ScheduleConfig> {
    let Some(s) = sched else {
        return Ok(ScheduleConfig::default());
    };
    match s.kind.as_str() {
        "boot" => Ok(ScheduleConfig::Boot),
        "periodic" => {
            let ms = s
                .period_ms
                .ok_or_else(|| KosError::InvalidConfig("periodic requires period_ms".into()))?;
            Ok(ScheduleConfig::Periodic { period_ms: ms })
        }
        "event" => {
            let trigger = s
                .trigger
                .clone()
                .ok_or_else(|| KosError::InvalidConfig("event requires trigger".into()))?;
            Ok(ScheduleConfig::Event { trigger })
        }
        "cron" => {
            let expr = s
                .expr
                .clone()
                .ok_or_else(|| KosError::InvalidConfig("cron requires expr".into()))?;
            Ok(ScheduleConfig::Cron { expr })
        }
        other => Err(KosError::InvalidConfig(format!(
            "unknown schedule type: {other}"
        ))),
    }
}

fn parse_restart(r: &Option<TomlRestart>) -> RestartPolicy {
    match r {
        Some(r) => {
            let d = RestartPolicy::default();
            RestartPolicy {
                strategy: if r.strategy == "warm" {
                    RestartStrategy::Warm
                } else {
                    RestartStrategy::Cold
                },
                max_retries: r.max_retries,
                backoff_ms: r.backoff_ms.unwrap_or(d.backoff_ms),
                backoff_max_ms: r.backoff_max_ms.unwrap_or(d.backoff_max_ms),
                watchdog_ms: r.watchdog_ms.filter(|ms| *ms > 0),
            }
        }
        None => RestartPolicy::default(),
    }
}

fn flatten_params(tbl: &Option<HashMap<String, toml::Value>>) -> HashMap<String, String> {
    let Some(tbl) = tbl else {
        return HashMap::new();
    };
    tbl.iter()
        .map(|(k, v)| {
            let val = match v {
                toml::Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            (k.clone(), val)
        })
        .collect()
}

fn parse_threads(threads: &Option<Vec<TomlThread>>) -> Vec<ThreadConfigToml> {
    let Some(threads) = threads else {
        return Vec::new();
    };
    threads
        .iter()
        .map(|t| ThreadConfigToml {
            name: t.name.clone(),
            trigger: t.trigger.clone(),
            period_ms: t.period_ms,
            event_topic: t.event_topic.clone(),
            priority: t.priority.clone(),
            cpu_affinity: t.cpu_affinity,
            subs: t.subs.clone(),
            pubs: t.pubs.clone(),
        })
        .collect()
}

fn schedule_to_app_schedule(sched: &ScheduleConfig) -> AppSchedule {
    match sched {
        ScheduleConfig::Boot => AppSchedule::Boot,
        ScheduleConfig::Periodic { period_ms } => AppSchedule::Periodic {
            period_ms: *period_ms,
        },
        ScheduleConfig::Event { trigger } => {
            let app_trigger = parse_trigger(trigger);
            AppSchedule::Event {
                trigger: app_trigger,
            }
        }
        ScheduleConfig::Cron { expr } => AppSchedule::Cron { expr: expr.clone() },
    }
}

fn parse_trigger(s: &str) -> AppTrigger {
    if let Some(name) = s.strip_prefix("signal:") {
        AppTrigger::Signal(name.to_string())
    } else if let Some(topic) = s.strip_prefix("topic:") {
        AppTrigger::Topic(topic.to_string())
    } else if let Some(state) = s.strip_prefix("state:") {
        let vs = match state.to_uppercase().as_str() {
            "PARKED" => VehicleState::Parked,
            "DRIVING" => VehicleState::Driving,
            "CHARGING" => VehicleState::Charging,
            "EMERGENCY" => VehicleState::Emergency,
            _ => VehicleState::Parked,
        };
        AppTrigger::VehicleState(vs)
    } else {
        AppTrigger::Signal(s.to_string())
    }
}

const SUSPEND_GRACE: Duration = Duration::from_millis(1000);

pub struct Launcher {
    pub domains: DomainController,
    pub dependency: DependencyResolver,
    pub app_manager: AppManager,
    pub app_scheduler: AppScheduler,
    pub task_scheduler: TaskScheduler,
    pub lifecycle: Lifecycle,
    app_configs: Vec<AppConfig>,
    _cstate_guard: Option<crate::cgroup::CstateGuard>,
    _irq_guard: Option<crate::cgroup::IrqAffinityGuard>,
    pending_restarts: Vec<(String, Instant)>,
    sched: SchedState,
    diag: Option<Box<dyn DiagNotifier>>,
    health: crate::health_monitor::HealthMonitor,
    heartbeat_seen: HashMap<String, (u32, u64)>,
    auto_restart: bool,
}

#[derive(Default)]
struct SchedState {
    next_periodic: HashMap<String, Instant>,
    cron_fired: HashMap<String, (i32, u32, u32)>,
    disabled: HashSet<String>,
    pending_signals: Vec<String>,
    vehicle_state: VehicleState,
    state_changed: bool,
    event_transport: Option<Arc<dyn Transport>>,
    topic_readers: HashMap<String, Box<dyn TopicSubscriber>>,
}

fn local_time() -> (i32, u32, u32) {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    unsafe { libc::localtime_r(&now, &mut tm) };
    (tm.tm_yday, tm.tm_hour as u32, tm.tm_min as u32)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitOutcome {
    Ignored,
    Completed,
    RestartScheduled { delay: Duration },
    Restarted { pid: u32, restarts: u32 },
    GaveUp { restarts: u32 },
    RestartFailed(String),
    NotRestarted,
}

impl Launcher {
    pub fn from_toml(content: &str) -> Result<Self> {
        let root: TomlRoot =
            toml::from_str(content).map_err(|e| KosError::InvalidConfig(e.to_string()))?;

        let domain_configs: Vec<DomainConfig> = root
            .domain
            .iter()
            .map(|d| {
                Ok(DomainConfig {
                    id: d.id.clone(),
                    asil: parse_asil(&d.asil)?,
                    cores: d.cores.clone(),
                    rt_priority: d.rt_priority,
                    memory_limit_mb: d.memory_limit_mb,
                    max_pids: d.max_pids,
                    cpu_quota: d.cpu_quota,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        let domains = DomainController::from_config(&domain_configs)?;

        let app_configs: Vec<AppConfig> = root
            .app
            .iter()
            .map(|a| {
                Ok(AppConfig {
                    id: a.id.clone(),
                    binary: a.binary.clone(),
                    args: a.args.clone(),
                    domain: a.domain.clone(),
                    depends_on: a.depends_on.clone(),
                    restart: parse_restart(&a.restart),
                    schedule: parse_schedule(&a.schedule)?,
                    priority: a.priority.clone().unwrap_or_else(|| "normal".into()),
                    params: flatten_params(&a.params),
                    threads: parse_threads(&a.thread),
                })
            })
            .collect::<Result<Vec<_>>>()?;

        for app in &app_configs {
            if domains.domain_config(&app.domain).is_err() {
                return Err(KosError::InvalidConfig(format!(
                    "app '{}': unknown domain '{}'",
                    app.id, app.domain
                )));
            }
        }

        let dependency = DependencyResolver::build(&app_configs)?;
        if let Some(cycle) = dependency.check_circular() {
            return Err(KosError::InvalidConfig(format!(
                "circular dependency: {cycle:?}"
            )));
        }

        Ok(Self {
            domains,
            dependency,
            app_manager: AppManager::new(),
            app_scheduler: AppScheduler::new(),
            task_scheduler: TaskScheduler::new(),
            lifecycle: Lifecycle::new(),
            app_configs,
            _cstate_guard: None,
            _irq_guard: None,
            pending_restarts: Vec::new(),
            sched: SchedState::default(),
            diag: None,
            health: crate::health_monitor::HealthMonitor::new(),
            heartbeat_seen: HashMap::new(),
            auto_restart: true,
        }
        .with_registered_apps())
    }

    fn with_registered_apps(mut self) -> Self {
        for app in &self.app_configs {
            let _ = self.domains.assign_app(&app.id, &app.domain);
            let _ = self.lifecycle.register(&app.id);
        }
        self
    }

    pub fn set_auto_restart(&mut self, enabled: bool) {
        self.auto_restart = enabled;
    }

    pub fn set_diag(&mut self, diag: Box<dyn DiagNotifier>) {
        self.diag = Some(diag);
    }

    fn cgroup_needs(&self) -> (bool, bool, bool) {
        let mut needs = (false, false, false);
        for app in &self.app_configs {
            if let Ok(d) = self.domains.domain_config(&app.domain) {
                needs.0 |= d.memory_limit_mb.is_some();
                needs.1 |= d.max_pids.is_some();
                needs.2 |= d.cpu_quota.is_some();
            }
        }
        needs
    }

    fn prepare_cgroup_root(&mut self) {
        let (memory, pids, cpu) = self.cgroup_needs();
        crate::cgroup::ensure_kos_cgroup_root(memory, pids, cpu);
        if let Some(apps) = crate::cgroup::apps_root() {
            self.app_manager.set_cgroup_root(apps);
        }
    }

    pub fn start(&mut self) -> Result<()> {
        let auto: Vec<String> = self
            .app_configs
            .iter()
            .filter(|a| matches!(a.schedule, ScheduleConfig::Boot | ScheduleConfig::Periodic { .. }))
            .map(|a| a.id.clone())
            .collect();
        let all: Vec<String> = self.app_configs.iter().map(|a| a.id.clone()).collect();
        self.prepare_system(&all);
        self.start_selected(&auto)?;
        Ok(())
    }

    pub fn start_selected(&mut self, app_ids: &[String]) -> Result<Vec<String>> {
        let mut selected: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut stack: Vec<String> = app_ids.to_vec();
        while let Some(id) = stack.pop() {
            let cfg = self
                .app_configs
                .iter()
                .find(|a| a.id == id)
                .ok_or_else(|| KosError::NotFound(format!("app {id}")))?;
            if selected.insert(id.clone()) {
                stack.extend(cfg.depends_on.iter().cloned());
            }
        }

        let ordered: Vec<String> = self
            .dependency
            .resolve_order()?
            .into_iter()
            .flatten()
            .filter(|id| selected.contains(id))
            .collect();

        self.prepare_system(&ordered);
        for app_id in &ordered {
            self.start_app(app_id)?;
        }
        Ok(ordered)
    }

    fn prepare_system(&mut self, app_ids: &[String]) {
        let caps = crate::capability::RuntimeCaps::detect();
        caps.print_summary();

        self.prepare_cgroup_root();

        let mut critical_cores: Vec<u32> = Vec::new();
        for app in self.app_configs.iter().filter(|a| app_ids.contains(&a.id)) {
            if let Ok(dc) = self.domains.domain_config(&app.domain) {
                if dc.asil >= kos_safety::AsilLevel::AsilC {
                    critical_cores.extend_from_slice(&dc.cores);
                }
            }
        }
        critical_cores.sort();
        critical_cores.dedup();
        if critical_cores.is_empty() {
            return;
        }

        let total_cores = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) } as u32;
        if self._irq_guard.is_none() {
            self._irq_guard =
                crate::cgroup::isolate_irq_from_cores_guarded(&critical_cores, total_cores);
        }
        if self._cstate_guard.is_none() {
            self._cstate_guard = crate::cgroup::disable_cstates_for_cores(&critical_cores);
        }
    }

    pub fn start_app(&mut self, app_id: &str) -> Result<()> {
        let config = self
            .app_configs
            .iter()
            .find(|a| a.id == app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?
            .clone();

        let _ = self.domains.assign_app(app_id, &config.domain);

        let _ = self.lifecycle.register(app_id);
        if self.lifecycle.get_state(app_id) == Ok(AppState::Terminated) {
            self.lifecycle.reset(app_id)?;
        }

        self.prepare_cgroup_root();

        let resources = self.build_spawn_resources(app_id);
        self.app_manager.spawn(&config, &resources)?;

        self.lifecycle.transition(app_id, AppState::Init)?;
        self.lifecycle.transition(app_id, AppState::Running)?;

        self.sched.disabled.remove(app_id);
        if let ScheduleConfig::Periodic { period_ms } = &config.schedule {
            self.sched
                .next_periodic
                .insert(app_id.to_string(), Instant::now() + Duration::from_millis(*period_ms as u64));
        }

        let app_sched = schedule_to_app_schedule(&config.schedule);
        let _ = self.app_scheduler.register(app_id, app_sched);

        if let ScheduleConfig::Periodic { period_ms } = &config.schedule {
            let prio = parse_priority(&config.priority);
            let _ = self.task_scheduler.register_periodic(
                app_id,
                *period_ms,
                prio,
                OverrunPolicy::Skip,
            );
        }

        Ok(())
    }

    pub fn shutdown(&mut self) -> Result<()> {
        self.pending_restarts.clear();
        let order = self.dependency.resolve_order()?;

        for layer in order.iter().rev() {
            for app_id in layer {
                if self.lifecycle.get_state(app_id).is_ok() {
                    let _ = self.lifecycle.transition(app_id, AppState::Terminated);
                }
                let _ = self.app_manager.kill(app_id);
                let _ = self.task_scheduler.unregister(app_id);
                let _ = self.app_scheduler.unregister(app_id);
            }
        }

        for domain_id in self.domains.domain_ids() {
            crate::cgroup::cleanup_cgroup(domain_id);
        }
        crate::cgroup::cleanup_kos_cgroup_root();

        let removed = crate::shm_inspect::cleanup_stale_topics(&self.known_topics(), false);
        if !removed.is_empty() {
            eprintln!("[kos-exec] removed {} stale SHM topic(s) left by apps", removed.len());
        }

        Ok(())
    }

    fn build_spawn_resources(&self, app_id: &str) -> SpawnResources {
        match self.domains.domain_config_for(app_id) {
            Ok(dc) => SpawnResources {
                cores: dc.cores.clone(),
                domain_id: dc.id.clone(),
                asil: Some(dc.asil),
                rt_priority: dc.rt_priority,
                memory_limit_mb: dc.memory_limit_mb,
                max_pids: dc.max_pids,
                cpu_quota: dc.cpu_quota,
            },
            Err(_) => SpawnResources::default(),
        }
    }

    pub fn stop_app(&mut self, app_id: &str) -> Result<()> {
        self.pending_restarts.retain(|(id, _)| id != app_id);
        self.sched.disabled.insert(app_id.to_string());
        let state = self.lifecycle.get_state(app_id)?;
        if state == AppState::Terminated {
            return Ok(());
        }
        self.lifecycle.transition(app_id, AppState::Terminated)?;
        let _ = self.app_manager.kill(app_id);
        let _ = self.task_scheduler.unregister(app_id);
        let _ = self.app_scheduler.unregister(app_id);
        Ok(())
    }

    pub fn suspend_app(&mut self, app_id: &str) -> Result<()> {
        use nix::sys::signal::Signal;
        if self.lifecycle.get_state(app_id)? != AppState::Running {
            return Err(KosError::InvalidTransition(format!("app {app_id} is not running")));
        }
        self.app_manager.signal(app_id, Signal::SIGTSTP)?;
        let stopped = self.wait_proc_state(app_id, SUSPEND_GRACE, |s| s == Some('T') || s.is_none());
        if !stopped {
            eprintln!("[kos-exec] app '{app_id}' did not stop on SIGTSTP; sending SIGSTOP");
        }
        self.app_manager.signal(app_id, Signal::SIGSTOP)?;
        self.wait_proc_state(app_id, Duration::from_millis(200), |s| s == Some('T') || s.is_none());
        self.lifecycle.transition(app_id, AppState::Suspended)?;
        Ok(())
    }

    pub fn resume_app(&mut self, app_id: &str) -> Result<()> {
        use nix::sys::signal::Signal;
        if self.lifecycle.get_state(app_id)? != AppState::Suspended {
            return Err(KosError::InvalidTransition(format!("app {app_id} is not suspended")));
        }
        self.app_manager.signal(app_id, Signal::SIGCONT)?;
        for _ in 0..5 {
            if self.wait_proc_state(app_id, Duration::from_millis(40), |s| s != Some('T')) {
                break;
            }
            let _ = self.app_manager.signal(app_id, Signal::SIGCONT);
        }
        self.lifecycle.transition(app_id, AppState::Running)?;
        Ok(())
    }

    fn wait_proc_state(&self, app_id: &str, within: Duration, done: impl Fn(Option<char>) -> bool) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if done(self.app_manager.proc_state(app_id)) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    pub fn restart_app(&mut self, app_id: &str) -> Result<()> {
        let config = self
            .app_configs
            .iter()
            .find(|a| a.id == app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?
            .clone();
        self.pending_restarts.retain(|(id, _)| id != app_id);

        let _ = self.app_manager.kill(app_id);
        let _ = self.task_scheduler.unregister(app_id);
        let _ = self.app_scheduler.unregister(app_id);

        let _ = self.lifecycle.transition(app_id, AppState::Terminated);
        let _ = self.lifecycle.reset(app_id);

        let resources = self.build_spawn_resources(app_id);
        self.app_manager.spawn(&config, &resources)?;

        self.lifecycle.transition(app_id, AppState::Init)?;
        self.lifecycle.transition(app_id, AppState::Running)?;

        let app_sched = schedule_to_app_schedule(&config.schedule);
        let _ = self.app_scheduler.register(app_id, app_sched);

        if let ScheduleConfig::Periodic { period_ms } = &config.schedule {
            let prio = parse_priority(&config.priority);
            let _ = self.task_scheduler.register_periodic(
                app_id,
                *period_ms,
                prio,
                OverrunPolicy::Skip,
            );
        }

        Ok(())
    }

    pub fn app_info(&self, app_id: &str) -> Result<AppInfo> {
        let config = self
            .app_configs
            .iter()
            .find(|a| a.id == app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;

        let state = self
            .lifecycle
            .get_state(app_id)
            .unwrap_or(AppState::Installed);

        let pid = self.app_manager.pid_of(app_id).ok().filter(|_| self.app_manager.is_alive(app_id));
        let restarts = self.app_manager.restart_count(app_id).unwrap_or(0);

        let cores = self.domains.cores_for(app_id).ok().map(|c| c.to_vec());

        let schedule_str = match &config.schedule {
            ScheduleConfig::Boot => "boot".to_string(),
            ScheduleConfig::Periodic { period_ms } => format!("periodic/{}ms", period_ms),
            ScheduleConfig::Event { trigger } => format!("event/{trigger}"),
            ScheduleConfig::Cron { expr } => format!("cron/{expr}"),
        };

        let threads: Vec<ThreadInfo> = config
            .threads
            .iter()
            .map(|t| {
                let trigger_str = if t.trigger == "periodic" {
                    t.period_ms
                        .map(|ms| format!("Periodic {}ms", ms))
                        .unwrap_or_else(|| "Periodic".into())
                } else {
                    t.event_topic
                        .as_ref()
                        .map(|e| format!("Event {e}"))
                        .unwrap_or_else(|| "Event".into())
                };
                ThreadInfo {
                    name: t.name.clone(),
                    trigger: trigger_str,
                    priority: t.priority.clone(),
                    cpu_affinity: t.cpu_affinity,
                    subs: t.subs.clone(),
                    pubs: t.pubs.clone(),
                    state: "Running".into(),
                }
            })
            .collect();

        Ok(AppInfo {
            app_id: config.id.clone(),
            domain: config.domain.clone(),
            state,
            pid,
            cores,
            schedule: schedule_str,
            restarts,
            threads,
        })
    }

    pub fn all_app_info(&self) -> Vec<AppInfo> {
        self.app_configs
            .iter()
            .filter_map(|c| self.app_info(&c.id).ok())
            .collect()
    }

    pub fn app_configs(&self) -> &[AppConfig] {
        &self.app_configs
    }

    pub fn known_topics(&self) -> Vec<String> {
        let mut topics: Vec<String> = Vec::new();
        for app in &self.app_configs {
            for t in &app.threads {
                topics.extend(t.subs.iter().cloned());
                topics.extend(t.pubs.iter().cloned());
                topics.extend(t.event_topic.iter().cloned());
            }
            if let ScheduleConfig::Event { trigger } = &app.schedule {
                if let Some(topic) = trigger.strip_prefix("topic:") {
                    topics.push(topic.to_string());
                }
            }
        }
        topics.sort();
        topics.dedup();
        topics
    }

    pub fn raise_signal(&mut self, name: &str) {
        self.sched.pending_signals.push(name.to_string());
    }

    pub fn set_vehicle_state(&mut self, state: VehicleState) {
        if self.sched.vehicle_state != state {
            self.sched.vehicle_state = state;
            self.sched.state_changed = true;
        }
    }

    pub fn vehicle_state(&self) -> VehicleState {
        self.sched.vehicle_state
    }

    pub fn set_event_transport(&mut self, transport: Arc<dyn Transport>) {
        self.sched.event_transport = Some(transport);
        self.sched.topic_readers.clear();
    }

    pub fn run_schedules(&mut self) -> Vec<String> {
        let (yday, hour, minute) = local_time();
        self.run_schedules_at(Instant::now(), yday, hour, minute)
    }

    fn run_schedules_at(&mut self, now: Instant, yday: i32, hour: u32, minute: u32) -> Vec<String> {
        self.ensure_topic_readers();
        let fired_topics = self.poll_topic_triggers();
        let signals = std::mem::take(&mut self.sched.pending_signals);
        let state_changed = std::mem::replace(&mut self.sched.state_changed, false);

        let mut due = Vec::new();
        for app in &self.app_configs {
            if !self.schedulable(&app.id) {
                continue;
            }
            let fire = match &app.schedule {
                ScheduleConfig::Boot => false,
                ScheduleConfig::Periodic { .. } => {
                    self.sched.next_periodic.get(&app.id).is_some_and(|t| *t <= now)
                }
                ScheduleConfig::Cron { expr } => {
                    crate::app_scheduler::cron_matches_hm(expr, hour, minute)
                        && self.sched.cron_fired.get(&app.id) != Some(&(yday, hour, minute))
                }
                ScheduleConfig::Event { trigger } => match parse_trigger(trigger) {
                    AppTrigger::Signal(name) => signals.contains(&name),
                    AppTrigger::Topic(topic) => fired_topics.contains(&topic),
                    AppTrigger::VehicleState(s) => state_changed && self.sched.vehicle_state == s,
                },
            };
            if fire {
                due.push(app.id.clone());
            }
        }

        let mut started = Vec::new();
        for id in due {
            if let Some(ScheduleConfig::Cron { .. }) = self.app_configs.iter().find(|a| a.id == id).map(|a| &a.schedule) {
                self.sched.cron_fired.insert(id.clone(), (yday, hour, minute));
            }
            match self.start_app(&id) {
                Ok(()) => {
                    eprintln!("[kos-exec] app '{id}' started by schedule");
                    started.push(id);
                }
                Err(e) => eprintln!("[kos-exec] app '{id}' scheduled start failed: {e}"),
            }
        }
        started
    }

    fn schedulable(&self, app_id: &str) -> bool {
        if self.sched.disabled.contains(app_id)
            || self.app_manager.is_alive(app_id)
            || self.pending_restarts.iter().any(|(id, _)| id == app_id)
        {
            return false;
        }
        matches!(
            self.lifecycle.get_state(app_id),
            Ok(AppState::Installed) | Ok(AppState::Terminated)
        )
    }

    fn next_periodic_in(&self) -> Option<Duration> {
        let now = Instant::now();
        self.sched
            .next_periodic
            .iter()
            .filter(|(id, _)| self.schedulable(id))
            .map(|(_, t)| t.saturating_duration_since(now))
            .min()
    }

    fn has_topic_triggers(&self) -> bool {
        self.app_configs.iter().any(|a| {
            matches!(&a.schedule, ScheduleConfig::Event { trigger } if trigger.starts_with("topic:"))
        })
    }

    fn ensure_topic_readers(&mut self) {
        let topics: Vec<String> = self
            .app_configs
            .iter()
            .filter_map(|a| match &a.schedule {
                ScheduleConfig::Event { trigger } => match parse_trigger(trigger) {
                    AppTrigger::Topic(t) => Some(t),
                    _ => None,
                },
                _ => None,
            })
            .filter(|t| !self.sched.topic_readers.contains_key(t))
            .collect();
        if topics.is_empty() {
            return;
        }
        let transport = self
            .sched
            .event_transport
            .get_or_insert_with(|| Arc::new(crate::comm::ShmTransport::new("kos-launcher")))
            .clone();
        for topic in topics {
            match transport.subscriber(&topic) {
                Ok(r) => {
                    self.sched.topic_readers.insert(topic, r);
                }
                Err(e) => eprintln!("[kos-exec] topic trigger '{topic}': {e}"),
            }
        }
    }

    fn poll_topic_triggers(&mut self) -> Vec<String> {
        let mut fired = Vec::new();
        for (topic, reader) in self.sched.topic_readers.iter_mut() {
            if let Ok(Some(_)) = reader.try_recv_latest() {
                fired.push(topic.clone());
            }
        }
        fired
    }

    pub fn handle_exit(&mut self, event: &AppCrashEvent) -> ExitOutcome {
        if !self.app_manager.is_current(event) {
            return ExitOutcome::Ignored;
        }
        let app_id = event.app_id.as_str();

        if event.is_clean_exit() {
            let next = match self.app_configs.iter().find(|a| a.id == app_id).map(|a| &a.schedule) {
                Some(ScheduleConfig::Periodic { .. }) => "; runs again next period",
                Some(ScheduleConfig::Event { .. }) => "; runs again on next trigger",
                Some(ScheduleConfig::Cron { .. }) => "; runs again at next matching time",
                _ => "",
            };
            eprintln!("[kos-exec] app '{app_id}' exited normally (pid {}){next}", event.pid);
            let _ = self.lifecycle.transition(app_id, AppState::Terminated);
            let _ = self.app_manager.kill(app_id);
            let _ = self.task_scheduler.unregister(app_id);
            let _ = self.app_scheduler.unregister(app_id);
            return ExitOutcome::Completed;
        }

        let reason = match event.exit_code {
            Some(code) if code < 0 => format!("signal {}", -code),
            Some(code) => format!("exit code {code}"),
            None => "unknown status".to_string(),
        };
        let _ = self.lifecycle.transition(app_id, AppState::Error);

        if !self.auto_restart {
            let err = KosError::Failed(format!("crashed ({reason}), not restarted"));
            eprintln!("[kos-exec] app '{app_id}' crashed ({reason}); auto-restart is off (use --supervise)");
            self.notify_diag(app_id, &err);
            return ExitOutcome::NotRestarted;
        }

        let restarts = self.app_manager.restart_count(app_id).unwrap_or(0);
        let policy = self
            .app_configs
            .iter()
            .find(|a| a.id == app_id)
            .map(|a| a.restart.clone())
            .unwrap_or_default();

        if restarts >= policy.max_retries {
            let err = KosError::PermissionDenied(format!(
                "app {app_id} exceeded max_retries ({})",
                policy.max_retries
            ));
            eprintln!("[kos-exec] app '{app_id}' crashed ({reason}); not restarting: {err}");
            self.notify_diag(app_id, &err);
            return ExitOutcome::GaveUp { restarts };
        }

        let delay = policy.backoff(restarts);
        eprintln!(
            "[kos-exec] app '{app_id}' crashed ({reason}); restarting in {}ms ({}/{})",
            delay.as_millis(),
            restarts + 1,
            policy.max_retries
        );
        self.pending_restarts.retain(|(id, _)| id != app_id);
        self.pending_restarts.push((app_id.to_string(), Instant::now() + delay));
        ExitOutcome::RestartScheduled { delay }
    }

    pub fn run_due_restarts(&mut self) -> Vec<(String, ExitOutcome)> {
        let now = Instant::now();
        let (due, later): (Vec<_>, Vec<_>) =
            self.pending_restarts.drain(..).partition(|(_, at)| *at <= now);
        self.pending_restarts = later;

        let mut results = Vec::new();
        for (app_id, _) in due {
            let outcome = match self.app_manager.respawn_exited(&app_id) {
                Ok(pid) => {
                    let restarts = self.app_manager.restart_count(&app_id).unwrap_or(0);
                    eprintln!("[kos-exec] app '{app_id}' restarted as pid {pid} (restart #{restarts})");
                    self.enter_running_after_restart(&app_id);
                    ExitOutcome::Restarted { pid, restarts }
                }
                Err(KosError::PermissionDenied(msg)) => {
                    let restarts = self.app_manager.restart_count(&app_id).unwrap_or(0);
                    self.notify_diag(&app_id, &KosError::PermissionDenied(msg));
                    ExitOutcome::GaveUp { restarts }
                }
                Err(e) => {
                    eprintln!("[kos-exec] app '{app_id}' restart failed: {e}");
                    self.notify_diag(&app_id, &e);
                    ExitOutcome::RestartFailed(e.to_string())
                }
            };
            results.push((app_id, outcome));
        }
        results
    }

    fn next_restart_in(&self) -> Option<Duration> {
        let now = Instant::now();
        self.pending_restarts
            .iter()
            .map(|(_, at)| at.saturating_duration_since(now))
            .min()
    }

    fn enter_running_after_restart(&mut self, app_id: &str) {
        if self.lifecycle.get_state(app_id) != Ok(AppState::Error) {
            let _ = self.lifecycle.transition(app_id, AppState::Terminated);
            let _ = self.lifecycle.reset(app_id);
        }
        let _ = self.lifecycle.transition(app_id, AppState::Init);
        let _ = self.lifecycle.transition(app_id, AppState::Running);
    }

    pub fn check_watchdogs(&mut self) -> Vec<String> {
        let watched: Vec<(String, u64)> = self
            .app_configs
            .iter()
            .filter_map(|a| a.restart.watchdog_ms.map(|ms| (a.id.clone(), ms)))
            .collect();
        for (app_id, timeout_ms) in &watched {
            let running = self.lifecycle.get_state(app_id) == Ok(AppState::Running)
                && self.app_manager.is_alive(app_id);
            let pid = self.app_manager.pid_of(app_id).ok();
            let count = self.app_manager.heartbeat_count(app_id);
            let (Some(pid), Some(count), true) = (pid, count, running) else {
                self.health.unregister(app_id);
                self.heartbeat_seen.remove(app_id);
                continue;
            };
            let prev = self.heartbeat_seen.insert(app_id.clone(), (pid, count));
            let fresh_process = prev.map(|(p, _)| p) != Some(pid);
            if fresh_process {
                self.health.unregister(app_id);
            }
            if count == 0 {
                continue;
            }
            if !self.health.is_registered(app_id) {
                let config = crate::health_monitor::HealthConfig {
                    heartbeat_interval_ms: (*timeout_ms / 2).clamp(1, u32::MAX as u64) as u32,
                    deadline_ms: None,
                };
                let _ = self.health.register(app_id, &config);
            } else if prev.map(|(_, c)| c) != Some(count) {
                self.health.heartbeat(app_id);
            }
        }

        let mut hung = Vec::new();
        for event in self.health.tick() {
            if let crate::health_monitor::HealthEvent::HeartbeatTimeout(app_id) = event {
                let timeout_ms = watched.iter().find(|(id, _)| *id == app_id).map(|(_, ms)| *ms).unwrap_or(0);
                let err = KosError::Failed(format!("hung: no heartbeat for {timeout_ms}ms, killed"));
                eprintln!("[kos-exec] app '{app_id}' hung (no heartbeat for {timeout_ms}ms); killing");
                self.notify_diag(&app_id, &err);
                let _ = self.app_manager.signal(&app_id, nix::sys::signal::Signal::SIGKILL);
                let _ = self.app_manager.signal(&app_id, nix::sys::signal::Signal::SIGCONT);
                self.health.unregister(&app_id);
                self.heartbeat_seen.remove(&app_id);
                hung.push(app_id);
            }
        }
        hung
    }

    fn notify_diag(&self, app_id: &str, error: &KosError) {
        if let Some(diag) = &self.diag {
            diag.notify_incident(app_id, error);
        }
    }

    pub fn supervise(&mut self, stop: &AtomicBool) {
        self.supervise_with(stop, None);
    }

    pub fn supervise_with(&mut self, stop: &AtomicBool, control: Option<&crate::control::ControlServer>) {
        let rx = self.app_manager.watch();
        let tick = if control.is_some() || self.has_topic_triggers() {
            Duration::from_millis(20)
        } else {
            Duration::from_millis(100)
        };
        while !stop.load(Ordering::Acquire) && !control.is_some_and(|c| c.shutdown_requested()) {
            let wait = [self.next_restart_in(), self.next_periodic_in()]
                .into_iter()
                .flatten()
                .fold(tick, Duration::min);
            if let Ok(event) = rx.recv_timeout(wait) {
                self.handle_exit(&event);
            }
            self.run_due_restarts();
            self.run_schedules();
            self.check_watchdogs();
            if let Some(c) = control {
                c.poll(self);
            }
        }
    }

    pub fn supervise_until_signal(&mut self) -> Result<()> {
        self.supervise_until_signal_with(None)
    }

    pub fn supervise_until_signal_with(&mut self, control: Option<&crate::control::ControlServer>) -> Result<()> {
        use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};

        static STOP: AtomicBool = AtomicBool::new(false);
        extern "C" fn on_signal(_: libc::c_int) {
            STOP.store(true, Ordering::Release);
        }

        STOP.store(false, Ordering::Release);
        let action = SigAction::new(SigHandler::Handler(on_signal), SaFlags::empty(), SigSet::empty());
        for sig in [Signal::SIGINT, Signal::SIGTERM] {
            unsafe { sigaction(sig, &action) }
                .map_err(|e| KosError::InvalidConfig(format!("sigaction {sig}: {e}")))?;
        }

        self.supervise_with(&STOP, control);
        eprintln!("[kos-exec] shutting down");
        self.shutdown()
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThreadInfo {
    pub name: String,
    pub trigger: String,
    pub priority: String,
    pub cpu_affinity: Option<u32>,
    pub subs: Vec<String>,
    pub pubs: Vec<String>,
    pub state: String,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct AppInfo {
    pub app_id: String,
    pub domain: String,
    pub state: AppState,
    pub pid: Option<u32>,
    pub cores: Option<Vec<u32>>,
    pub schedule: String,
    pub restarts: u32,
    pub threads: Vec<ThreadInfo>,
}

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_TOML: &str = r#"
[[domain]]
id = "adas"
asil = "D"
cores = [0, 1]

[[domain]]
id = "ivi"
asil = "QM"
cores = [2, 3]

[[app]]
id = "adas.camera"
binary = "sleep"
args = ["999"]
domain = "adas"
priority = "critical"

[app.schedule]
type = "periodic"
period_ms = 10

[app.restart]
strategy = "cold"
max_retries = 3

[[app]]
id = "adas.lka"
binary = "sleep"
args = ["999"]
domain = "adas"
depends_on = ["adas.camera"]
priority = "critical"

[app.schedule]
type = "periodic"
period_ms = 20

  [app.params]
  steering_gain = "1.2"
  max_angle = "35.0"

[[app]]
id = "ivi.media"
binary = "sleep"
args = ["999"]
domain = "ivi"
priority = "low"

[app.schedule]
type = "event"
trigger = "signal:media_play"
"#;

    #[test]
    fn parse_valid_toml() {
        let launcher = Launcher::from_toml(VALID_TOML).unwrap();
        assert_eq!(launcher.app_configs().len(), 3);

        let lka = launcher
            .app_configs()
            .iter()
            .find(|a| a.id == "adas.lka")
            .unwrap();
        assert_eq!(lka.depends_on, vec!["adas.camera"]);
        assert_eq!(lka.params.get("steering_gain").unwrap(), "1.2");
    }

    #[test]
    fn parse_invalid_toml_rejected() {
        let bad = "this is not valid toml [[[";
        assert!(matches!(
            Launcher::from_toml(bad),
            Err(KosError::InvalidConfig(_))
        ));
    }

    #[test]
    fn start_and_lifecycle() {
        let mut launcher = Launcher::from_toml(VALID_TOML).unwrap();
        launcher.start().unwrap();

        let states = (
            launcher.lifecycle.get_state("adas.camera").unwrap(),
            launcher.lifecycle.get_state("adas.lka").unwrap(),
            launcher.lifecycle.get_state("ivi.media").unwrap(),
        );
        launcher.shutdown().unwrap();
        assert_eq!(states, (AppState::Running, AppState::Running, AppState::Installed));
    }

    #[test]
    fn dependency_order_respected() {
        let launcher = Launcher::from_toml(VALID_TOML).unwrap();
        let order = launcher.dependency.resolve_order().unwrap();

        let first_layer = &order[0];
        assert!(first_layer.contains(&"adas.camera".to_string()));

        let has_lka_after_camera = order
            .iter()
            .position(|l| l.contains(&"adas.lka".to_string()))
            > order
                .iter()
                .position(|l| l.contains(&"adas.camera".to_string()));
        assert!(has_lka_after_camera);
    }

    #[test]
    fn shutdown_reverse_order() {
        let mut launcher = Launcher::from_toml(VALID_TOML).unwrap();
        launcher.start().unwrap();
        launcher.shutdown().unwrap();

        assert_eq!(
            launcher.lifecycle.get_state("adas.camera").unwrap(),
            AppState::Terminated,
        );
        assert_eq!(
            launcher.lifecycle.get_state("adas.lka").unwrap(),
            AppState::Terminated,
        );
    }

    #[test]
    fn parse_thread_configs() {
        let toml = r#"
[[domain]]
id = "adas"
asil = "D"
cores = [0, 1]

[[app]]
id = "adas.fusion"
binary = "sleep"
args = ["999"]
domain = "adas"

[app.schedule]
type = "periodic"
period_ms = 10

[[app.thread]]
name = "fusion"
trigger = "periodic"
period_ms = 10
priority = "critical"
cpu_affinity = 0
subs = ["lidar", "radar"]
pubs = ["control_cmd"]

[[app.thread]]
name = "diag"
trigger = "event"
event_topic = "diag_req"
priority = "low"
subs = ["diag_req"]
pubs = ["diag_resp"]
"#;
        let launcher = Launcher::from_toml(toml).unwrap();
        let app = &launcher.app_configs()[0];
        assert_eq!(app.threads.len(), 2);

        let fusion = &app.threads[0];
        assert_eq!(fusion.name, "fusion");
        assert_eq!(fusion.trigger, "periodic");
        assert_eq!(fusion.period_ms, Some(10));
        assert_eq!(fusion.priority, "critical");
        assert_eq!(fusion.cpu_affinity, Some(0));
        assert_eq!(fusion.subs, vec!["lidar", "radar"]);
        assert_eq!(fusion.pubs, vec!["control_cmd"]);

        let diag = &app.threads[1];
        assert_eq!(diag.name, "diag");
        assert_eq!(diag.trigger, "event");
        assert_eq!(diag.event_topic, Some("diag_req".to_string()));
        assert_eq!(diag.priority, "low");
        assert_eq!(diag.cpu_affinity, None);
    }

    #[test]
    fn app_without_threads_has_empty_vec() {
        let launcher = Launcher::from_toml(VALID_TOML).unwrap();
        for app in launcher.app_configs() {
            assert!(app.threads.is_empty());
        }
    }

    #[test]
    fn thread_defaults() {
        let toml = r#"
[[domain]]
id = "d"
asil = "QM"
cores = [0]

[[app]]
id = "a"
binary = "sleep"
domain = "d"

[app.schedule]
type = "periodic"
period_ms = 10

[[app.thread]]
name = "worker"
subs = ["input"]
"#;
        let launcher = Launcher::from_toml(toml).unwrap();
        let t = &launcher.app_configs()[0].threads[0];
        assert_eq!(t.trigger, "periodic");
        assert_eq!(t.priority, "normal");
        assert_eq!(t.period_ms, None);
        assert_eq!(t.cpu_affinity, None);
        assert!(t.pubs.is_empty());
    }

    #[test]
    fn circular_dependency_rejected() {
        let toml = r#"
[[domain]]
id = "d"
asil = "QM"
cores = [0]

[[app]]
id = "a"
binary = "sleep"
domain = "d"
depends_on = ["b"]

[app.schedule]
type = "periodic"
period_ms = 10

[[app]]
id = "b"
binary = "sleep"
domain = "d"
depends_on = ["a"]

[app.schedule]
type = "periodic"
period_ms = 10
"#;
        assert!(matches!(
            Launcher::from_toml(toml),
            Err(KosError::InvalidConfig(_))
        ));
    }

    #[test]
    fn cgroup_needs_uses_domain_of_each_app_before_start() {
        let toml = r#"
[[domain]]
id = "limited"
asil = "QM"
cores = [0]
memory_limit_mb = 64
cpu_quota = 50

[[domain]]
id = "free"
asil = "QM"
cores = [0]

[[app]]
id = "a"
binary = "sleep"
domain = "limited"
"#;
        let launcher = Launcher::from_toml(toml).unwrap();
        assert_eq!(launcher.cgroup_needs(), (true, false, true));
    }

    #[test]
    fn cgroup_needs_ignores_domains_without_apps() {
        let toml = r#"
[[domain]]
id = "limited"
asil = "QM"
cores = [0]
max_pids = 10

[[domain]]
id = "free"
asil = "QM"
cores = [0]

[[app]]
id = "a"
binary = "sleep"
domain = "free"
"#;
        let launcher = Launcher::from_toml(toml).unwrap();
        assert_eq!(launcher.cgroup_needs(), (false, false, false));
    }

    fn supervise_toml(script: &str, max_retries: u32) -> String {
        supervise_toml_backoff(script, max_retries, 10, 20)
    }

    fn supervise_toml_backoff(script: &str, max_retries: u32, backoff: u64, backoff_max: u64) -> String {
        format!(
            r#"
[[domain]]
id = "test"
asil = "QM"
cores = [0]

[[app]]
id = "t.app"
binary = "sh"
args = ["-c", "{script}"]
domain = "test"

[app.restart]
max_retries = {max_retries}
backoff_ms = {backoff}
backoff_max_ms = {backoff_max}
"#
        )
    }

    fn next_outcome(launcher: &mut Launcher) -> ExitOutcome {
        let rx = launcher.app_manager.watch();
        let ev = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("no exit event within 5s");
        launcher.handle_exit(&ev)
    }

    fn run_scheduled(launcher: &mut Launcher) -> ExitOutcome {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut done = launcher.run_due_restarts();
            if let Some((_, outcome)) = done.pop() {
                return outcome;
            }
            assert!(Instant::now() < deadline, "scheduled restart did not run");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn crashed_app_is_restarted_until_max_retries() {
        let mut l = Launcher::from_toml(&supervise_toml("exit 3", 2)).unwrap();
        l.start_app("t.app").unwrap();

        assert!(matches!(next_outcome(&mut l), ExitOutcome::RestartScheduled { .. }));
        assert_eq!(l.lifecycle.get_state("t.app").unwrap(), AppState::Error);
        assert!(matches!(run_scheduled(&mut l), ExitOutcome::Restarted { restarts: 1, .. }));
        assert_eq!(l.lifecycle.get_state("t.app").unwrap(), AppState::Running);

        assert!(matches!(next_outcome(&mut l), ExitOutcome::RestartScheduled { .. }));
        assert!(matches!(run_scheduled(&mut l), ExitOutcome::Restarted { restarts: 2, .. }));

        assert_eq!(next_outcome(&mut l), ExitOutcome::GaveUp { restarts: 2 });
        assert_eq!(l.lifecycle.get_state("t.app").unwrap(), AppState::Error);
    }

    #[test]
    fn restart_backoff_doubles_up_to_max() {
        let mut l = Launcher::from_toml(&supervise_toml_backoff("exit 1", 5, 100, 250)).unwrap();
        l.start_app("t.app").unwrap();
        assert_eq!(
            next_outcome(&mut l),
            ExitOutcome::RestartScheduled { delay: Duration::from_millis(100) }
        );

        let policy = RestartPolicy { backoff_ms: 100, backoff_max_ms: 250, ..Default::default() };
        assert_eq!(policy.backoff(0), Duration::from_millis(100));
        assert_eq!(policy.backoff(1), Duration::from_millis(200));
        assert_eq!(policy.backoff(2), Duration::from_millis(250));
        assert_eq!(policy.backoff(40), Duration::from_millis(250));
        l.shutdown().unwrap();
    }

    #[test]
    fn restart_is_not_run_before_backoff() {
        let mut l = Launcher::from_toml(&supervise_toml_backoff("exit 1", 3, 300, 300)).unwrap();
        l.start_app("t.app").unwrap();
        assert!(matches!(next_outcome(&mut l), ExitOutcome::RestartScheduled { .. }));
        assert!(l.run_due_restarts().is_empty());
        assert!(!l.app_manager.is_alive("t.app"));
        assert!(matches!(run_scheduled(&mut l), ExitOutcome::Restarted { .. }));
        l.shutdown().unwrap();
    }

    #[test]
    fn stop_cancels_pending_restart() {
        let mut l = Launcher::from_toml(&supervise_toml_backoff("exit 1", 3, 50, 50)).unwrap();
        l.start_app("t.app").unwrap();
        assert!(matches!(next_outcome(&mut l), ExitOutcome::RestartScheduled { .. }));
        l.stop_app("t.app").unwrap();
        std::thread::sleep(Duration::from_millis(80));
        assert!(l.run_due_restarts().is_empty());
        assert!(l.app_manager.pid_of("t.app").is_err());
    }

    #[test]
    fn give_up_notifies_diag() {
        use crate::diag::CountingDiag;
        use std::sync::Arc;

        struct Shared(Arc<CountingDiag>);
        impl DiagNotifier for Shared {
            fn notify_incident(&self, app_id: &str, error: &KosError) {
                self.0.notify_incident(app_id, error);
            }
        }

        let diag = Arc::new(CountingDiag::new());
        let mut l = Launcher::from_toml(&supervise_toml("exit 1", 0)).unwrap();
        l.set_diag(Box::new(Shared(diag.clone())));
        l.start_app("t.app").unwrap();

        assert_eq!(next_outcome(&mut l), ExitOutcome::GaveUp { restarts: 0 });
        assert_eq!(diag.incident_count(), 1);
    }

    #[test]
    fn crash_is_not_restarted_when_auto_restart_is_off() {
        let mut l = Launcher::from_toml(&supervise_toml("exit 3", 3)).unwrap();
        l.set_auto_restart(false);
        l.start_app("t.app").unwrap();
        assert_eq!(next_outcome(&mut l), ExitOutcome::NotRestarted);
        assert_eq!(l.lifecycle.get_state("t.app"), Ok(AppState::Error));
        assert!(l.run_due_restarts().is_empty());
        l.shutdown().unwrap();
    }

    #[test]
    fn clean_exit_is_not_restarted() {
        let mut l = Launcher::from_toml(&supervise_toml("exit 0", 3)).unwrap();
        l.start_app("t.app").unwrap();

        assert_eq!(next_outcome(&mut l), ExitOutcome::Completed);
        assert_eq!(l.lifecycle.get_state("t.app").unwrap(), AppState::Terminated);
        assert!(l.app_manager.pid_of("t.app").is_err());
    }

    #[test]
    fn signal_kill_is_treated_as_crash() {
        let mut l = Launcher::from_toml(&supervise_toml("sleep 30", 1)).unwrap();
        l.start_app("t.app").unwrap();
        let pid = l.app_manager.pid_of("t.app").unwrap();
        unsafe { libc::kill(pid as i32, libc::SIGKILL) };

        assert!(matches!(next_outcome(&mut l), ExitOutcome::RestartScheduled { .. }));
        assert!(matches!(run_scheduled(&mut l), ExitOutcome::Restarted { restarts: 1, .. }));
        assert_ne!(l.app_manager.pid_of("t.app").unwrap(), pid);
        l.shutdown().unwrap();
    }

    #[test]
    fn intentional_stop_is_ignored() {
        let mut l = Launcher::from_toml(&supervise_toml("sleep 30", 3)).unwrap();
        l.start_app("t.app").unwrap();
        l.stop_app("t.app").unwrap();

        assert_eq!(next_outcome(&mut l), ExitOutcome::Ignored);
        assert_eq!(l.lifecycle.get_state("t.app").unwrap(), AppState::Terminated);
    }

    #[test]
    fn manual_restart_old_pid_event_is_ignored() {
        let mut l = Launcher::from_toml(&supervise_toml("sleep 30", 3)).unwrap();
        l.start_app("t.app").unwrap();
        l.restart_app("t.app").unwrap();

        assert_eq!(next_outcome(&mut l), ExitOutcome::Ignored);
        assert!(l.app_manager.is_alive("t.app"));
        l.shutdown().unwrap();
    }

    #[test]
    fn supervise_stops_on_flag() {
        let mut l = Launcher::from_toml(&supervise_toml("exit 2", 5)).unwrap();
        l.start_app("t.app").unwrap();
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(800));
                stop.store(true, Ordering::Release);
            });
            l.supervise(&stop);
        });
        assert_eq!(l.app_manager.restart_count("t.app").unwrap(), 5);
        assert_eq!(l.lifecycle.get_state("t.app").unwrap(), AppState::Error);
    }

    #[test]
    fn unknown_domain_is_rejected() {
        let toml = r#"
[[domain]]
id = "adas"
asil = "D"
cores = [0]

[[app]]
id = "a"
binary = "sleep"
domain = "adsa"
"#;
        match Launcher::from_toml(toml) {
            Err(KosError::InvalidConfig(msg)) => assert!(msg.contains("adsa"), "{msg}"),
            other => panic!("expected InvalidConfig, got {:?}", other.map(|_| ())),
        }
    }

    #[test]
    fn start_selected_includes_dependencies_in_order() {
        let toml = r#"
[[domain]]
id = "d"
asil = "QM"
cores = [0]

[[app]]
id = "base"
binary = "sleep"
args = ["30"]
domain = "d"

[[app]]
id = "mid"
binary = "sleep"
args = ["30"]
domain = "d"
depends_on = ["base"]

[[app]]
id = "top"
binary = "sleep"
args = ["30"]
domain = "d"
depends_on = ["mid"]

[[app]]
id = "other"
binary = "sleep"
args = ["30"]
domain = "d"
"#;
        let mut l = Launcher::from_toml(toml).unwrap();
        let started = l.start_selected(&["top".to_string()]).unwrap();
        assert_eq!(started, vec!["base", "mid", "top"]);
        assert!(l.app_manager.is_alive("base"));
        assert!(l.app_manager.pid_of("other").is_err());
        l.shutdown().unwrap();
    }

    #[test]
    fn spawned_app_receives_launch_env() {
        let out = std::env::temp_dir().join(format!("kos_launch_env_{}.txt", std::process::id()));
        let toml = format!(
            r#"
[[domain]]
id = "adas"
asil = "QM"
cores = [0]

[[app]]
id = "adas.fusion"
binary = "sh"
args = ["-c", "env | grep ^KOS_ > {out}"]
domain = "adas"

[app.params]
gain = 0.5
mode = "fast"

[[app.thread]]
name = "control"
period_ms = 5
priority = "critical"
"#,
            out = out.display()
        );
        let mut l = Launcher::from_toml(&toml).unwrap();
        l.start_app("adas.fusion").unwrap();
        let rx = l.app_manager.watch();
        rx.recv_timeout(Duration::from_secs(5)).expect("app did not exit");

        let text = std::fs::read_to_string(&out).unwrap();
        let _ = std::fs::remove_file(&out);
        let env = crate::launch_env::LaunchEnv::from_vars(text.lines().filter_map(|l| {
            l.split_once('=').map(|(k, v)| (k.to_string(), v.to_string()))
        }));
        assert_eq!(env.app_id.as_deref(), Some("adas.fusion"));
        assert_eq!(env.domain.as_deref(), Some("adas"));
        assert_eq!(env.params.get("gain").map(String::as_str), Some("0.5"));
        assert_eq!(env.params.get("mode").map(String::as_str), Some("fast"));
        assert!(text.contains("KOS_APP_THREADS="), "{text}");
    }

    fn sched_toml(schedule: &str, script: &str) -> String {
        format!(
            r#"
[[domain]]
id = "d"
asil = "QM"
cores = [0]

[[app]]
id = "job"
binary = "sh"
args = ["-c", "{script}"]
domain = "d"

{schedule}
"#
        )
    }

    fn count_lines(path: &std::path::Path) -> usize {
        std::fs::read_to_string(path).map(|s| s.lines().count()).unwrap_or(0)
    }

    fn supervise_for(l: &mut Launcher, ms: u64) {
        let stop = AtomicBool::new(false);
        std::thread::scope(|scope| {
            scope.spawn(|| {
                std::thread::sleep(Duration::from_millis(ms));
                stop.store(true, Ordering::Release);
            });
            l.supervise(&stop);
        });
    }

    #[test]
    fn periodic_job_runs_again_each_period() {
        let out = std::env::temp_dir().join(format!("kos_periodic_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let toml = sched_toml(
            "[app.schedule]\ntype = \"periodic\"\nperiod_ms = 50",
            &format!("echo run >> {}", out.display()),
        );
        let mut l = Launcher::from_toml(&toml).unwrap();
        l.start().unwrap();
        supervise_for(&mut l, 420);
        l.shutdown().unwrap();
        let runs = count_lines(&out);
        let _ = std::fs::remove_file(&out);
        assert!((5..=10).contains(&runs), "expected ~8 runs in 420ms at 50ms period, got {runs}");
    }

    #[test]
    fn stopped_periodic_job_is_not_rerun_until_started() {
        let out = std::env::temp_dir().join(format!("kos_periodic_stop_{}.txt", std::process::id()));
        let _ = std::fs::remove_file(&out);
        let toml = sched_toml(
            "[app.schedule]\ntype = \"periodic\"\nperiod_ms = 30",
            &format!("echo run >> {}", out.display()),
        );
        let mut l = Launcher::from_toml(&toml).unwrap();
        l.start().unwrap();
        l.stop_app("job").unwrap();
        supervise_for(&mut l, 200);
        let runs_while_stopped = count_lines(&out);
        l.start_app("job").unwrap();
        supervise_for(&mut l, 200);
        l.shutdown().unwrap();
        let runs_after = count_lines(&out);
        let _ = std::fs::remove_file(&out);
        assert!(runs_while_stopped <= 1, "ran {runs_while_stopped} times while stopped");
        assert!(runs_after >= runs_while_stopped + 3, "{runs_while_stopped} -> {runs_after}");
    }

    #[test]
    fn boot_app_clean_exit_is_not_rerun() {
        let mut l = Launcher::from_toml(&sched_toml("", "exit 0")).unwrap();
        l.start().unwrap();
        supervise_for(&mut l, 150);
        assert_eq!(l.lifecycle.get_state("job").unwrap(), AppState::Terminated);
        assert!(l.run_schedules().is_empty());
    }

    #[test]
    fn event_signal_starts_app_once_per_trigger() {
        let toml = sched_toml("[app.schedule]\ntype = \"event\"\ntrigger = \"signal:go\"", "sleep 30");
        let mut l = Launcher::from_toml(&toml).unwrap();
        l.start().unwrap();
        assert_eq!(l.lifecycle.get_state("job").unwrap(), AppState::Installed);
        assert!(l.run_schedules().is_empty());

        l.raise_signal("other");
        assert!(l.run_schedules().is_empty());
        l.raise_signal("go");
        assert_eq!(l.run_schedules(), vec!["job"]);
        l.raise_signal("go");
        assert!(l.run_schedules().is_empty());
        l.shutdown().unwrap();
    }

    #[test]
    fn vehicle_state_trigger_fires_on_change() {
        let toml = sched_toml("[app.schedule]\ntype = \"event\"\ntrigger = \"state:DRIVING\"", "exit 0");
        let mut l = Launcher::from_toml(&toml).unwrap();
        l.start().unwrap();
        l.set_vehicle_state(VehicleState::Charging);
        assert!(l.run_schedules().is_empty());
        l.set_vehicle_state(VehicleState::Driving);
        assert_eq!(l.run_schedules(), vec!["job"]);
        supervise_for(&mut l, 100);
        l.set_vehicle_state(VehicleState::Driving);
        assert!(l.run_schedules().is_empty());
        l.set_vehicle_state(VehicleState::Parked);
        l.set_vehicle_state(VehicleState::Driving);
        assert_eq!(l.run_schedules(), vec!["job"]);
        l.shutdown().unwrap();
    }

    #[test]
    fn cron_fires_once_per_matching_minute() {
        let toml = sched_toml("[app.schedule]\ntype = \"cron\"\nexpr = \"30 3 * * *\"", "exit 0");
        let mut l = Launcher::from_toml(&toml).unwrap();
        l.start().unwrap();
        let now = Instant::now();
        assert!(l.run_schedules_at(now, 10, 3, 29).is_empty());
        assert_eq!(l.run_schedules_at(now, 10, 3, 30), vec!["job"]);
        supervise_for(&mut l, 100);
        assert!(l.run_schedules_at(now, 10, 3, 30).is_empty(), "same minute must not fire twice");
        assert_eq!(l.run_schedules_at(now, 11, 3, 30), vec!["job"], "next day fires again");
        l.shutdown().unwrap();
    }

    #[test]
    fn topic_trigger_starts_app_on_new_data() {
        use crate::comm::{MockTransport, Transport};
        let toml = sched_toml("[app.schedule]\ntype = \"event\"\ntrigger = \"topic:t/alarm\"", "sleep 30");
        let transport = Arc::new(MockTransport::new());
        let mut l = Launcher::from_toml(&toml).unwrap();
        l.set_event_transport(transport.clone());
        l.start().unwrap();
        assert!(l.run_schedules().is_empty());
        Transport::publisher(transport.as_ref(), "t/alarm").unwrap().publish(b"!").unwrap();
        assert_eq!(l.run_schedules(), vec!["job"]);
        l.shutdown().unwrap();
    }

    #[test]
    #[ignore]
    fn helper_app_for_watchdog() {
        let Some(mode) = crate::launch_env::LaunchEnv::current().params.get("helper").cloned() else {
            return;
        };
        if mode == "lifecycle" {
            struct Recorder(std::path::PathBuf);
            impl Recorder {
                fn note(&self, what: &str) {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.0).unwrap();
                    writeln!(f, "{what}").unwrap();
                }
            }
            impl crate::traits::KosApp for Recorder {
                fn on_init(&mut self, _: &mut crate::context::AppContext) -> Result<()> {
                    self.note("init");
                    Ok(())
                }
                fn on_run(&mut self, _: &mut crate::context::AppContext) -> Result<()> {
                    std::thread::sleep(Duration::from_millis(5));
                    Ok(())
                }
                fn on_suspend(&mut self, _: &mut crate::context::AppContext) -> Result<()> {
                    self.note("suspend");
                    Ok(())
                }
                fn on_resume(&mut self, _: &mut crate::context::AppContext) -> Result<()> {
                    self.note("resume");
                    Ok(())
                }
            }
            let log = crate::launch_env::LaunchEnv::current().params.get("log").cloned().unwrap();
            let _ = crate::runtime::run(Recorder(log.into()), crate::RuntimeConfig::from_env("helper"));
            return;
        }
        if mode.starts_with("node") {
            use crate::thread_manager::{ThreadCallbacks, ThreadConfig, ThreadManager};
            let stuck = mode == "node_hang";
            let started = Instant::now();
            let mut tm = ThreadManager::new();
            tm.register(
                ThreadConfig::periodic("worker", Duration::from_millis(20)),
                ThreadCallbacks::new(move |_| {
                    if stuck && started.elapsed() > Duration::from_millis(300) {
                        std::thread::sleep(Duration::from_secs(60));
                    }
                }),
            )
            .unwrap();
            tm.register(ThreadConfig::event("idle", "never/published"), ThreadCallbacks::new(|_| {})).unwrap();
            tm.start_all().unwrap();
            std::thread::sleep(Duration::from_secs(60));
            return;
        }
        let started = Instant::now();
        loop {
            if mode == "hang" && started.elapsed() > Duration::from_millis(300) {
                std::thread::sleep(Duration::from_secs(60));
            }
            crate::heartbeat::beat();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn watchdog_toml(helper: Option<&str>, watchdog_ms: u64) -> String {
        let (binary, args, params) = match helper {
            Some(mode) => (
                std::env::current_exe().unwrap().display().to_string(),
                r#"["--exact", "launch::tests::helper_app_for_watchdog", "--ignored", "--nocapture", "--test-threads=1"]"#.to_string(),
                format!("[app.params]\nhelper = \"{mode}\"\n"),
            ),
            None => ("sleep".to_string(), r#"["30"]"#.to_string(), String::new()),
        };
        format!(
            r#"
[[domain]]
id = "test"
asil = "QM"
cores = [0]

[[app]]
id = "t.wd"
binary = "{binary}"
args = {args}
domain = "test"

[app.restart]
max_retries = 0
watchdog_ms = {watchdog_ms}

{params}"#
        )
    }

    fn watch_for_hang(l: &mut Launcher, ms: u64) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        let mut hung = Vec::new();
        while Instant::now() < deadline && hung.is_empty() {
            hung = l.check_watchdogs();
            std::thread::sleep(Duration::from_millis(20));
        }
        hung
    }

    #[test]
    fn watchdog_kills_app_that_stops_beating() {
        let mut l = Launcher::from_toml(&watchdog_toml(Some("hang"), 200)).unwrap();
        l.start_app("t.wd").unwrap();
        let hung = watch_for_hang(&mut l, 5000);
        assert_eq!(hung, vec!["t.wd".to_string()]);
        let ev = l.app_manager.watch().recv_timeout(Duration::from_secs(5)).expect("exit after kill");
        assert_eq!(ev.exit_code, Some(-(libc::SIGKILL)));
        l.shutdown().unwrap();
    }

    fn wait_for_lines(path: &std::path::Path, want: &[&str]) -> Vec<String> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let lines: Vec<String> =
                std::fs::read_to_string(path).unwrap_or_default().lines().map(str::to_string).collect();
            if lines == want || Instant::now() >= deadline {
                return lines;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn suspend_and_resume_invoke_app_callbacks() {
        let log = std::env::temp_dir().join(format!("kos_lifecycle_{}.log", std::process::id()));
        let _ = std::fs::remove_file(&log);
        let toml = watchdog_toml(Some("lifecycle"), 0).replace("watchdog_ms = 0", "")
            + &format!("log = \"{}\"\n", log.display());
        let mut l = Launcher::from_toml(&toml).unwrap();
        l.start_app("t.wd").unwrap();
        assert_eq!(wait_for_lines(&log, &["init"]), vec!["init"]);

        l.suspend_app("t.wd").unwrap();
        assert_eq!(l.app_manager.proc_state("t.wd"), Some('T'));
        assert_eq!(wait_for_lines(&log, &["init", "suspend"]), vec!["init", "suspend"]);

        l.resume_app("t.wd").unwrap();
        assert_eq!(wait_for_lines(&log, &["init", "suspend", "resume"]), vec!["init", "suspend", "resume"]);
        assert_ne!(l.app_manager.proc_state("t.wd"), Some('T'));
        l.shutdown().unwrap();
        let _ = std::fs::remove_file(&log);
    }

    #[test]
    fn suspend_falls_back_to_sigstop_for_apps_ignoring_sigtstp() {
        let mut l = Launcher::from_toml(&supervise_toml("trap '' TSTP; sleep 30 & wait", 0)).unwrap();
        l.start_app("t.app").unwrap();
        std::thread::sleep(Duration::from_millis(100));
        l.suspend_app("t.app").unwrap();
        assert_eq!(l.app_manager.proc_state("t.app"), Some('T'));
        l.resume_app("t.app").unwrap();
        assert_ne!(l.app_manager.proc_state("t.app"), Some('T'));
        l.shutdown().unwrap();
    }

    #[test]
    fn watchdog_detects_one_stuck_thread_in_node() {
        let mut l = Launcher::from_toml(&watchdog_toml(Some("node_hang"), 200)).unwrap();
        l.start_app("t.wd").unwrap();
        assert_eq!(watch_for_hang(&mut l, 5000), vec!["t.wd".to_string()]);
        l.shutdown().unwrap();
    }

    #[test]
    fn watchdog_leaves_healthy_node_alone() {
        let mut l = Launcher::from_toml(&watchdog_toml(Some("node_ok"), 200)).unwrap();
        l.start_app("t.wd").unwrap();
        assert!(watch_for_hang(&mut l, 1200).is_empty());
        assert!(l.app_manager.heartbeat_count("t.wd").unwrap() > 0, "node should beat");
        l.shutdown().unwrap();
    }

    #[test]
    fn watchdog_leaves_beating_app_alone() {
        let mut l = Launcher::from_toml(&watchdog_toml(Some("beat"), 200)).unwrap();
        l.start_app("t.wd").unwrap();
        assert!(watch_for_hang(&mut l, 1200).is_empty());
        assert!(l.app_manager.heartbeat_count("t.wd").unwrap() > 0, "helper should beat");
        assert!(l.app_manager.is_alive("t.wd"));
        l.shutdown().unwrap();
    }

    #[test]
    fn watchdog_ignores_app_without_heartbeat_support() {
        let mut l = Launcher::from_toml(&watchdog_toml(None, 100)).unwrap();
        l.start_app("t.wd").unwrap();
        assert!(watch_for_hang(&mut l, 600).is_empty());
        assert!(l.app_manager.is_alive("t.wd"));
        l.shutdown().unwrap();
    }

    #[test]
    fn watchdog_pauses_while_suspended() {
        let mut l = Launcher::from_toml(&watchdog_toml(Some("beat"), 150)).unwrap();
        l.start_app("t.wd").unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while l.app_manager.heartbeat_count("t.wd").unwrap_or(0) == 0 && Instant::now() < deadline {
            l.check_watchdogs();
            std::thread::sleep(Duration::from_millis(10));
        }
        l.suspend_app("t.wd").unwrap();
        assert!(watch_for_hang(&mut l, 600).is_empty());
        l.resume_app("t.wd").unwrap();
        assert!(watch_for_hang(&mut l, 600).is_empty());
        assert!(l.app_manager.is_alive("t.wd"));
        l.shutdown().unwrap();
    }
}
