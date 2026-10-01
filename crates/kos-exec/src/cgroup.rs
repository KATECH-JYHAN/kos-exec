// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static KOS_SLICE_PATH: OnceLock<PathBuf> = OnceLock::new();

const APPS_DIR: &str = "apps";

pub fn ensure_kos_cgroup_root(
    need_memory: bool,
    need_pids: bool,
    need_cpu: bool,
) -> bool {
    let required = need_memory || need_pids || need_cpu;
    KOS_SLICE_PATH
        .get_or_init(|| {
            macro_rules! eprintln {
                ($($arg:tt)*) => {
                    if required {
                        std::eprintln!($($arg)*);
                    }
                };
            }
            let base = match find_cgroup_base() {
                Some(b) => b,
                None => {
                    eprintln!(
                        "[kos-exec] WARNING: could not determine cgroup v2 base path. \
                         cgroup resource limits will be unavailable."
                    );
                    return PathBuf::new();
                }
            };

            let self_pid = std::process::id() as i32;
            let ours = |pid: i32| pid == self_pid || parent_pid(pid) == Some(self_pid);
            if let Some(other) = cgroup_pids(&base).into_iter().find(|&pid| !ours(pid)) {
                eprintln!(
                    "[kos-exec] WARNING: cgroup {} is shared with other processes (pid {other}); \
                     not using cgroups. Run via `kos launch` or a systemd unit with Delegate=yes.",
                    base.display()
                );
                return PathBuf::new();
            }

            let kos_slice = base.join("kos.slice");
            let init_dir = kos_slice.join("init");

            if let Err(e) = std::fs::create_dir_all(&init_dir) {
                eprintln!(
                    "[kos-exec] WARNING: failed to create cgroup dir {}: {e}. \
                     (Requires systemd Delegate=yes or CAP_SYS_ADMIN.)",
                    init_dir.display()
                );
                return PathBuf::new();
            }

            for pid in cgroup_pids(&base).into_iter().filter(|&pid| ours(pid)) {
                if let Err(e) = std::fs::write(init_dir.join("cgroup.procs"), pid.to_string()) {
                    eprintln!(
                        "[kos-exec] WARNING: failed to move pid {pid} to {}: {e}",
                        init_dir.display()
                    );
                    return PathBuf::new();
                }
            }

            let controllers = build_controllers(need_memory, need_pids, need_cpu);
            if let Err(e) = std::fs::write(base.join("cgroup.subtree_control"), &controllers) {
                eprintln!(
                    "[kos-exec] WARNING: failed to enable controllers '{}' at scope root {}: {e}",
                    controllers,
                    base.display()
                );
            }

            if let Err(e) = std::fs::write(kos_slice.join("cgroup.subtree_control"), &controllers) {
                eprintln!(
                    "[kos-exec] WARNING: failed to enable controllers '{}' in {}: {e}",
                    controllers,
                    kos_slice.display()
                );
            }

            let apps = kos_slice.join(APPS_DIR);
            if let Err(e) = std::fs::create_dir_all(&apps) {
                eprintln!("[kos-exec] WARNING: failed to create cgroup dir {}: {e}", apps.display());
                return PathBuf::new();
            }
            if let Err(e) = std::fs::write(apps.join("cgroup.subtree_control"), &controllers) {
                eprintln!(
                    "[kos-exec] WARNING: failed to enable controllers '{}' in {}: {e}",
                    controllers,
                    apps.display()
                );
            }

            kos_slice
        })
        .as_os_str()
        .len()
        > 0
}

pub fn apps_root() -> Option<PathBuf> {
    KOS_SLICE_PATH
        .get()
        .filter(|p| p.as_os_str().len() > 0)
        .map(|p| p.join(APPS_DIR))
}

pub fn prepare_app_cgroup(
    domain_id: &str,
    app_id: &str,
    memory_limit_mb: Option<u64>,
    max_pids: Option<u64>,
    cpu_quota: Option<u8>,
) -> Option<PathBuf> {
    let domain_dir = apps_root()?.join(cgroup_name(domain_id));
    let app_dir = domain_dir.join(cgroup_name(app_id));
    if let Err(e) = std::fs::create_dir_all(&app_dir) {
        eprintln!("[kos-exec] WARNING: failed to create cgroup dir {}: {e}", app_dir.display());
        return None;
    }

    if let Some(mb) = memory_limit_mb {
        let bytes = mb * 1024 * 1024;
        if let Err(e) = std::fs::write(domain_dir.join("memory.max"), bytes.to_string()) {
            eprintln!(
                "[kos-exec] WARNING: failed to set memory.max={mb}MB for domain '{domain_id}': {e}"
            );
        }
    }
    if let Some(max) = max_pids {
        if let Err(e) = std::fs::write(domain_dir.join("pids.max"), max.to_string()) {
            eprintln!(
                "[kos-exec] WARNING: failed to set pids.max={max} for domain '{domain_id}': {e}"
            );
        }
    }
    if let Some(quota) = cpu_quota {
        let value = format!("{} 100000", (quota as u64) * 1000);
        if let Err(e) = std::fs::write(domain_dir.join("cpu.max"), &value) {
            eprintln!(
                "[kos-exec] WARNING: failed to set cpu.max={value} for domain '{domain_id}': {e}"
            );
        }
    }
    Some(app_dir)
}

