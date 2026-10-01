// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use kos_exec::{
    ThreadCallbacks, ThreadConfig, ThreadManager, Priority,
    cgroup,
};

mod common;

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const CRITICAL_CORE_COUNT: usize = 4;

const CRITICAL_PERIOD_MS: u64 = 10;
const CRITICAL_WORK_US: u64 = 2000;
const CRITICAL_PROCS_PER_CORE: usize = 2;

struct Stats {
    samples: Vec<u64>,
}

impl Stats {
    fn with_capacity(n: usize) -> Self {
        Self { samples: Vec::with_capacity(n) }
    }

    fn add(&mut self, ns: u64) {
        self.samples.push(ns);
    }

    fn compute(&mut self) -> StatResult {
        self.samples.sort_unstable();
        let n = self.samples.len();
        if n == 0 {
            return StatResult::default();
        }
        let sum: u64 = self.samples.iter().sum();
        let avg = sum as f64 / n as f64;
        let p50 = self.samples[n / 2];
        let p99 = self.samples[std::cmp::min((n as f64 * 0.99) as usize, n - 1)];
        let max = self.samples[n - 1];
        let min = self.samples[0];
        StatResult { count: n, min, avg, p50, p99, max }
    }
}

#[derive(Default, Clone)]
struct StatResult {
    count: usize,
    min: u64,
    avg: f64,
    p50: u64,
    p99: u64,
    max: u64,
}

fn cpu_burn(duration_us: u64) {
    common::cpu_burn(duration_us);
}

fn pin_thread_to_core(core: usize) {
    use nix::sched::{sched_setaffinity, CpuSet};
    use nix::unistd::Pid;
    let mut cpuset = CpuSet::new();
    let _ = cpuset.set(core);
    let _ = sched_setaffinity(Pid::from_raw(0), &cpuset);
}

fn num_cpus() -> usize {
    common::num_cpus()
}

fn set_rt_fifo(priority: u8) {
    use libc::{sched_param, sched_setscheduler, SCHED_FIFO};
    let param = sched_param {
        sched_priority: priority as i32,
    };
    let ret = unsafe { sched_setscheduler(0, SCHED_FIFO, &param) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        eprintln!("[bench] WARNING: SCHED_FIFO prio {priority} failed: {err}");
    }
}

fn reset_sched_other() {
    use libc::{sched_param, sched_setscheduler, SCHED_OTHER};
    let param = sched_param { sched_priority: 0 };
    unsafe { sched_setscheduler(0, SCHED_OTHER, &param) };
}

struct DefenseConfig {
    #[allow(dead_code)]
    label: &'static str,
    isolate: bool,
    cstate_off: bool,
    irq_isolate: bool,
    rt_fifo: bool,
    nc_nice: i32,
}

const DEFENSE_LEVELS: [DefenseConfig; 2] = [
    DefenseConfig {
        label: "Native",
        isolate: false,
        cstate_off: false,
        irq_isolate: false,
        rt_fifo: false,
        nc_nice: 0,
    },
    DefenseConfig {
        label: "KOS",
        isolate: true,
        cstate_off: true,
        irq_isolate: true,
        rt_fifo: true,
        nc_nice: 19,
    },
];

struct DefenseGuard {
    _cstate_guard: Option<cgroup::CstateGuard>,
    _pmqos_file: Option<std::fs::File>,
    _irq_guard: Option<cgroup::IrqAffinityGuard>,
}

impl DefenseGuard {
    fn setup(config: &DefenseConfig, total_cpus: usize) -> Self {
        let topo = common::topology();
        let reserved = topo.reserved_u32();

        let (cstate_guard, pmqos) = if config.cstate_off {
            if config.isolate {
                let guard = cgroup::disable_cstates_for_cores(&reserved);
                let global = if guard.is_none() { cgroup::disable_cstates() } else { None };
                (guard, global)
            } else {
                (None, cgroup::disable_cstates())
            }
        } else {
            (None, None)
        };

        let irq_guard = if config.irq_isolate && config.isolate {
            cgroup::isolate_irq_from_cores_guarded(&reserved, total_cpus as u32)
        } else {
            None
        };

        DefenseGuard { _cstate_guard: cstate_guard, _pmqos_file: pmqos, _irq_guard: irq_guard }
    }
}

