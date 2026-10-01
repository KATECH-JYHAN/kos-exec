// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;
use std::time::{Duration, Instant};

use nix::sched::{sched_setaffinity, CpuSet};
use nix::unistd::Pid;

pub struct ResourceConfig {
    pub watch_threshold: f64,
    pub yellow_threshold: f64,
    pub red_threshold: f64,
    pub return_threshold: f64,

    pub watch_hold: Duration,
    pub yellow_hold: Duration,
    pub return_hold: Duration,

    pub sampling_interval: Duration,

    pub min_migration_interval: Duration,
    pub warmup_grace_periods: u32,
    pub min_slack_pct: f64,

    pub critical_cores: Vec<usize>,
    pub nc_cores: Vec<usize>,
    pub critical_min: usize,
    pub nc_min: usize,
}

impl Default for ResourceConfig {
    fn default() -> Self {
        Self {
            watch_threshold: 70.0,
            yellow_threshold: 75.0,
            red_threshold: 80.0,
            return_threshold: 60.0,
            watch_hold: Duration::from_secs(2),
            yellow_hold: Duration::from_secs(3),
            return_hold: Duration::from_secs(5),
            sampling_interval: Duration::from_millis(100),
            min_migration_interval: Duration::from_secs(10),
            warmup_grace_periods: 2,
            min_slack_pct: 30.0,
            critical_cores: vec![0, 1, 2, 3],
            nc_cores: vec![4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
            critical_min: 4,
            nc_min: 4,
        }
    }
}

pub struct ProcessState {
    pub pid: u32,
    pub app_id: String,
    pub asil: AsilLevel,
    pub assigned_core: usize,
    pub expected_utilization: f64,
    pub movable: bool,

    prev_ticks: u64,
    prev_sample_time: Instant,
    pub current_utilization: f64,
    pub baseline_utilization: f64,
    utilization_history: Vec<f64>,
    pub peak_utilization: f64,

    pub last_migration: Option<Instant>,
    pub warmup_remaining: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum AsilLevel {
    QM = 0,
    A = 1,
    B = 2,
    C = 3,
    D = 4,
}

pub struct CoreState {
    pub core_id: usize,
    pub processes: Vec<u32>,
    pub total_utilization: f64,
    pub max_processes: usize,
    pub alert: AlertLevel,
}

pub enum AlertLevel {
    Normal,
    Watch { since: Instant },
    Yellow { since: Instant },
    Red,
}

#[derive(Debug)]
pub enum ResourceEvent {
    ProcessMigrated {
        pid: u32,
        app_id: String,
        from_core: usize,
        to_core: usize,
        reason: String,
    },
    CoreWarning {
        core_id: usize,
        utilization: f64,
    },
    CoreCritical {
        core_id: usize,
        utilization: f64,
    },
    DegradedSignal {
        pid: u32,
        app_id: String,
    },
    CoreRecovered {
        core_id: usize,
        utilization: f64,
    },
    UtilizationSpike {
        pid: u32,
        app_id: String,
        from_pct: f64,
        to_pct: f64,
    },
}

pub struct ResourceManager {
    config: ResourceConfig,
    processes: HashMap<u32, ProcessState>,
    cores: HashMap<usize, CoreState>,
    borrowed_cores: Vec<usize>,
    last_sample: Instant,
    clk_tck: u64,
}

impl ResourceManager {
    pub fn new(config: ResourceConfig) -> Self {
        let mut cores = HashMap::new();
        for &core_id in &config.critical_cores {
            cores.insert(core_id, CoreState {
                core_id,
                processes: Vec::new(),
                total_utilization: 0.0,
                max_processes: 5,
                alert: AlertLevel::Normal,
            });
        }
        for &core_id in &config.nc_cores {
            cores.insert(core_id, CoreState {
                core_id,
                processes: Vec::new(),
                total_utilization: 0.0,
                max_processes: 10,
                alert: AlertLevel::Normal,
            });
        }

        let clk_tck = unsafe { libc::sysconf(libc::_SC_CLK_TCK) as u64 };

        ResourceManager {
            config,
            processes: HashMap::new(),
            cores,
            borrowed_cores: Vec::new(),
            last_sample: Instant::now(),
            clk_tck,
        }
    }