pub fn add_pid(dir: &Path, pid: u32) -> std::io::Result<()> {
    std::fs::write(dir.join("cgroup.procs"), pid.to_string())
}

pub fn contains_pid(dir: &Path, pid: u32) -> bool {
    cgroup_pids(dir).contains(&(pid as i32))
}

pub fn cgroup_pids(dir: &Path) -> Vec<i32> {
    std::fs::read_to_string(dir.join("cgroup.procs"))
        .map(|s| s.lines().filter_map(|l| l.trim().parse().ok()).collect())
        .unwrap_or_default()
}

pub fn signal_cgroup(dir: &Path, sig: libc::c_int) {
    for pid in cgroup_pids(dir) {
        unsafe { libc::kill(pid, sig) };
    }
}

pub fn is_populated(dir: &Path) -> bool {
    match std::fs::read_to_string(dir.join("cgroup.events")) {
        Ok(events) => events.lines().any(|l| l.trim() == "populated 1"),
        Err(_) => !cgroup_pids(dir).is_empty(),
    }
}

pub fn kill_cgroup(dir: &Path, timeout: std::time::Duration) -> bool {
    let use_kill_file = std::fs::write(dir.join("cgroup.kill"), "1").is_ok();
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if !is_populated(dir) {
            return true;
        }
        if !use_kill_file {
            signal_cgroup(dir, libc::SIGKILL);
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(std::time::Duration::from_millis(2));
    }
}

pub fn remove_cgroup(dir: &Path) {
    let _ = std::fs::remove_dir(dir);
}

pub fn cleanup_cgroup(domain_id: &str) {
    if let Some(apps) = apps_root() {
        let domain_dir = apps.join(cgroup_name(domain_id));
        if let Ok(entries) = std::fs::read_dir(&domain_dir) {
            for entry in entries.flatten() {
                if entry.path().is_dir() {
                    let _ = kill_cgroup(&entry.path(), std::time::Duration::from_millis(500));
                    let _ = std::fs::remove_dir(entry.path());
                }
            }
        }
        let _ = std::fs::remove_dir(&domain_dir);
    }
}

fn parent_pid(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit(')').next()?.split_whitespace().nth(1)?.parse().ok()
}

fn cgroup_name(id: &str) -> String {
    let name: String = id
        .chars()
        .map(|c| if c == '/' || c.is_control() { '_' } else { c })
        .collect();
    match name.as_str() {
        "" | "." | ".." => format!("_{name}"),
        _ if name.starts_with("cgroup.") => format!("_{name}"),
        _ => name,
    }
}

pub fn cleanup_kos_cgroup_root() {
    let Some(kos_slice) = KOS_SLICE_PATH.get().filter(|p| p.as_os_str().len() > 0) else {
        return;
    };
    if let Some(base) = kos_slice.parent() {

        for pid in cgroup_pids(&kos_slice.join("init")) {
            let _ = std::fs::write(base.join("cgroup.procs"), pid.to_string());
        }

        let _ = std::fs::remove_dir(kos_slice.join(APPS_DIR));
        let _ = std::fs::remove_dir(kos_slice.join("init"));
        let _ = std::fs::remove_dir(&kos_slice);
    }
}

fn find_cgroup_base() -> Option<PathBuf> {
    find_cgroup_base_raw()
}

fn find_cgroup_base_raw() -> Option<PathBuf> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    for line in content.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            return Some(
                PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')),
            );
        }
    }
    None
}

pub fn isolate_irq_from_cores(isolated_cores: &[u32], total_cores: u32) {
    if isolated_cores.is_empty() || total_cores == 0 {
        return;
    }

    let allowed: Vec<u32> = (0..total_cores)
        .filter(|c| !isolated_cores.contains(c))
        .collect();
    if allowed.is_empty() {
        eprintln!(
            "[kos-exec] WARNING: IRQ isolation would leave no cores for IRQ handling. Skipping."
        );
        return;
    }
    let affinity_str = cores_to_list_string(&allowed);

    let irq_dir = std::path::Path::new("/proc/irq");
    let entries = match std::fs::read_dir(irq_dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("[kos-exec] WARNING: cannot read /proc/irq: {e}");
            return;
        }
    };

    let mut ok_count = 0u32;
    let mut fail_count = 0u32;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let affinity_file = path.join("smp_affinity_list");
        if !affinity_file.exists() {
            continue;
        }
        match std::fs::write(&affinity_file, &affinity_str) {
            Ok(_) => ok_count += 1,
            Err(_) => fail_count += 1,
        }
    }

    eprintln!(
        "[kos-exec] IRQ isolation: moved {ok_count} IRQs away from cores {:?} \
         ({fail_count} kernel-pinned, unchanged)",
        isolated_cores
    );
}

