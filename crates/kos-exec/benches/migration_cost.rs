// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

mod common;

use std::io::{BufRead, BufReader};
use std::time::Duration;
use std::thread;

use nix::sched::{sched_setaffinity, CpuSet};
use nix::unistd::Pid;

use kos_exec::cgroup;

const SPIKE_WINDOW: usize = 5;

const NC_NICE: i32 = 19;

fn cpu_burn(duration_us: u64) {
    common::cpu_burn(duration_us);
}

fn pin_to_core(core: usize) {
    let mut cpuset = CpuSet::new();
    let _ = cpuset.set(core);
    let _ = sched_setaffinity(Pid::from_raw(0), &cpuset);
}

fn pin_pid_to_core(pid: i32, core: usize) {
    let mut cpuset = CpuSet::new();
    let _ = cpuset.set(core);
    let _ = sched_setaffinity(Pid::from_raw(pid), &cpuset);
}

fn set_rt_fifo(priority: u8) {
    use libc::{sched_param, sched_setscheduler, SCHED_FIFO};
    let param = sched_param { sched_priority: priority as i32 };
    unsafe { sched_setscheduler(0, SCHED_FIFO, &param) };
}

fn self_exe() -> std::path::PathBuf {
    std::env::current_exe().expect("cannot determine self exe path")
}

fn median_u64(v: &mut [u64]) -> u64 {
    v.sort_unstable();
    v[v.len() / 2]
}