    pub fn register_process(
        &mut self,
        pid: u32,
        app_id: String,
        asil: AsilLevel,
        core: usize,
        expected_utilization: f64,
    ) {
        let ticks = read_process_ticks(pid).unwrap_or(0);
        let movable = expected_utilization < (100.0 - self.config.min_slack_pct);

        let state = ProcessState {
            pid,
            app_id,
            asil,
            assigned_core: core,
            expected_utilization,
            movable,
            prev_ticks: ticks,
            prev_sample_time: Instant::now(),
            current_utilization: 0.0,
            baseline_utilization: expected_utilization,
            utilization_history: Vec::with_capacity(64),
            peak_utilization: expected_utilization,
            last_migration: None,
            warmup_remaining: 0,
        };

        if let Some(core_state) = self.cores.get_mut(&core) {
            core_state.processes.push(pid);
        }

        self.processes.insert(pid, state);
    }

    pub fn unregister_process(&mut self, pid: u32) {
        if let Some(state) = self.processes.remove(&pid) {
            if let Some(core_state) = self.cores.get_mut(&state.assigned_core) {
                core_state.processes.retain(|&p| p != pid);
            }
        }
    }

    pub fn find_best_core(&self, expected_util: f64, asil: AsilLevel) -> Option<usize> {
        let target_cores = if asil >= AsilLevel::C {
            &self.config.critical_cores
        } else {
            &self.config.nc_cores
        };

        let mut best_core = None;
        let mut best_score = f64::MIN;

        for &core_id in target_cores {
            if let Some(core_state) = self.cores.get(&core_id) {
                let util_headroom = self.config.yellow_threshold - core_state.total_utilization - expected_util;
                if util_headroom < 0.0 {
                    continue;
                }
                let proc_headroom = (core_state.max_processes - core_state.processes.len()) as f64;
                let score = util_headroom * 0.7 + proc_headroom * 0.3;

                if score > best_score {
                    best_score = score;
                    best_core = Some(core_id);
                }
            }
        }

        best_core
    }

    pub fn validate_capacity(&self, processes: &[(f64, AsilLevel)]) -> Result<(), String> {
        let critical_capacity = self.config.critical_cores.len() as f64 * self.config.yellow_threshold;
        let nc_capacity = self.config.nc_cores.len() as f64 * self.config.yellow_threshold;

        let critical_demand: f64 = processes.iter()
            .filter(|(_, asil)| *asil >= AsilLevel::C)
            .map(|(util, _)| util)
            .sum();
        let nc_demand: f64 = processes.iter()
            .filter(|(_, asil)| *asil < AsilLevel::C)
            .map(|(util, _)| util)
            .sum();

        if critical_demand > critical_capacity {
            return Err(format!(
                "Critical domain over capacity: {:.0}% requested, {:.0}% available ({} cores × {:.0}%)",
                critical_demand, critical_capacity,
                self.config.critical_cores.len(), self.config.yellow_threshold
            ));
        }
        if nc_demand > nc_capacity {
            return Err(format!(
                "NC domain over capacity: {:.0}% requested, {:.0}% available ({} cores × {:.0}%)",
                nc_demand, nc_capacity,
                self.config.nc_cores.len(), self.config.yellow_threshold
            ));
        }
        Ok(())
    }

    pub fn tick(&mut self) -> Vec<ResourceEvent> {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last_sample);
        if elapsed < self.config.sampling_interval {
            return Vec::new();
        }
        self.last_sample = now;

        let mut events = Vec::new();

        self.update_process_utilization(elapsed, &mut events);

