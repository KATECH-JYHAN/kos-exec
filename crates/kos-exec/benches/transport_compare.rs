// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

mod common;

use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

use kos_exec::comm::{ShmTransport, Transport};

const SIZES: [usize; 3] = [64, 1024, 4096];
const SUBSCRIBERS: [usize; 3] = [1, 4, 16];

fn make_transport(backend: &str) -> Arc<dyn Transport> {
    match backend {
        #[cfg(feature = "iceoryx2")]
        "iceoryx2" => {
            iceoryx2::prelude::set_log_level_from_env_or(iceoryx2::prelude::LogLevel::Error);
            Arc::new(kos_exec::comm_iox2::Iox2Transport::new("bench").expect("iceoryx2 node"))
        }
        "kos-comm" => Arc::new(ShmTransport::new("bench")),
        other => panic!("unknown backend {other}"),
    }
}

fn backends() -> Vec<&'static str> {
    let mut v = vec!["kos-comm"];
    if cfg!(feature = "iceoryx2") {
        v.push("iceoryx2");
    }
    v
}

fn init_worker() {
    common::init_measure_worker();
    if let Some(prio) = std::env::var("KOS_BENCH_RT_PRIO").ok().and_then(|v| v.parse::<i32>().ok()) {
        let param = libc::sched_param { sched_priority: prio };
        if unsafe { libc::sched_setscheduler(0, libc::SCHED_FIFO, &param) } != 0 {
            eprintln!("[bench] WARNING: SCHED_FIFO {prio} failed (requires root)");
        }
    }
}

fn cpu_time_ns() -> u64 {
    let mut ru: libc::rusage = unsafe { std::mem::zeroed() };
    unsafe { libc::getrusage(libc::RUSAGE_SELF, &mut ru) };
    let tv = |t: libc::timeval| t.tv_sec as u64 * 1_000_000_000 + t.tv_usec as u64 * 1000;
    tv(ru.ru_utime) + tv(ru.ru_stime)
}

fn publisher_main(backend: &str, topic: &str, size: usize, count: u64, rate_hz: u64) {
    init_worker();
    let transport = make_transport(backend);
    let mut publisher = transport.publisher(topic).expect("publisher");
    let period_ns = 1_000_000_000 / rate_hz;
    let mut buf = vec![0u8; size];

    std::thread::sleep(Duration::from_millis(100));
    let mut next = common::mono_now_ns();
    for seq in 0..count {
        common::sleep_until_ns(next);
        buf[0..8].copy_from_slice(&seq.to_ne_bytes());
        buf[8..16].copy_from_slice(&common::mono_now_ns().to_ne_bytes());
        publisher.publish(&buf).expect("publish");
        next += period_ns;
    }
    std::thread::sleep(Duration::from_millis(20));
    buf[0..8].copy_from_slice(&u64::MAX.to_ne_bytes());
    let _ = publisher.publish(&buf);
    std::thread::sleep(Duration::from_millis(50));
}

fn subscriber_main(backend: &str, topic: &str, count: u64) {
    init_worker();
    let transport = make_transport(backend);
    let mut sub = transport.subscriber(topic).expect("subscriber");
    println!("READY");

    let mut latencies: Vec<u64> = Vec::with_capacity(count as usize);
    let mut received = 0u64;
    let cpu_start = cpu_time_ns();
    let wall_start = common::mono_now_ns();
    let mut first_ns = 0u64;
    let deadline = common::mono_now_ns() + 30_000_000_000;

    loop {
        if common::mono_now_ns() > deadline {
            break;
        }
        let Ok(Some(data)) = sub.wait_latest(Duration::from_millis(100)) else { continue };
        let now = common::mono_now_ns();
        if data.len() < 16 {
            continue;
        }
        let seq = u64::from_ne_bytes(data[0..8].try_into().unwrap());
        if seq == u64::MAX {
            break;
        }
        let sent = u64::from_ne_bytes(data[8..16].try_into().unwrap());
        if first_ns == 0 {
            first_ns = now;
        }
        latencies.push(now.saturating_sub(sent));
        received += 1;
    }
    let cpu = cpu_time_ns() - cpu_start;
    let active_wall = common::mono_now_ns().saturating_sub(if first_ns > 0 { first_ns } else { wall_start });
    let cpu_pct = if active_wall > 0 { cpu as f64 / active_wall as f64 * 100.0 } else { 0.0 };

    println!("RECEIVED {received}");
    println!("CPU {cpu_pct:.2}");
    println!(
        "LAT {}",
        latencies.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" ")
    );
}

