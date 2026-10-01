// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::env;
use std::os::unix::process::CommandExt;
use std::process::Command;

#[derive(Debug, Clone)]
pub struct RuntimeCaps {
    pub rt_priority: bool,
    pub irq_isolate: bool,
    pub cgroup: bool,
}

impl RuntimeCaps {
    pub fn detect() -> Self {
        Self {
            rt_priority: probe_rt_priority(),
            irq_isolate: probe_irq_writable(),
            cgroup: probe_cgroup_delegate(),
        }
    }

    pub fn all_available(&self) -> bool {
        self.rt_priority && self.irq_isolate && self.cgroup
    }

    pub fn print_summary(&self) {
        eprintln!("[kos-exec] Runtime capabilities:");
        Self::print_cap("SCHED_FIFO (rt_priority)", self.rt_priority,
            "setcap cap_sys_nice+ep <binary>");
        Self::print_cap("IRQ isolation", self.irq_isolate,
            "root or systemd service");
        Self::print_cap("cgroup v2 (pids/memory/cpu)", self.cgroup,
            "systemd-run --scope -p Delegate=yes");
    }

    fn print_cap(name: &str, available: bool, hint: &str) {
        if available {
            eprintln!("  [OK] {name}");
        } else {
            eprintln!("  [--] {name} (needs: {hint})");
        }
    }
}

pub fn ensure_delegated() -> std::io::Result<bool> {
    if env::var("KOS_DELEGATED").is_ok() || (probe_cgroup_delegate() && !cgroup_shared_with_others()) {
        return Ok(false);
    }

    let mode = scope_args(unsafe { libc::getuid() });
    let usable = Command::new("systemd-run")
        .args(&mode)
        .args(["--quiet", "--", "true"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !usable {
        eprintln!(
            "[kos-exec] WARNING: cannot create a systemd scope (systemd-run {}). \
             cgroup features will be unavailable; run kos as a systemd service with Delegate=yes.",
            mode.join(" ")
        );
        return Ok(false);
    }

    let exe = env::current_exe()?;
    let args: Vec<String> = env::args().skip(1).collect();

    eprintln!("[kos-exec] cgroup delegation not available, re-executing with systemd-run...");

    let mut cmd = Command::new("systemd-run");
    cmd.args(&mode)
        .arg("-p").arg("Delegate=yes")
        .arg("--")
        .arg(&exe)
        .args(&args)
        .env("KOS_DELEGATED", "1");

    for (key, val) in env::vars() {
        if key != "KOS_DELEGATED" {
            cmd.env(&key, &val);
        }
    }

    Err(cmd.exec())
}

fn scope_args(uid: libc::uid_t) -> Vec<&'static str> {
    if uid == 0 {
        vec!["--scope"]
    } else {
        vec!["--user", "--scope"]
    }
}

fn probe_rt_priority() -> bool {
    use libc::{sched_param, sched_setscheduler, SCHED_FIFO, SCHED_OTHER};

    let param = sched_param { sched_priority: 1 };
    let ret = unsafe { sched_setscheduler(0, SCHED_FIFO, &param) };
    if ret == 0 {
        let param = sched_param { sched_priority: 0 };
        unsafe { sched_setscheduler(0, SCHED_OTHER, &param) };
        true
    } else {
        false
    }
}

fn probe_irq_writable() -> bool {
    let irq_dir = std::path::Path::new("/proc/irq");
    let entries = match std::fs::read_dir(irq_dir) {
        Ok(e) => e,
        Err(_) => return false,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() { continue; }
        let affinity_file = path.join("smp_affinity_list");
        if !affinity_file.exists() { continue; }

        if let Ok(current) = std::fs::read_to_string(&affinity_file) {
            if std::fs::write(&affinity_file, current.trim()).is_ok() {
                return true;
            }
        }
        return false;
    }
    false
}

fn cgroup_shared_with_others() -> bool {
    let Some(base) = own_cgroup_dir() else {
        return false;
    };
    let me = std::process::id();
    match std::fs::read_to_string(base.join("cgroup.procs")) {
        Ok(procs) => procs
            .lines()
            .filter_map(|l| l.trim().parse::<u32>().ok())
            .any(|pid| pid != me),
        Err(_) => false,
    }
}

fn own_cgroup_dir() -> Option<std::path::PathBuf> {
    let content = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let path = content.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(std::path::PathBuf::from("/sys/fs/cgroup").join(path.trim_start_matches('/')))
}

fn probe_cgroup_delegate() -> bool {
    let content = match std::fs::read_to_string("/proc/self/cgroup") {
        Ok(c) => c,
        Err(_) => return false,
    };

    for line in content.lines() {
        if let Some(path) = line.strip_prefix("0::") {
            let base = std::path::PathBuf::from("/sys/fs/cgroup")
                .join(path.trim_start_matches('/'));
            let test_dir = base.join(".kos_probe");
            if std::fs::create_dir(&test_dir).is_ok() {
                let _ = std::fs::remove_dir(&test_dir);
                return true;
            }
            return false;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_uses_system_scope_and_users_use_user_scope() {
        assert_eq!(scope_args(0), vec!["--scope"]);
        assert_eq!(scope_args(1000), vec!["--user", "--scope"]);
    }
}
