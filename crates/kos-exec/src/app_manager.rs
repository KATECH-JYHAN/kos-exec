// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use nix::sys::signal::{self, Signal};
use nix::sys::wait::{waitpid, WaitStatus};
use nix::unistd::Pid;

use crate::config::AppConfig;
use crate::error::{KosError, Result};
use crate::thread_manager::ThreadState;

#[derive(Debug, Clone, Default)]
pub struct SpawnResources {
    pub cores: Vec<u32>,
    pub domain_id: String,
    pub asil: Option<kos_safety::AsilLevel>,
    pub rt_priority: Option<u8>,
    pub memory_limit_mb: Option<u64>,
    pub max_pids: Option<u64>,
    pub cpu_quota: Option<u8>,
}

#[derive(Debug, Clone)]
pub struct AppCrashEvent {
    pub app_id: String,
    pub pid: u32,
    pub exit_code: Option<i32>,
}

impl AppCrashEvent {
    pub fn is_clean_exit(&self) -> bool {
        self.exit_code == Some(0)
    }
}

#[derive(Debug, Clone)]
pub struct ShmLifecycle {
    pub advertised_topics: Vec<String>,
    pub subscribed_topics: Vec<String>,
    pub active: bool,
    pub preserve_on_crash: bool,
}

impl Default for ShmLifecycle {
    fn default() -> Self {
        Self {
            advertised_topics: Vec::new(),
            subscribed_topics: Vec::new(),
            active: false,
            preserve_on_crash: true,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ThreadStatusReport {
    pub app_id: String,
    pub thread_name: String,
    pub state: ThreadState,
    pub failure_count: u32,
}

struct AppProcess {
    pid: u32,
    exited: Arc<AtomicBool>,
    config: AppConfig,
    restart_count: u32,
    resources: SpawnResources,
    shm: ShmLifecycle,
    thread_failures: HashMap<String, u32>,
    max_thread_failures: u32,
    heartbeat: Option<crate::heartbeat::HeartbeatCell>,
    cgroup: Option<std::path::PathBuf>,
}

pub struct AppManager {
    apps: HashMap<String, AppProcess>,
    kill_with_parent: bool,
    reaper: Option<crate::reaper::Reaper>,
    cgroup_root: Option<std::path::PathBuf>,
    instance: u32,
    crash_tx: Sender<AppCrashEvent>,
    crash_rx: Receiver<AppCrashEvent>,
}

static NEXT_INSTANCE: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

const CGROUP_KILL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

fn signal_app(proc: &AppProcess, sig: Signal) {
    let _ = signal::killpg(Pid::from_raw(proc.pid as i32), sig);
    signal_escaped(proc, sig);
}

fn signal_escaped(proc: &AppProcess, sig: Signal) {
    let Some(dir) = &proc.cgroup else {
        return;
    };
    for pid in crate::cgroup::cgroup_pids(dir) {
        if unsafe { libc::getpgid(pid) } != proc.pid as i32 {
            unsafe { libc::kill(pid, sig as libc::c_int) };
        }
    }
}

impl AppManager {
    pub fn new() -> Self {
        let (crash_tx, crash_rx) = crossbeam_channel::unbounded();
        Self {
            apps: HashMap::new(),
            kill_with_parent: false,
            reaper: None,
            cgroup_root: None,
            instance: NEXT_INSTANCE.fetch_add(1, Ordering::Relaxed),
            crash_tx,
            crash_rx,
        }
    }

    pub fn set_kill_with_parent(&mut self, enabled: bool) {
        self.kill_with_parent = enabled;
        if enabled && self.reaper.is_none() {
            match crate::reaper::Reaper::start() {
                Ok(reaper) => {
                    for proc in self.apps.values().filter(|p| p.pid != 0) {
                        reaper.track(proc.pid);
                    }
                    if let Some(root) = &self.cgroup_root {
                        reaper.set_cgroup(root);
                    }
                    self.reaper = Some(reaper);
                }
                Err(e) => eprintln!("[kos-exec] WARNING: cannot start reaper process: {e}"),
            }
        }
    }

    pub fn set_cgroup_root(&mut self, root: std::path::PathBuf) {
        if let Some(reaper) = &self.reaper {
            reaper.set_cgroup(&root);
        }
        self.cgroup_root = Some(root);
    }

    fn untrack(&self, pid: u32) {
        if let (Some(reaper), true) = (&self.reaper, pid != 0) {
            reaper.untrack(pid);
        }
    }

    pub fn spawn(&mut self, config: &AppConfig, resources: &SpawnResources) -> Result<u32> {
        if self.apps.contains_key(&config.id) {
            return Err(KosError::AlreadyExists(format!("app {}", config.id)));
        }

        let binary_path = resolve_binary(&config.binary, &config.domain);
        verify_binary(&binary_path)?;
        let mut command = Command::new(&binary_path);
        command.args(&config.args).envs(crate::launch_env::LaunchEnv::vars_for(config));
        std::os::unix::process::CommandExt::process_group(&mut command, 0);
        let cgroup_leaf = match self.instance {
            0 => config.id.clone(),
            n => format!("{}.{n}", config.id),
        };
        let cgroup = crate::cgroup::prepare_app_cgroup(
            &resources.domain_id,
            &cgroup_leaf,
            resources.memory_limit_mb,
            resources.max_pids,
            resources.cpu_quota,
        );
        if let Some(dir) = &cgroup {
            let procs = std::ffi::CString::new(dir.join("cgroup.procs").into_os_string().into_encoded_bytes())
                .map_err(|_| KosError::InvalidConfig(format!("cgroup path {}", dir.display())))?;
            unsafe {
                std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                    let fd = libc::open(procs.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC);
                    if fd >= 0 {
                        libc::write(fd, b"0".as_ptr().cast(), 1);
                        libc::close(fd);
                    }
                    Ok(())
                });
            }
        }
        if self.kill_with_parent {
            let parent = std::process::id() as libc::pid_t;
            unsafe {
                std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::getppid() != parent {
                        libc::raise(libc::SIGTERM);
                    }
                    Ok(())
                });
            }
        }
        let heartbeat = match config.restart.watchdog_ms {
            Some(ms) => match crate::heartbeat::HeartbeatCell::create() {
                Ok((cell, fd)) => {
                    command.env(crate::heartbeat::ENV_HEARTBEAT, crate::heartbeat::env_value(&fd, ms));
                    let raw = std::os::fd::AsRawFd::as_raw_fd(&fd);
                    unsafe {
                        std::os::unix::process::CommandExt::pre_exec(&mut command, move || {
                            crate::heartbeat::inherit_in_child(raw)
                        });
                    }
                    Some((cell, fd))
                }
                Err(e) => {
                    eprintln!("[kos-exec] WARNING: app '{}': heartbeat unavailable: {e}", config.id);
                    None
                }
            },
            None => None,
        };
        let child = command
            .spawn()
            .map_err(|e| KosError::NotFound(format!("binary {}: {e}", config.binary)))?;

