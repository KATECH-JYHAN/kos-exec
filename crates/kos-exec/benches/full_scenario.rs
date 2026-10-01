// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

mod common;

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nix::sched::{sched_setaffinity, CpuSet};
use nix::unistd::Pid;

use kos_exec::cgroup;
use kos_exec::notify::{NotifyCommand, NotifyServer, NotifyStatus};

fn self_exe() -> PathBuf {
    std::env::current_exe().expect("self exe")
}

fn num_cpus() -> usize {
    common::num_cpus()
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
struct Stats { avg: f64, p50: u64, p99: u64, max: u64, min: u64 }

fn compute_stats(vals: &[u64]) -> Stats {
    if vals.is_empty() { return Stats { avg: 0.0, p50: 0, p99: 0, max: 0, min: 0 }; }
    let sum: u64 = vals.iter().sum();
    let avg = sum as f64 / vals.len() as f64;
    let mut sorted = vals.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[(sorted.len() as f64 * 0.50) as usize];
    let p99 = sorted[((sorted.len() as f64 * 0.99) as usize).min(sorted.len() - 1)];
    Stats { avg, p50, p99, max: *sorted.last().unwrap(), min: sorted[0] }
}

fn handle_notify(
    client: &kos_exec::notify::NotifyClient,
    msg: &kos_exec::notify::NotifyMessage,
    total_burn_ns: &mut u64,
    sample_start: &mut Instant,
) {
    match msg.as_command() {
        Some(NotifyCommand::ReportStatus) => {
            let elapsed = sample_start.elapsed().as_nanos() as u64;
            let util = if elapsed > 0 {
                (*total_burn_ns as f64 / elapsed as f64) * 100.0
            } else { 0.0 };
            let _ = client.report_utilization(util);
        }
        Some(NotifyCommand::PrepareMigrate) => {
            let _ = client.report_ready_to_migrate();
        }
        Some(NotifyCommand::Degrade) => {
            let _ = client.ack();
        }
        _ => {}
    }
}

fn notify_worker_main(core: i32, period_ms: u64, work_us: u64, rt_prio: u8, sock_path: &str) {
    common::init_measure_worker();
    if core >= 0 { pin_to_core(core as usize); }
    if rt_prio > 0 { set_rt_fifo(rt_prio); }

    let client = kos_exec::notify::NotifyClient::connect(std::path::Path::new(sock_path))
        .expect("connect notify");

    let period = Duration::from_millis(period_ms);
    let pid = std::process::id();
    println!("READY {}", pid);

    thread::sleep(Duration::from_millis(50));

    let mut next_wake = Instant::now() + period;
    let mut idx: u64 = 0;
    let mut total_burn_ns: u64 = 0;
    let mut sample_start = Instant::now();

    loop {
        'sleep_loop: loop {
            let now = Instant::now();
            let remaining = match next_wake.checked_duration_since(now) {
                Some(r) if !r.is_zero() => r,
                _ => break 'sleep_loop,
            };
            if remaining.as_millis() == 0 {
                thread::sleep(remaining);
                break 'sleep_loop;
            }
            match client.poll(Some(remaining)) {
                Ok(Some(msg)) => {
                    handle_notify(&client, &msg, &mut total_burn_ns, &mut sample_start);
                    if msg.as_command() == Some(NotifyCommand::Shutdown) {
                        return;
                    }
                }
                _ => {}
            }
        }

        let actual_wake = Instant::now();
        let latency_ns = if actual_wake > next_wake {
            (actual_wake - next_wake).as_nanos() as u64
        } else { 0 };
        let current_core = unsafe { libc::sched_getcpu() } as usize;

        println!("LAT {} {} {}", idx, latency_ns, current_core);

        let burn_start = Instant::now();
        cpu_burn(work_us);
        total_burn_ns += burn_start.elapsed().as_nanos() as u64;

        if idx % 10 == 9 {
            let elapsed = sample_start.elapsed().as_nanos() as u64;
            if elapsed > 0 {
                let util = (total_burn_ns as f64 / elapsed as f64) * 100.0;
                let _ = client.report_utilization(util);
            }
            total_burn_ns = 0;
            sample_start = Instant::now();
        }

        if let Ok(Some(msg)) = client.try_recv() {
            handle_notify(&client, &msg, &mut total_burn_ns, &mut sample_start);
            if msg.as_command() == Some(NotifyCommand::Shutdown) { break; }
        }

        next_wake += period;
        idx += 1;
    }
}