        self.update_core_utilization();

        self.evaluate_alerts(&mut events);

        self.check_core_return(&mut events);

        events
    }

    fn update_process_utilization(&mut self, elapsed: Duration, events: &mut Vec<ResourceEvent>) {
        let elapsed_secs = elapsed.as_secs_f64();
        if elapsed_secs <= 0.0 {
            return;
        }

        let pids: Vec<u32> = self.processes.keys().copied().collect();
        for pid in pids {
            let new_ticks = read_process_ticks(pid).unwrap_or(0);

            if let Some(state) = self.processes.get_mut(&pid) {
                if state.warmup_remaining > 0 {
                    state.warmup_remaining -= 1;
                }

                let tick_delta = new_ticks.saturating_sub(state.prev_ticks);
                let cpu_secs = tick_delta as f64 / self.clk_tck as f64;
                let utilization = (cpu_secs / elapsed_secs) * 100.0;

                let prev_util = state.current_utilization;
                state.current_utilization = utilization;
                state.prev_ticks = new_ticks;
                state.prev_sample_time = Instant::now();

                if state.utilization_history.len() >= 64 {
                    state.utilization_history.remove(0);
                }
                state.utilization_history.push(utilization);

                if utilization > state.peak_utilization {
                    state.peak_utilization = utilization;
                }

                if state.utilization_history.len() >= 10 {
                    let sum: f64 = state.utilization_history.iter().sum();
                    state.baseline_utilization = sum / state.utilization_history.len() as f64;
                }

                state.movable = state.current_utilization < (100.0 - self.config.min_slack_pct);

                if prev_util > 0.0
                    && utilization > state.baseline_utilization * 2.0
                    && utilization > 30.0
                    && state.warmup_remaining == 0
                {
                    events.push(ResourceEvent::UtilizationSpike {
                        pid: state.pid,
                        app_id: state.app_id.clone(),
                        from_pct: prev_util,
                        to_pct: utilization,
                    });
                }
            }
        }
    }

    fn update_core_utilization(&mut self) {
        for core_state in self.cores.values_mut() {
            core_state.total_utilization = 0.0;
        }

        for state in self.processes.values() {
            if let Some(core_state) = self.cores.get_mut(&state.assigned_core) {
                core_state.total_utilization += state.current_utilization;
            }
        }
    }

    fn evaluate_alerts(&mut self, events: &mut Vec<ResourceEvent>) {
        let now = Instant::now();
        let critical_cores: Vec<usize> = self.config.critical_cores.clone();

        for &core_id in &critical_cores {
            let (util, should_act) = {
                let core = match self.cores.get(&core_id) {
                    Some(c) => c,
                    None => continue,
                };
                let util = core.total_utilization;

                let new_alert = if util >= self.config.red_threshold {
                    AlertLevel::Red
                } else if util >= self.config.yellow_threshold {
                    match &core.alert {
                        AlertLevel::Yellow { since } => {
                            AlertLevel::Yellow { since: *since }
                        }
                        _ => AlertLevel::Yellow { since: now },
                    }
                } else if util >= self.config.watch_threshold {
                    match &core.alert {
                        AlertLevel::Watch { since } => {
                            AlertLevel::Watch { since: *since }
                        }
                        _ => AlertLevel::Watch { since: now },
                    }
                } else {
                    if !matches!(core.alert, AlertLevel::Normal) {
                        events.push(ResourceEvent::CoreRecovered {
                            core_id,
                            utilization: util,
                        });
                    }
                    AlertLevel::Normal
                };

                let should_act = match &new_alert {
                    AlertLevel::Red => true,
                    AlertLevel::Yellow { since } => {
                        now.duration_since(*since) >= self.config.yellow_hold
                    }
                    _ => false,
                };

                (util, should_act)
            };

            if let Some(core) = self.cores.get_mut(&core_id) {
                let new_alert = if util >= self.config.red_threshold {
                    events.push(ResourceEvent::CoreCritical { core_id, utilization: util });
                    AlertLevel::Red
                } else if util >= self.config.yellow_threshold {
                    match &core.alert {
                        AlertLevel::Yellow { since } => AlertLevel::Yellow { since: *since },
                        _ => {
                            events.push(ResourceEvent::CoreWarning { core_id, utilization: util });
                            AlertLevel::Yellow { since: now }
                        }
                    }
                } else if util >= self.config.watch_threshold {
                    match &core.alert {
                        AlertLevel::Watch { since } => AlertLevel::Watch { since: *since },
                        _ => AlertLevel::Watch { since: now },
                    }
                } else {
                    AlertLevel::Normal
                };
                core.alert = new_alert;
            }

            if should_act {
                self.respond_to_overload(core_id, events);
            }
        }
    }