        let pid = child.id();

        if !resources.cores.is_empty() {
            let _ = set_affinity(pid, &resources.cores);
        }

        if let Some(prio) = resources.rt_priority {
            set_rt_priority(pid, prio);
        }

        if let Some(asil) = resources.asil {
            if asil < kos_safety::AsilLevel::AsilC && resources.rt_priority.is_none() {
                set_nice(pid, 19);
            }
        }

        if let Some(dir) = &cgroup {
            if !crate::cgroup::contains_pid(dir, pid) {
                if let Err(e) = crate::cgroup::add_pid(dir, pid) {
                    eprintln!(
                        "[kos-exec] WARNING: app '{}': failed to add pid {pid} to cgroup {}: {e}",
                        config.id,
                        dir.display()
                    );
                }
            }
        }

        let app_id = config.id.clone();
        let tx = self.crash_tx.clone();
        let exited = Arc::new(AtomicBool::new(false));
        let exited_w = exited.clone();
        std::thread::spawn(move || {
            let nix_pid = Pid::from_raw(pid as i32);
            let status = waitpid(nix_pid, None);
            exited_w.store(true, Ordering::Release);
            match status {
                Ok(WaitStatus::Exited(_, code)) => {
                    let _ = tx.send(AppCrashEvent {
                        app_id,
                        pid,
                        exit_code: Some(code),
                    });
                }
                Ok(WaitStatus::Signaled(_, sig, _)) => {
                    let _ = tx.send(AppCrashEvent {
                        app_id,
                        pid,
                        exit_code: Some(-(sig as i32)),
                    });
                }
                _ => {
                    let _ = tx.send(AppCrashEvent {
                        app_id,
                        pid,
                        exit_code: None,
                    });
                }
            }
        });

        if let Some(reaper) = &self.reaper {
            reaper.track(pid);
        }
        self.apps.insert(
            config.id.clone(),
            AppProcess {
                pid,
                exited,
                config: config.clone(),
                restart_count: 0,
                resources: resources.clone(),
                shm: ShmLifecycle::default(),
                thread_failures: HashMap::new(),
                max_thread_failures: 3,
                heartbeat: heartbeat.map(|(cell, _fd)| cell),
                cgroup,
            },
        );

