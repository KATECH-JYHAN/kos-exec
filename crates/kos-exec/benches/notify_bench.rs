// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use kos_exec::notify::{NotifyCommand, NotifyServer, NotifyStatus};

fn self_exe() -> PathBuf {
    std::env::current_exe().expect("self exe")
}

#[inline(always)]
fn now_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts); }
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

#[derive(Clone, Default)]
struct Stats {
    min: u64,
    avg: f64,
    p50: u64,
    p99: u64,
    max: u64,
    count: usize,
}

fn compute_stats(samples: &mut [u64]) -> Stats {
    if samples.is_empty() { return Stats::default(); }
    samples.sort_unstable();
    let n = samples.len();
    let sum: u64 = samples.iter().sum();
    Stats {
        min: samples[0],
        avg: sum as f64 / n as f64,
        p50: samples[n / 2],
        p99: samples[(n as f64 * 0.99) as usize].min(*samples.last().unwrap()),
        max: *samples.last().unwrap(),
        count: n,
    }
}

fn print_table_header() {
    println!("┌──────────────────┬────────┬──────────┬────────┬────────┬──────────┐");
    println!("│ Scenario         │    min │      avg │    p50 │    p99 │      max │");
    println!("├──────────────────┼────────┼──────────┼────────┼────────┼──────────┤");
}

fn print_table_row(label: &str, s: &Stats) {
    println!("│ {:<16} │ {:>6} │ {:>8.1} │ {:>6} │ {:>6} │ {:>8} │",
        label, s.min, s.avg, s.p50, s.p99, s.max);
}

fn print_table_footer() {
    println!("└──────────────────┴────────┴──────────┴────────┴────────┴──────────┘");
}

