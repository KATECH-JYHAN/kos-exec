// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use std::thread;

use nix::sched::{sched_setaffinity, CpuSet};
use nix::unistd::Pid;

use kos_exec::cgroup;

const SPIKE_WINDOW: usize = 5;

fn rt_unthrottled() -> bool {
    common::read_rt_runtime() == Some(-1)
}

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

#[allow(dead_code)]
fn set_pid_rt_fifo(pid: i32, priority: u8) {
    use libc::{sched_param, sched_setscheduler, SCHED_FIFO};
    let param = sched_param { sched_priority: priority as i32 };
    unsafe { sched_setscheduler(pid, SCHED_FIFO, &param) };
}

fn num_cpus() -> usize {
    common::num_cpus()
}

fn self_exe() -> std::path::PathBuf {
    std::env::current_exe().expect("cannot determine self exe path")
}

fn periodic_worker_main(core: i32, period_ms: u64, work_us: u64, rt_prio: u8) {
    common::init_measure_worker();
    if core >= 0 { pin_to_core(core as usize); }
    if rt_prio > 0 { set_rt_fifo(rt_prio); }

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

fn nosleep_worker_main(core: i32, rt_prio: u8) {
    common::init_measure_worker();
    if core >= 0 { pin_to_core(core as usize); }
    if rt_prio > 0 { set_rt_fifo(rt_prio); }

    println!("READY {}", std::process::id());

    let report_interval = Duration::from_millis(10);
    let mut next_report = Instant::now() + report_interval;
    let mut idx: u64 = 0;
    let mut last_wake = Instant::now();

    loop {
        std::hint::black_box(0u64.wrapping_mul(42));

        let now = Instant::now();
        if now >= next_report {
            let latency_ns = now.duration_since(last_wake).as_nanos() as u64;
            let current_core = unsafe { libc::sched_getcpu() } as usize;
            println!("LAT {} {} {}", idx, latency_ns, current_core);
            last_wake = now;
            next_report = now + report_interval;
            idx += 1;
        }
    }
}

struct WorkerHandle {
    child: Child,
    pid: i32,
    lines: std::io::Lines<BufReader<std::process::ChildStdout>>,
}

fn spawn_periodic_worker(core: usize, period_ms: u64, work_us: u64, rt_prio: u8) -> Option<WorkerHandle> {
    let exe = self_exe();
    let mut child = Command::new(&exe)
        .args(["--sns-periodic", &(core as i32).to_string(),
               &period_ms.to_string(), &work_us.to_string(), &rt_prio.to_string()])
        .stdout(Stdio::piped()).stderr(Stdio::inherit())
        .spawn().ok()?;
    let stdout = child.stdout.take()?;
    let mut lines = BufReader::new(stdout).lines();
    let pid = wait_ready(&mut lines)?;
    Some(WorkerHandle { child, pid, lines })
}

fn spawn_nosleep_worker(core: usize, rt_prio: u8) -> Option<WorkerHandle> {
    let exe = self_exe();
    let mut child = Command::new(&exe)
        .args(["--sns-nosleep", &(core as i32).to_string(), &rt_prio.to_string()])
        .stdout(Stdio::piped()).stderr(Stdio::inherit())
        .spawn().ok()?;
    let stdout = child.stdout.take()?;
    let mut lines = BufReader::new(stdout).lines();
    let pid = wait_ready(&mut lines)?;
    Some(WorkerHandle { child, pid, lines })
}

fn spawn_burn_on_core(core: usize, rt_prio: u8) -> Option<Child> {
    let exe = self_exe();
    let child = Command::new("taskset")
        .args(["-c", &core.to_string()])
        .arg(&exe)
        .args(["--sns-burn", &rt_prio.to_string()])
        .stdout(Stdio::null()).stderr(Stdio::null())
        .spawn().ok()?;
    thread::sleep(Duration::from_millis(50));
    Some(child)
}

fn wait_ready(lines: &mut std::io::Lines<BufReader<std::process::ChildStdout>>) -> Option<i32> {
    loop {
        if let Some(Ok(line)) = lines.next() {
            if let Some(rest) = line.strip_prefix("READY ") {
                return rest.trim().parse().ok();
            }
        } else {
            return None;
        }
    }
}

fn collect_lats_timeout(handle: &mut WorkerHandle, count: usize, timeout: Duration) -> Vec<(u64, u64, usize)> {
    let mut lats = Vec::with_capacity(count);
    let deadline = Instant::now() + timeout;

    for _ in 0..count {
        if Instant::now() >= deadline {
            break;
        }
        if handle.child.try_wait().ok().flatten().is_some() {
            break;
        }
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
        } else {
            break;
        }
    }
    lats
}