        Ok(pid)
    }

    pub fn kill(&mut self, app_id: &str) -> Result<()> {
        let proc = self
            .apps
            .get(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;

        let pgid = Pid::from_raw(proc.pid as i32);
        if !proc.exited.load(Ordering::Acquire) {
            signal_app(proc, Signal::SIGTERM);
            signal_app(proc, Signal::SIGCONT);

            let deadline = std::time::Instant::now() + std::time::Duration::from_millis(100);
            while !proc.exited.load(Ordering::Acquire) && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
        }
        let _ = signal::killpg(pgid, Signal::SIGKILL);
        if let Some(dir) = &proc.cgroup {
            if !crate::cgroup::kill_cgroup(dir, CGROUP_KILL_TIMEOUT) {
                eprintln!("[kos-exec] WARNING: app '{app_id}': processes left in cgroup {}", dir.display());
            }
            crate::cgroup::remove_cgroup(dir);
        }

        let pid = proc.pid;
        self.apps.remove(app_id);
        self.untrack(pid);
        Ok(())
    }

    pub fn restart(&mut self, app_id: &str) -> Result<u32> {
        let proc = self
            .apps
            .get(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;

        if proc.restart_count >= proc.config.restart.max_retries {
            return Err(KosError::PermissionDenied(format!(
                "app {app_id} exceeded max_retries ({})",
                proc.config.restart.max_retries
            )));
        }

        let config = proc.config.clone();
        let resources = proc.resources.clone();
        let prev_restart_count = proc.restart_count;

        let binary_path = resolve_binary(&config.binary, &config.domain);
        verify_binary(&binary_path)?;

        self.kill(app_id)?;

        let new_pid = self.spawn(&config, &resources)?;

        if let Some(proc) = self.apps.get_mut(app_id) {
            proc.restart_count = prev_restart_count + 1;
        }

        Ok(new_pid)
    }

    pub fn respawn_exited(&mut self, app_id: &str) -> Result<u32> {
        let proc = self
            .apps
            .get(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;

        if !proc.exited.load(Ordering::Acquire) {
            return Err(KosError::InvalidTransition(format!(
                "app {app_id} is still running"
            )));
        }
        if proc.restart_count >= proc.config.restart.max_retries {
            return Err(KosError::PermissionDenied(format!(
                "app {app_id} exceeded max_retries ({})",
                proc.config.restart.max_retries
            )));
        }

        let config = proc.config.clone();
        let resources = proc.resources.clone();
        let prev_restart_count = proc.restart_count;

        let binary_path = resolve_binary(&config.binary, &config.domain);
        verify_binary(&binary_path)?;

        let old_pid = proc.pid;
        let _ = signal::killpg(Pid::from_raw(old_pid as i32), Signal::SIGKILL);
        if let Some(dir) = &proc.cgroup {
            let _ = crate::cgroup::kill_cgroup(dir, CGROUP_KILL_TIMEOUT);
        }
        self.apps.remove(app_id);
        self.untrack(old_pid);
        let new_pid = match self.spawn(&config, &resources) {
            Ok(pid) => pid,
            Err(e) => {
                self.apps.insert(
                    app_id.to_string(),
                    AppProcess {
                        pid: 0,
                        exited: Arc::new(AtomicBool::new(true)),
                        config,
                        restart_count: prev_restart_count + 1,
                        resources,
                        shm: ShmLifecycle::default(),
                        thread_failures: HashMap::new(),
                        max_thread_failures: 3,
                        heartbeat: None,
                        cgroup: None,
                    },
                );
                return Err(e);
            }
        };

        if let Some(proc) = self.apps.get_mut(app_id) {
            proc.restart_count = prev_restart_count + 1;
        }
        Ok(new_pid)
    }

    pub fn is_current(&self, event: &AppCrashEvent) -> bool {
        self.apps
            .get(&event.app_id)
            .is_some_and(|p| p.pid == event.pid)
    }

    pub fn signal(&self, app_id: &str, sig: Signal) -> Result<()> {
        let proc = self
            .apps
            .get(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;
        if proc.exited.load(Ordering::Acquire) {
            return Err(KosError::InvalidTransition(format!("app {app_id} is not running")));
        }
        signal::killpg(Pid::from_raw(proc.pid as i32), sig)
            .map_err(|e| KosError::InvalidConfig(format!("signal {sig} to {app_id}: {e}")))?;
        signal_escaped(proc, sig);
        Ok(())
    }

    pub fn heartbeat_count(&self, app_id: &str) -> Option<u64> {
        self.apps.get(app_id)?.heartbeat.as_ref().map(|h| h.count())
    }

    pub fn proc_state(&self, app_id: &str) -> Option<char> {
        let proc = self.apps.get(app_id)?;
        if proc.exited.load(Ordering::Acquire) {
            return None;
        }
        let stat = std::fs::read_to_string(format!("/proc/{}/stat", proc.pid)).ok()?;
        stat.rsplit(')').next()?.trim().chars().next()
    }

    pub fn is_alive(&self, app_id: &str) -> bool {
        self.apps
            .get(app_id)
            .is_some_and(|p| !p.exited.load(Ordering::Acquire))
    }

    pub fn watch(&self) -> Receiver<AppCrashEvent> {
        self.crash_rx.clone()
    }

    pub fn pid_of(&self, app_id: &str) -> Result<u32> {
        self.apps
            .get(app_id)
            .map(|p| p.pid)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))
    }

    pub fn restart_count(&self, app_id: &str) -> Result<u32> {
        self.apps
            .get(app_id)
            .map(|p| p.restart_count)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))
    }

    pub fn setup_shm(
        &mut self,
        app_id: &str,
        advertised: Vec<String>,
        subscribed: Vec<String>,
    ) -> Result<()> {
        let proc = self.apps.get_mut(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;

        proc.shm.advertised_topics = advertised;
        proc.shm.subscribed_topics = subscribed;
        proc.shm.active = true;

        Ok(())
    }

    pub fn cleanup_shm(&mut self, app_id: &str) -> Result<()> {
        let proc = self.apps.get_mut(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;

        if !proc.shm.active {
            return Ok(());
        }

        proc.shm.active = false;
        proc.shm.advertised_topics.clear();
        proc.shm.subscribed_topics.clear();

        Ok(())
    }

    pub fn shm_state(&self, app_id: &str) -> Result<&ShmLifecycle> {
        self.apps.get(app_id)
            .map(|p| &p.shm)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))
    }

    pub fn handle_crash_shm(&mut self, app_id: &str) -> Result<()> {
        let proc = self.apps.get_mut(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;

        if proc.shm.preserve_on_crash {
            proc.shm.active = false;
        } else {
            proc.shm.active = false;
            proc.shm.advertised_topics.clear();
            proc.shm.subscribed_topics.clear();
        }

        Ok(())
    }

    pub fn report_thread_status(&mut self, report: &ThreadStatusReport) -> Result<bool> {
        let proc = self.apps.get_mut(&report.app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {}", report.app_id)))?;

        match report.state {
            ThreadState::Failed => {
                let count = proc.thread_failures
                    .entry(report.thread_name.clone())
                    .or_insert(0);
                *count += 1;

                if *count >= proc.max_thread_failures {
                    return Ok(true);
                }
            }
            ThreadState::Running => {
                proc.thread_failures.remove(&report.thread_name);
            }
            _ => {}
        }

        Ok(false)
    }

    pub fn thread_failures(&self, app_id: &str) -> Result<&HashMap<String, u32>> {
        self.apps.get(app_id)
            .map(|p| &p.thread_failures)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))
    }

    pub fn set_max_thread_failures(&mut self, app_id: &str, max: u32) -> Result<()> {
        let proc = self.apps.get_mut(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;
        proc.max_thread_failures = max;
        Ok(())
    }
}