pub fn isolate_irq_from_cores_guarded(
    isolated_cores: &[u32],
    total_cores: u32,
) -> Option<IrqAffinityGuard> {
    if isolated_cores.is_empty() || total_cores == 0 {
        return None;
    }

    let mut saved: Vec<(std::path::PathBuf, String)> = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/proc/irq") {
        for entry in entries.flatten() {
            let affinity_file = entry.path().join("smp_affinity_list");
            if let Ok(orig) = std::fs::read_to_string(&affinity_file) {
                saved.push((affinity_file, orig.trim().to_string()));
            }
        }
    }

    isolate_irq_from_cores(isolated_cores, total_cores);

    if saved.is_empty() {
        return None;
    }
    Some(IrqAffinityGuard { saved })
}

pub struct IrqAffinityGuard {
    saved: Vec<(std::path::PathBuf, String)>,
}

impl Drop for IrqAffinityGuard {
    fn drop(&mut self) {
        let mut restored = 0u32;
        for (path, orig) in &self.saved {
            if std::fs::write(path, orig).is_ok() {
                restored += 1;
            }
        }
        if restored > 0 {
            eprintln!("[kos-exec] IRQ isolation: restored {restored} IRQ affinities on drop");
        }
    }
}

pub fn disable_cstates() -> Option<std::fs::File> {
    use std::io::Write;
    match std::fs::File::options().write(true).open("/dev/cpu_dma_latency") {
        Ok(mut f) => {
            if let Err(e) = f.write_all(&0u32.to_ne_bytes()) {
                eprintln!("[kos-exec] WARNING: failed to write to /dev/cpu_dma_latency: {e}");
                return None;
            }
            eprintln!("[kos-exec] PM QoS: C-states disabled globally (cpu_dma_latency=0)");
            Some(f)
        }
        Err(e) => {
            eprintln!(
                "[kos-exec] WARNING: cannot open /dev/cpu_dma_latency: {e} \
                 (idle jitter may be higher — run as root or grant CAP_SYS_ADMIN)"
            );
            None
        }
    }
}

pub fn disable_cstates_for_cores(cores: &[u32]) -> Option<CstateGuard> {
    if cores.is_empty() {
        return None;
    }

    let mut disabled_states: Vec<std::path::PathBuf> = Vec::new();

    for &core in cores {
        let cpuidle_dir = std::path::PathBuf::from(format!(
            "/sys/devices/system/cpu/cpu{core}/cpuidle"
        ));
        if !cpuidle_dir.exists() {
            eprintln!(
                "[kos-exec] WARNING: cpuidle not available for core {core} — \
                 C-state control skipped"
            );
            continue;
        }

        let entries = match std::fs::read_dir(&cpuidle_dir) {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "[kos-exec] WARNING: cannot read {}: {e}",
                    cpuidle_dir.display()
                );
                continue;
            }
        };

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if !name_str.starts_with("state") || name_str == "state0" {
                continue;
            }

            let disable_file = entry.path().join("disable");
            if !disable_file.exists() {
                continue;
            }

            match std::fs::write(&disable_file, "1") {
                Ok(_) => {
                    disabled_states.push(disable_file);
                }
                Err(e) => {
                    eprintln!(
                        "[kos-exec] WARNING: failed to disable C-state {} on core {core}: {e}",
                        name_str
                    );
                }
            }
        }
    }

    if disabled_states.is_empty() {
        eprintln!(
            "[kos-exec] WARNING: no C-states disabled — \
             per-core cpuidle control unavailable (root required)"
        );
        return None;
    }

    eprintln!(
        "[kos-exec] PM QoS: C-states disabled on cores {:?} ({} states)",
        cores,
        disabled_states.len()
    );

    Some(CstateGuard { disabled_states })
}

pub struct CstateGuard {
    disabled_states: Vec<std::path::PathBuf>,
}

impl Drop for CstateGuard {
    fn drop(&mut self) {
        let mut restored = 0u32;
        for path in &self.disabled_states {
            if std::fs::write(path, "0").is_ok() {
                restored += 1;
            }
        }
        if restored > 0 {
            eprintln!(
                "[kos-exec] PM QoS: restored {restored} C-state entries on drop"
            );
        }
    }
}

fn cores_to_list_string(cores: &[u32]) -> String {
    if cores.is_empty() {
        return String::new();
    }
    let mut result = String::new();
    let mut start = cores[0];
    let mut end = cores[0];

    for &c in &cores[1..] {
        if c == end + 1 {
            end = c;
        } else {
            if !result.is_empty() {
                result.push(',');
            }
            if start == end {
                result.push_str(&start.to_string());
            } else {
                result.push_str(&format!("{start}-{end}"));
            }
            start = c;
            end = c;
        }
    }
    if !result.is_empty() {
        result.push(',');
    }
    if start == end {
        result.push_str(&start.to_string());
    } else {
        result.push_str(&format!("{start}-{end}"));
    }
    result
}

fn build_controllers(memory: bool, pids: bool, cpu: bool) -> String {
    let mut c = String::new();
    if memory {
        c.push_str("+memory");
    }
    if pids {
        if !c.is_empty() {
            c.push(' ');
        }
        c.push_str("+pids");
    }
    if cpu {
        if !c.is_empty() {
            c.push(' ');
        }
        c.push_str("+cpu");
    }
    c
}