struct MeasureResult {
    latency: StatResult,
    jitter: StatResult,
    miss_count: u64,
    total: usize,
    max_core: usize,
    max_idx: usize,
    max_of_total: usize,
}

fn self_exe() -> std::path::PathBuf {
    std::env::current_exe().expect("cannot determine self exe path")
}

fn spawn_nc_load(duty_pct: u32, pinned: bool, nice: i32) -> Vec<std::process::Child> {
    let mut children = Vec::new();
    let exe = self_exe();
    let nc_cores = &common::topology().nc;
    let period_us = CRITICAL_PERIOD_MS * 1000;

    let full = duty_pct / 100;
    let rem = duty_pct % 100;
    let mut jobs: Vec<(u64, u64)> = vec![(5000, 0); full as usize];
    if rem > 0 {
        let burn_us = period_us * rem as u64 / 100;
        jobs.push((burn_us, period_us - burn_us));
    }

    for &core in nc_cores {
        for &(burn_us, sleep_us) in &jobs {
            let mut cmd = if pinned {
                let mut c = std::process::Command::new("taskset");
                c.args(["-c", &core.to_string()]).arg(&exe);
                c
            } else {
                std::process::Command::new(&exe)
            };
            cmd.args(["--worker", &burn_us.to_string(), &sleep_us.to_string()])
                .env("KOS_BENCH_NICE", nice.to_string())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Ok(child) = cmd.spawn() {
                children.push(child);
            }
        }
    }

    thread::sleep(Duration::from_millis(200));
    children
}

fn kill_all(children: &mut Vec<std::process::Child>) {
    for child in children.iter_mut() {
        let _ = child.kill();
    }
    for child in children.iter_mut() {
        let _ = child.wait();
    }
    children.clear();
}

fn critical_worker_main(
    core: i32,
    period_ms: u64,
    work_us: u64,
    duration_ms: u64,
    rt_prio: u8,
    epoch_ns: u64,
    phase_ns: u64,
) {
    common::init_measure_worker();

    if core >= 0 {
        pin_thread_to_core(core as usize);
    }

    if rt_prio > 0 {
        set_rt_fifo(rt_prio);
    }

    let period_ns = period_ms * 1_000_000;
    let deadline_ns = (period_ms * 1000 - work_us) * 1000;

    let expected = (duration_ms / period_ms) as usize + 1;
    let mut latencies = Vec::with_capacity(expected);
    let mut jitters = Vec::with_capacity(expected);
    let mut miss_count = 0u64;
    let mut max_lat_ns = 0u64;
    let mut max_lat_idx = 0usize;

    let mut next_wake = epoch_ns + phase_ns;
    common::sleep_until_ns(next_wake);
    let mut last_wake = common::mono_now_ns();
    next_wake += period_ns;
    cpu_burn(work_us);

    let measure_end = next_wake + duration_ms * 1_000_000;

    while next_wake < measure_end {
        common::sleep_until_ns(next_wake);
        let actual_wake = common::mono_now_ns();

        let latency_ns = actual_wake.saturating_sub(next_wake);
        let actual_period = actual_wake - last_wake;
        let jitter_ns = actual_period.abs_diff(period_ns);

        if latency_ns > deadline_ns {
            miss_count += 1;
        }
        if latency_ns > max_lat_ns {
            max_lat_ns = latency_ns;
            max_lat_idx = latencies.len();
        }

        latencies.push(latency_ns);
        jitters.push(jitter_ns);

        last_wake = actual_wake;
        next_wake += period_ns;
        cpu_burn(work_us);
    }

    if rt_prio > 0 {
        reset_sched_other();
    }

    let core_id = if core >= 0 { core as usize } else { usize::MAX };

    println!("CRIT_MISS {}", miss_count);
    println!("CRIT_CORE {}", core_id);
    println!("CRIT_MAXIDX {} {}", max_lat_idx, latencies.len());

    let lat_str: String = latencies.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" ");
    println!("CRIT_LAT {}", lat_str);

    let jit_str: String = jitters.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" ");
    println!("CRIT_JIT {}", jit_str);
}

