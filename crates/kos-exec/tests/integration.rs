// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use kos_exec::comm::{CommHandle, MockTransport};
use kos_exec::diag::{DiagNotifier, LogOnlyDiag};
use kos_exec::domain::{AsilLevel, DomainConfig, DomainController};
use kos_exec::launch::{AppInfo, Launcher};
use kos_exec::node::{Node, NodeConfig};
use kos_exec::thread_context::ThreadContext;
use kos_exec::thread_manager::{Priority, ThreadCallbacks, ThreadConfig};
use kos_exec::main_task::{MainTask, MainTaskConfig};
use kos_exec::KosError;

fn load_toml(name: &str) -> String {
    let path = format!(
        "{}/tests/scenarios/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {path}: {e}"))
}

fn state_str(info: &AppInfo) -> String {
    format!("{}", info.state).to_uppercase()
}

#[test]
fn scenario1_adas_pipeline_launch_and_shutdown() {
    let toml = load_toml("adas_pipeline.toml");
    let mut launcher = Launcher::from_toml(&toml).unwrap();

    let order = launcher.dependency.resolve_order().unwrap();
    assert!(order.len() >= 2, "should have at least 2 dependency layers");

    let camera_layer = order
        .iter()
        .position(|l| l.contains(&"adas.camera".to_string()))
        .unwrap();
    let fusion_layer = order
        .iter()
        .position(|l| l.contains(&"adas.fusion".to_string()))
        .unwrap();
    let lka_layer = order
        .iter()
        .position(|l| l.contains(&"adas.lka".to_string()))
        .unwrap();

    assert!(camera_layer < fusion_layer);
    assert!(fusion_layer < lka_layer);

    launcher.start().unwrap();

    let infos = launcher.all_app_info();
    assert_eq!(infos.len(), 3);
    for info in &infos {
        assert_eq!(state_str(info), "RUNNING", "app {} not running", info.app_id);
        assert!(info.pid.is_some(), "app {} has no PID", info.app_id);
    }

    let lka = launcher.app_info("adas.lka").unwrap();
    assert_eq!(lka.domain, "adas");
    assert!(lka.schedule.contains("periodic"));

    launcher.shutdown().unwrap();

    for info in &launcher.all_app_info() {
        assert_eq!(state_str(info), "TERMINATED");
    }
}

#[test]
fn scenario2_multi_domain_launch() {
    let toml = load_toml("multi_domain.toml");
    let mut launcher = Launcher::from_toml(&toml).unwrap();

    launcher.start().unwrap();

    let infos = launcher.all_app_info();
    assert_eq!(infos.len(), 5);

    let adas_apps: Vec<_> = infos.iter().filter(|i| i.domain == "adas").collect();
    let ivi_apps: Vec<_> = infos.iter().filter(|i| i.domain == "ivi").collect();
    let body_apps: Vec<_> = infos.iter().filter(|i| i.domain == "body").collect();

    assert_eq!(adas_apps.len(), 1);
    assert_eq!(ivi_apps.len(), 2);
    assert_eq!(body_apps.len(), 2);

    let camera = launcher.app_info("adas.camera").unwrap();
    assert!(camera.schedule.contains("periodic/10ms"));

    let media = launcher.app_info("ivi.media").unwrap();
    assert!(media.schedule.contains("event"));

    let battery = launcher.app_info("body.battery").unwrap();
    assert!(battery.schedule.contains("cron"));

    launcher.shutdown().unwrap();
}

#[test]
fn scenario2_domain_filter_launch() {
    let toml = load_toml("multi_domain.toml");
    let mut launcher = Launcher::from_toml(&toml).unwrap();

    let ivi_apps: Vec<String> = launcher
        .app_configs()
        .iter()
        .filter(|c| c.domain == "ivi")
        .map(|c| c.id.clone())
        .collect();

    for id in &ivi_apps {
        launcher.start_app(id).unwrap();
    }

    let media_info = launcher.app_info("ivi.media").unwrap();
    assert_eq!(state_str(&media_info), "RUNNING");

    let navi_info = launcher.app_info("ivi.navi").unwrap();
    assert_eq!(state_str(&navi_info), "RUNNING");

    let camera_info = launcher.app_info("adas.camera").unwrap();
    assert_eq!(state_str(&camera_info), "INSTALLED");

    launcher.shutdown().unwrap();
}

