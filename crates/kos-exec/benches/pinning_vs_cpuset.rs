// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::Duration;
use std::thread;
use std::path::{Path, PathBuf};
use std::fs;

use nix::sched::{sched_setaffinity, CpuSet};
use nix::unistd::Pid;

use kos_exec::cgroup;

const SPIKE_WINDOW: usize = 5;
const NC_NICE: i32 = 19;
const CGROUP_BASE: &str = "/sys/fs/cgroup";
const BENCH_SLICE: &str = "kos_bench.slice";

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

fn num_cpus() -> usize {
    common::num_cpus()
}

fn self_exe() -> std::path::PathBuf {
    std::env::current_exe().expect("cannot determine self exe path")
}

fn setup_cpuset_cgroup() -> bool {
    let slice_path = PathBuf::from(CGROUP_BASE).join(BENCH_SLICE);

    if !slice_path.exists() {
        if fs::create_dir_all(&slice_path).is_err() {
            eprintln!("[cpuset] WARN: cannot create {}", slice_path.display());
            return false;
        }
    }

    let root_ctrl = PathBuf::from(CGROUP_BASE).join("cgroup.subtree_control");
    let _ = fs::write(&root_ctrl, "+cpuset");

    let slice_ctrl = slice_path.join("cgroup.subtree_control");
    let _ = fs::write(&slice_ctrl, "+cpuset");

    let controllers = fs::read_to_string(slice_path.join("cgroup.controllers"))
        .unwrap_or_default();
    if !controllers.contains("cpuset") {
        eprintln!("[cpuset] WARN: cpuset controller not available in {}", BENCH_SLICE);
        return false;
    }

    true
}

fn create_cpuset_group(name: &str, cpus: &str) -> Option<PathBuf> {
    let group_path = PathBuf::from(CGROUP_BASE).join(BENCH_SLICE).join(name);

    if !group_path.exists() {
        if fs::create_dir_all(&group_path).is_err() {
            eprintln!("[cpuset] WARN: cannot create {}", group_path.display());
            return None;
        }
    }

    if fs::write(group_path.join("cpuset.cpus"), cpus).is_err() {
        eprintln!("[cpuset] WARN: cannot set cpuset.cpus={} for {}", cpus, name);
        return None;
    }

    Some(group_path)
}

fn move_to_cpuset(group_path: &Path, pid: u32) -> bool {
    fs::write(group_path.join("cgroup.procs"), pid.to_string()).is_ok()
}

fn change_cpuset_cores(group_path: &Path, cpus: &str) -> bool {
    fs::write(group_path.join("cpuset.cpus"), cpus).is_ok()
}

fn cleanup_cpuset_group(name: &str) {
    let group_path = PathBuf::from(CGROUP_BASE).join(BENCH_SLICE).join(name);
    if let Ok(procs) = fs::read_to_string(group_path.join("cgroup.procs")) {
        for pid_str in procs.lines() {
            let root_procs = PathBuf::from(CGROUP_BASE).join("cgroup.procs");
            let _ = fs::write(&root_procs, pid_str);
        }
    }
    let _ = fs::remove_dir(&group_path);
}

fn cleanup_bench_slice() {
    let slice_path = PathBuf::from(CGROUP_BASE).join(BENCH_SLICE);
    if let Ok(entries) = fs::read_dir(&slice_path) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = entry.file_name();
                cleanup_cpuset_group(name.to_str().unwrap_or(""));
            }
        }
    }
    let _ = fs::remove_dir(&slice_path);
}