fn echo_worker_main(sock_path: &str) {
    let client = kos_exec::notify::NotifyClient::connect(std::path::Path::new(sock_path))
        .expect("connect");
    println!("READY {}", std::process::id());

    loop {
        match client.poll(Some(Duration::from_secs(10))) {
            Ok(Some(msg)) => {
                match msg.as_command() {
                    Some(NotifyCommand::Shutdown) => break,
                    Some(NotifyCommand::ReportStatus) => {
                        let ts = now_ns();
                        let _ = client.send_status(NotifyStatus::Utilization, (ts & 0xFFFFFFFF) as u32);
                    }
                    _ => {
                        let _ = client.ack();
                    }
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
}

fn timestamp_worker_main(sock_path: &str) {
    let client = kos_exec::notify::NotifyClient::connect(std::path::Path::new(sock_path))
        .expect("connect");
    println!("READY {}", std::process::id());

    loop {
        match client.poll(Some(Duration::from_secs(10))) {
            Ok(Some(msg)) => {
                match msg.as_command() {
                    Some(NotifyCommand::Shutdown) => break,
                    _ => {
                        let _ = client.ack();
                    }
                }
            }
            Ok(None) => {}
            Err(_) => break,
        }
    }
}

struct WorkerHandle {
    child: Child,
    pid: u32,
}

fn spawn_echo_worker(sock_path: &str) -> Option<WorkerHandle> {
    let exe = self_exe();
    let mut child = Command::new(&exe)
        .args(["--nb-echo", sock_path])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn().ok()?;

    let stdout = child.stdout.take()?;
    let mut lines = BufReader::new(stdout).lines();
    let pid: u32;
    loop {
        if let Some(Ok(line)) = lines.next() {
            if let Some(rest) = line.strip_prefix("READY ") {
                pid = rest.trim().parse().unwrap_or(0);
                break;
            }
        } else { return None; }
    }
    Some(WorkerHandle { child, pid })
}

fn kill_workers(handles: &mut Vec<WorkerHandle>, server: &NotifyServer) {
    for h in handles.iter() {
        let _ = server.send_command(h.pid, NotifyCommand::Shutdown, 0);
    }
    thread::sleep(Duration::from_millis(50));
    for h in handles.iter_mut() {
        let _ = h.child.kill();
        let _ = h.child.wait();
    }
    handles.clear();
}

fn setup_workers(n: usize, sock_path: &PathBuf, server: &mut NotifyServer) -> Vec<WorkerHandle> {
    let mut handles = Vec::new();
    for _ in 0..n {
        if let Some(h) = spawn_echo_worker(sock_path.to_str().unwrap()) {
            handles.push(h);
        }
    }
    thread::sleep(Duration::from_millis(100 + n as u64 * 20));
    let accepted = server.accept_pending().unwrap_or_default();
    for (i, &temp_id) in accepted.iter().enumerate() {
        if i < handles.len() {
            server.register_client(temp_id, handles[i].pid, format!("w{}", i));
        }
    }
    for h in &handles {
        for _ in 0..200 {
            let _ = server.send_command(h.pid, NotifyCommand::ReportStatus, 0);
        }
    }
    thread::sleep(Duration::from_millis(100));
    let _ = server.poll(0);
    handles
}

fn test_one_to_one() {
    println!("── Notify Test 1: 1:1 Roundtrip (RM ↔ 1 Worker, 8B) ──");
    println!();

    let iters = 50000;
    let sock_path = PathBuf::from("/tmp/kos_nb_t1.sock");
    let mut server = NotifyServer::new(&sock_path).expect("server");
    let mut handles = setup_workers(1, &sock_path, &mut server);
    let pid = handles[0].pid;

    let mut samples = Vec::with_capacity(iters);

    for _ in 0..iters {
        let t0 = now_ns();
        server.send_command(pid, NotifyCommand::ReportStatus, 0).unwrap();
        loop {
            if let Ok(msgs) = server.poll(100) {
                if !msgs.is_empty() { break; }
            }
        }
        let t1 = now_ns();
        samples.push(t1 - t0);
    }

    let s = compute_stats(&mut samples);
    print_table_header();
    print_table_row("1:1 roundtrip", &s);
    print_table_footer();

    kill_workers(&mut handles, &server);
    println!();
}

fn test_fanout() {
    println!("── Notify Test 2: Fan-out (RM → N Workers broadcast, 8B) ──");
    println!();

    let iters = 50000;

    print_table_header();

    for n in [1, 2, 4, 8, 16, 32, 64] {
        let sock_path = PathBuf::from(format!("/tmp/kos_nb_t2_{}.sock", n));
        let mut server = NotifyServer::new(&sock_path).expect("server");
        let mut handles = setup_workers(n, &sock_path, &mut server);

        if handles.len() < n {
            print_table_row(&format!("{:>2} subs (SKIP)", n), &Stats::default());
            kill_workers(&mut handles, &server);
            continue;
        }

        let mut samples = Vec::with_capacity(iters);

        for _ in 0..iters {
            let t0 = now_ns();
            server.broadcast(NotifyCommand::ReportStatus, 0);
            let mut received = 0;
            let deadline = Instant::now() + Duration::from_secs(1);
            while received < n && Instant::now() < deadline {
                if let Ok(msgs) = server.poll(10) {
                    received += msgs.len();
                }
            }
            let t1 = now_ns();
            if received >= n {
                samples.push(t1 - t0);
            }
        }

        let s = compute_stats(&mut samples);
        print_table_row(&format!("{:>2} subs", n), &s);

        kill_workers(&mut handles, &server);
    }

    print_table_footer();
    println!();
}

fn test_collect() {
    println!("── Notify Test 3: Collect (N Workers → RM sequential, 8B) ──");
    println!();

    let iters = 50000;

    print_table_header();

    for n in [1, 2, 4, 8, 16, 32, 64] {
        let sock_path = PathBuf::from(format!("/tmp/kos_nb_t3_{}.sock", n));
        let mut server = NotifyServer::new(&sock_path).expect("server");
        let mut handles = setup_workers(n, &sock_path, &mut server);

        if handles.len() < n {
            print_table_row(&format!("{:>2} pubs (SKIP)", n), &Stats::default());
            kill_workers(&mut handles, &server);
            continue;
        }

        let pids: Vec<u32> = handles.iter().map(|h| h.pid).collect();
        let mut samples = Vec::with_capacity(iters);

        for _ in 0..iters {
            let t0 = now_ns();
            for &pid in &pids {
                server.send_command(pid, NotifyCommand::ReportStatus, 0).unwrap();
            }
            let mut received = 0;
            let deadline = Instant::now() + Duration::from_secs(1);
            while received < n && Instant::now() < deadline {
                if let Ok(msgs) = server.poll(10) {
                    received += msgs.len();
                }
            }
            let t1 = now_ns();
            if received >= n {
                samples.push(t1 - t0);
            }
        }

        let s = compute_stats(&mut samples);
        print_table_row(&format!("{:>2} pubs", n), &s);

        kill_workers(&mut handles, &server);
    }

    print_table_footer();
    println!();
}

fn test_scaling() {
    println!("── Notify Test 4: Scaling (N connections, 1:1 ping, 8B) ──");
    println!();

    let iters = 50000;

    print_table_header();

    for n in [1, 2, 4, 8, 16, 32, 64] {
        let sock_path = PathBuf::from(format!("/tmp/kos_nb_t4_{}.sock", n));
        let mut server = NotifyServer::new(&sock_path).expect("server");
        let mut handles = setup_workers(n, &sock_path, &mut server);

        if handles.is_empty() {
            print_table_row(&format!("{:>2} conns (SKIP)", n), &Stats::default());
            kill_workers(&mut handles, &server);
            continue;
        }

        let target_pid = handles[0].pid;
        let mut samples = Vec::with_capacity(iters);

        for _ in 0..iters {
            let t0 = now_ns();
            server.send_command(target_pid, NotifyCommand::ReportStatus, 0).unwrap();
            loop {
                if let Ok(msgs) = server.poll(100) {
                    if msgs.iter().any(|(pid, _)| *pid == target_pid) { break; }
                }
            }
            let t1 = now_ns();
            samples.push(t1 - t0);
        }

        let s = compute_stats(&mut samples);
        print_table_row(&format!("{:>2} conns", n), &s);

        kill_workers(&mut handles, &server);
    }

    print_table_footer();
    println!();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 3 && args[1] == "--nb-echo" {
        echo_worker_main(&args[2]);
        return;
    }
    if args.len() >= 3 && args[1] == "--nb-ts" {
        timestamp_worker_main(&args[2]);
        return;
    }

    println!();
    println!("═══════════════════════════════════════════════════════════════════════");
    println!("  KOS Notify Channel — IPC Benchmark");
    println!("  iterations: 50000  msg: 8B  (all times in ns)");
    println!("═══════════════════════════════════════════════════════════════════════");
    println!();

    test_one_to_one();
    test_fanout();
    test_collect();
    test_scaling();

    println!("═══════════════════════════════════════════════════════════════════════");
    println!("  Done");
    println!("═══════════════════════════════════════════════════════════════════════");
}