#[test]
fn scenario3_diamond_dependency() {
    let toml = load_toml("dependency_chain.toml");
    let mut launcher = Launcher::from_toml(&toml).unwrap();

    let order = launcher.dependency.resolve_order().unwrap();

    let driver_layer = order
        .iter()
        .position(|l| l.contains(&"sys.driver".to_string()))
        .unwrap();

    let logger_layer = order
        .iter()
        .position(|l| l.contains(&"sys.logger".to_string()))
        .unwrap();
    let monitor_layer = order
        .iter()
        .position(|l| l.contains(&"sys.monitor".to_string()))
        .unwrap();

    let dashboard_layer = order
        .iter()
        .position(|l| l.contains(&"sys.dashboard".to_string()))
        .unwrap();

    assert!(driver_layer < logger_layer);
    assert!(driver_layer < monitor_layer);
    assert_eq!(logger_layer, monitor_layer, "logger & monitor should be same layer");
    assert!(dashboard_layer > logger_layer);

    launcher.start().unwrap();

    let before: Vec<(String, String)> = launcher
        .all_app_info()
        .iter()
        .map(|i| (i.app_id.clone(), state_str(i)))
        .collect();
    launcher.raise_signal("refresh");
    let started = launcher.run_schedules();
    let dashboard = state_str(&launcher.app_info("sys.dashboard").unwrap());
    launcher.shutdown().unwrap();

    for (id, state) in &before {
        let expected = if id == "sys.dashboard" { "INSTALLED" } else { "RUNNING" };
        assert_eq!(state, expected, "{id}");
    }
    assert_eq!(started, vec!["sys.dashboard"]);
    assert_eq!(dashboard, "RUNNING");
}

#[test]
fn scenario4_single_app_with_params() {
    let toml = load_toml("single_app.toml");
    let mut launcher = Launcher::from_toml(&toml).unwrap();

    assert_eq!(launcher.app_configs().len(), 1);

    let cfg = &launcher.app_configs()[0];
    assert_eq!(cfg.id, "test.hello");
    assert_eq!(cfg.params.get("message").unwrap(), "hello world");
    assert_eq!(cfg.params.get("count").unwrap(), "10");

    launcher.start().unwrap();

    let info = launcher.app_info("test.hello").unwrap();
    assert_eq!(state_str(&info), "RUNNING");
    assert!(info.schedule.contains("periodic/1000ms"));

    launcher.shutdown().unwrap();
}

#[test]
fn scenario5_suspend_resume_cycle() {
    let toml = load_toml("multi_domain.toml");
    let mut launcher = Launcher::from_toml(&toml).unwrap();
    launcher.start().unwrap();
    launcher.raise_signal("media_play");
    launcher.run_schedules();

    launcher.suspend_app("ivi.media").unwrap();
    let info = launcher.app_info("ivi.media").unwrap();
    assert_eq!(state_str(&info), "SUSPENDED");

    let camera = launcher.app_info("adas.camera").unwrap();
    assert_eq!(state_str(&camera), "RUNNING");

    launcher.resume_app("ivi.media").unwrap();
    let info = launcher.app_info("ivi.media").unwrap();
    assert_eq!(state_str(&info), "RUNNING");

    launcher.suspend_app("body.door").unwrap();
    assert_eq!(
        state_str(&launcher.app_info("body.door").unwrap()),
        "SUSPENDED"
    );
    launcher.resume_app("body.door").unwrap();
    assert_eq!(
        state_str(&launcher.app_info("body.door").unwrap()),
        "RUNNING"
    );

    launcher.shutdown().unwrap();
}

#[test]
fn scenario6_stop_and_start_individual() {
    let toml = load_toml("multi_domain.toml");
    let mut launcher = Launcher::from_toml(&toml).unwrap();
    launcher.start().unwrap();
    launcher.raise_signal("media_play");
    launcher.raise_signal("navi_start");
    launcher.run_schedules();

    launcher.stop_app("ivi.navi").unwrap();
    let info = launcher.app_info("ivi.navi").unwrap();
    assert_eq!(state_str(&info), "TERMINATED");

    assert_eq!(
        state_str(&launcher.app_info("ivi.media").unwrap()),
        "RUNNING"
    );
    assert_eq!(
        state_str(&launcher.app_info("adas.camera").unwrap()),
        "RUNNING"
    );

    launcher.stop_app("ivi.navi").unwrap();

    launcher.shutdown().unwrap();
}

