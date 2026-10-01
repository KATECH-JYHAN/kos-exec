// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::io::{BufRead, BufReader};
use std::time::{Duration, Instant};
use std::thread;

use kos_exec::notify::{NotifyCommand, NotifyServer};

fn self_exe() -> PathBuf {
    std::env::current_exe().expect("self exe")
}

struct Stats {
    avg: f64,
    p50: u64,
    p99: u64,
    max: u64,
    min: u64,
}

fn compute_stats(vals: &[u64]) -> Stats {
    if vals.is_empty() {
        return Stats { avg: 0.0, p50: 0, p99: 0, max: 0, min: 0 };
    }
    let sum: u64 = vals.iter().sum();
    let avg = sum as f64 / vals.len() as f64;
    let mut sorted = vals.to_vec();
    sorted.sort_unstable();
    let p50 = sorted[(sorted.len() as f64 * 0.50) as usize];
    let p99 = sorted[((sorted.len() as f64 * 0.99) as usize).min(sorted.len() - 1)];
    Stats { avg, p50, p99, max: *sorted.last().unwrap(), min: sorted[0] }
}

fn test_roundtrip_latency() {
    println!("── Test 1. Notification round trip (RM → process → RM) ──");
    println!("   NotifyServer sends ReportStatus → process replies with Utilization");
    println!("   1000 iterations");
    println!();

    let sock_path = NotifyServer::default_path();
    let mut server = NotifyServer::new(&sock_path).expect("server bind");

    let exe = self_exe();
    let mut child = Command::new(&exe)
        .args(["--notify-echo-worker", sock_path.to_str().unwrap()])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn worker");

    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout).lines();

    loop {
        if let Some(Ok(line)) = lines.next() {
            if line.starts_with("READY") { break; }
        }
    }

    thread::sleep(Duration::from_millis(100));
    let accepted = server.accept_pending().expect("accept");
    let temp_id = accepted[0];
    let worker_pid = child.id();
    server.register_client(temp_id, worker_pid, "echo_worker".into());

    for _ in 0..50 {
        let _ = server.send_command(worker_pid, NotifyCommand::ReportStatus, 0);
        let _ = server.poll(100);
    }

    let mut latencies = Vec::with_capacity(1000);

    for _ in 0..1000 {
        let start = Instant::now();
        server.send_command(worker_pid, NotifyCommand::ReportStatus, 0).expect("send");
        let msgs = server.poll(1000).expect("poll");
        let elapsed = start.elapsed().as_nanos() as u64;

        if !msgs.is_empty() {
            latencies.push(elapsed);
        }
    }

    let s = compute_stats(&latencies);
    println!("   {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "samples", "avg", "min", "p50", "p99", "max");
    println!("   {}", "─".repeat(66));
    println!("   {:>10} {:>8.1}μs {:>8.1}μs {:>8.1}μs {:>8.1}μs {:>8.1}μs",
        latencies.len(),
        s.avg / 1000.0, s.min as f64 / 1000.0,
        s.p50 as f64 / 1000.0, s.p99 as f64 / 1000.0,
        s.max as f64 / 1000.0);

    server.send_command(worker_pid, NotifyCommand::Shutdown, 0).ok();
    let _ = child.kill();
    let _ = child.wait();
    println!();
}