impl Default for AppManager {
    fn default() -> Self {
        Self::new()
    }
}

fn resolve_binary(binary: &str, domain: &str) -> PathBuf {
    let path = Path::new(binary);
    if path.is_absolute() {
        return path.to_path_buf();
    }
    let base = std::env::var("KOS_APP_DIR").unwrap_or_else(|_| "/opt/kos/app".into());
    let resolved = PathBuf::from(&base).join(domain).join(binary).join(binary);
    if resolved.exists() {
        resolved
    } else {
        path.to_path_buf()
    }
}

fn verify_binary(binary_path: &Path) -> Result<()> {
    if !binary_path.is_absolute() {
        return Ok(());
    }

    let checksum_path = binary_path.with_extension("sha256");
    if !checksum_path.exists() {
        return Ok(());
    }

    let expected = std::fs::read_to_string(&checksum_path)
        .map_err(|e| KosError::NotFound(format!("checksum {}: {e}", checksum_path.display())))?
        .trim()
        .to_string();

    if compute_sha256(binary_path)? == expected {
        return Ok(());
    }

    let file_name = binary_path.file_name().unwrap();
    let backup = binary_path.parent().unwrap().join(".backup").join(file_name);
    if !backup.exists() {
        return Err(KosError::NotFound("binary corrupted, no backup".into()));
    }
    std::fs::copy(&backup, binary_path)
        .map_err(|e| KosError::NotFound(format!("backup restore failed: {e}")))?;

    if compute_sha256(binary_path)? != expected {
        return Err(KosError::NotFound("backup also corrupted".into()));
    }
    Ok(())
}

fn compute_sha256(path: &Path) -> Result<String> {
    use sha2::{Digest, Sha256};
    let bytes = std::fs::read(path)
        .map_err(|e| KosError::NotFound(format!("read {}: {e}", path.display())))?;
    let hash = Sha256::digest(&bytes);
    Ok(format!("{:x}", hash))
}

fn set_rt_priority(pid: u32, priority: u8) {
    use libc::{sched_param, sched_setscheduler, SCHED_FIFO};

    let param = sched_param {
        sched_priority: priority as i32,
    };
    let ret = unsafe { sched_setscheduler(pid as i32, SCHED_FIFO, &param) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        eprintln!(
            "[kos-exec] WARNING: failed to set SCHED_FIFO priority {priority} for pid {pid}: {err}. \
             Continuing with CFS. (Requires CAP_SYS_NICE.)"
        );
    }
}

fn set_nice(pid: u32, nice: i32) {
    let ret = unsafe { libc::setpriority(libc::PRIO_PROCESS, pid, nice) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        eprintln!(
            "[kos-exec] WARNING: failed to set nice {nice} for pid {pid}: {err}"
        );
    }
}