#[test]
fn scenario7_restart_app() {
    let toml = load_toml("adas_pipeline.toml");
    let mut launcher = Launcher::from_toml(&toml).unwrap();
    launcher.start().unwrap();

    let pid_before = launcher.app_info("adas.camera").unwrap().pid;

    launcher.restart_app("adas.camera").unwrap();

    let info = launcher.app_info("adas.camera").unwrap();
    assert_eq!(state_str(&info), "RUNNING");

    assert_ne!(info.pid, pid_before, "PID should change after restart");

    launcher.shutdown().unwrap();
}

#[test]
fn scenario8_invalid_transitions() {
    let toml = load_toml("single_app.toml");
    let mut launcher = Launcher::from_toml(&toml).unwrap();
    launcher.start().unwrap();

    launcher.stop_app("test.hello").unwrap();
    let result = launcher.suspend_app("test.hello");
    assert!(result.is_err());

    let result = launcher.resume_app("test.hello");
    assert!(result.is_err());
}

#[test]
fn scenario9_domain_access_policy() {
    let configs = vec![
        DomainConfig {
            id: "adas".into(),
            asil: AsilLevel::AsilD,
            cores: vec![0, 1],
            rt_priority: None, memory_limit_mb: None, max_pids: None, cpu_quota: None,
        },
        DomainConfig {
            id: "ivi".into(),
            asil: AsilLevel::QM,
            cores: vec![2, 3],
            rt_priority: None, memory_limit_mb: None, max_pids: None, cpu_quota: None,
        },
        DomainConfig {
            id: "body".into(),
            asil: AsilLevel::AsilB,
            cores: vec![4],
            rt_priority: None, memory_limit_mb: None, max_pids: None, cpu_quota: None,
        },
    ];

    let mut dc = DomainController::from_config(&configs).unwrap();
    dc.assign_app("adas.camera", "adas").unwrap();
    dc.assign_app("ivi.media", "ivi").unwrap();
    dc.assign_app("body.lights", "body").unwrap();

    let dc = Arc::new(dc);
    let transport = MockTransport::new();

    let mut qm = CommHandle::new("ivi.media", Arc::clone(&dc), transport.clone());

    let mut pub_ivi = qm.advertise("ivi/media/track").unwrap();
    pub_ivi.publish(b"track_data").unwrap();

    let result = qm.advertise("adas/camera/frame");
    assert!(matches!(result, Err(KosError::PermissionDenied(_))));

    let result = qm.advertise("body/lights/cmd");
    assert!(matches!(result, Err(KosError::PermissionDenied(_))));

    let sub_adas = qm.subscribe("adas/camera/frame").unwrap();
    assert_eq!(sub_adas.topic(), "adas/camera/frame");

    let mut high = CommHandle::new("adas.camera", Arc::clone(&dc), transport.clone());

    assert!(high.advertise("ivi/notification").is_ok());

    let mut pub_adas = high.advertise("adas/camera/frame").unwrap();
    pub_adas.publish(b"frame_001").unwrap();

    let mut mid = CommHandle::new("body.lights", Arc::clone(&dc), transport.clone());

    assert!(mid.advertise("ivi/body_status").is_ok());

    assert!(mid.advertise("adas/safety/override").is_err());

    assert!(mid.advertise("body/lights/state").is_ok());

    let msgs = transport.messages("ivi/media/track");
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0], b"track_data");
}

#[test]
fn scenario10_diag_on_max_retries() {
    let diag = LogOnlyDiag::new();

    let error = KosError::PermissionDenied("app adas.lka exceeded max_retries (3)".into());
    diag.notify_incident("adas.lka", &error);

    let error2 = KosError::PermissionDenied("app ivi.media exceeded max_retries (2)".into());
    diag.notify_incident("ivi.media", &error2);

    let logs = diag.drain_logs();
    assert_eq!(logs.len(), 2);
    assert!(logs[0].contains("[DIAG] adas.lka"));
    assert!(logs[0].contains("max_retries (3)"));
    assert!(logs[1].contains("[DIAG] ivi.media"));
    assert!(logs[1].contains("max_retries (2)"));
}

#[test]
fn scenario13_single_thread_node() {
    let count = Arc::new(AtomicU32::new(0));
    let cc = count.clone();

    let config = NodeConfig::periodic(std::time::Duration::from_millis(5));
    let mut node = Node::new("single_test", config);

    node.on_run(move |_ctx| {
        cc.fetch_add(1, Ordering::Relaxed);
    });

    node.spin_for(std::time::Duration::from_millis(50)).unwrap();
    assert!(count.load(Ordering::Relaxed) > 0, "on_run should have been called");
}