    fn respond_to_overload(&mut self, core_id: usize, events: &mut Vec<ResourceEvent>) {
        let candidate = {
            let core = match self.cores.get(&core_id) {
                Some(c) => c,
                None => return,
            };

            let mut candidates: Vec<(u32, AsilLevel, f64)> = Vec::new();
            for &pid in &core.processes {
                if let Some(state) = self.processes.get(&pid) {
                    if !state.movable {
                        continue;
                    }
                    if let Some(last) = state.last_migration {
                        if last.elapsed() < self.config.min_migration_interval {
                            continue;
                        }
                    }
                    candidates.push((pid, state.asil, state.current_utilization));
                }
            }

            candidates.sort_by(|a, b| {
                a.1.cmp(&b.1)
                    .then(b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal))
            });

            candidates.first().map(|&(pid, _, _)| pid)
        };

        if let Some(pid) = candidate {
            let target = self.find_least_loaded_core(core_id);
            if let Some(target_core) = target {
                self.migrate_process(pid, target_core, events);
            } else {
                self.send_degraded_signal(pid, events);
            }
        }
    }

    fn find_least_loaded_core(&self, exclude_core: usize) -> Option<usize> {
        let mut best = None;
        let mut best_util = f64::MAX;

        let all_critical: Vec<usize> = self.config.critical_cores.iter()
            .chain(self.borrowed_cores.iter())
            .copied()
            .collect();

        for &core_id in &all_critical {
            if core_id == exclude_core {
                continue;
            }
            if let Some(core) = self.cores.get(&core_id) {
                if core.total_utilization < best_util
                    && core.total_utilization < self.config.yellow_threshold
                {
                    best_util = core.total_utilization;
                    best = Some(core_id);
                }
            }
        }

        if best.is_none() {
            best = self.try_borrow_core();
        }

        best
    }

    fn try_borrow_core(&self) -> Option<usize> {
        let nc_available = self.config.nc_cores.len() - self.borrowed_cores.len();
        if nc_available <= self.config.nc_min {
            return None;
        }

        let mut best = None;
        let mut best_util = f64::MAX;

        for &core_id in &self.config.nc_cores {
            if self.borrowed_cores.contains(&core_id) {
                continue;
            }
            if let Some(core) = self.cores.get(&core_id) {
                if core.total_utilization < best_util {
                    best_util = core.total_utilization;
                    best = Some(core_id);
                }
            }
        }

        best
    }

    fn migrate_process(&mut self, pid: u32, target_core: usize, events: &mut Vec<ResourceEvent>) {
        let (from_core, app_id) = match self.processes.get(&pid) {
            Some(state) => (state.assigned_core, state.app_id.clone()),
            None => return,
        };

        let deadline = Instant::now() + Duration::from_millis(20);
        loop {
            if is_process_sleeping(pid) {
                break;
            }
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_micros(100));
        }

        let mut cpuset = CpuSet::new();
        let _ = cpuset.set(target_core);
        let result = sched_setaffinity(Pid::from_raw(pid as i32), &cpuset);