fn median_f64(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn migration_worker_main(initial_core: i32, period_ms: u64, work_us: u64, rt_prio: u8) {
    common::init_measure_worker();
    if initial_core >= 0 {
        pin_to_core(initial_core as usize);
    }
    if rt_prio > 0 {
        set_rt_fifo(rt_prio);
    }

    let period_ns = period_ms * 1_000_000;

    println!("READY {}", std::process::id());

    let mut next_wake = common::mono_now_ns() + 50_000_000 + period_ns;
    let mut idx: u64 = 0;

    loop {
        common::sleep_until_ns(next_wake);
        let latency_ns = common::mono_now_ns().saturating_sub(next_wake);

        let current_core = unsafe { libc::sched_getcpu() } as usize;

        println!("LAT {} {} {}", idx, latency_ns, current_core);

        next_wake += period_ns;
        cpu_burn(work_us);
        idx += 1;
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Timing {
    Running,
    Sleeping,
}

struct MigrationResult {
    baseline_avg_ns: f64,
    spike_ns: u64,
    window_max_ns: u64,
    spike_vs_baseline: f64,
    recovery_periods: usize,
    post_avg_ns: f64,
    runs: usize,
}

#[allow(clippy::too_many_arguments)]
fn run_migration_once(
    from_core: usize,
    to_core: usize,
    period_ms: u64,
    work_us: u64,
    rt_prio: u8,
    baseline_periods: usize,
    post_periods: usize,
    timing: Timing,
) -> Option<MigrationResult> {
    let exe = self_exe();

    let mut child = std::process::Command::new(&exe)
        .args([
            "--migration-worker",
            &(from_core as i32).to_string(),
            &period_ms.to_string(),
            &work_us.to_string(),
            &rt_prio.to_string(),
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .spawn()
        .expect("failed to spawn migration worker");

    let stdout = child.stdout.take().unwrap();
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    let parse_lat = |line: &str| -> Option<(u64, usize)> {
        let rest = line.strip_prefix("LAT ")?;
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() < 3 {
            return None;
        }
        Some((parts[1].parse().ok()?, parts[2].parse().ok()?))
    };

    let pid: i32 = loop {
        match lines.next() {
            Some(Ok(line)) => {
                if let Some(rest) = line.strip_prefix("READY ") {
                    break rest.trim().parse().unwrap_or(0);
                }
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    };

    let mut baseline_lats: Vec<u64> = Vec::with_capacity(baseline_periods);
    while baseline_lats.len() < baseline_periods {
        match lines.next() {
            Some(Ok(line)) => {
                if let Some((lat, _)) = parse_lat(&line) {
                    baseline_lats.push(lat);
                }
            }
            _ => break,
        }
    }

    if baseline_lats.is_empty() {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }

    let baseline_avg = baseline_lats.iter().sum::<u64>() as f64 / baseline_lats.len() as f64;
    let mut sorted_baseline = baseline_lats.clone();
    sorted_baseline.sort_unstable();
    let baseline_p99 = sorted_baseline[std::cmp::min(
        (sorted_baseline.len() as f64 * 0.99) as usize,
        sorted_baseline.len() - 1,
    )];

    let wait = match timing {
        Timing::Running => Duration::from_micros(work_us / 2),
        Timing::Sleeping => {
            let sleep = Duration::from_millis(period_ms) - Duration::from_micros(work_us);
            Duration::from_micros(work_us) + sleep / 2
        }
    };
    thread::sleep(wait);
    pin_pid_to_core(pid, to_core);

    let mut post_lats: Vec<(u64, usize)> = Vec::with_capacity(post_periods);
    while post_lats.len() < post_periods {
        match lines.next() {
            Some(Ok(line)) => {
                if let Some(v) = parse_lat(&line) {
                    post_lats.push(v);
                }
            }
            _ => break,
        }
    }

    let _ = child.kill();
    let _ = child.wait();

    let first_on_new = post_lats.iter().position(|&(_, core)| core == to_core)?;
    let after: Vec<u64> = post_lats[first_on_new..].iter().map(|&(l, _)| l).collect();
    if after.is_empty() {
        return None;
    }

    let spike_ns = after.iter().take(SPIKE_WINDOW).copied().max().unwrap_or(0);
    let window_max_ns = post_lats.iter().map(|&(l, _)| l).max().unwrap_or(0);
    let spike_ratio = if baseline_avg > 0.0 { spike_ns as f64 / baseline_avg } else { 0.0 };

    let recovery_threshold = baseline_p99 * 2;
    let recovery_periods = after
        .iter()
        .position(|&lat| lat <= recovery_threshold)
        .unwrap_or(after.len());

    let stable_start = std::cmp::min(recovery_periods + SPIKE_WINDOW, after.len());
    let stable = if stable_start < after.len() { &after[stable_start..] } else { &after[..] };
    let post_avg = stable.iter().sum::<u64>() as f64 / stable.len() as f64;

    Some(MigrationResult {
        baseline_avg_ns: baseline_avg,
        spike_ns,
        window_max_ns,
        spike_vs_baseline: spike_ratio,
        recovery_periods,
        post_avg_ns: post_avg,
        runs: 1,
    })
}

#[allow(clippy::too_many_arguments)]
fn run_migration_test(
    from_core: usize,
    to_core: usize,
    period_ms: u64,
    work_us: u64,
    rt_prio: u8,
    baseline_periods: usize,
    post_periods: usize,
    timing: Timing,
) -> Option<MigrationResult> {
    let repeat = common::env_u64("KOS_BENCH_REPEAT", 5).max(1) as usize;
    let results: Vec<MigrationResult> = (0..repeat)
        .filter_map(|_| {
            run_migration_once(from_core, to_core, period_ms, work_us, rt_prio, baseline_periods, post_periods, timing)
        })
        .collect();
    if results.is_empty() {
        return None;
    }

    let f = |g: fn(&MigrationResult) -> f64| median_f64(&mut results.iter().map(g).collect::<Vec<_>>());
    let baseline_avg_ns = f(|r| r.baseline_avg_ns);
    let spike_vs_baseline = f(|r| r.spike_vs_baseline);
    let post_avg_ns = f(|r| r.post_avg_ns);
    let u = |g: fn(&MigrationResult) -> u64| median_u64(&mut results.iter().map(g).collect::<Vec<_>>());
    let spike_ns = u(|r| r.spike_ns);
    let window_max_ns = u(|r| r.window_max_ns);
    let recovery_periods = u(|r| r.recovery_periods as u64) as usize;

    Some(MigrationResult {
        baseline_avg_ns,
        spike_ns,
        window_max_ns,
        spike_vs_baseline,
        recovery_periods,
        post_avg_ns,
        runs: results.len(),
    })
}

fn print_table_header(first_col: &str) {
    println!(
        "   {:>14} │ {:>12} {:>12} {:>12} {:>8} {:>8} {:>12} {:>3}",
        first_col, "baseline", "spike", "window max", "ratio", "recovery", "avg after", "n"
    );
    println!("   {}", "─".repeat(100));
}

fn print_row(label: &str, r: &MigrationResult) {
    println!(
        "   {:>14} │ {:>10.1}ns {:>10.1}ns {:>10.1}ns {:>7.1}x {:>8} {:>10.1}ns {:>3}",
        label,
        r.baseline_avg_ns,
        r.spike_ns as f64,
        r.window_max_ns as f64,
        r.spike_vs_baseline,
        r.recovery_periods,
        r.post_avg_ns,
        r.runs
    );
}

fn bench_migration_cost() {
    let topo = common::topology();
    let total_cpus = topo.total;
    let repeat = common::env_u64("KOS_BENCH_REPEAT", 5);

    println!();
    println!("══════════════════════════════════════════════════════════════════");
    println!("  KOS Core Migration Cost Benchmark");
    println!("  CPU: {} cores, {} repetitions (median per metric)", total_cpus, repeat);
    println!("  spike = max of the first {} periods on the new core, window max = max over the whole window after the move", SPIKE_WINDOW);
    println!("══════════════════════════════════════════════════════════════════");
    common::print_env_report();

    let reserved = topo.reserved_u32();
    let _rt_guard = common::RtThrottleGuard::from_env();
    let _cstate_guard = cgroup::disable_cstates_for_cores(&reserved);
    let _pmqos = if _cstate_guard.is_none() { cgroup::disable_cstates() } else { None };
    let _irq_guard = cgroup::isolate_irq_from_cores_guarded(&reserved, total_cpus as u32);

    println!();
    println!("── Test 1. Migration cost by period (core 0 → core 2) ──");
    println!("   setup: KOS (RT FIFO prio 80), moved while running");
    println!();
    print_table_header("period/work");

    let test_configs: Vec<(u64, u64)> = vec![
        (1, 200),
        (2, 400),
        (5, 1000),
        (10, 2000),
        (20, 4000),
        (50, 10000),
    ];

    for &(period_ms, work_us) in &test_configs {
        let baseline_n = std::cmp::max(200, (2000 / period_ms) as usize);
        let post_n = std::cmp::max(100, (2000 / period_ms) as usize);

        if let Some(r) = run_migration_test(0, 2, period_ms, work_us, 80, baseline_n, post_n, Timing::Running) {
            print_row(&format!("{}ms/{}μs", period_ms, work_us), &r);
        }
    }

    println!();
    println!("── Test 2. Move timing: while running vs while sleeping (10ms period) ──");
    println!();
    print_table_header("timing");

    for (label, timing) in [("running", Timing::Running), ("sleeping", Timing::Sleeping)] {
        if let Some(r) = run_migration_test(0, 2, 10, 2000, 80, 200, 200, timing) {
            print_row(label, &r);
        }
    }

    println!();
    println!("── Test 3. Migration cost: RT FIFO vs CFS (10ms period) ──");
    println!("   (timer slack 1ns in both cases — compares schedulers only, not slack)");
    println!();
    print_table_header("scheduler");

    for (label, prio) in [("CFS", 0u8), ("RT FIFO", 80u8)] {
        if let Some(r) = run_migration_test(0, 2, 10, 2000, prio, 200, 200, Timing::Running) {
            print_row(label, &r);
        }
    }

    println!();
    println!("── Test 4. Migration cost by core distance (10ms period, RT FIFO) ──");
    println!("   HT sibling on the same physical core vs another physical core");
    println!();
    print_table_header("path");

    let mut core_pairs: Vec<(usize, usize, String)> = Vec::new();
    if let Some(&ht) = common::smt_siblings(0).iter().find(|&&c| c != 0) {
        core_pairs.push((0, ht, format!("0→{ht} (HT pair)")));
    } else {
        println!("   (no SMT — HT pair skipped)");
    }
    for to in [1usize, 2, 3] {
        core_pairs.push((0, to, format!("0→{to}")));
    }

    for (from, to, label) in &core_pairs {
        if *to < total_cpus {
            if let Some(r) = run_migration_test(*from, *to, 10, 2000, 80, 200, 200, Timing::Running) {
                print_row(label, &r);
            }
        }
    }

    println!();
    println!("── Test 5. Migration cost under NC load (10ms period, RT FIFO) ──");
    println!("   NC cores {} (excluding HT siblings), nice {}", common::format_cpu_list(&topo.nc), NC_NICE);
    println!();
    print_table_header("NC load");

    for nc_load in [0u32, 100, 200] {
        let mut nc_children = if nc_load > 0 {
            spawn_nc_load(nc_load, &topo.nc)
        } else {
            Vec::new()
        };

        if let Some(r) = run_migration_test(0, 2, 10, 2000, 80, 200, 200, Timing::Running) {
            print_row(&format!("{}%", nc_load), &r);
        }

        kill_all(&mut nc_children);
    }

    println!();
    common::thermal_snapshot("end");
    println!("══════════════════════════════════════════════════════════════════");
    println!("  Done");
    println!("══════════════════════════════════════════════════════════════════");
}

fn spawn_nc_load(duty_pct: u32, nc_cores: &[usize]) -> Vec<std::process::Child> {
    let mut children = Vec::new();
    let exe = self_exe();
    let period_us: u64 = 10_000;

    let mut jobs: Vec<(u64, u64)> = vec![(5000, 0); (duty_pct / 100) as usize];
    let rem = duty_pct % 100;
    if rem > 0 {
        let burn_us = period_us * rem as u64 / 100;
        jobs.push((burn_us, period_us - burn_us));
    }

    for &core in nc_cores {
        for &(burn_us, sleep_us) in &jobs {
            let mut cmd = std::process::Command::new("taskset");
            cmd.args(["-c", &core.to_string()])
                .arg(&exe)
                .args(["--worker", &burn_us.to_string(), &sleep_us.to_string()])
                .env("KOS_BENCH_NICE", NC_NICE.to_string())
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

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 4 && args[1] == "--worker" {
        common::apply_worker_nice_from_env();
        let burn_us: u64 = args[2].parse().unwrap_or(5000);
        let sleep_us: u64 = args[3].parse().unwrap_or(0);
        if sleep_us == 0 {
            loop { std::hint::black_box(0u64.wrapping_mul(42)); }
        } else {
            loop {
                cpu_burn(burn_us);
                thread::sleep(Duration::from_micros(sleep_us));
            }
        }
    }

    if args.len() >= 6 && args[1] == "--migration-worker" {
        let core: i32 = args[2].parse().unwrap_or(-1);
        let period_ms: u64 = args[3].parse().unwrap_or(10);
        let work_us: u64 = args[4].parse().unwrap_or(2000);
        let rt_prio: u8 = args[5].parse().unwrap_or(0);
        migration_worker_main(core, period_ms, work_us, rt_prio);
        return;
    }

    bench_migration_cost();
}