struct RunResult {
    p50: u64,
    p99: u64,
    max: u64,
    skip_pct: f64,
    cpu_pct: f64,
}

fn run_case(backend: &str, size: usize, subs: usize, count: u64, rate_hz: u64) -> RunResult {
    let exe = std::env::current_exe().unwrap();
    let topic = format!("kosbench/{backend}_{size}_{subs}_{}", std::process::id());

    let mut children = Vec::new();
    for _ in 0..subs {
        let mut child = Command::new(&exe)
            .args(["--sub", backend, &topic, &count.to_string()])
            .stdout(Stdio::piped())
            .spawn()
            .expect("spawn subscriber");
        let mut reader = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "READY", "subscriber failed to start");
        children.push((child, reader));
    }

    let status = Command::new(&exe)
        .args(["--pub", backend, &topic, &size.to_string(), &count.to_string(), &rate_hz.to_string()])
        .status()
        .expect("spawn publisher");
    assert!(status.success(), "publisher failed");

    let mut all = Vec::new();
    let mut received_total = 0u64;
    let mut cpu_sum = 0.0;
    for (mut child, mut reader) in children {
        let mut out = String::new();
        std::io::Read::read_to_string(&mut reader, &mut out).unwrap();
        let _ = child.wait();
        for line in out.lines() {
            if let Some(v) = line.strip_prefix("RECEIVED ") {
                received_total += v.trim().parse::<u64>().unwrap_or(0);
            } else if let Some(v) = line.strip_prefix("CPU ") {
                cpu_sum += v.trim().parse::<f64>().unwrap_or(0.0);
            } else if let Some(v) = line.strip_prefix("LAT ") {
                all.extend(v.split_whitespace().filter_map(|x| x.parse::<u64>().ok()));
            }
        }
    }
    all.sort_unstable();
    let pct = |p: f64| all.get(((all.len() as f64 * p) as usize).min(all.len().saturating_sub(1))).copied().unwrap_or(0);
    let expected = count * subs as u64;
    RunResult {
        p50: pct(0.50),
        p99: pct(0.99),
        max: all.last().copied().unwrap_or(0),
        skip_pct: (expected.saturating_sub(received_total)) as f64 / expected as f64 * 100.0,
        cpu_pct: cpu_sum / subs as f64,
    }
}

fn us(ns: u64) -> String {
    format!("{:.1}", ns as f64 / 1000.0)
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() >= 7 && args[1] == "--pub" {
        publisher_main(&args[2], &args[3], args[4].parse().unwrap(), args[5].parse().unwrap(), args[6].parse().unwrap());
        return;
    }
    if args.len() >= 5 && args[1] == "--sub" {
        subscriber_main(&args[2], &args[3], args[4].parse().unwrap());
        return;
    }

    let count = common::env_u64("KOS_BENCH_MESSAGES", 2000);
    let rate_hz = common::env_u64("KOS_BENCH_RATE_HZ", 1000);

    println!();
    println!("══════════════════════════════════════════════════════════════════");
    println!("  pub/sub backend comparison (through the Transport interface)");
    println!("  1 publisher + N subscribers (separate processes), {rate_hz}Hz × {count} messages, subscribers use wait_latest");
    println!("  backends: {}", backends().join(", "));
    println!("══════════════════════════════════════════════════════════════════");
    common::print_env_report();

    println!(
        "   {:>9} {:>6} {:>5} │ {:>9} {:>9} {:>9} {:>7} {:>8}",
        "backend", "size", "subs", "p50(us)", "p99(us)", "max(us)", "skip%", "subCPU%"
    );
    println!("   {}", "─".repeat(78));
    for &size in &SIZES {
        for &subs in &SUBSCRIBERS {
            for backend in backends() {
                let r = run_case(backend, size, subs, count, rate_hz);
                println!(
                    "   {:>9} {:>5}B {:>5} │ {:>9} {:>9} {:>9} {:>6.2}% {:>7.2}%",
                    backend, size, subs, us(r.p50), us(r.p99), us(r.max), r.skip_pct, r.cpu_pct
                );
            }
        }
        println!();
    }
    common::thermal_snapshot("end");
    println!("   * latency = time just before publish → time just after receive (same-host CLOCK_MONOTONIC)");
    println!("   * skip% = share of messages not received. Subscribers read the latest value, so a slow subscriber skips intermediate ones");
    println!("   * subCPU% = average CPU usage per subscriber process");
    match std::env::var("KOS_BENCH_RT_PRIO") {
        Ok(p) => println!("   * workers SCHED_FIFO {p}"),
        Err(_) => println!("   * workers CFS (KOS_BENCH_RT_PRIO not set)"),
    }
}
