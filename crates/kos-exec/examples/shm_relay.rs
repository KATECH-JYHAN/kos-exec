// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use kos_exec::comm::{ShmTransport, Transport};
use kos_exec::{Node, NodeConfig, ThreadCallbacks, ThreadConfig};

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Sample {
    seq: u64,
    sent_ns: u64,
}

fn mono_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn make_transport() -> kos_exec::Result<Arc<dyn Transport>> {
    match std::env::var("KOS_TRANSPORT").as_deref() {
        #[cfg(feature = "iceoryx2")]
        Ok("iceoryx2") => Ok(Arc::new(kos_exec::comm_iox2::Iox2Transport::new("shm_relay")?)),
        #[cfg(not(feature = "iceoryx2"))]
        Ok("iceoryx2") => {
            eprintln!("iceoryx2 backend requires --features iceoryx2");
            std::process::exit(2);
        }
        _ => Ok(Arc::new(ShmTransport::new("shm_relay"))),
    }
}

fn main() -> kos_exec::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let mode = args.get(1).map(String::as_str).unwrap_or("");
    let topic = args.get(2).cloned().unwrap_or_else(|| "demo/counter".into());
    let run_for = args.get(3).and_then(|s| s.parse::<u64>().ok()).map(Duration::from_secs);

    let mut node = Node::new("shm_relay", NodeConfig::periodic(Duration::from_millis(10)));
    node.set_transport(make_transport()?);

    let count = Arc::new(AtomicU64::new(0));
    match mode {
        "pub" => {
            let c = count.clone();
            let t = topic.clone();
            node.create_thread(
                ThreadConfig::periodic("producer", Duration::from_millis(10)).with_pubs(vec![&topic]),
                ThreadCallbacks::new(move |ctx| {
                    let seq = c.fetch_add(1, Ordering::Relaxed) + 1;
                    ctx.write(&t, &Sample { seq, sent_ns: mono_ns() });
                }),
            )?;
        }
        "sub" => {
            let c = count.clone();
            let t = topic.clone();
            node.create_thread(
                ThreadConfig::event("consumer", &topic),
                ThreadCallbacks::new(move |ctx| {
                    let s: Sample = ctx.read(&t);
                    let n = c.fetch_add(1, Ordering::Relaxed) + 1;
                    let lat_us = mono_ns().saturating_sub(s.sent_ns) as f64 / 1000.0;
                    println!("recv #{n}: seq={} latency={lat_us:.1}us", s.seq);
                }),
            )?;
        }
        _ => {
            eprintln!("usage: shm_relay <pub|sub> [topic] [seconds]");
            std::process::exit(2);
        }
    }

    let start = Instant::now();
    node.spin()?;
    while run_for.is_none_or(|d| start.elapsed() < d) {
        std::thread::sleep(Duration::from_millis(50));
    }
    node.shutdown();
    eprintln!("{mode}: {} messages", count.load(Ordering::Relaxed));
    Ok(())
}