struct WorkerHandle {
    child: Child,
    pid: u32,
    lines: std::io::Lines<BufReader<std::process::ChildStdout>>,
}

fn spawn_notify_worker(core: usize, period_ms: u64, work_us: u64, rt_prio: u8, sock_path: &str) -> Option<WorkerHandle> {
    let exe = self_exe();
    let mut child = Command::new(&exe)
        .args([
            "--full-worker",
            &(core as i32).to_string(),
            &period_ms.to_string(),
            &work_us.to_string(),
            &rt_prio.to_string(),
            sock_path,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .ok()?;

    let stdout = child.stdout.take()?;
    let mut lines = BufReader::new(stdout).lines();

    let pid: u32;
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

fn collect_lats(handle: &mut WorkerHandle, count: usize, timeout: Duration) -> Vec<(u64, u64, usize)> {
    let mut lats = Vec::with_capacity(count);
    let deadline = Instant::now() + timeout;
    for _ in 0..count {
        if Instant::now() >= deadline { break; }
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
        } else { break; }
    }
    lats
}

fn kill_worker(h: &mut WorkerHandle) {
    let _ = h.child.kill();
    let _ = h.child.wait();
}

fn test_normal_operation() {
    println!("── Test 1. Normal operation: effect of Notify reporting on task latency ──");
    println!("   10ms period, 2ms work, RT FIFO 80, core 0");
    println!("   plain worker without Notify vs worker connected to Notify and reporting");
    println!();
    println!("   {:>20} │ {:>10} {:>10} {:>10} {:>10}",
        "mode", "avg", "p50", "p99", "max");
    println!("   {}", "─".repeat(66));

    let measure_n = 500;

    {
        let exe = self_exe();
        let mut child = Command::new(&exe)
            .args(["--full-plain-worker", "0", "10", "2000", "80"])
            .stdout(Stdio::piped()).stderr(Stdio::inherit())
            .spawn().expect("spawn");
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        loop {
            if let Some(Ok(line)) = lines.next() {
                if line.starts_with("READY") { break; }
            }
        }
        let mut handle = WorkerHandle { child, pid: 0, lines };
        let _ = collect_lats(&mut handle, 50, Duration::from_secs(5));
        let data = collect_lats(&mut handle, measure_n, Duration::from_secs(10));
        let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
        let s = compute_stats(&vals);
        println!("   {:>20} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
            "plain (no Notify)", s.avg, s.p50, s.p99, s.max);
        kill_worker(&mut handle);
    }

    {
        let sock_path = PathBuf::from("/tmp/kos_full_t1.sock");
        let mut server = NotifyServer::new(&sock_path).expect("server");

        let mut handle = spawn_notify_worker(0, 10, 2000, 80, sock_path.to_str().unwrap())
            .expect("spawn notify worker");

        thread::sleep(Duration::from_millis(200));
        let accepted = server.accept_pending().expect("accept");
        if !accepted.is_empty() {
            server.register_client(accepted[0], handle.pid, "worker".into());
        }

        let _ = collect_lats(&mut handle, 50, Duration::from_secs(5));
        let data = collect_lats(&mut handle, measure_n, Duration::from_secs(10));
        let vals: Vec<u64> = data.iter().map(|&(_, l, _)| l).collect();
        let s = compute_stats(&vals);
        println!("   {:>20} │ {:>8.1}ns {:>8}ns {:>8}ns {:>8}ns",
            "Notify connected + reporting", s.avg, s.p50, s.p99, s.max);

        server.send_command(handle.pid, NotifyCommand::Shutdown, 0).ok();
        kill_worker(&mut handle);
    }

    println!();
}

fn test_e2e_migration() {
    println!("── Test 2. End-to-end migration latency (channel cost only) ──");
    println!("   Notify: send command → receive reply → sched_setaffinity done");
    println!("   /proc:  poll until sleeping → sched_setaffinity done");
    println!("   workers wait in epoll (no burn), 1000 iterations");
    println!();
    println!("   {:>20} │ {:>10} {:>10} {:>10} {:>10}",
        "method", "avg", "p50", "p99", "max");
    println!("   {}", "─".repeat(66));

    {
        let sock_path = PathBuf::from("/tmp/kos_full_t2n.sock");
        let mut server = NotifyServer::new(&sock_path).expect("server");

        let mut handle = spawn_notify_worker(0, 10, 0, 80, sock_path.to_str().unwrap())
            .expect("spawn");

        thread::sleep(Duration::from_millis(200));
        let accepted = server.accept_pending().expect("accept");
        if !accepted.is_empty() {
            server.register_client(accepted[0], handle.pid, "worker".into());
        }

        thread::sleep(Duration::from_millis(500));

        let mut latencies = Vec::with_capacity(1000);

        for _ in 0..1000 {
            let start = Instant::now();

            server.send_command(handle.pid, NotifyCommand::PrepareMigrate, 2).expect("send");

            let deadline = Instant::now() + Duration::from_millis(100);
            let mut ready = false;
            while Instant::now() < deadline {
                if let Ok(msgs) = server.poll(1) {
                    for (_, msg) in &msgs {
                        if msg.as_status() == Some(NotifyStatus::ReadyToMigrate) {
                            ready = true;
                        }
                    }
                    if ready { break; }
                }
            }

            if ready {
                pin_pid_to_core(handle.pid as i32, 2);
                let elapsed = start.elapsed().as_nanos() as u64;
                latencies.push(elapsed);

                pin_pid_to_core(handle.pid as i32, 0);
            }
            thread::sleep(Duration::from_micros(100));
        }

        let s = compute_stats(&latencies);
        println!("   {:>20} │ {:>8.1}μs {:>8.1}μs {:>8.1}μs {:>8.1}μs  ({} runs)",
            "Notify E2E",
            s.avg / 1000.0, s.p50 as f64 / 1000.0,
            s.p99 as f64 / 1000.0, s.max as f64 / 1000.0,
            latencies.len());

        server.send_command(handle.pid, NotifyCommand::Shutdown, 0).ok();
        kill_worker(&mut handle);
    }

    {
        let exe = self_exe();
        let mut child = Command::new(&exe)
            .args(["--full-plain-worker", "0", "10", "0", "80"])
            .stdout(Stdio::piped()).stderr(Stdio::inherit())
            .spawn().expect("spawn");
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        let pid: i32;
        loop {
            if let Some(Ok(line)) = lines.next() {
                if let Some(rest) = line.strip_prefix("READY ") {
                    pid = rest.trim().parse().unwrap_or(0);
                    break;
                }
            }
        }
        let mut handle = WorkerHandle { child, pid: pid as u32, lines };
        let _ = collect_lats(&mut handle, 50, Duration::from_secs(5));

        let mut latencies = Vec::with_capacity(1000);

        thread::sleep(Duration::from_millis(500));

        for _ in 0..1000 {
            let start = Instant::now();

            let deadline = Instant::now() + Duration::from_millis(100);
            loop {
                let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).unwrap_or_default();
                if let Some(pos) = stat.rfind(')') {
                    if stat[pos + 2..].starts_with('S') {
                        break;
                    }
                }
                if Instant::now() >= deadline { break; }
                std::hint::spin_loop();
            }

            pin_pid_to_core(pid, 2);
            let elapsed = start.elapsed().as_nanos() as u64;
            latencies.push(elapsed);

            pin_pid_to_core(pid, 0);
            thread::sleep(Duration::from_micros(100));
        }

        let s = compute_stats(&latencies);
        println!("   {:>20} │ {:>8.1}μs {:>8.1}μs {:>8.1}μs {:>8.1}μs  ({} runs)",
            "/proc polling E2E",
            s.avg / 1000.0, s.p50 as f64 / 1000.0,
            s.p99 as f64 / 1000.0, s.max as f64 / 1000.0,
            latencies.len());

        kill_worker(&mut handle);
    }

    println!();
}

fn test_multi_process_overhead() {
    println!("── Test 3. Overhead of running many processes ──");
    println!("   N processes (10ms period, 2ms work, RT FIFO 80)");
    println!("   one per core, connected to Notify and reporting utilization periodically");
    println!("   measures the cost of the RM collecting all status every 100ms");
    println!();
    println!("   {:>6} {:>10} {:>10} {:>12} {:>12}",
        "N", "task avg", "task p99", "RM collect avg", "RM collect max");
    println!("   {}", "─".repeat(58));

    for n_workers in [1, 2, 4] {
        let sock_path = PathBuf::from(format!("/tmp/kos_full_t3_{}.sock", n_workers));
        let mut server = NotifyServer::new(&sock_path).expect("server");

        let mut handles: Vec<WorkerHandle> = Vec::new();

        for i in 0..n_workers {
            let core = i % 4;
            if let Some(h) = spawn_notify_worker(core, 10, 2000, 80, sock_path.to_str().unwrap()) {
                handles.push(h);
            }
        }

        thread::sleep(Duration::from_millis(300));
        let accepted = server.accept_pending().expect("accept");
        for (i, &temp_id) in accepted.iter().enumerate() {
            if i < handles.len() {
                server.register_client(temp_id, handles[i].pid, format!("w_{}", i));
            }
        }

        for h in handles.iter_mut() {
            let _ = collect_lats(h, 20, Duration::from_secs(3));
        }

        let mut rm_latencies = Vec::with_capacity(100);

        for _ in 0..100 {
            let start = Instant::now();
            server.broadcast(NotifyCommand::ReportStatus, 0);
            let mut received = 0;
            let deadline = Instant::now() + Duration::from_millis(500);
            while received < n_workers && Instant::now() < deadline {
                if let Ok(msgs) = server.poll(10) {
                    received += msgs.len();
                }
            }
            if received >= n_workers {
                rm_latencies.push(start.elapsed().as_nanos() as u64);
            }
        }

        let mut all_lats: Vec<u64> = Vec::new();
        for h in handles.iter_mut() {
            let data = collect_lats(h, 200, Duration::from_secs(5));
            all_lats.extend(data.iter().map(|&(_, l, _)| l));
        }
        let task_s = compute_stats(&all_lats);
        let rm_s = compute_stats(&rm_latencies);

        println!("   {:>6} {:>8.1}ns {:>8}ns {:>10.1}μs {:>10.1}μs",
            n_workers, task_s.avg, task_s.p99,
            rm_s.avg / 1000.0, rm_s.max as f64 / 1000.0);

        for h in handles.iter_mut() {
            server.send_command(h.pid, NotifyCommand::Shutdown, 0).ok();
            kill_worker(h);
        }
    }

    println!();
}

fn test_monitoring_comparison() {
    println!("── Test 4. Monitoring cost: /proc polling vs Notify (16 processes) ──");
    println!("   collect CPU utilization of 16 processes 1000 times");
    println!();
    println!("   {:>20} │ {:>10} {:>10} {:>10} {:>10}",
        "method", "avg", "p50", "p99", "max");
    println!("   {}", "─".repeat(66));

    let n_workers = 16usize;

    let sock_path = PathBuf::from("/tmp/kos_full_t4.sock");
    let mut server = NotifyServer::new(&sock_path).expect("server");

    let mut handles: Vec<WorkerHandle> = Vec::new();
    for i in 0..n_workers {
        let core = i % 4;
        if let Some(h) = spawn_notify_worker(core, 10, 2000, 0, sock_path.to_str().unwrap()) {
            handles.push(h);
        }
    }

    thread::sleep(Duration::from_millis(500));
    let accepted = server.accept_pending().expect("accept");
    for (i, &temp_id) in accepted.iter().enumerate() {
        if i < handles.len() {
            server.register_client(temp_id, handles[i].pid, format!("w_{}", i));
        }
    }
    let pids: Vec<u32> = handles.iter().map(|h| h.pid).collect();

    thread::sleep(Duration::from_secs(1));

    {
        let mut latencies = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = Instant::now();
            for &pid in &pids {
                let path = format!("/proc/{}/stat", pid);
                if let Ok(content) = std::fs::read_to_string(&path) {
                    if let Some(pos) = content.rfind(')') {
                        let fields: Vec<&str> = content[pos + 2..].split_whitespace().collect();
                        if fields.len() > 13 {
                            let _utime: u64 = fields[11].parse().unwrap_or(0);
                            let _stime: u64 = fields[12].parse().unwrap_or(0);
                        }
                    }
                }
            }
            latencies.push(start.elapsed().as_nanos() as u64);
        }
        let s = compute_stats(&latencies);
        println!("   {:>20} │ {:>8.1}μs {:>8.1}μs {:>8.1}μs {:>8.1}μs",
            "/proc × 16",
            s.avg / 1000.0, s.p50 as f64 / 1000.0,
            s.p99 as f64 / 1000.0, s.max as f64 / 1000.0);
    }

    {
        for _ in 0..20 {
            server.broadcast(NotifyCommand::ReportStatus, 0);
            thread::sleep(Duration::from_millis(20));
            let _ = server.poll(50);
        }

        let mut latencies = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = Instant::now();
            server.broadcast(NotifyCommand::ReportStatus, 0);
            let mut received = 0;
            let deadline = Instant::now() + Duration::from_millis(500);
            while received < n_workers && Instant::now() < deadline {
                if let Ok(msgs) = server.poll(5) {
                    received += msgs.len();
                }
            }
            if received >= n_workers {
                latencies.push(start.elapsed().as_nanos() as u64);
            }
        }
        let s = compute_stats(&latencies);
        println!("   {:>20} │ {:>8.1}μs {:>8.1}μs {:>8.1}μs {:>8.1}μs  ({} runs)",
            "Notify broadcast×16",
            s.avg / 1000.0, s.p50 as f64 / 1000.0,
            s.p99 as f64 / 1000.0, s.max as f64 / 1000.0,
            latencies.len());
    }

    for h in handles.iter_mut() {
        server.send_command(h.pid, NotifyCommand::Shutdown, 0).ok();
        kill_worker(h);
    }

    println!();
}

