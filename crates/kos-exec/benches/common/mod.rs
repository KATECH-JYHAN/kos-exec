// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

#![allow(dead_code)]

use std::sync::OnceLock;

pub fn env_u64(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

pub fn env_flag(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("1") | Ok("true") | Ok("on") | Ok("yes"))
}

pub fn num_cpus() -> usize {
    let n = unsafe { libc::sysconf(libc::_SC_NPROCESSORS_ONLN) };
    if n > 0 { n as usize } else { 1 }
}

pub fn set_min_timer_slack() {
    unsafe {
        libc::prctl(libc::PR_SET_TIMERSLACK, 1 as libc::c_ulong, 0, 0, 0);
    }
}

pub fn set_nice(nice: i32) {
    unsafe {
        libc::setpriority(libc::PRIO_PROCESS, 0, nice);
    }
}

pub fn apply_worker_nice_from_env() {
    let nice = std::env::var("KOS_BENCH_NICE").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    set_nice(nice);
}

pub fn init_measure_worker() {
    set_min_timer_slack();
    set_nice(0);
}

pub fn mono_now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub fn sleep_until_ns(target_ns: u64) {
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

pub fn thread_cpu_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

pub fn cpu_burn(duration_us: u64) {
    let target = thread_cpu_ns() + duration_us * 1000;
    while thread_cpu_ns() < target {
        for _ in 0..64 {
            std::hint::black_box(0u64.wrapping_mul(42));
        }
    }
}

pub fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.trim().split(',').filter(|p| !p.is_empty()) {
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.trim().parse::<usize>(), b.trim().parse::<usize>()) {
                out.extend(a..=b);
            }
        } else if let Ok(v) = part.trim().parse() {
            out.push(v);
        }
    }
    out
}

pub fn format_cpu_list(cores: &[usize]) -> String {
    let mut v = cores.to_vec();
    v.sort_unstable();
    v.dedup();
    let mut parts = Vec::new();
    let mut i = 0;
    while i < v.len() {
        let start = v[i];
        let mut end = start;
        while i + 1 < v.len() && v[i + 1] == end + 1 {
            i += 1;
            end = v[i];
        }
        parts.push(if start == end { format!("{start}") } else { format!("{start}-{end}") });
        i += 1;
    }
    parts.join(",")
}

pub fn smt_siblings(cpu: usize) -> Vec<usize> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list");
    match std::fs::read_to_string(path) {
        Ok(s) => parse_cpu_list(&s),
        Err(_) => vec![cpu],
    }
}

pub struct Topology {
    pub critical: Vec<usize>,
    pub reserved: Vec<usize>,
    pub nc: Vec<usize>,
    pub total: usize,
}

impl Topology {
    pub fn new(critical: &[usize]) -> Self {
        let total = num_cpus();
        let mut reserved: Vec<usize> = critical
            .iter()
            .flat_map(|&c| smt_siblings(c))
            .filter(|&c| c < total)
            .collect();
        reserved.sort_unstable();
        reserved.dedup();
        let nc = (0..total).filter(|c| !reserved.contains(c)).collect();
        Topology { critical: critical.to_vec(), reserved, nc, total }
    }

    pub fn reserved_u32(&self) -> Vec<u32> {
        self.reserved.iter().map(|&c| c as u32).collect()
    }
}

static TOPOLOGY: OnceLock<Topology> = OnceLock::new();

pub fn topology() -> &'static Topology {
    TOPOLOGY.get_or_init(|| Topology::new(&[0, 1, 2, 3]))
}

const RT_RUNTIME_PATH: &str = "/proc/sys/kernel/sched_rt_runtime_us";
const RT_PERIOD_PATH: &str = "/proc/sys/kernel/sched_rt_period_us";

pub fn read_rt_runtime() -> Option<i64> {
    std::fs::read_to_string(RT_RUNTIME_PATH).ok()?.trim().parse().ok()
}

pub fn read_rt_period() -> Option<i64> {
    std::fs::read_to_string(RT_PERIOD_PATH).ok()?.trim().parse().ok()
}

pub struct RtThrottleGuard {
    original: Option<i64>,
}

impl RtThrottleGuard {
    pub fn from_env() -> Self {
        if std::env::var("KOS_BENCH_RT_THROTTLE").as_deref() != Ok("off") {
            return RtThrottleGuard { original: None };
        }
        let original = read_rt_runtime();
        match std::fs::write(RT_RUNTIME_PATH, "-1") {
            Ok(_) => {
                eprintln!("[bench] RT throttling disabled (sched_rt_runtime_us=-1)");
                RtThrottleGuard { original }
            }
            Err(e) => {
                eprintln!("[bench] WARNING: cannot disable RT throttling: {e}");
                RtThrottleGuard { original: None }
            }
        }
    }
}