#[test]
fn scenario14_multi_thread_node() {
    let periodic_count = Arc::new(AtomicU32::new(0));
    let event_count = Arc::new(AtomicU32::new(0));
    let pc = periodic_count.clone();
    let ec = event_count.clone();

    let config = NodeConfig::periodic(std::time::Duration::from_millis(5));
    let mut node = Node::new("multi_thread_test", config);

    let periodic_cfg = ThreadConfig::periodic("sensor", std::time::Duration::from_millis(10))
        .with_priority(Priority::Critical)
        .with_subs(vec!["input"])
        .with_pubs(vec!["output"]);

    let event_cfg = ThreadConfig::event("handler", "trigger_topic")
        .with_priority(Priority::Normal);

    let periodic_cb = ThreadCallbacks::new(move |_ctx| {
        pc.fetch_add(1, Ordering::Relaxed);
    });

    let event_cb = ThreadCallbacks::new(move |_ctx| {
        ec.fetch_add(1, Ordering::Relaxed);
    });

    node.create_thread(periodic_cfg, periodic_cb).unwrap();
    node.create_thread(event_cfg, event_cb).unwrap();

    node.spin_for(std::time::Duration::from_millis(50)).unwrap();

    assert!(periodic_count.load(Ordering::Relaxed) > 0, "periodic thread should run");
    assert!(event_count.load(Ordering::Relaxed) > 0, "event thread should run");
}

#[test]
fn scenario15_context_auto_lock_unlock() {
    let mut ctx = ThreadContext::new(
        "test_thread",
        vec!["sensor/imu".into()],
        vec!["output/cmd".into()],
    );

    ctx.inject_topic_data("sensor/imu", vec![1, 2, 3, 4]);

    ctx.lock_topics();

    let val: u32 = ctx.read("sensor/imu");
    assert_eq!(val, u32::from_ne_bytes([1, 2, 3, 4]));

    ctx.write("output/cmd", &42u32);

    ctx.unlock_topics();
}

#[test]
fn scenario16_callback_order() {
    let order = Arc::new(std::sync::Mutex::new(Vec::new()));
    let o1 = order.clone();
    let o2 = order.clone();
    let o3 = order.clone();

    let config = NodeConfig::periodic(std::time::Duration::from_millis(5));
    let mut node = Node::new("callback_order", config);

    let cfg = ThreadConfig::periodic("worker", std::time::Duration::from_millis(10));
    let cb = ThreadCallbacks::new(move |_ctx| {
        o2.lock().unwrap().push("run");
    })
    .with_init(move |_ctx| {
        o1.lock().unwrap().push("init");
        Ok(())
    })
    .with_shutdown(move |_ctx| {
        o3.lock().unwrap().push("shutdown");
    });

    node.create_thread(cfg, cb).unwrap();
    node.spin_for(std::time::Duration::from_millis(30)).unwrap();

    let log = order.lock().unwrap();
    assert!(!log.is_empty());
    assert_eq!(log[0], "init");
    assert!(log.len() >= 2);
    assert_eq!(log[1], "run");
    if log.len() > 2 {
        assert_eq!(*log.last().unwrap(), "shutdown");
    }
}

#[test]
#[should_panic(expected = "not in subscription list")]
fn scenario17_read_unsubscribed_panics() {
    let ctx = ThreadContext::new("test", vec!["valid".into()], vec![]);
    let _: u32 = ctx.read("invalid_topic");
}

#[test]
#[should_panic(expected = "not in publish list")]
fn scenario18_write_unpublished_panics() {
    let ctx = ThreadContext::new("test", vec![], vec!["valid".into()]);
    ctx.write("invalid_topic", &42u32);
}

#[test]
fn scenario19_main_task_health_check() {
    let run_count = Arc::new(AtomicU32::new(0));
    let rc = run_count.clone();

    let node_config = NodeConfig::periodic(std::time::Duration::from_millis(5));
    let mt_config = MainTaskConfig::new("test_main", node_config);

    let mut main_task = MainTask::new(mt_config);
    let cfg = ThreadConfig::periodic("worker", std::time::Duration::from_millis(5));
    let cb = ThreadCallbacks::new(move |_ctx| {
        rc.fetch_add(1, Ordering::Relaxed);
    });
    main_task.node_mut().create_thread(cfg, cb).unwrap();
    main_task.run_for(std::time::Duration::from_millis(100)).unwrap();

    assert!(run_count.load(Ordering::Relaxed) > 0);
}