fn collect_lats(handle: &mut WorkerHandle, count: usize) -> Vec<(u64, u64, usize)> {
    let timeout = Duration::from_millis(count as u64 * 50 + 5000);
    collect_lats_timeout(handle, count, timeout)
}

fn kill_worker(h: &mut WorkerHandle) {
    let _ = h.child.kill();
    let _ = h.child.wait();
}

struct Stats { avg: f64, p50: u64, p99: u64, max: u64 }

fn compute_stats(lats: &[u64]) -> Stats {
    if lats.is_empty() { return Stats { avg: 0.0, p50: 0, p99: 0, max: 0 }; }
    let sum: u64 = lats.iter().sum();
    let avg = sum as f64 / lats.len() as f64;
    let mut sorted = lats.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[(sorted.len() as f64 * 0.50) as usize];
    let p99 = sorted[((sorted.len() as f64 * 0.99) as usize).min(sorted.len() - 1)];
    let max = *sorted.last().unwrap();
    Stats { avg, p50, p99, max }
}

fn analyze_migration(baseline: &[(u64, u64, usize)], post: &[(u64, u64, usize)], to_core: usize)
    -> Option<(f64, u64, u64, f64, usize, f64)>
{
    let bvals: Vec<u64> = baseline.iter().map(|&(_, l, _)| l).collect();
    if bvals.is_empty() { return None; }
    let bstats = compute_stats(&bvals);
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
    Some((bstats.avg, spike, window_max, ratio, recovery, post_avg))
}

fn print_mig_row(label: &str, r: Option<(f64, u64, u64, f64, usize, f64)>) {
    if let Some((b, spike, wmax, ratio, rec, post)) = r {
        println!("   {:>22} │ {:>8.1}ns {:>8}ns {:>8}ns {:>6.1}x {:>6} {:>8.1}ns",
            label, b, spike, wmax, ratio, rec, post);
    } else {
        println!("   {:>22} │ (measurement failed: no samples on the new core)", label);
    }
}

fn test_migration_cost() {
    println!("── Test 1. Migration cost: sleep vs no-sleep (core 0 → 2) ──");
    println!("   RT FIFO prio 80, 200-period baseline + 200-period post for each worker type");
    println!();
    println!("   {:>22} │ {:>10} {:>10} {:>10} {:>7} {:>6} {:>10}",
        "worker type", "baseline", "spike", "window max", "ratio", "recovery", "avg after");
    println!("   {}", "─".repeat(86));

    let baseline_n = 200;
    let post_n = 200;

    for &(label, period_ms, work_us) in &[
        ("periodic(20%)", 10u64, 2000u64),
        ("periodic(50%)", 10, 5000),
        ("periodic(80%)", 10, 8000),
    ] {
        if let Some(mut h) = spawn_periodic_worker(0, period_ms, work_us, 80) {
            let baseline = collect_lats(&mut h, baseline_n);

            let sleep_dur = Duration::from_millis(period_ms).saturating_sub(Duration::from_micros(work_us));
            thread::sleep(Duration::from_micros(work_us) + sleep_dur / 2);
            pin_pid_to_core(h.pid, 2);

            let post = collect_lats(&mut h, post_n);
            print_mig_row(label, analyze_migration(&baseline, &post, 2));
            kill_worker(&mut h);
        }
    }

    if rt_unthrottled() {
        println!("   {:>22} │ SKIP (RT throttling off: a 100% busy RT task would monopolize the CPU)", "no-sleep(100%)");
    } else if let Some(mut h) = spawn_nosleep_worker(0, 80) {
        let baseline = collect_lats(&mut h, baseline_n);

        pin_pid_to_core(h.pid, 2);

        let post = collect_lats(&mut h, post_n);
        print_mig_row("no-sleep(100%)*", analyze_migration(&baseline, &post, 2));
        kill_worker(&mut h);
        println!("   * the no-sleep row shows the LAT report interval (target 10ms), not wake latency");
    }

    println!();
}