fn plain_worker_main(core: i32, period_ms: u64, work_us: u64, rt_prio: u8) {
    if core >= 0 { pin_to_core(core as usize); }
    if rt_prio > 0 { set_rt_fifo(rt_prio); }

    let period = Duration::from_millis(period_ms);
    println!("READY {}", std::process::id());
    thread::sleep(Duration::from_millis(50));

    let mut next_wake = Instant::now() + period;
    let mut idx: u64 = 0;
    loop {
        let now = Instant::now();
        if let Some(remaining) = next_wake.checked_duration_since(now) {
            thread::sleep(remaining);
        }
        let actual_wake = Instant::now();
        let latency_ns = if actual_wake > next_wake {
            (actual_wake - next_wake).as_nanos() as u64
        } else { 0 };
        let current_core = unsafe { libc::sched_getcpu() } as usize;
        println!("LAT {} {} {}", idx, latency_ns, current_core);
        next_wake += period;
        cpu_burn(work_us);
        idx += 1;
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 7 && args[1] == "--full-worker" {
        let core: i32 = args[2].parse().unwrap_or(-1);
        let period_ms: u64 = args[3].parse().unwrap_or(10);
        let work_us: u64 = args[4].parse().unwrap_or(2000);
        let rt_prio: u8 = args[5].parse().unwrap_or(0);
        let sock_path = &args[6];
        notify_worker_main(core, period_ms, work_us, rt_prio, sock_path);
        return;
    }

    if args.len() >= 6 && args[1] == "--full-plain-worker" {
        let core: i32 = args[2].parse().unwrap_or(-1);
        let period_ms: u64 = args[3].parse().unwrap_or(10);
        let work_us: u64 = args[4].parse().unwrap_or(2000);
        let rt_prio: u8 = args[5].parse().unwrap_or(0);
        plain_worker_main(core, period_ms, work_us, rt_prio);
        return;
    }

    let total_cpus = num_cpus();
    println!();
    println!("══════════════════════════════════════════════════════════════════");
    println!("  KOS full scenario benchmark");
    println!("  CPU: {} cores", total_cpus);
    println!("══════════════════════════════════════════════════════════════════");

    common::print_env_report();
    let reserved = common::topology().reserved_u32();
    let _cstate_guard = cgroup::disable_cstates_for_cores(&reserved);
    let _pmqos = if _cstate_guard.is_none() { cgroup::disable_cstates() } else { None };
    let _irq_guard = cgroup::isolate_irq_from_cores_guarded(&reserved, total_cpus as u32);
    println!();

    test_normal_operation();
    test_e2e_migration();
    test_multi_process_overhead();
    test_monitoring_comparison();

    println!("══════════════════════════════════════════════════════════════════");
    println!("  Done");
    println!("══════════════════════════════════════════════════════════════════");
}