fn spawn_critical_workers(
    cores: &[i32],
    period_ms: u64,
    work_us: u64,
    duration_ms: u64,
    rt_prio: u8,
) -> Vec<std::process::Child> {
    let exe = self_exe();
    let mut children = Vec::new();
    let aligned = common::env_flag("KOS_BENCH_ALIGNED");
    let period_ns = period_ms * 1_000_000;
    let n = cores.len();

    let epoch_ns = common::mono_now_ns() + 100_000_000 + n as u64 * 5_000_000;

    for (i, &core) in cores.iter().enumerate() {
        let group = i % CRITICAL_CORE_COUNT;
        let slot = i / CRITICAL_CORE_COUNT;
        let in_group = (n - group).div_ceil(CRITICAL_CORE_COUNT);
        let phase_ns = if aligned { 0 } else { period_ns * slot as u64 / in_group as u64 };

        let child = std::process::Command::new(&exe)
            .args([
                "--critical-worker",
                &core.to_string(),
                &period_ms.to_string(),
                &work_us.to_string(),
                &duration_ms.to_string(),
                &rt_prio.to_string(),
                &epoch_ns.to_string(),
                &phase_ns.to_string(),
            ])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .expect("failed to spawn critical worker");
        children.push(child);
    }

    children
}

fn collect_critical_results(children: Vec<std::process::Child>) -> MeasureResult {
    let mut all_lats = Vec::new();
    let mut all_jits = Vec::new();
    let mut total_miss = 0u64;
    let mut global_max_ns = 0u64;
    let mut global_max_core = 0usize;
    let mut global_max_idx = 0usize;
    let mut global_max_total = 0usize;

    for child in children {
        let output = child.wait_with_output().expect("failed to wait for critical worker");
        let stdout = String::from_utf8_lossy(&output.stdout);

        let mut miss = 0u64;
        let mut core_id = 0usize;
        let mut max_idx = 0usize;
        let mut max_total = 0usize;
        let mut lats = Vec::new();
        let mut jits = Vec::new();

        for line in stdout.lines() {
            if let Some(rest) = line.strip_prefix("CRIT_MISS ") {
                miss = rest.trim().parse().unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("CRIT_CORE ") {
                core_id = rest.trim().parse().unwrap_or(0);
            } else if let Some(rest) = line.strip_prefix("CRIT_MAXIDX ") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() >= 2 {
                    max_idx = parts[0].parse().unwrap_or(0);
                    max_total = parts[1].parse().unwrap_or(0);
                }
            } else if let Some(rest) = line.strip_prefix("CRIT_LAT ") {
                lats = rest.split_whitespace()
                    .filter_map(|s| s.parse::<u64>().ok())
                    .collect();
            } else if let Some(rest) = line.strip_prefix("CRIT_JIT ") {
                jits = rest.split_whitespace()
                    .filter_map(|s| s.parse::<u64>().ok())
                    .collect();
            }
        }

        let worker_max_ns = lats.iter().copied().max().unwrap_or(0);
        if worker_max_ns > global_max_ns {
            global_max_ns = worker_max_ns;
            global_max_core = core_id;
            global_max_idx = max_idx;
            global_max_total = max_total;
        }

        total_miss += miss;
        all_lats.extend_from_slice(&lats);
        all_jits.extend_from_slice(&jits);
    }

    let total_count = all_lats.len();
    let lat_result = {
        let mut s = Stats::with_capacity(all_lats.len());
        for &v in &all_lats { s.add(v); }
        s.compute()
    };
    let jit_result = {
        let mut s = Stats::with_capacity(all_jits.len());
        for &v in &all_jits { s.add(v); }
        s.compute()
    };

    MeasureResult {
        latency: lat_result,
        jitter: jit_result,
        miss_count: total_miss,
        total: total_count,
        max_core: global_max_core,
        max_idx: global_max_idx,
        max_of_total: global_max_total,
    }
}