fn set_affinity(pid: u32, cores: &[u32]) -> std::result::Result<(), nix::Error> {
    use nix::sched::{sched_setaffinity, CpuSet};

    let mut cpuset = CpuSet::new();
    for &core in cores {
        cpuset.set(core as usize)?;
    }
    sched_setaffinity(Pid::from_raw(pid as i32), &cpuset)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::RestartPolicy;
    use std::time::Duration;

    fn dummy_config(id: &str) -> AppConfig {
        AppConfig {
            id: id.into(),
            binary: "sleep".into(),
            args: vec!["999".into()],
            domain: "default".into(),
            depends_on: vec![],
            restart: RestartPolicy {
                max_retries: 2,
                ..Default::default()
            },
            schedule: Default::default(),
            priority: "normal".into(),
            params: Default::default(),
            threads: Vec::new(),
        }
    }

    #[test]
    fn spawn_returns_pid_and_process_is_alive() {
        let mut mgr = AppManager::new();
        let cfg = dummy_config("app-1");
        let pid = mgr.spawn(&cfg, &SpawnResources::default()).unwrap();
        assert!(pid > 0);
        assert!(mgr.is_alive("app-1"));
        mgr.kill("app-1").unwrap();
    }

    #[test]
    fn kill_terminates_process() {
        let mut mgr = AppManager::new();
        let cfg = dummy_config("app-1");
        let pid = mgr.spawn(&cfg, &SpawnResources::default()).unwrap();
        assert!(pid > 0);

        mgr.kill("app-1").unwrap();
        assert!(!mgr.is_alive("app-1"));
    }

    #[test]
    fn crash_detection_via_watch() {
        let mut mgr = AppManager::new();
        let cfg = AppConfig {
            id: "crasher".into(),
            binary: "true".into(),
            args: vec![],
            domain: "default".into(),
            depends_on: vec![],
            restart: Default::default(),
            schedule: Default::default(),
            priority: "normal".into(),
            params: Default::default(),
            threads: Vec::new(),
        };
        let rx = mgr.watch();
        mgr.spawn(&cfg, &SpawnResources::default()).unwrap();

        let event = rx.recv_timeout(Duration::from_secs(2)).unwrap();
        assert_eq!(event.app_id, "crasher");
        assert_eq!(event.exit_code, Some(0));
    }

    #[test]
    fn restart_increments_count() {
        let mut mgr = AppManager::new();
        let cfg = dummy_config("app-1");
        mgr.spawn(&cfg, &SpawnResources::default()).unwrap();

        assert_eq!(mgr.restart_count("app-1").unwrap(), 0);
        mgr.restart("app-1").unwrap();
        assert_eq!(mgr.restart_count("app-1").unwrap(), 1);
        mgr.restart("app-1").unwrap();
        assert_eq!(mgr.restart_count("app-1").unwrap(), 2);

        mgr.kill("app-1").unwrap();
    }

    #[test]
    fn max_retries_exceeded() {
        let mut mgr = AppManager::new();
        let cfg = dummy_config("app-1");
        mgr.spawn(&cfg, &SpawnResources::default()).unwrap();

        mgr.restart("app-1").unwrap();
        mgr.restart("app-1").unwrap();

        let err = mgr.restart("app-1").unwrap_err();
        assert!(matches!(err, KosError::PermissionDenied(_)));

        mgr.kill("app-1").unwrap();
    }

    #[test]
    fn not_found_on_unknown_app() {
        let mut mgr = AppManager::new();
        assert!(matches!(
            mgr.kill("ghost"),
            Err(KosError::NotFound(_))
        ));
        assert!(!mgr.is_alive("ghost"));
    }

    #[test]
    fn setup_and_cleanup_shm() {
        let mut mgr = AppManager::new();
        let cfg = dummy_config("app-1");
        mgr.spawn(&cfg, &SpawnResources::default()).unwrap();

        mgr.setup_shm(
            "app-1",
            vec!["lidar_scan".into()],
            vec!["radar_data".into()],
        ).unwrap();

        let shm = mgr.shm_state("app-1").unwrap();
        assert!(shm.active);
        assert_eq!(shm.advertised_topics, vec!["lidar_scan"]);
        assert_eq!(shm.subscribed_topics, vec!["radar_data"]);

        mgr.cleanup_shm("app-1").unwrap();
        let shm = mgr.shm_state("app-1").unwrap();
        assert!(!shm.active);
        assert!(shm.advertised_topics.is_empty());

        mgr.kill("app-1").unwrap();
    }

    #[test]
    fn crash_preserves_shm() {
        let mut mgr = AppManager::new();
        let cfg = dummy_config("app-1");
        mgr.spawn(&cfg, &SpawnResources::default()).unwrap();

        mgr.setup_shm(
            "app-1",
            vec!["output".into()],
            vec!["input".into()],
        ).unwrap();

        mgr.handle_crash_shm("app-1").unwrap();
        let shm = mgr.shm_state("app-1").unwrap();
        assert!(!shm.active);
        assert_eq!(shm.advertised_topics, vec!["output"]);

        mgr.kill("app-1").unwrap();
    }

    #[test]
    fn cleanup_idempotent_when_inactive() {
        let mut mgr = AppManager::new();
        let cfg = dummy_config("app-1");
        mgr.spawn(&cfg, &SpawnResources::default()).unwrap();

        mgr.cleanup_shm("app-1").unwrap();

        mgr.kill("app-1").unwrap();
    }

    #[test]
    fn thread_failure_accumulates() {
        let mut mgr = AppManager::new();
        let cfg = dummy_config("app-1");
        mgr.spawn(&cfg, &SpawnResources::default()).unwrap();

        let report = ThreadStatusReport {
            app_id: "app-1".into(),
            thread_name: "worker".into(),
            state: ThreadState::Failed,
            failure_count: 1,
        };

        assert!(!mgr.report_thread_status(&report).unwrap());
        assert_eq!(mgr.thread_failures("app-1").unwrap()["worker"], 1);

        assert!(!mgr.report_thread_status(&report).unwrap());
        assert_eq!(mgr.thread_failures("app-1").unwrap()["worker"], 2);

        assert!(mgr.report_thread_status(&report).unwrap());

        mgr.kill("app-1").unwrap();
    }

    #[test]
    fn thread_recovery_resets_count() {
        let mut mgr = AppManager::new();
        let cfg = dummy_config("app-1");
        mgr.spawn(&cfg, &SpawnResources::default()).unwrap();

        let fail_report = ThreadStatusReport {
            app_id: "app-1".into(),
            thread_name: "worker".into(),
            state: ThreadState::Failed,
            failure_count: 1,
        };
        mgr.report_thread_status(&fail_report).unwrap();
        mgr.report_thread_status(&fail_report).unwrap();
        assert_eq!(mgr.thread_failures("app-1").unwrap()["worker"], 2);

        let ok_report = ThreadStatusReport {
            app_id: "app-1".into(),
            thread_name: "worker".into(),
            state: ThreadState::Running,
            failure_count: 0,
        };
        mgr.report_thread_status(&ok_report).unwrap();
        assert!(!mgr.thread_failures("app-1").unwrap().contains_key("worker"));

        mgr.kill("app-1").unwrap();
    }

    #[test]
    fn custom_max_thread_failures() {
        let mut mgr = AppManager::new();
        let cfg = dummy_config("app-1");
        mgr.spawn(&cfg, &SpawnResources::default()).unwrap();
        mgr.set_max_thread_failures("app-1", 1).unwrap();

        let report = ThreadStatusReport {
            app_id: "app-1".into(),
            thread_name: "critical".into(),
            state: ThreadState::Failed,
            failure_count: 1,
        };

        assert!(mgr.report_thread_status(&report).unwrap());

        mgr.kill("app-1").unwrap();
    }

    fn sleeper(id: &str) -> AppConfig {
        AppConfig {
            id: id.into(),
            binary: "sleep".into(),
            args: vec!["30".into()],
            domain: "d".into(),
            depends_on: vec![],
            restart: RestartPolicy::default(),
            schedule: crate::config::ScheduleConfig::default(),
            priority: "normal".into(),
            params: HashMap::new(),
            threads: vec![],
        }
    }

    fn spawn_from_exiting_thread(kill_with_parent: bool) -> (AppManager, u32) {
        let mut mgr = AppManager::new();
        mgr.set_kill_with_parent(kill_with_parent);
        let (mgr, pid) = std::thread::spawn(move || {
            let pid = mgr.spawn(&sleeper("child"), &SpawnResources::default()).unwrap();
            (mgr, pid)
        })
        .join()
        .unwrap();
        (mgr, pid)
    }

    #[test]
    fn kill_with_parent_terminates_child_when_spawner_exits() {
        let (mgr, pid) = spawn_from_exiting_thread(true);
        let ev = mgr.watch().recv_timeout(std::time::Duration::from_secs(5)).expect("child should exit");
        assert_eq!(ev.pid, pid);
        assert_eq!(ev.exit_code, Some(-(libc::SIGTERM)));
    }

    #[test]
    fn child_survives_spawner_exit_by_default() {
        let (mut mgr, pid) = spawn_from_exiting_thread(false);
        assert!(mgr.watch().recv_timeout(std::time::Duration::from_millis(200)).is_err(), "child must keep running");
        assert!(mgr.is_alive("child"));
        mgr.kill("child").unwrap();
        let _ = pid;
    }

    fn spawn_with_grandchild(mgr: &mut AppManager, id: &str) -> i32 {
        let pidfile = std::env::temp_dir().join(format!("kos_gc_{}_{id}", std::process::id()));
        let _ = std::fs::remove_file(&pidfile);
        let mut cfg = sleeper(id);
        cfg.binary = "sh".into();
        cfg.args = vec!["-c".into(), format!("sleep 30 & echo $! > {}; wait", pidfile.display())];
        mgr.spawn(&cfg, &SpawnResources::default()).unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            if let Some(pid) = std::fs::read_to_string(&pidfile).ok().and_then(|s| s.trim().parse().ok()) {
                let _ = std::fs::remove_file(&pidfile);
                return pid;
            }
            assert!(std::time::Instant::now() < deadline, "grandchild pid not written");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }

    fn proc_state(pid: i32) -> Option<char> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        let state = stat.rsplit(')').next()?.trim().chars().next()?;
        (state != 'Z').then_some(state)
    }

    fn wait_state(pid: i32, want: impl Fn(Option<char>) -> bool) -> bool {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            if want(proc_state(pid)) {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        false
    }

    #[test]
    fn kill_also_terminates_grandchildren() {
        let mut mgr = AppManager::new();
        let gc = spawn_with_grandchild(&mut mgr, "gc-kill");
        assert!(proc_state(gc).is_some());
        mgr.kill("gc-kill").unwrap();
        assert!(wait_state(gc, |s| s.is_none()), "grandchild {gc} left behind");
    }

    #[test]
    fn suspend_and_resume_apply_to_grandchildren() {
        let mut mgr = AppManager::new();
        let gc = spawn_with_grandchild(&mut mgr, "gc-stop");
        mgr.signal("gc-stop", Signal::SIGSTOP).unwrap();
        assert!(wait_state(gc, |s| s == Some('T')), "grandchild not stopped");
        mgr.signal("gc-stop", Signal::SIGCONT).unwrap();
        assert!(wait_state(gc, |s| s.is_some_and(|c| c != 'T')), "grandchild not resumed");
        mgr.kill("gc-stop").unwrap();
        assert!(wait_state(gc, |s| s.is_none()));
    }

    #[test]
    fn respawn_cleans_up_previous_grandchildren() {
        let mut mgr = AppManager::new();
        let gc = spawn_with_grandchild(&mut mgr, "gc-respawn");
        let leader = mgr.pid_of("gc-respawn").unwrap();
        signal::kill(Pid::from_raw(leader as i32), Signal::SIGKILL).unwrap();
        mgr.watch().recv_timeout(std::time::Duration::from_secs(5)).expect("leader exit");
        assert!(proc_state(gc).is_some(), "grandchild should outlive the leader");
        mgr.respawn_exited("gc-respawn").unwrap();
        assert!(wait_state(gc, |s| s.is_none()), "old grandchild {gc} left behind");
        mgr.kill("gc-respawn").unwrap();
    }

    #[test]
    #[ignore]
    fn helper_launcher_for_reaper() {
        let Ok(dir) = std::env::var("KOS_TEST_REAPER_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let mut mgr = AppManager::new();
        mgr.set_kill_with_parent(true);
        let mut cfg = sleeper("reaped");
        cfg.binary = "sh".into();
        cfg.args = vec!["-c".into(), format!("sleep 30 & echo $! > {}; wait", dir.join("gc").display())];
        mgr.spawn(&cfg, &SpawnResources::default()).unwrap();
        std::fs::write(dir.join("ready"), std::process::id().to_string()).unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
    }

    #[test]
    fn grandchildren_are_killed_when_launcher_is_sigkilled() {
        let dir = std::env::temp_dir().join(format!("kos_reaper_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut launcher = std::process::Command::new(std::env::current_exe().unwrap());
        launcher
            .args(["--exact", "app_manager::tests::helper_launcher_for_reaper", "--ignored", "--test-threads=1"])
            .env("KOS_TEST_REAPER_DIR", &dir)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut launcher, 0);
        let mut launcher = launcher.spawn().unwrap();

        let read_pid = |name: &str| -> Option<i32> {
            std::fs::read_to_string(dir.join(name)).ok()?.trim().parse().ok()
        };
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while (read_pid("ready").is_none() || read_pid("gc").is_none()) && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let gc = read_pid("gc").expect("grandchild pid");
        assert!(proc_state(gc).is_some());

        launcher.kill().unwrap();
        launcher.wait().unwrap();
        let gone = wait_state(gc, |s| s.is_none());
        if !gone {
            let _ = signal::kill(Pid::from_raw(gc), Signal::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "grandchild {gc} survived a SIGKILLed launcher");
    }

    fn setsid_app(dir: &std::path::Path) -> AppConfig {
        let mut cfg = sleeper("escaper");
        cfg.binary = "sh".into();
        cfg.args = vec!["-c".into(), format!("setsid sleep 30 & echo $! > {}; wait", dir.join("gc").display())];
        cfg
    }

    fn read_pid(path: &std::path::Path) -> Option<i32> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            if let Some(pid) = std::fs::read_to_string(path).ok().and_then(|s| s.trim().parse().ok()) {
                return Some(pid);
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        None
    }

    fn report(dir: &std::path::Path, result: &str) {
        std::fs::write(dir.join("result"), result).unwrap();
    }

    #[test]
    #[ignore]
    fn helper_cgroup_escape() {
        let Ok(dir) = std::env::var("KOS_TEST_CGROUP_DIR") else {
            return;
        };
        let dir = std::path::PathBuf::from(dir);
        let mode = std::env::var("KOS_TEST_CGROUP_MODE").unwrap_or_default();
        let mut mgr = AppManager::new();
        if mode != "kill" {
            mgr.set_kill_with_parent(true);
        }
        crate::cgroup::ensure_kos_cgroup_root(false, true, false);
        let Some(root) = crate::cgroup::apps_root() else {
            return report(&dir, "skip: no cgroup delegation");
        };
        mgr.set_cgroup_root(root.clone());
        let resources = SpawnResources { domain_id: "d".into(), max_pids: Some(16), ..Default::default() };
        if mode == "limits" {
            mgr.spawn(&sleeper("limited"), &resources).unwrap();
            let pids_max = std::fs::read_to_string(root.join("d/pids.max")).unwrap_or_default();
            mgr.kill("limited").unwrap();
            return report(&dir, if pids_max.trim() == "16" { "ok" } else { "fail: pids.max not applied" });
        }
        let leader = mgr.spawn(&setsid_app(&dir), &resources).unwrap() as i32;
        let Some(gc) = read_pid(&dir.join("gc")) else {
            return report(&dir, "fail: grandchild pid not written");
        };
        if unsafe { libc::getpgid(gc) } == leader {
            return report(&dir, "fail: grandchild did not leave the process group");
        }
        if mode == "reaper" {
            std::fs::write(dir.join("ready"), "1").unwrap();
            std::thread::sleep(std::time::Duration::from_secs(60));
            return;
        }
        mgr.signal("escaper", Signal::SIGSTOP).unwrap();
        if !wait_state(gc, |s| s == Some('T')) {
            return report(&dir, "fail: escaped grandchild not stopped");
        }
        mgr.signal("escaper", Signal::SIGCONT).unwrap();
        mgr.kill("escaper").unwrap();
        if !wait_state(gc, |s| s.is_none()) {
            let _ = signal::kill(Pid::from_raw(gc), Signal::SIGKILL);
            return report(&dir, "fail: escaped grandchild survived kill");
        }
        report(&dir, "ok");
    }

    fn run_in_delegated_scope(mode: &str) -> Option<(std::process::Child, std::path::PathBuf)> {
        if std::process::Command::new("systemd-run").arg("--version").output().is_err() {
            return None;
        }
        let dir = std::env::temp_dir().join(format!("kos_cg_{mode}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut cmd = std::process::Command::new("systemd-run");
        cmd.args(["--user", "--scope", "--quiet", "-p", "Delegate=yes", "--"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "app_manager::tests::helper_cgroup_escape", "--ignored", "--test-threads=1"])
            .env("KOS_TEST_CGROUP_DIR", &dir)
            .env("KOS_TEST_CGROUP_MODE", mode)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        std::os::unix::process::CommandExt::process_group(&mut cmd, 0);
        Some((cmd.spawn().ok()?, dir))
    }

    fn wait_file(path: &std::path::Path, secs: u64) -> Option<String> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
        while std::time::Instant::now() < deadline {
            if let Ok(s) = std::fs::read_to_string(path) {
                if !s.is_empty() {
                    return Some(s);
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        None
    }

    #[test]
    fn escaped_children_are_stopped_and_killed_via_cgroup() {
        let Some((mut child, dir)) = run_in_delegated_scope("kill") else {
            eprintln!("skip: systemd-run not available");
            return;
        };
        let result = wait_file(&dir.join("result"), 30);
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        match result.as_deref() {
            Some(r) if r.starts_with("skip") => eprintln!("{r}"),
            other => assert_eq!(other, Some("ok")),
        }
    }

    #[test]
    fn domain_limits_apply_with_reaper_running() {
        let Some((mut child, dir)) = run_in_delegated_scope("limits") else {
            eprintln!("skip: systemd-run not available");
            return;
        };
        let result = wait_file(&dir.join("result"), 30);
        let _ = child.wait();
        let _ = std::fs::remove_dir_all(&dir);
        match result.as_deref() {
            Some(r) if r.starts_with("skip") => eprintln!("{r}"),
            other => assert_eq!(other, Some("ok")),
        }
    }

    #[test]
    fn escaped_children_are_killed_when_launcher_is_sigkilled() {
        let Some((mut child, dir)) = run_in_delegated_scope("reaper") else {
            eprintln!("skip: systemd-run not available");
            return;
        };
        let ready = wait_file(&dir.join("ready"), 30);
        let early = std::fs::read_to_string(dir.join("result")).ok();
        let gc = read_pid(&dir.join("gc"));
        let launcher = std::fs::read_dir("/proc").ok().and_then(|entries| {
            entries.flatten().find_map(|e| {
                let pid: i32 = e.file_name().to_str()?.parse().ok()?;
                let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
                let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
                let marker = format!("KOS_TEST_CGROUP_DIR={}", dir.display());
                (env.split(|b| *b == 0).any(|v| v == marker.as_bytes())
                    && cmdline.windows(20).any(|w| w == b"helper_cgroup_escape"))
                .then_some(pid)
            })
        });
        if let Some(r) = early.clone().filter(|r| r.starts_with("skip")) {
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&dir);
            eprintln!("{r}");
            return;
        }
        assert!(ready.is_some(), "helper did not get ready: {early:?}");
        let (gc, launcher) = (gc.expect("grandchild pid"), launcher.expect("helper launcher pid"));
        assert!(proc_state(gc).is_some());
        signal::kill(Pid::from_raw(launcher), Signal::SIGKILL).unwrap();
        let _ = child.wait();
        let gone = wait_state(gc, |s| s.is_none());
        if !gone {
            let _ = signal::kill(Pid::from_raw(gc), Signal::SIGKILL);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert!(gone, "escaped grandchild {gc} survived a SIGKILLed launcher");
    }
}