#[test]
fn scenario20_toml_thread_config() {
    let toml = r#"
[[domain]]
id = "adas"
asil = "D"
cores = [0, 1]

[[app]]
id = "adas.fusion"
binary = "sleep"
args = ["999"]
domain = "adas"

[app.schedule]
type = "periodic"
period_ms = 10

[[app.thread]]
name = "fusion"
trigger = "periodic"
period_ms = 10
priority = "critical"
cpu_affinity = 0
subs = ["lidar", "radar"]
pubs = ["control_cmd"]

[[app.thread]]
name = "diag"
trigger = "event"
event_topic = "diag_req"
priority = "low"
subs = ["diag_req"]
pubs = ["diag_resp"]
"#;

    let mut launcher = Launcher::from_toml(toml).unwrap();
    launcher.start().unwrap();

    let info = launcher.app_info("adas.fusion").unwrap();
    assert_eq!(state_str(&info), "RUNNING");
    assert_eq!(info.threads.len(), 2);
    assert_eq!(info.threads[0].name, "fusion");
    assert!(info.threads[0].trigger.contains("Periodic"));
    assert_eq!(info.threads[0].subs, vec!["lidar", "radar"]);
    assert_eq!(info.threads[0].pubs, vec!["control_cmd"]);
    assert_eq!(info.threads[1].name, "diag");
    assert!(info.threads[1].trigger.contains("Event"));

    launcher.shutdown().unwrap();
}

#[test]
fn scenario11_full_lifecycle_flow() {
    let toml = load_toml("adas_pipeline.toml");
    let mut launcher = Launcher::from_toml(&toml).unwrap();

    launcher.start().unwrap();
    assert!(launcher
        .all_app_info()
        .iter()
        .all(|i| state_str(i) == "RUNNING"));

    launcher.suspend_app("adas.fusion").unwrap();
    assert_eq!(
        state_str(&launcher.app_info("adas.fusion").unwrap()),
        "SUSPENDED"
    );

    assert_eq!(
        state_str(&launcher.app_info("adas.camera").unwrap()),
        "RUNNING"
    );
    assert_eq!(
        state_str(&launcher.app_info("adas.lka").unwrap()),
        "RUNNING"
    );

    launcher.resume_app("adas.fusion").unwrap();
    assert_eq!(
        state_str(&launcher.app_info("adas.fusion").unwrap()),
        "RUNNING"
    );

    let old_pid = launcher.app_info("adas.lka").unwrap().pid;
    launcher.restart_app("adas.lka").unwrap();
    let new_pid = launcher.app_info("adas.lka").unwrap().pid;
    assert_ne!(old_pid, new_pid);
    assert_eq!(
        state_str(&launcher.app_info("adas.lka").unwrap()),
        "RUNNING"
    );

    launcher.stop_app("adas.camera").unwrap();
    assert_eq!(
        state_str(&launcher.app_info("adas.camera").unwrap()),
        "TERMINATED"
    );

    launcher.shutdown().unwrap();
    assert!(launcher
        .all_app_info()
        .iter()
        .all(|i| state_str(i) == "TERMINATED"));
}

#[test]
fn scenario12_health_monitor_integration() {
    use kos_exec::health_monitor::{HealthConfig, HealthMonitor, HealthStatus};

    let mut hm = HealthMonitor::new();

    hm.register(
        "adas.camera",
        &HealthConfig {
            heartbeat_interval_ms: 500,
            deadline_ms: None,
        },
    )
    .unwrap();

    hm.register(
        "ivi.media",
        &HealthConfig {
            heartbeat_interval_ms: 500,
            deadline_ms: Some(200),
        },
    )
    .unwrap();

    assert_eq!(hm.status("adas.camera").unwrap(), HealthStatus::Healthy);
    assert_eq!(hm.status("ivi.media").unwrap(), HealthStatus::Healthy);

    hm.heartbeat("adas.camera");
    hm.heartbeat("ivi.media");

    let events = hm.tick();
    assert!(events.is_empty());

    hm.deadline_start("ivi.media");
    hm.deadline_reset("ivi.media");
    let events = hm.tick();
    assert!(events.is_empty());

    hm.heartbeat("ghost.app");

    assert!(hm
        .register(
            "adas.camera",
            &HealthConfig {
                heartbeat_interval_ms: 100,
                deadline_ms: None,
            }
        )
        .is_err());
}