fn print_header() {
    println!(
        "   {:<20} │ {:^30} │ {:^30} │ {:>12}",
        "C-state", "Latency (μs)", "Jitter (μs)", "Deadline Miss"
    );
    println!(
        "   {:<20} │ {:>8} {:>8} {:>8}    │ {:>8} {:>8} {:>8}    │",
        "", "min", "avg", "max", "min", "avg", "max"
    );
    println!("   {}", "─".repeat(96));
}

fn print_result_row(label: &str, r: &MeasureResult) {
    let miss_pct = if r.total > 0 {
        r.miss_count as f64 / r.total as f64 * 100.0
    } else {
        0.0
    };
    let core_str = if r.max_core == usize::MAX {
        "any".to_string()
    } else {
        format!("c{}", r.max_core)
    };
    println!(
        "   {:<20} │ {:>8.1} {:>8.1} {:>8.1}    │ {:>8.1} {:>8.1} {:>8.1}    │ {:>4}/{:<4}({:.1}%)  max@{} #{}/{}",
        label,
        r.latency.min as f64 / 1000.0,
        r.latency.avg / 1000.0,
        r.latency.max as f64 / 1000.0,
        r.jitter.min as f64 / 1000.0,
        r.jitter.avg / 1000.0,
        r.jitter.max as f64 / 1000.0,
        r.miss_count, r.total, miss_pct,
        core_str, r.max_idx, r.max_of_total,
    );
}

fn bench_thread_manager_stress(test_duration_ms: u64) {
    println!("── Scenario 2. ThreadManager API stress (KOS execution engine) ──");
    println!("   (Critical 2ms + High 5ms + Normal 10ms + Low 20ms, concurrent)");
    println!("   measured: on_run() start interval - configured period (period jitter)\n");

    let tasks: [(&str, u64, u64, Priority); 4] = [
        ("Critical 2ms (300μs work)", 2, 300, Priority::Critical),
        ("High 5ms (1ms work)", 5, 1000, Priority::High),
        ("Normal 10ms (3ms work)", 10, 3000, Priority::Normal),
        ("Low 20ms (8ms work)", 20, 8000, Priority::Low),
    ];

    let mut mgr = ThreadManager::new();
    let mut collectors = Vec::new();

    for &(name, period_ms, work_us, prio) in &tasks {
        let jitters = Arc::new(Mutex::new(Vec::<u64>::new()));
        let runs = Arc::new(AtomicU64::new(0));
        let last_start = Arc::new(Mutex::new(None::<Instant>));
        let period = Duration::from_millis(period_ms);

        let (j, r, l) = (jitters.clone(), runs.clone(), last_start.clone());
        let config = ThreadConfig::periodic(name, period).with_priority(prio);
        let callbacks = ThreadCallbacks::new(move |_ctx| {
            let start = Instant::now();
            let mut last = l.lock().unwrap();
            if let Some(prev) = *last {
                let interval = start.duration_since(prev);
                j.lock().unwrap().push(interval.abs_diff(period).as_nanos() as u64);
            }
            *last = Some(start);
            drop(last);
            cpu_burn(work_us);
            r.fetch_add(1, Ordering::Relaxed);
        });
        mgr.register(config, callbacks).unwrap();
        collectors.push((name, jitters, runs));
    }

    mgr.start_all().unwrap();
    thread::sleep(Duration::from_millis(test_duration_ms));
    mgr.shutdown_all();

    println!("┌──────────────────────────────────────┬────────┬──────────┬────────┬────────┬──────────┬────────┐");
    println!(
        "│ {:<36} │ {:>6} │ {:>8} │ {:>6} │ {:>6} │ {:>8} │ {:>6} │",
        "Task", "min", "avg", "p50", "p99", "max", "count"
    );
    println!("├──────────────────────────────────────┼────────┼──────────┼────────┼────────┼──────────┼────────┤");

    for (name, jitters, runs) in &collectors {
        let raw = jitters.lock().unwrap();
        let run_count = runs.load(Ordering::Relaxed);

        if raw.len() < 3 {
            let label = format!("{name} (n={run_count})");
            println!("│ {:<36} │ (insufficient data)                                     │", label);
            continue;
        }

        let mut stats = Stats::with_capacity(raw.len());
        for &v in raw.iter() {
            stats.add(v);
        }
        let r = stats.compute();
        let label = format!("{name} (n={run_count})");
        println!(
            "│ {:<36} │ {:>6} │ {:>8.1} │ {:>6} │ {:>6} │ {:>8} │ {:>6} │",
            label, r.min, r.avg, r.p50, r.p99, r.max, r.count
        );
    }

    println!("└──────────────────────────────────────┴────────┴──────────┴────────┴────────┴──────────┴────────┘");
    println!("   (|on_run start interval - period|, in nanoseconds)");
}

