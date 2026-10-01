// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use kos_exec::resource_manager::{AsilLevel, ResourceConfig, ResourceEvent, ResourceManager};
use kos_exec::cgroup;

fn self_exe() -> std::path::PathBuf {
    std::env::current_exe().expect("cannot determine self exe path")
}

fn cpu_burn(duration_us: u64) {
    let start = Instant::now();
    let target = Duration::from_micros(duration_us);
    while start.elapsed() < target {
        std::hint::black_box(0u64.wrapping_mul(42));
    }
}

#[allow(dead_code)]
fn pin_to_core(core: usize) {
    use nix::sched::{sched_setaffinity, CpuSet};
    use nix::unistd::Pid;
    let mut cpuset = CpuSet::new();
    let _ = cpuset.set(core);
    let _ = sched_setaffinity(Pid::from_raw(0), &cpuset);
}

fn num_cpus() -> usize {
    std::fs::read_to_string("/proc/cpuinfo")
        .map(|s| s.matches("processor").count())
        .unwrap_or(1)
}

fn spawn_burn(core: usize, burn_us: u64, sleep_us: u64) -> Child {
    let exe = self_exe();
    Command::new("taskset")
        .args(["-c", &core.to_string()])
        .arg(&exe)
        .args(["--rm-worker", &burn_us.to_string(), &sleep_us.to_string()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn worker")
}

fn spawn_dynamic_burn(core: usize, low_burn_us: u64, high_burn_us: u64, sleep_us: u64, phase1_s: u64) -> Child {
    let exe = self_exe();
    Command::new("taskset")
        .args(["-c", &core.to_string()])
        .arg(&exe)
        .args([
            "--rm-dynamic-worker",
            &low_burn_us.to_string(),
            &high_burn_us.to_string(),
            &sleep_us.to_string(),
            &phase1_s.to_string(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("failed to spawn dynamic worker")
}

fn kill_children(children: &mut Vec<Child>) {
    for child in children.iter_mut() {
        let _ = child.kill();
    }
    for child in children.iter_mut() {
        let _ = child.wait();
    }
    children.clear();
}

#[allow(dead_code)]
fn get_process_core(pid: u32) -> Option<usize> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", pid)).ok()?;
    let after_comm = stat.rfind(')')? + 2;
    let fields: Vec<&str> = stat[after_comm..].split_whitespace().collect();
    if fields.len() > 36 {
        fields[36].parse().ok()
    } else {
        None
    }
}

fn test_utilization_tracking() {
    println!("── Test 1. CPU utilization tracking accuracy ──");
    println!("   measured utilization of processes at 20%, 50%, and 80% load");
    println!();

    let config = ResourceConfig {
        critical_cores: vec![0, 1, 2, 3],
        nc_cores: (4..num_cpus()).collect(),
        sampling_interval: Duration::from_millis(100),
        ..Default::default()
    };
    let mut rm = ResourceManager::new(config);

    let test_cases: Vec<(&str, usize, u64, u64, f64)> = vec![
        ("low",  0, 2000,  8000, 20.0),
        ("mid",  1, 5000,  5000, 50.0),
        ("high", 2, 8000,  2000, 80.0),
    ];

    let mut children = Vec::new();

    for &(label, core, burn, sleep, expected) in &test_cases {
        let child = spawn_burn(core, burn, sleep);
        let pid = child.id();
        rm.register_process(pid, label.to_string(), AsilLevel::D, core, expected);
        children.push(child);
    }

    thread::sleep(Duration::from_secs(1));

    println!("   {:>8} {:>6} {:>10} {:>10} {:>10}",
        "process", "core", "expected", "measured", "error");
    println!("   {}", "─".repeat(52));

    for _ in 0..10 {
        thread::sleep(Duration::from_millis(200));
        let _ = rm.tick();
    }

    for &(label, core, _, _, expected) in &test_cases {
        let pid = children.iter()
            .find(|c| {
                rm.process_utilization(c.id()).is_some()
                    && rm.process_app_id(c.id()) == Some(label)
            })
            .map(|c| c.id())
            .unwrap_or(0);

        if let Some(actual) = rm.process_utilization(pid) {
            let error = (actual - expected).abs();
            let status = if error < 10.0 { "✅" } else { "⚠️" };
            println!("   {:>8} {:>6} {:>9.1}% {:>9.1}% {:>8.1}% {}",
                label, core, expected, actual, error, status);
        }
    }

    println!();
    println!("   total utilization per core:");
    for core in 0..4 {
        if let Some(util) = rm.core_utilization(core) {
            println!("   core {}: {:.1}%", core, util);
        }
    }

    kill_children(&mut children);
    println!();
}

fn test_overload_detection() {
    println!("── Test 2. Automatic overload detection ──");
    println!("   4 processes on core 0 (20% each) → 80% → RED detected");
    println!();

    let config = ResourceConfig {
        critical_cores: vec![0, 1, 2, 3],
        nc_cores: (4..num_cpus()).collect(),
        sampling_interval: Duration::from_millis(100),
        ..Default::default()
    };
    let mut rm = ResourceManager::new(config);
    let mut children = Vec::new();

    for i in 0..4 {
        let child = spawn_burn(0, 2000, 8000);
        let pid = child.id();
        rm.register_process(pid, format!("app_{}", i), AsilLevel::D, 0, 20.0);
        children.push(child);
    }

    thread::sleep(Duration::from_secs(1));

    println!("   time(s)  core0 util  alert  event");
    println!("   {}", "─".repeat(60));

    let start = Instant::now();
    let mut total_events = Vec::new();

    for _ in 0..40 {
        thread::sleep(Duration::from_millis(200));
        let events = rm.tick();

        let elapsed = start.elapsed().as_secs_f64();
        let core0_util = rm.core_utilization(0).unwrap_or(0.0);

        if !events.is_empty() {
            for event in &events {
                let event_str = match event {
                    ResourceEvent::CoreWarning { core_id, utilization } =>
                        format!("⚠ CoreWarning core={} util={:.1}%", core_id, utilization),
                    ResourceEvent::CoreCritical { core_id, utilization } =>
                        format!("🔴 CoreCritical core={} util={:.1}%", core_id, utilization),
                    ResourceEvent::ProcessMigrated { pid, from_core, to_core, reason, .. } =>
                        format!("→ Migrated pid={} core {}→{} ({})", pid, from_core, to_core, reason),
                    ResourceEvent::CoreRecovered { core_id, utilization } =>
                        format!("✅ Recovered core={} util={:.1}%", core_id, utilization),
                    ResourceEvent::DegradedSignal { pid, app_id } =>
                        format!("⬇ Degraded pid={} app={}", pid, app_id),
                    ResourceEvent::UtilizationSpike { pid, app_id, from_pct, to_pct } =>
                        format!("📈 Spike pid={} {} {:.1}%→{:.1}%", pid, app_id, from_pct, to_pct),
                };
                println!("   {:>6.1}s  {:>10.1}%  {:>10}  {}",
                    elapsed, core0_util, "", event_str);
            }
            total_events.extend(events);
        }
    }

    let migrations = total_events.iter()
        .filter(|e| matches!(e, ResourceEvent::ProcessMigrated { .. }))
        .count();
    let warnings = total_events.iter()
        .filter(|e| matches!(e, ResourceEvent::CoreWarning { .. } | ResourceEvent::CoreCritical { .. }))
        .count();

    println!();
    println!("   result: {} warnings, {} migrations", warnings, migrations);

    println!();
    println!("   cores after migration:");
    let snap = rm.snapshot();
    for cs in &snap.cores {
        if cs.core_id < 4 && cs.process_count > 0 {
            println!("   core {}: {} processes, {:.1}%", cs.core_id, cs.process_count, cs.utilization);
        }
    }

    kill_children(&mut children);
    println!();
}

fn test_dynamic_load_response() {
    println!("── Test 3. Reacting to changing load ──");
    println!("   core 0: app_a (20%) + app_b (20%) → app_b jumps to 60% after 3 seconds");
    println!("   checks that the resource manager detects and reacts automatically");
    println!();

    let config = ResourceConfig {
        critical_cores: vec![0, 1, 2, 3],
        nc_cores: (4..num_cpus()).collect(),
        sampling_interval: Duration::from_millis(100),
        yellow_hold: Duration::from_secs(2),
        ..Default::default()
    };
    let mut rm = ResourceManager::new(config);
    let mut children = Vec::new();

    let child_a = spawn_burn(0, 2000, 8000);
    rm.register_process(child_a.id(), "app_a".to_string(), AsilLevel::D, 0, 20.0);
    children.push(child_a);

    let child_b = spawn_dynamic_burn(0, 2000, 6000, 4000, 3);
    rm.register_process(child_b.id(), "app_b".to_string(), AsilLevel::C, 0, 20.0);
    children.push(child_b);

    thread::sleep(Duration::from_millis(500));

    println!("   time(s)  core0    app_a    app_b    event");
    println!("   {}", "─".repeat(65));

    let start = Instant::now();
    let pid_a = children[0].id();
    let pid_b = children[1].id();

    for _ in 0..60 {
        thread::sleep(Duration::from_millis(200));
        let events = rm.tick();

        let elapsed = start.elapsed().as_secs_f64();
        let core0 = rm.core_utilization(0).unwrap_or(0.0);
        let util_a = rm.process_utilization(pid_a).unwrap_or(0.0);
        let util_b = rm.process_utilization(pid_b).unwrap_or(0.0);

        let event_str: String = events.iter().map(|e| match e {
            ResourceEvent::ProcessMigrated { app_id, from_core, to_core, .. } =>
                format!("→{} {}→{}", app_id, from_core, to_core),
            ResourceEvent::CoreWarning { core_id, .. } =>
                format!("⚠c{}", core_id),
            ResourceEvent::CoreCritical { core_id, .. } =>
                format!("🔴c{}", core_id),
            ResourceEvent::UtilizationSpike { app_id, to_pct, .. } =>
                format!("📈{} {:.0}%", app_id, to_pct),
            ResourceEvent::CoreRecovered { core_id, .. } =>
                format!("✅c{}", core_id),
            _ => String::new(),
        }).collect::<Vec<_>>().join(" ");

        if !events.is_empty() || (elapsed * 5.0) as u64 % 5 == 0 {
            println!("   {:>6.1}s  {:>5.1}%  {:>6.1}%  {:>6.1}%  {}",
                elapsed, core0, util_a, util_b, event_str);
        }
    }

    println!();
    println!("   final cores:");
    let snap = rm.snapshot();
    for ps in &snap.processes {
        println!("   {} (pid {}): core {}, {:.1}%, movable={}",
            ps.app_id, ps.pid, ps.core, ps.utilization, ps.movable);
    }

    kill_children(&mut children);
    println!();
}

fn test_initial_placement() {
    println!("── Test 4. Initial placement algorithm ──");
    println!("   spread 6 processes over 4 cores by expected_utilization");
    println!();

    let config = ResourceConfig {
        critical_cores: vec![0, 1, 2, 3],
        nc_cores: (4..num_cpus()).collect(),
        ..Default::default()
    };
    let critical_count = config.critical_cores.len();
    let yellow_thresh = config.yellow_threshold;
    let rm = ResourceManager::new(config);

    let processes = vec![
        (25.0, AsilLevel::D),
        (20.0, AsilLevel::D),
        (20.0, AsilLevel::C),
        (15.0, AsilLevel::D),
        (10.0, AsilLevel::C),
        (5.0,  AsilLevel::C),
    ];

    match rm.validate_capacity(&processes) {
        Ok(()) => println!("   capacity check: ✅ passed (total {:.0}%, available {:.0}%)",
            processes.iter().map(|(u, _)| u).sum::<f64>(),
            critical_count as f64 * yellow_thresh),
        Err(e) => println!("   capacity check: ❌ {}", e),
    }

    println!();
    println!("   {:>12} {:>6} {:>10} {:>10}",
        "process", "ASIL", "expected", "core");
    println!("   {}", "─".repeat(44));

    let mut sorted = processes.clone();
    sorted.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());

    let mut placements = Vec::new();
    let mut sim_rm = ResourceManager::new(ResourceConfig {
        critical_cores: vec![0, 1, 2, 3],
        nc_cores: (4..num_cpus()).collect(),
        ..Default::default()
    });

    for (i, &(util, asil)) in sorted.iter().enumerate() {
        let core = sim_rm.find_best_core(util, asil);
        let label = format!("proc_{}", i);
        if let Some(c) = core {
            let fake_pid = (1000 + i) as u32;
            sim_rm.register_process(
                fake_pid,
                label.clone(),
                asil,
                c,
                util,
            );
            sim_rm.set_simulated_load(fake_pid, util);
            placements.push((label, asil, util, c));
        }
    }

    for (label, asil, util, core) in &placements {
        println!("   {:>12} {:>6?} {:>9.1}% {:>10}",
            label, asil, util, core);
    }

    println!();
    println!("   total per core:");
    for core in 0..4 {
        let util = sim_rm.core_utilization(core).unwrap_or(0.0);
        let count = sim_rm.core_process_count(core);
        println!("   core {}: {:.1}% ({} processes)", core, util, count);
    }

    println!();
}

fn main() {
    let args: Vec<String> = std::env::args().collect();

    if args.len() >= 4 && args[1] == "--rm-worker" {
        let burn_us: u64 = args[2].parse().unwrap_or(2000);
        let sleep_us: u64 = args[3].parse().unwrap_or(8000);
        loop {
            cpu_burn(burn_us);
            if sleep_us > 0 {
                thread::sleep(Duration::from_micros(sleep_us));
            }
        }
    }

    if args.len() >= 6 && args[1] == "--rm-dynamic-worker" {
        let low_burn: u64 = args[2].parse().unwrap_or(2000);
        let high_burn: u64 = args[3].parse().unwrap_or(6000);
        let sleep_us: u64 = args[4].parse().unwrap_or(4000);
        let phase1_s: u64 = args[5].parse().unwrap_or(3);

        let start = Instant::now();
        let phase1_dur = Duration::from_secs(phase1_s);

        loop {
            let burn = if start.elapsed() < phase1_dur { low_burn } else { high_burn };
            cpu_burn(burn);
            if sleep_us > 0 {
                thread::sleep(Duration::from_micros(sleep_us));
            }
        }
    }

    let total_cpus = num_cpus();
    println!();
    println!("══════════════════════════════════════════════════════════════════");
    println!("  KOS Resource Manager integration test");
    println!("  CPU: {} cores", total_cpus);
    println!("══════════════════════════════════════════════════════════════════");

    let cores_u32: Vec<u32> = [0u32, 1, 2, 3].to_vec();
    let _cstate_guard = cgroup::disable_cstates_for_cores(&cores_u32);
    let _pmqos = if _cstate_guard.is_none() { cgroup::disable_cstates() } else { None };
    println!();

    test_initial_placement();
    test_utilization_tracking();
    test_overload_detection();
    test_dynamic_load_response();

    println!("══════════════════════════════════════════════════════════════════");
    println!("  Done");
    println!("══════════════════════════════════════════════════════════════════");
}