        if result.is_ok() {
            if let Some(core_state) = self.cores.get_mut(&from_core) {
                core_state.processes.retain(|&p| p != pid);
            }
            if let Some(core_state) = self.cores.get_mut(&target_core) {
                core_state.processes.push(pid);
            }
            if let Some(state) = self.processes.get_mut(&pid) {
                state.assigned_core = target_core;
                state.last_migration = Some(Instant::now());
                state.warmup_remaining = self.config.warmup_grace_periods;
            }

            if self.config.nc_cores.contains(&target_core)
                && !self.borrowed_cores.contains(&target_core)
            {
                self.borrowed_cores.push(target_core);
            }

            events.push(ResourceEvent::ProcessMigrated {
                pid,
                app_id,
                from_core,
                to_core: target_core,
                reason: format!(
                    "core {} utilization {:.1}% exceeded",
                    from_core,
                    self.cores.get(&from_core).map(|c| c.total_utilization).unwrap_or(0.0)
                ),
            });
        }
    }

    fn send_degraded_signal(&mut self, pid: u32, events: &mut Vec<ResourceEvent>) {
        let app_id = self.processes.get(&pid)
            .map(|s| s.app_id.clone())
            .unwrap_or_default();

        unsafe {
            libc::kill(pid as i32, libc::SIGUSR1);
        }

        events.push(ResourceEvent::DegradedSignal { pid, app_id });
    }

    fn check_core_return(&mut self, events: &mut Vec<ResourceEvent>) {
        if self.borrowed_cores.is_empty() {
            return;
        }

        let all_low = self.config.critical_cores.iter().all(|&core_id| {
            self.cores.get(&core_id)
                .map(|c| c.total_utilization <= self.config.return_threshold)
                .unwrap_or(true)
        });

        if !all_low {
            return;
        }

        let mut to_return = Vec::new();
        for &core_id in &self.borrowed_cores {
            if let Some(core) = self.cores.get(&core_id) {
                if core.processes.is_empty() {
                    to_return.push(core_id);
                }
            }
        }

        for core_id in to_return {
            self.borrowed_cores.retain(|&c| c != core_id);
            events.push(ResourceEvent::CoreRecovered {
                core_id,
                utilization: 0.0,
            });
        }
    }

    pub fn snapshot(&self) -> ResourceSnapshot {
        let core_states: Vec<CoreSnapshot> = self.cores.values().map(|c| {
            CoreSnapshot {
                core_id: c.core_id,
                utilization: c.total_utilization,
                process_count: c.processes.len(),
                alert: match &c.alert {
                    AlertLevel::Normal => "normal",
                    AlertLevel::Watch { .. } => "watch",
                    AlertLevel::Yellow { .. } => "yellow",
                    AlertLevel::Red => "red",
                },
            }
        }).collect();

        let process_states: Vec<ProcessSnapshot> = self.processes.values().map(|p| {
            ProcessSnapshot {
                pid: p.pid,
                app_id: p.app_id.clone(),
                core: p.assigned_core,
                utilization: p.current_utilization,
                baseline: p.baseline_utilization,
                peak: p.peak_utilization,
                movable: p.movable,
            }
        }).collect();

        ResourceSnapshot {
            cores: core_states,
            processes: process_states,
            borrowed_cores: self.borrowed_cores.clone(),
        }
    }

    pub fn process_utilization(&self, pid: u32) -> Option<f64> {
        self.processes.get(&pid).map(|s| s.current_utilization)
    }

    pub fn core_utilization(&self, core_id: usize) -> Option<f64> {
        self.cores.get(&core_id).map(|c| c.total_utilization)
    }

    pub fn config(&self) -> &ResourceConfig {
        &self.config
    }

    pub fn process_app_id(&self, pid: u32) -> Option<&str> {
        self.processes.get(&pid).map(|s| s.app_id.as_str())
    }

    pub fn core_process_count(&self, core_id: usize) -> usize {
        self.cores.get(&core_id).map(|c| c.processes.len()).unwrap_or(0)
    }

    pub fn set_simulated_load(&mut self, pid: u32, utilization: f64) {
        if let Some(state) = self.processes.get_mut(&pid) {
            state.current_utilization = utilization;
        }
        self.update_core_utilization();
    }
}