fn test_proc_read_cost() {
    println!("── Test 2. Cost of reading /proc/[pid]/stat ──");
    println!("   Read /proc/self/stat 10000 times");
    println!();

    let pid = std::process::id();
    let path = format!("/proc/{}/stat", pid);

    for _ in 0..100 {
        let _ = std::fs::read_to_string(&path);
    }

    let mut latencies = Vec::with_capacity(10000);
    for _ in 0..10000 {
        let start = Instant::now();
        let content = std::fs::read_to_string(&path).unwrap();
        let after_comm = content.rfind(')').unwrap() + 2;
        let fields: Vec<&str> = content[after_comm..].split_whitespace().collect();
        let _utime: u64 = fields[11].parse().unwrap_or(0);
        let _stime: u64 = fields[12].parse().unwrap_or(0);
        let elapsed = start.elapsed().as_nanos() as u64;
        latencies.push(elapsed);
    }

    let s = compute_stats(&latencies);
    println!("   {:>10} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "samples", "avg", "min", "p50", "p99", "max");
    println!("   {}", "─".repeat(66));
    println!("   {:>10} {:>8.1}μs {:>8.1}μs {:>8.1}μs {:>8.1}μs {:>8.1}μs",
        latencies.len(),
        s.avg / 1000.0, s.min as f64 / 1000.0,
        s.p50 as f64 / 1000.0, s.p99 as f64 / 1000.0,
        s.max as f64 / 1000.0);
    println!();
}

fn test_fanout_latency() {
    println!("── Test 3. Multi-process fan-out notification ──");
    println!("   Broadcast to N processes → time until all replies arrive");
    println!();

    for n_workers in [1, 4, 8, 16] {
        let sock_path = PathBuf::from(format!("/tmp/kos_notify_fan_{}.sock", n_workers));
        let mut server = NotifyServer::new(&sock_path).expect("server");
        let exe = self_exe();

        let mut children: Vec<Child> = Vec::new();

        for _ in 0..n_workers {
            let mut child = Command::new(&exe)
                .args(["--notify-echo-worker", sock_path.to_str().unwrap()])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("spawn");

            let stdout = child.stdout.take().unwrap();
            let mut lines = BufReader::new(stdout).lines();
            loop {
                if let Some(Ok(line)) = lines.next() {
                    if line.starts_with("READY") { break; }
                }
            }
            children.push(child);
        }

        thread::sleep(Duration::from_millis(200));
        let accepted = server.accept_pending().expect("accept");
        for (i, &temp_id) in accepted.iter().enumerate() {
            let pid = children[i].id();
            server.register_client(temp_id, pid, format!("worker_{}", i));
        }

        for _ in 0..20 {
            server.broadcast(NotifyCommand::ReportStatus, 0);
            thread::sleep(Duration::from_millis(10));
            let _ = server.poll(50);
        }

        let mut latencies = Vec::with_capacity(100);

        for _ in 0..100 {
            let start = Instant::now();
            server.broadcast(NotifyCommand::ReportStatus, 0);

            let mut received = 0;
            let deadline = Instant::now() + Duration::from_secs(1);
            while received < n_workers && Instant::now() < deadline {
                if let Ok(msgs) = server.poll(10) {
                    received += msgs.len();
                }
            }
            let elapsed = start.elapsed().as_nanos() as u64;
            if received >= n_workers {
                latencies.push(elapsed);
            }
        }

        let s = compute_stats(&latencies);
        println!("   {} processes: avg {:>8.1}μs  p50 {:>8.1}μs  p99 {:>8.1}μs  max {:>8.1}μs  ({} succeeded)",
            n_workers,
            s.avg / 1000.0, s.p50 as f64 / 1000.0,
            s.p99 as f64 / 1000.0, s.max as f64 / 1000.0,
            latencies.len());

        for child in children.iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    println!();
}

fn test_scaling_1to1() {
    println!("── Test 4. 1:1 latency as connections grow (ping 1 of N connections) ──");
    println!("   Checks that 1:1 round-trip latency holds as the number of connections grows");
    println!();
    println!("   {:>6} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "conns", "avg", "min", "p50", "p99", "max");
    println!("   {}", "─".repeat(62));

    for n_total in [1, 4, 8, 16] {
        let sock_path = PathBuf::from(format!("/tmp/kos_notify_s1_{}.sock", n_total));
        let mut server = NotifyServer::new(&sock_path).expect("server");
        let exe = self_exe();

        let mut children: Vec<Child> = Vec::new();
        let mut pids: Vec<u32> = Vec::new();

        for _ in 0..n_total {
            let mut child = Command::new(&exe)
                .args(["--notify-echo-worker", sock_path.to_str().unwrap()])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("spawn");

            let stdout = child.stdout.take().unwrap();
            let mut lines = BufReader::new(stdout).lines();
            loop {
                if let Some(Ok(line)) = lines.next() {
                    if line.starts_with("READY") { break; }
                }
            }
            pids.push(child.id());
            children.push(child);
        }

        thread::sleep(Duration::from_millis(200));
        let accepted = server.accept_pending().expect("accept");
        for (i, &temp_id) in accepted.iter().enumerate() {
            server.register_client(temp_id, pids[i], format!("w_{}", i));
        }

        let target_pid = pids[0];

        for _ in 0..50 {
            let _ = server.send_command(target_pid, NotifyCommand::ReportStatus, 0);
            let _ = server.poll(100);
        }

        let mut latencies = Vec::with_capacity(1000);
        for _ in 0..1000 {
            let start = Instant::now();
            server.send_command(target_pid, NotifyCommand::ReportStatus, 0).expect("send");
            let deadline = Instant::now() + Duration::from_millis(100);
            let mut got = false;
            while Instant::now() < deadline {
                if let Ok(msgs) = server.poll(10) {
                    for (pid, _msg) in &msgs {
                        if *pid == target_pid {
                            got = true;
                        }
                    }
                    if got { break; }
                }
            }
            if got {
                latencies.push(start.elapsed().as_nanos() as u64);
            }
        }

        let s = compute_stats(&latencies);
        println!("   {:>6} {:>8.1}μs {:>8.1}μs {:>8.1}μs {:>8.1}μs {:>8.1}μs",
            n_total,
            s.avg / 1000.0, s.min as f64 / 1000.0,
            s.p50 as f64 / 1000.0, s.p99 as f64 / 1000.0,
            s.max as f64 / 1000.0);

        for child in children.iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    println!();
}

fn test_sequential_ping() {
    println!("── Test 5. Sequential pings (1:1 round trip to each of N processes) ──");
    println!("   Pings each process in turn with send_command instead of broadcast");
    println!();
    println!("   {:>6} {:>12} {:>12} {:>12}",
        "N", "total", "per process", "vs broadcast");
    println!("   {}", "─".repeat(50));

    for n_total in [1, 4, 8, 16] {
        let sock_path = PathBuf::from(format!("/tmp/kos_notify_sq_{}.sock", n_total));
        let mut server = NotifyServer::new(&sock_path).expect("server");
        let exe = self_exe();

        let mut children: Vec<Child> = Vec::new();
        let mut pids: Vec<u32> = Vec::new();

        for _ in 0..n_total {
            let mut child = Command::new(&exe)
                .args(["--notify-echo-worker", sock_path.to_str().unwrap()])
                .stdout(Stdio::piped())
                .stderr(Stdio::inherit())
                .spawn()
                .expect("spawn");

            let stdout = child.stdout.take().unwrap();
            let mut lines = BufReader::new(stdout).lines();
            loop {
                if let Some(Ok(line)) = lines.next() {
                    if line.starts_with("READY") { break; }
                }
            }
            pids.push(child.id());
            children.push(child);
        }

        thread::sleep(Duration::from_millis(200));
        let accepted = server.accept_pending().expect("accept");
        for (i, &temp_id) in accepted.iter().enumerate() {
            server.register_client(temp_id, pids[i], format!("w_{}", i));
        }

        for _ in 0..20 {
            for &pid in &pids {
                let _ = server.send_command(pid, NotifyCommand::ReportStatus, 0);
            }
            thread::sleep(Duration::from_millis(10));
            let _ = server.poll(50);
        }

        let mut total_latencies = Vec::with_capacity(100);

        for _ in 0..100 {
            let start = Instant::now();
            let mut received = 0;

            for &pid in &pids {
                server.send_command(pid, NotifyCommand::ReportStatus, 0).expect("send");
            }

            let deadline = Instant::now() + Duration::from_secs(1);
            while received < n_total && Instant::now() < deadline {
                if let Ok(msgs) = server.poll(10) {
                    received += msgs.len();
                }
            }

            if received >= n_total {
                total_latencies.push(start.elapsed().as_nanos() as u64);
            }
        }

        let s = compute_stats(&total_latencies);
        let per_proc = s.avg / n_total as f64;

        println!("   {:>6} {:>10.1}μs {:>10.1}μs {:>10}",
            n_total,
            s.avg / 1000.0,
            per_proc / 1000.0,
            "");

        for child in children.iter_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    println!();
}

fn echo_worker_main(sock_path: &str) {
    println!("READY {}", std::process::id());

    let client = kos_exec::notify::NotifyClient::connect(std::path::Path::new(sock_path))
        .expect("connect to notify server");

    loop {
        match client.poll(Some(Duration::from_secs(5))) {
            Ok(Some(msg)) => {
                match msg.as_command() {
                    Some(NotifyCommand::ReportStatus) => {
                        let _ = client.report_utilization(42.5);
                    }
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

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 3 && args[1] == "--notify-echo-worker" {
        echo_worker_main(&args[2]);
        return;
    }

    println!();
    println!("══════════════════════════════════════════════════════════════════");
    println!("  KOS Notify Channel latency benchmark");
    println!("══════════════════════════════════════════════════════════════════");
    println!();

    test_roundtrip_latency();
    test_proc_read_cost();
    test_fanout_latency();
    test_scaling_1to1();
    test_sequential_ping();

    println!("══════════════════════════════════════════════════════════════════");
    println!("  Done");
    println!("══════════════════════════════════════════════════════════════════");
}