fn worker_main(initial_core: i32, period_ms: u64, work_us: u64, rt_prio: u8) {
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

struct WorkerHandle {
    child: Child,
    pid: i32,
    lines: std::io::Lines<BufReader<std::process::ChildStdout>>,
}

fn spawn_worker(period_ms: u64, work_us: u64, rt_prio: u8, use_pinning: bool, initial_core: usize) -> Option<WorkerHandle> {
    let exe = self_exe();

    let core_arg = if use_pinning {
        initial_core as i32
    } else {
        -1
    };

    let mut child = Command::new(&exe)
        .args([
            "--pvc-worker",
            &core_arg.to_string(),
            &period_ms.to_string(),
            &work_us.to_string(),
            &rt_prio.to_string(),
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let reader = BufReader::new(stdout);
    let mut lines = reader.lines();

    let pid: i32;
    loop {
        if let Some(Ok(line)) = lines.next() {
            if let Some(rest) = line.strip_prefix("READY ") {
                pid = rest.trim().parse().unwrap_or(0);
                break;
            }
        } else {
            let _ = child.kill();
            return None;
        }
    }

    Some(WorkerHandle { child, pid, lines })
}

fn collect_lats(handle: &mut WorkerHandle, count: usize) -> Vec<(u64, u64, usize)> {
    let mut lats = Vec::with_capacity(count);
    for _ in 0..count {
        if let Some(Ok(line)) = handle.lines.next() {
            if let Some(rest) = line.strip_prefix("LAT ") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() >= 3 {
                    let idx: u64 = parts[0].parse().unwrap_or(0);
                    let lat: u64 = parts[1].parse().unwrap_or(0);
                    let core: usize = parts[2].parse().unwrap_or(0);
                    lats.push((idx, lat, core));
                }
            }
        }
    }
    lats
}

fn kill_worker(handle: &mut WorkerHandle) {
    let _ = handle.child.kill();
    let _ = handle.child.wait();
}

fn kill_all(children: &mut Vec<Child>) {
    for c in children.iter_mut() { let _ = c.kill(); }
    for c in children.iter_mut() { let _ = c.wait(); }
    children.clear();
}

struct LatStats {
    avg: f64,
    p50: u64,
    p99: u64,
    max: u64,
}

fn compute_stats(lats: &[u64]) -> LatStats {
    if lats.is_empty() {
        return LatStats { avg: 0.0, p50: 0, p99: 0, max: 0 };
    }
    let sum: u64 = lats.iter().sum();
    let avg = sum as f64 / lats.len() as f64;
    let mut sorted = lats.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[(sorted.len() as f64 * 0.50) as usize];
    let p99 = sorted[((sorted.len() as f64 * 0.99) as usize).min(sorted.len() - 1)];
    let max = *sorted.last().unwrap();
    LatStats { avg, p50, p99, max }
}

fn collect_lats_parallel(handles: &mut [WorkerHandle], count: usize) -> Vec<Vec<(u64, u64, usize)>> {
    thread::scope(|scope| {
        let joins: Vec<_> = handles
            .iter_mut()
            .map(|h| scope.spawn(move || collect_lats(h, count)))
            .collect();
        joins.into_iter().map(|j| j.join().unwrap_or_default()).collect()
    })
}

struct MigStats {
    baseline_avg: f64,
    spike: u64,
    window_max: u64,
    ratio: f64,
    recovery: usize,
    post_avg: f64,
}

fn wait_for_timing(period_ms: u64, work_us: u64, during_sleep: bool) {
    let wait = if during_sleep {
        let sleep = Duration::from_millis(period_ms) - Duration::from_micros(work_us);
        Duration::from_micros(work_us) + sleep / 2
    } else {
        Duration::from_micros(work_us / 2)
    };
    thread::sleep(wait);
}

fn analyze_migration(baseline: &[(u64, u64, usize)], post: &[(u64, u64, usize)], to_core: usize) -> Option<MigStats> {
    let baseline_vals: Vec<u64> = baseline.iter().map(|&(_, l, _)| l).collect();
    if baseline_vals.is_empty() {
        return None;
    }
    let bstats = compute_stats(&baseline_vals);

    let first_on_new = post.iter().position(|&(_, _, c)| c == to_core)?;
    let after: Vec<u64> = post[first_on_new..].iter().map(|&(_, l, _)| l).collect();
    let spike = after.iter().take(SPIKE_WINDOW).copied().max()?;
    let window_max = post.iter().map(|&(_, l, _)| l).max().unwrap_or(0);
    let ratio = if bstats.avg > 0.0 { spike as f64 / bstats.avg } else { 0.0 };
    let threshold = bstats.p99 * 2;
    let recovery = after.iter().position(|&l| l <= threshold).unwrap_or(after.len());
    let stable_start = std::cmp::min(recovery + SPIKE_WINDOW, after.len());
    let stable = if stable_start < after.len() { &after[stable_start..] } else { &after[..] };
    let post_avg = stable.iter().sum::<u64>() as f64 / stable.len() as f64;

    Some(MigStats { baseline_avg: bstats.avg, spike, window_max, ratio, recovery, post_avg })
}

fn median_mig(results: &[MigStats]) -> Option<MigStats> {
    if results.is_empty() {
        return None;
    }
    fn med_f(mut v: Vec<f64>) -> f64 { v.sort_by(|a, b| a.partial_cmp(b).unwrap()); v[v.len() / 2] }
    fn med_u(mut v: Vec<u64>) -> u64 { v.sort_unstable(); v[v.len() / 2] }
    Some(MigStats {
        baseline_avg: med_f(results.iter().map(|r| r.baseline_avg).collect()),
        spike: med_u(results.iter().map(|r| r.spike).collect()),
        window_max: med_u(results.iter().map(|r| r.window_max).collect()),
        ratio: med_f(results.iter().map(|r| r.ratio).collect()),
        recovery: med_u(results.iter().map(|r| r.recovery as u64).collect()) as usize,
        post_avg: med_f(results.iter().map(|r| r.post_avg).collect()),
    })
}

fn print_mig_header() {
    println!("   {:>18} │ {:>10} {:>10} {:>10} {:>7} {:>5} {:>10} {:>3}",
        "method", "baseline", "spike", "window max", "ratio", "recovery", "avg after", "n");
    println!("   {}", "─".repeat(90));
}

fn print_mig_row(label: &str, results: &[MigStats]) {
    if let Some(m) = median_mig(results) {
        println!("   {:>18} │ {:>8.1}ns {:>8}ns {:>8}ns {:>6.1}x {:>5} {:>8.1}ns {:>3}",
            label, m.baseline_avg, m.spike, m.window_max, m.ratio, m.recovery, m.post_avg, results.len());
    }
}

fn migrate_once_setaffinity(period_ms: u64, work_us: u64, rt_prio: u8, during_sleep: bool, to_core: usize) -> Option<MigStats> {
    let mut handle = spawn_worker(period_ms, work_us, rt_prio, true, 0)?;
    let baseline = collect_lats(&mut handle, 200);
    wait_for_timing(period_ms, work_us, during_sleep);
    pin_pid_to_core(handle.pid, to_core);
    let post = collect_lats(&mut handle, 200);
    kill_worker(&mut handle);
    analyze_migration(&baseline, &post, to_core)
}

fn migrate_once_cpuset(group: &str, period_ms: u64, work_us: u64, rt_prio: u8, during_sleep: bool, to_core: usize) -> Option<MigStats> {
    let group_path = create_cpuset_group(group, "0")?;
    let result = (|| {
        let mut handle = spawn_worker(period_ms, work_us, rt_prio, false, 0)?;
        move_to_cpuset(&group_path, handle.pid as u32);
        thread::sleep(Duration::from_millis(100));
        let baseline = collect_lats(&mut handle, 200);
        wait_for_timing(period_ms, work_us, during_sleep);
        change_cpuset_cores(&group_path, &to_core.to_string());
        let post = collect_lats(&mut handle, 200);
        kill_worker(&mut handle);
        analyze_migration(&baseline, &post, to_core)
    })();
    cleanup_cpuset_group(group);
    result
}

fn repeat_count() -> usize {
    common::env_u64("KOS_BENCH_REPEAT", 5).max(1) as usize
}

fn test_migration_latency(cpuset_available: bool) {
    println!("── Test 1. Migration latency (core 0 → core 2, 10ms period) ──");
    println!("   RT FIFO prio 80, moved while running / while sleeping");
    println!();
    print_mig_header();

    let period_ms = 10u64;
    let work_us = 2000u64;
    let rt_prio = 80u8;
    let n = repeat_count();

    for (timing, during_sleep) in [("running", false), ("sleeping", true)] {
        let r: Vec<MigStats> = (0..n)
            .filter_map(|_| migrate_once_setaffinity(period_ms, work_us, rt_prio, during_sleep, 2))
            .collect();
        print_mig_row(&format!("setaffinity({timing})"), &r);
    }

    if cpuset_available {
        for (timing, during_sleep) in [("running", false), ("sleeping", true)] {
            let r: Vec<MigStats> = (0..n)
                .filter_map(|_| migrate_once_cpuset("mig_test", period_ms, work_us, rt_prio, during_sleep, 2))
                .collect();
            print_mig_row(&format!("cpuset({timing})"), &r);
        }
    }

    println!();
}

fn test_steady_state(cpuset_available: bool) {
    println!("── Test 2. Steady-state latency (10ms period, RT FIFO) ──");
    println!("   latency distribution over 5 seconds on a fixed core");
    println!();
    println!("   {:>20} │ {:>10} {:>10} {:>10} {:>10}",
        "method", "avg", "p50", "p99", "max");
    println!("   {}", "─".repeat(62));

    let period_ms = 10u64;
    let work_us = 2000u64;
    let rt_prio = 80u8;
    let measure_periods = 500;

    if let Some(mut handle) = spawn_worker(period_ms, work_us, rt_prio, true, 0) {
        let _ = collect_lats(&mut handle, 50);
        let data = collect_lats(&mut handle, measure_periods);
        let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
        let s = compute_stats(&vals);
        println!("   {:>20} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
            "setaffinity(core0)", s.avg, s.p50, s.p99, s.max);
        kill_worker(&mut handle);
    }

    if cpuset_available {
        if let Some(group_path) = create_cpuset_group("steady_1c", "0") {
            if let Some(mut handle) = spawn_worker(period_ms, work_us, rt_prio, false, 0) {
                move_to_cpuset(&group_path, handle.pid as u32);
                thread::sleep(Duration::from_millis(100));
                let _ = collect_lats(&mut handle, 50);
                let data = collect_lats(&mut handle, measure_periods);
                let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
                let s = compute_stats(&vals);
                println!("   {:>20} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
                    "cpuset(core0)", s.avg, s.p50, s.p99, s.max);
                kill_worker(&mut handle);
            }
            cleanup_cpuset_group("steady_1c");
        }

        if let Some(group_path) = create_cpuset_group("steady_2c", "0,2") {
            if let Some(mut handle) = spawn_worker(period_ms, work_us, rt_prio, false, 0) {
                move_to_cpuset(&group_path, handle.pid as u32);
                thread::sleep(Duration::from_millis(100));
                let _ = collect_lats(&mut handle, 50);
                let data = collect_lats(&mut handle, measure_periods);
                let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
                let s = compute_stats(&vals);

                let on_core0 = data.iter().filter(|&&(_, _, c)| c == 0).count();
                let on_core2 = data.iter().filter(|&&(_, _, c)| c == 2).count();
                println!("   {:>20} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns  (c0:{} c2:{})",
                    "cpuset(core0,2)", s.avg, s.p50, s.p99, s.max, on_core0, on_core2);
                kill_worker(&mut handle);
            }
            cleanup_cpuset_group("steady_2c");
        }

        if let Some(group_path) = create_cpuset_group("steady_4c", "0-3") {
            if let Some(mut handle) = spawn_worker(period_ms, work_us, rt_prio, false, 0) {
                move_to_cpuset(&group_path, handle.pid as u32);
                thread::sleep(Duration::from_millis(100));
                let _ = collect_lats(&mut handle, 50);
                let data = collect_lats(&mut handle, measure_periods);
                let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
                let s = compute_stats(&vals);
                println!("   {:>20} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
                    "cpuset(core0-3)", s.avg, s.p50, s.p99, s.max);
                kill_worker(&mut handle);
            }
            cleanup_cpuset_group("steady_4c");
        }
    }

    println!();
}

fn test_multiprocess_distribution(cpuset_available: bool) {
    println!("── Test 3. Spreading multiple processes (10ms period, 4 processes) ──");
    println!("   setaffinity: one process each on cores 0,1,2,3, placed manually");
    println!("   cpuset:      4 processes on cores 0-3 → spread by the kernel (RT push/pull for RT FIFO)");
    println!();
    println!("   {:>20} │ {:>10} {:>10} {:>10} {:>10} {:>14}",
        "method", "avg", "p50", "p99", "max", "per core");
    println!("   {}", "─".repeat(80));

    let period_ms = 10u64;
    let work_us = 2000u64;
    let rt_prio = 80u8;
    let measure_periods = 300;

    {
        let mut handles: Vec<WorkerHandle> = Vec::new();
        for core in 0..4usize {
            if let Some(h) = spawn_worker(period_ms, work_us, rt_prio, true, core) {
                handles.push(h);
            }
        }

        thread::sleep(Duration::from_secs(1));
        let _ = collect_lats_parallel(&mut handles, 50);

        let all_data: Vec<(u64, u64, usize)> =
            collect_lats_parallel(&mut handles, measure_periods).into_iter().flatten().collect();
        let all_lats: Vec<u64> = all_data.iter().map(|&(_, l, _)| l).collect();
        let s = compute_stats(&all_lats);

        let mut core_counts = [0usize; 4];
        for &(_, _, c) in &all_data {
            if c < 4 { core_counts[c] += 1; }
        }
        let dist = format!("{}/{}/{}/{}",
            core_counts[0], core_counts[1], core_counts[2], core_counts[3]);

        println!("   {:>20} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns  {}",
            "setaffinity(4 cores)", s.avg, s.p50, s.p99, s.max, dist);

        for h in handles.iter_mut() { kill_worker(h); }
    }

    if cpuset_available {
        if let Some(group_path) = create_cpuset_group("multi_4", "0-3") {
            let mut handles: Vec<WorkerHandle> = Vec::new();
            for _ in 0..4 {
                if let Some(h) = spawn_worker(period_ms, work_us, rt_prio, false, 0) {
                    move_to_cpuset(&group_path, h.pid as u32);
                    handles.push(h);
                }
            }

            thread::sleep(Duration::from_secs(1));
            let _ = collect_lats_parallel(&mut handles, 50);

            let all_data: Vec<(u64, u64, usize)> =
                collect_lats_parallel(&mut handles, measure_periods).into_iter().flatten().collect();
            let all_lats: Vec<u64> = all_data.iter().map(|&(_, l, _)| l).collect();
            let s = compute_stats(&all_lats);

            let mut core_counts = [0usize; 4];
            for &(_, _, c) in &all_data {
                if c < 4 { core_counts[c] += 1; }
            }
            let dist = format!("{}/{}/{}/{}",
                core_counts[0], core_counts[1], core_counts[2], core_counts[3]);

            println!("   {:>20} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns  {}",
                "cpuset(core0-3)", s.avg, s.p50, s.p99, s.max, dist);

            for h in handles.iter_mut() { kill_worker(h); }
            cleanup_cpuset_group("multi_4");
        }
    }

    println!();
}

fn test_migration_under_load(cpuset_available: bool) {
    let nc_cores = common::topology().nc.clone();
    println!("── Test 4. Migration under NC load (10ms period, RT FIFO) ──");
    println!("   NC cores {} (excluding HT siblings) under 100% load (nice {}), core 0 → core 2 while running",
        common::format_cpu_list(&nc_cores), NC_NICE);
    println!();
    print_mig_header();

    let period_ms = 10u64;
    let work_us = 2000u64;
    let rt_prio = 80u8;
    let n = repeat_count();

    let mut nc_children = spawn_nc_load(&nc_cores);
    thread::sleep(Duration::from_millis(500));

    let r: Vec<MigStats> = (0..n)
        .filter_map(|_| migrate_once_setaffinity(period_ms, work_us, rt_prio, false, 2))
        .collect();
    print_mig_row("setaffinity", &r);

    if cpuset_available {
        let r: Vec<MigStats> = (0..n)
            .filter_map(|_| migrate_once_cpuset("load_test", period_ms, work_us, rt_prio, false, 2))
            .collect();
        print_mig_row("cpuset", &r);
    }

    kill_all(&mut nc_children);
    println!();
}

fn test_mixed_strategy(cpuset_available: bool) {
    if !cpuset_available { return; }

    let topo = common::topology();
    let nc_set: Vec<usize> = topo.nc.iter().copied().take(4).collect();
    if nc_set.is_empty() { return; }
    let nc_list = common::format_cpu_list(&nc_set);

    println!("── Test 5. Mixed strategy: critical pinning + NC cpuset ──");
    println!("   Critical: one process each on cores 0,1 (setaffinity)");
    println!("   NC:       4 processes in cpuset {} (spread by CFS)", nc_list);
    println!("   checks whether the NC cpuset affects critical latency");
    println!();
    println!("   {:>20} │ {:>10} {:>10} {:>10} {:>10}",
        "process", "avg", "p50", "p99", "max");
    println!("   {}", "─".repeat(62));

    let period_ms = 10u64;
    let work_us = 2000u64;
    let rt_prio = 80u8;
    let measure_periods = 500;

    let mut handles: Vec<WorkerHandle> = Vec::new();
    let mut labels: Vec<String> = Vec::new();
    for core in [0usize, 1] {
        if let Some(h) = spawn_worker(period_ms, work_us, rt_prio, true, core) {
            handles.push(h);
            labels.push(format!("critical(core{})", core));
        }
    }
    let n_crit = handles.len();

    let nc_group = create_cpuset_group("mixed_nc", &nc_list);
    if let Some(ref gp) = nc_group {
        for _ in 0..4 {
            if let Some(h) = spawn_worker(period_ms, work_us, 0, false, 0) {
                move_to_cpuset(gp, h.pid as u32);
                handles.push(h);
            }
        }
    }

    thread::sleep(Duration::from_secs(1));
    let _ = collect_lats_parallel(&mut handles, 50);

    let data = collect_lats_parallel(&mut handles, measure_periods);

    for (label, d) in labels.iter().zip(data.iter().take(n_crit)) {
        let vals: Vec<u64> = d.iter().map(|&(_, l, _)| l).collect();
        let s = compute_stats(&vals);
        println!("   {:>20} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
            label, s.avg, s.p50, s.p99, s.max);
    }

    let nc_all_lats: Vec<u64> = data.iter().skip(n_crit).flatten().map(|&(_, l, _)| l).collect();
    let nc_stats = compute_stats(&nc_all_lats);
    println!("   {:>20} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
        format!("NC(cpuset {})", nc_list), nc_stats.avg, nc_stats.p50, nc_stats.p99, nc_stats.max);

    for h in handles.iter_mut() { kill_worker(h); }
    if nc_group.is_some() { cleanup_cpuset_group("mixed_nc"); }
    println!();
}

fn spawn_nc_load(nc_cores: &[usize]) -> Vec<Child> {
    let mut children = Vec::new();
    let exe = self_exe();
    for &core in nc_cores {
        let mut cmd = Command::new("taskset");
        cmd.args(["-c", &core.to_string()])
            .arg(&exe)
            .args(["--pvc-worker-burn"])
            .env("KOS_BENCH_NICE", NC_NICE.to_string())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Ok(child) = cmd.spawn() {
            children.push(child);
        }
    }
    thread::sleep(Duration::from_millis(200));
    children
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 6 && args[1] == "--pvc-worker" {
        let core: i32 = args[2].parse().unwrap_or(-1);
        let period_ms: u64 = args[3].parse().unwrap_or(10);
        let work_us: u64 = args[4].parse().unwrap_or(2000);
        let rt_prio: u8 = args[5].parse().unwrap_or(0);
        worker_main(core, period_ms, work_us, rt_prio);
        return;
    }

    if args.len() >= 2 && args[1] == "--pvc-worker-burn" {
        common::apply_worker_nice_from_env();
        loop { std::hint::black_box(0u64.wrapping_mul(42)); }
    }

    let total_cpus = num_cpus();
    println!();
    println!("══════════════════════════════════════════════════════════════════");
    println!("  KOS pinning vs cpuset comparison");
    println!("  CPU: {} cores", total_cpus);
    println!("══════════════════════════════════════════════════════════════════");

    common::print_env_report();

    let reserved = common::topology().reserved_u32();
    let _rt_guard = common::RtThrottleGuard::from_env();
    let _cstate_guard = cgroup::disable_cstates_for_cores(&reserved);
    let _pmqos = if _cstate_guard.is_none() { cgroup::disable_cstates() } else { None };
    let _irq_guard = cgroup::isolate_irq_from_cores_guarded(&reserved, total_cpus as u32);

    let cpuset_available = setup_cpuset_cgroup();
    if cpuset_available {
        println!("[setup] cpuset cgroup: ✅ enabled");
    } else {
        println!("[setup] cpuset cgroup: ❌ unavailable (testing setaffinity only)");
    }
    println!();

    test_migration_latency(cpuset_available);
    test_steady_state(cpuset_available);
    test_multiprocess_distribution(cpuset_available);
    test_migration_under_load(cpuset_available);
    test_mixed_strategy(cpuset_available);

    if cpuset_available {
        cleanup_bench_slice();
    }

    common::thermal_snapshot("end");
    println!("══════════════════════════════════════════════════════════════════");
    println!("  Done");
    println!("══════════════════════════════════════════════════════════════════");
}