fn test_colocation_impact() {
    println!("── Test 2. Sharing a core: periodic task + no-sleep process ──");
    println!("   periodic task: 10ms period, 2ms work (core 0, RT FIFO 80)");
    println!("   co-runner: none / sleep worker (20%) / no-sleep worker (100%)");
    println!();
    println!("   {:>22} │ {:>10} {:>10} {:>10} {:>10}",
        "co-runner", "avg", "p50", "p99", "max");
    println!("   {}", "─".repeat(66));

    let period_ms = 10;
    let work_us = 2000;
    let measure_n = 500;

    if let Some(mut h) = spawn_periodic_worker(0, period_ms, work_us, 80) {
        let _ = collect_lats(&mut h, 50);
        let data = collect_lats(&mut h, measure_n);
        let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
        let s = compute_stats(&vals);
        println!("   {:>22} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
            "none (alone)", s.avg, s.p50, s.p99, s.max);
        kill_worker(&mut h);
    }

    if let Some(mut periodic) = spawn_periodic_worker(0, period_ms, work_us, 80) {
        if let Some(mut buddy) = spawn_periodic_worker(0, 10, 2000, 0) {
            let _ = collect_lats(&mut periodic, 50);
            let _ = collect_lats(&mut buddy, 50);
            let data = collect_lats(&mut periodic, measure_n);
            let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
            let s = compute_stats(&vals);
            println!("   {:>22} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
                "+ sleep(20%, CFS)", s.avg, s.p50, s.p99, s.max);
            kill_worker(&mut buddy);
        }
        kill_worker(&mut periodic);
    }

    if let Some(mut periodic) = spawn_periodic_worker(0, period_ms, work_us, 80) {
        if let Some(mut buddy) = spawn_periodic_worker(0, 10, 2000, 80) {
            let _ = collect_lats(&mut periodic, 50);
            let _ = collect_lats(&mut buddy, 50);
            let data = collect_lats(&mut periodic, measure_n);
            let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
            let s = compute_stats(&vals);
            println!("   {:>22} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
                "+ sleep(20%, RT80)", s.avg, s.p50, s.p99, s.max);
            kill_worker(&mut buddy);
        }
        kill_worker(&mut periodic);
    }

    if let Some(mut periodic) = spawn_periodic_worker(0, period_ms, work_us, 80) {
        if let Some(mut buddy) = spawn_burn_on_core(0, 0) {
            thread::sleep(Duration::from_millis(200));
            let _ = collect_lats(&mut periodic, 50);
            let data = collect_lats(&mut periodic, measure_n);
            let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
            let s = compute_stats(&vals);
            println!("   {:>22} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
                "+ no-sleep(100%,CFS)", s.avg, s.p50, s.p99, s.max);
            let _ = buddy.kill();
            let _ = buddy.wait();
        }
        kill_worker(&mut periodic);
    }

    println!("   {:>22} │  not measured — a same-priority FIFO hog would starve it (runs only in the throttling window)",
        "+no-sleep(100%,RT80)");

    if rt_unthrottled() {
        println!("   {:>22} │ SKIP (RT throttling off: a 100% busy RT task would monopolize the CPU)", "+no-sleep(100%,RT50)");
    } else if let Some(mut periodic) = spawn_periodic_worker(0, period_ms, work_us, 80) {
        if let Some(mut buddy) = spawn_burn_on_core(0, 50) {
            thread::sleep(Duration::from_millis(200));
            let _ = collect_lats(&mut periodic, 50);
            let data = collect_lats(&mut periodic, measure_n);
            let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
            let s = compute_stats(&vals);
            println!("   {:>22} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
                "+no-sleep(100%,RT50)", s.avg, s.p50, s.p99, s.max);
            let _ = buddy.kill();
            let _ = buddy.wait();
        }
        kill_worker(&mut periodic);
    }

    println!();
}

fn test_duty_cycle_sweep() {
    println!("── Test 3. Latency by duty cycle (10ms period, RT FIFO 80) ──");
    println!("   running alone, duty cycle 20% → 40% → 60% → 80% → 95% → 99% → 100%");
    match common::read_rt_runtime() {
        Some(-1) => println!("   RT throttling: OFF"),
        Some(r) => println!("   RT throttling: ON ({}%) — duty cycles above this collapse due to throttling",
            r as f64 / common::read_rt_period().unwrap_or(1_000_000) as f64 * 100.0),
        None => println!("   RT throttling: unknown"),
    }
    println!();
    println!("   {:>10} {:>8} {:>8} │ {:>10} {:>10} {:>10} {:>10}",
        "duty", "burn", "sleep", "avg", "p50", "p99", "max");
    println!("   {}", "─".repeat(80));

    let period_ms = 10u64;
    let measure_n = 500;

    let configs: Vec<(&str, u64, u64)> = vec![
        ("20%",  2000, 8000),
        ("40%",  4000, 6000),
        ("60%",  6000, 4000),
        ("80%",  8000, 2000),
        ("95%",  9500,  500),
        ("99%",  9900,  100),
    ];

    for &(label, work_us, _sleep_us) in &configs {
        if let Some(mut h) = spawn_periodic_worker(0, period_ms, work_us, 80) {
            let _ = collect_lats(&mut h, 50);
            let data = collect_lats(&mut h, measure_n);
            let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
            let s = compute_stats(&vals);
            println!("   {:>10} {:>6}μs {:>6}μs │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
                label, work_us, _sleep_us, s.avg, s.p50, s.p99, s.max);
            kill_worker(&mut h);
        }
    }

    if rt_unthrottled() {
        println!("   {:>10} SKIP (RT throttling off: a 100% busy RT task would monopolize the CPU)", "100%");
    } else if let Some(mut h) = spawn_nosleep_worker(0, 80) {
        let _ = collect_lats(&mut h, 50);
        let data = collect_lats(&mut h, measure_n);
        let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
        let s = compute_stats(&vals);
        println!("   {:>10} {:>6}μs {:>6}μs │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
            "100%", 10000, 0, s.avg, s.p50, s.p99, s.max);
        kill_worker(&mut h);
    }

    println!();
}