fn bench_percore_cstate(test_duration_ms: u64) {
    println!("\n── Scenario 3. Per-core C-states: global vs per-core ──");
    println!("   (cores 0-3 = Critical, pinned, no load, comparing C-state modes)\n");

    let total_cpus = num_cpus();
    if total_cpus < CRITICAL_CORE_COUNT + 1 {
        println!("   SKIP: need at least {} CPU cores", CRITICAL_CORE_COUNT + 1);
        return;
    }

    struct CaseResult {
        label: &'static str,
        result: MeasureResult,
    }

    let topo = common::topology();
    let critical_cores_u32: Vec<u32> = topo.reserved_u32();
    let cases: Vec<(&str, Box<dyn FnOnce() -> (Option<std::fs::File>, Option<cgroup::CstateGuard>)>)> = vec![
        ("C-state ON (default)", Box::new(|| (None, None))),
        ("Global OFF (PM QoS)", Box::new(|| (cgroup::disable_cstates(), None))),
        ("Per-core OFF (Critical only)", Box::new({
            let cores = critical_cores_u32.clone();
            move || (None, cgroup::disable_cstates_for_cores(&cores))
        })),
    ];

    let mut results = Vec::new();

    for (label, setup_fn) in cases {
        let (_global_guard, _cstate_guard) = setup_fn();

        if label.starts_with("Per-core") && _cstate_guard.is_none() {
            println!("   {label}: SKIP (cpuidle sysfs not accessible, requires root)");
            continue;
        }
        if label.starts_with("Global") && _global_guard.is_none() {
            println!("   {label}: SKIP (requires root)");
            continue;
        }

        let cores: Vec<i32> = (0..CRITICAL_CORE_COUNT * CRITICAL_PROCS_PER_CORE)
            .map(|i| topo.critical[i % topo.critical.len()] as i32)
            .collect();
        let children = spawn_critical_workers(
            &cores,
            CRITICAL_PERIOD_MS,
            CRITICAL_WORK_US,
            test_duration_ms,
            0,
        );
        let result = collect_critical_results(children);

        results.push(CaseResult {
            label: Box::leak(label.to_string().into_boxed_str()),
            result,
        });
    }

    print_header();
    for r in &results {
        print_result_row(r.label, &r.result);
    }
    println!();
    println!("   → if global OFF ≈ per-core OFF, non-critical cores can keep power saving with the per-core mode");
}

const SCALING_PERIOD_MS: u64 = 10;
const SCALING_WORK_US: u64 = 2000;