impl Drop for RtThrottleGuard {
    fn drop(&mut self) {
        if let Some(v) = self.original {
            if std::fs::write(RT_RUNTIME_PATH, v.to_string()).is_ok() {
                eprintln!("[bench] RT throttling restored (sched_rt_runtime_us={v})");
            }
        }
    }
}

fn read_trim(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

pub fn read_max_temp_c() -> Option<f64> {
    let mut pkg = None;
    let mut max = None::<f64>;
    for entry in std::fs::read_dir("/sys/class/thermal").ok()?.flatten() {
        let p = entry.path();
        let Some(t) = read_trim(&p.join("temp").to_string_lossy()).and_then(|s| s.parse::<f64>().ok()) else {
            continue;
        };
        let c = t / 1000.0;
        if read_trim(&p.join("type").to_string_lossy()).as_deref() == Some("x86_pkg_temp") {
            pkg = Some(c);
        }
        max = Some(max.map_or(c, |m: f64| m.max(c)));
    }
    pkg.or(max)
}

pub fn read_avg_mhz(cores: &[usize]) -> Option<f64> {
    let vals: Vec<f64> = cores
        .iter()
        .filter_map(|c| read_trim(&format!("/sys/devices/system/cpu/cpu{c}/cpufreq/scaling_cur_freq")))
        .filter_map(|s| s.parse::<f64>().ok())
        .map(|khz| khz / 1000.0)
        .collect();
    if vals.is_empty() { None } else { Some(vals.iter().sum::<f64>() / vals.len() as f64) }
}

pub fn thermal_snapshot(tag: &str) {
    let topo = topology();
    let temp = read_max_temp_c().map_or("?".into(), |t| format!("{t:.0}°C"));
    let crit = read_avg_mhz(&topo.critical).map_or("?".into(), |m| format!("{m:.0}MHz"));
    let nc = read_avg_mhz(&topo.nc).map_or("?".into(), |m| format!("{m:.0}MHz"));
    println!("   [thermal:{tag}] temp {temp}, critical clk {crit}, nc clk {nc}");
}

pub fn print_env_report() {
    let topo = topology();
    let cmdline = read_trim("/proc/cmdline").unwrap_or_default();
    let boot_params: Vec<&str> = cmdline
        .split_whitespace()
        .filter(|p| {
            ["isolcpus", "nohz_full", "rcu_nocbs", "irqaffinity", "intel_idle", "processor.max_cstate", "nosmt", "mitigations"]
                .iter()
                .any(|k| p.starts_with(k))
        })
        .collect();
    let turbo = match read_trim("/sys/devices/system/cpu/intel_pstate/no_turbo") {
        Some(v) => if v == "1" { "OFF (intel_pstate/no_turbo=1)".into() } else { "ON (intel_pstate/no_turbo=0)".to_string() },
        None => match read_trim("/sys/devices/system/cpu/cpufreq/boost") {
            Some(v) => if v == "1" { "ON (cpufreq/boost=1)".into() } else { "OFF (cpufreq/boost=0)".to_string() },
            None => "unknown".into(),
        },
    };
    let rt = match (read_rt_runtime(), read_rt_period()) {
        (Some(-1), _) => "OFF (sched_rt_runtime_us=-1)".to_string(),
        (Some(r), Some(p)) => format!("ON {r}/{p}μs (RT runs at most {:.0}% per CPU)", r as f64 / p as f64 * 100.0),
        _ => "unknown".into(),
    };

    println!("  ── Environment ──");
    println!("  kernel        : {}", read_trim("/proc/sys/kernel/osrelease").unwrap_or_default());
    println!("  boot params   : {}", if boot_params.is_empty() { "(no isolcpus/nohz_full/rcu_nocbs)".into() } else { boot_params.join(" ") });
    println!("  SMT           : {}", read_trim("/sys/devices/system/cpu/smt/active").map_or("unknown".into(), |v| if v == "1" { "ON".to_string() } else { "OFF".to_string() }));
    println!("  governor      : {}", read_trim("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor").unwrap_or("unknown".into()));
    println!("  turbo         : {turbo}");
    println!("  RT throttling : {rt}");
    println!("  timer slack   : measurement workers set to 1ns (PR_SET_TIMERSLACK)");
    println!("  phase         : {}", if env_flag("KOS_BENCH_ALIGNED") { "ALIGNED (KOS_BENCH_ALIGNED=1, worst case)" } else { "spread (workers on the same core split the period evenly)" });
    println!(
        "  topology      : critical {} / reserved(+HT sibling) {} / NC {}",
        format_cpu_list(&topo.critical),
        format_cpu_list(&topo.reserved),
        format_cpu_list(&topo.nc)
    );
    thermal_snapshot("start");
    println!();
}