fn test_migration_verification() {
    println!("── Test 4. Migration check: is a no-sleep process actually moved ──");
    println!("   start on core 0 → move to core 2 with sched_setaffinity");
    println!("   check the core it actually runs on after the move (20 periods)");
    println!();

    let post_n = 20;

    if let Some(mut h) = spawn_periodic_worker(0, 10, 2000, 80) {
        let _ = collect_lats(&mut h, 20);
        pin_pid_to_core(h.pid, 2);
        let post = collect_lats(&mut h, post_n);
        let cores: Vec<usize> = post.iter().map(|&(_, _, c)| c).collect();
        let on_target = cores.iter().filter(|&&c| c == 2).count();
        println!("   periodic(20%): cores after the move: {:?}", cores);
        println!("                  on target (core2): {}/{} ({:.0}%)",
            on_target, post_n, on_target as f64 / post_n as f64 * 100.0);
        kill_worker(&mut h);
    }

    if rt_unthrottled() {
        println!("   no-sleep(100%): SKIP (RT throttling off)");
    } else if let Some(mut h) = spawn_nosleep_worker(0, 80) {
        let _ = collect_lats(&mut h, 20);
        pin_pid_to_core(h.pid, 2);
        let post = collect_lats(&mut h, post_n);
        let cores: Vec<usize> = post.iter().map(|&(_, _, c)| c).collect();
        let on_target = cores.iter().filter(|&&c| c == 2).count();
        println!("   no-sleep(100%): cores after the move: {:?}", cores);
        println!("                   on target (core2): {}/{} ({:.0}%)",
            on_target, post_n, on_target as f64 / post_n as f64 * 100.0);
        kill_worker(&mut h);
    }

    println!();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 6 && args[1] == "--sns-periodic" {
        let core: i32 = args[2].parse().unwrap_or(-1);
        let period_ms: u64 = args[3].parse().unwrap_or(10);
        let work_us: u64 = args[4].parse().unwrap_or(2000);
        let rt_prio: u8 = args[5].parse().unwrap_or(0);
        periodic_worker_main(core, period_ms, work_us, rt_prio);
        return;
    }

    if args.len() >= 4 && args[1] == "--sns-nosleep" {
        let core: i32 = args[2].parse().unwrap_or(-1);
        let rt_prio: u8 = args[3].parse().unwrap_or(0);
        nosleep_worker_main(core, rt_prio);
        return;
    }

    if args.len() >= 3 && args[1] == "--sns-burn" {
        let rt_prio: u8 = args[2].parse().unwrap_or(0);
        if rt_prio > 0 { set_rt_fifo(rt_prio); }
        loop { std::hint::black_box(0u64.wrapping_mul(42)); }
    }

    let total_cpus = num_cpus();
    println!();
    println!("══════════════════════════════════════════════════════════════════");
    println!("  KOS sleep vs no-sleep comparison");
    println!("  CPU: {} cores", total_cpus);
    println!("══════════════════════════════════════════════════════════════════");

    let _rt_guard = common::RtThrottleGuard::from_env();
    common::print_env_report();
    let reserved = common::topology().reserved_u32();
    let _cstate_guard = cgroup::disable_cstates_for_cores(&reserved);
    let _pmqos = if _cstate_guard.is_none() { cgroup::disable_cstates() } else { None };
    let _irq_guard = cgroup::isolate_irq_from_cores_guarded(&reserved, total_cpus as u32);
    println!();

    test_migration_cost();
    test_colocation_impact();
    test_duty_cycle_sweep();
    test_migration_verification();

    common::thermal_snapshot("end");
    println!("══════════════════════════════════════════════════════════════════");
    println!("  Done");
    println!("══════════════════════════════════════════════════════════════════");
}