fn measure_critical_scaling(
    num_critical: usize,
    nc_duty_pct: u32,
    test_duration_ms: u64,
    total_cpus: usize,
    config: &DefenseConfig,
) -> MeasureResult {
    let _guard = DefenseGuard::setup(config, total_cpus);

    let mut nc_procs = if nc_duty_pct > 0 {
        spawn_nc_load(nc_duty_pct, config.isolate, config.nc_nice)
    } else {
        Vec::new()
    };

    let topo = common::topology();
    let critical_cores: Vec<i32> = if config.isolate {
        (0..num_critical).map(|i| topo.critical[i % topo.critical.len()] as i32).collect()
    } else {
        vec![-1; num_critical]
    };

    let rt_prio = if config.rt_fifo { 80u8 } else { 0u8 };

    let children = spawn_critical_workers(
        &critical_cores,
        SCALING_PERIOD_MS,
        SCALING_WORK_US,
        test_duration_ms,
        rt_prio,
    );

    let result = collect_critical_results(children);
    kill_all(&mut nc_procs);
    result
}

fn bench_critical_scaling(test_duration_ms: u64) {
    let total_cpus = num_cpus();
    let topo = common::topology();

    println!("\n── Scenario 4. Native vs KOS: scaling critical processes + NC load ──");
    println!("   Critical Domain: cores {} (KOS isolates the cores, HT siblings {} reserved)",
        common::format_cpu_list(&topo.critical), common::format_cpu_list(&topo.reserved));
    println!("   Non-Critical Domain: cores {} ({} cores)",
        common::format_cpu_list(&topo.nc), topo.nc.len());
    println!("   Critical processes: 1–24 (round-robin over cores)");
    println!("   each process: {}ms period, {}μs work ({}% utilization) — realistic vehicle ECU load",
        SCALING_PERIOD_MS, SCALING_WORK_US,
        SCALING_WORK_US * 100 / (SCALING_PERIOD_MS * 1000));
    println!("   NC load: 0%, 50%, 100%, 150%, 200%");
    println!("   compared: Native (no configuration) / KOS (isolation + C-state OFF + IRQ isolation + FIFO 80 + NC nice 19)");
    println!("   ※ deadline misses are unavoidable when total utilization per core > 100%\n");

    if topo.nc.is_empty() {
        println!("   SKIP: no NC cores left after reserving critical cores + HT siblings");
        return;
    }

    let critical_counts = [1, 2, 3, 4, 5, 6, 7, 8, 10, 12, 15, 16, 20, 24];
    let nc_loads = [0u32, 50, 100, 150, 200];

    for &nc_load in &nc_loads {
        let nc_desc = if nc_load == 0 {
            "No NC load".to_string()
        } else if nc_load <= 100 {
            format!("NC {}% load (1 process per core)", nc_load)
        } else {
            let per_core = (nc_load as f64 / 100.0).ceil() as usize;
            format!("NC {}% load ({} processes per core)", nc_load, per_core)
        };
        println!("   ┌─ [{nc_desc}]");
        println!(
            "   {:>5} {:>5} │ {:>8} {:>10} {:>10} │ {:>8} {:>10} {:>10}",
            "#Crit", "util%",
            "miss%", "avg_us", "p99_us",
            "miss%", "avg_us", "p99_us",
        );
        println!(
            "   {:>5} {:>5} │ {:^30} │ {:^30}",
            "", "", "Native", "KOS",
        );
        println!("   {}", "─".repeat(80));

        for &n_crit in &critical_counts {
            let procs_per_core = (n_crit as f64 / CRITICAL_CORE_COUNT as f64).ceil() as usize;
            let total_util = procs_per_core as u64 * SCALING_WORK_US * 100 / (SCALING_PERIOD_MS * 1000);

            let mut miss_pcts = [0.0f64; 2];
            let mut avg_lats = [0.0f64; 2];
            let mut p99_lats = [0.0f64; 2];

            for (di, config) in DEFENSE_LEVELS.iter().enumerate() {
                let r = measure_critical_scaling(n_crit, nc_load, test_duration_ms, total_cpus, config);
                miss_pcts[di] = if r.total > 0 {
                    r.miss_count as f64 / r.total as f64 * 100.0
                } else {
                    0.0
                };
                avg_lats[di] = r.latency.avg / 1000.0;
                p99_lats[di] = r.latency.p99 as f64 / 1000.0;
            }

            println!(
                "   {:>5} {:>4}% │ {:>7.1}% {:>10.1} {:>10.1} │ {:>7.1}% {:>10.1} {:>10.1}",
                n_crit, total_util,
                miss_pcts[0], avg_lats[0], p99_lats[0],
                miss_pcts[1], avg_lats[1], p99_lats[1],
            );
        }
        common::thermal_snapshot(&format!("NC {nc_load}%"));
        println!();
    }

    println!("   ※ #Crit: number of critical processes, util%: total CPU utilization per core");
    println!("     processes per core = ceil(#Crit/4), 20% utilization each");
    println!("     Native = no configuration, KOS = core isolation + C-state OFF + IRQ isolation + SCHED_FIFO 80 + NC nice 19");
    println!("     miss = wake latency > slack ({}μs = period - work)", SCALING_PERIOD_MS * 1000 - SCALING_WORK_US);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 4 && args[1] == "--worker" {
        common::apply_worker_nice_from_env();
        let burn_us: u64 = args[2].parse().unwrap_or(5000);
        let sleep_us: u64 = args[3].parse().unwrap_or(0);
        if sleep_us == 0 {
            loop {
                std::hint::black_box(0u64.wrapping_mul(42));
            }
        } else {
            loop {
                cpu_burn(burn_us);
                thread::sleep(Duration::from_micros(sleep_us));
            }
        }
    }

    if args.len() >= 9 && args[1] == "--critical-worker" {
        let core: i32 = args[2].parse().unwrap_or(-1);
        let period_ms: u64 = args[3].parse().unwrap_or(5);
        let work_us: u64 = args[4].parse().unwrap_or(4500);
        let duration_ms: u64 = args[5].parse().unwrap_or(2000);
        let rt_prio: u8 = args[6].parse().unwrap_or(0);
        let epoch_ns: u64 = args[7].parse().unwrap_or_else(|_| common::mono_now_ns());
        let phase_ns: u64 = args[8].parse().unwrap_or(0);
        critical_worker_main(core, period_ms, work_us, duration_ms, rt_prio, epoch_ns, phase_ns);
        return;
    }

    let test_duration_ms = common::env_u64("KOS_BENCH_DURATION_MS", 2000);
    let _rt_guard = common::RtThrottleGuard::from_env();

    let total_cpus = num_cpus();
    let topo = common::topology();

    println!();
    println!("══════════════════════════════════════════════════════════════════");
    println!("  KOS Exec Deadline & Domain Isolation Benchmark");
    println!("  test duration: {}ms per measurement", test_duration_ms);
    println!("  CPU cores: {} (Critical: {}, reserved: {}, Non-Critical: {})",
        total_cpus, topo.critical.len(), topo.reserved.len(), topo.nc.len());
    println!();
    println!("  Core layout (KOS):");
    println!("  Critical:     cores {} (+HT siblings reserved: {})",
        common::format_cpu_list(&topo.critical), common::format_cpu_list(&topo.reserved));
    println!("  Non-Critical: cores {} ({} cores)", common::format_cpu_list(&topo.nc), topo.nc.len());
    println!();
    println!("  Compared:");
    println!("  Native  (no isolation, default C-states, default IRQs, CFS)");
    println!("  KOS     (isolated, C-state OFF, IRQ isolation, SCHED_FIFO, NC nice 19)");
    println!("  * RT = SCHED_FIFO prio 80 (same as KOS app_manager)");
    println!("  * Critical tasks run as separate processes (same as real KOS)");
    println!("  * IRQ isolation: little effect on this machine; needs verification on a vehicle ECU");
    println!("══════════════════════════════════════════════════════════════════");
    common::print_env_report();

    bench_critical_scaling(test_duration_ms);
    bench_thread_manager_stress(test_duration_ms);
    bench_percore_cstate(test_duration_ms);

    println!();
    common::thermal_snapshot("end");
    println!("══════════════════════════════════════════════════════════════════");
    println!("  Done");
    println!("══════════════════════════════════════════════════════════════════");
}