pub struct ResourceSnapshot {
    pub cores: Vec<CoreSnapshot>,
    pub processes: Vec<ProcessSnapshot>,
    pub borrowed_cores: Vec<usize>,
}

pub struct CoreSnapshot {
    pub core_id: usize,
    pub utilization: f64,
    pub process_count: usize,
    pub alert: &'static str,
}

pub struct ProcessSnapshot {
    pub pid: u32,
    pub app_id: String,
    pub core: usize,
    pub utilization: f64,
    pub baseline: f64,
    pub peak: f64,
    pub movable: bool,
}

fn read_process_ticks(pid: u32) -> Option<u64> {
    let path = format!("/proc/{}/stat", pid);
    let content = std::fs::read_to_string(&path).ok()?;

    let after_comm = content.rfind(')')? + 2;
    let fields: Vec<&str> = content[after_comm..].split_whitespace().collect();

    if fields.len() < 13 {
        return None;
    }
    let utime: u64 = fields[11].parse().ok()?;
    let stime: u64 = fields[12].parse().ok()?;

    Some(utime + stime)
}

fn is_process_sleeping(pid: u32) -> bool {
    let path = format!("/proc/{}/stat", pid);
    if let Ok(content) = std::fs::read_to_string(&path) {
        if let Some(pos) = content.rfind(')') {
            let after = &content[pos + 2..];
            return after.starts_with('S');
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_self_ticks() {
        let mut x = 0u64;
        for i in 0..1_000_000 { x = x.wrapping_add(i); }
        std::hint::black_box(x);

        let pid = std::process::id();
        let ticks = read_process_ticks(pid);
        assert!(ticks.is_some());
    }

    #[test]
    fn test_is_sleeping() {
        let pid = std::process::id();
        let _ = is_process_sleeping(pid);
    }

    #[test]
    fn test_find_best_core() {
        let config = ResourceConfig {
            critical_cores: vec![0, 1, 2, 3],
            nc_cores: vec![4, 5],
            ..Default::default()
        };
        let rm = ResourceManager::new(config);

        let core = rm.find_best_core(20.0, AsilLevel::D);
        assert!(core.is_some());
        assert!(core.unwrap() <= 3);

        let core = rm.find_best_core(10.0, AsilLevel::QM);
        assert!(core.is_some());
        assert!(core.unwrap() >= 4);
    }

    #[test]
    fn test_validate_capacity() {
        let config = ResourceConfig {
            critical_cores: vec![0, 1],
            nc_cores: vec![2, 3],
            ..Default::default()
        };
        let rm = ResourceManager::new(config);

        let processes = vec![(60.0, AsilLevel::D), (60.0, AsilLevel::D)];
        assert!(rm.validate_capacity(&processes).is_ok());

        let processes = vec![(80.0, AsilLevel::D), (80.0, AsilLevel::D)];
        assert!(rm.validate_capacity(&processes).is_err());
    }

    #[test]
    fn test_register_unregister() {
        let config = ResourceConfig {
            critical_cores: vec![0, 1],
            nc_cores: vec![2, 3],
            ..Default::default()
        };
        let mut rm = ResourceManager::new(config);

        let pid = std::process::id();
        rm.register_process(pid, "test_app".to_string(), AsilLevel::D, 0, 20.0);

        assert!(rm.processes.contains_key(&pid));
        assert!(rm.cores.get(&0).unwrap().processes.contains(&pid));

        rm.unregister_process(pid);
        assert!(!rm.processes.contains_key(&pid));
        assert!(!rm.cores.get(&0).unwrap().processes.contains(&pid));
    }
}
