// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::fs;

use kos_exec::control::{self, ControlServer};
use kos_exec::launch::Launcher;

use crate::format;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn launch(
    toml_path: &str,
    domain: Option<String>,
    app: Option<String>,
    supervise: bool,
) -> Result<()> {
    let content = fs::read_to_string(toml_path)
        .map_err(|e| format!("cannot read {toml_path}: {e}"))?;

    let mut launcher = Launcher::from_toml(&content)?;

    let server = ControlServer::bind(&control::default_socket_path())?;
    launcher.app_manager.set_kill_with_parent(true);
    launcher.set_auto_restart(supervise);
    launcher.set_diag(Box::new(kos_exec::FileDiag::new(kos_exec::FileDiag::default_path())));

    match (&domain, &app) {
        (None, None) => {
            launcher.start()?;
            println!("All apps launched.");
        }
        (Some(dom), None) => {
            let apps: Vec<String> = launcher
                .app_configs()
                .iter()
                .filter(|c| c.domain == *dom)
                .map(|c| c.id.clone())
                .collect();

            if apps.is_empty() {
                return Err(format!("no apps found in domain '{dom}'").into());
            }

            let started = launcher.start_selected(&apps)?;
            println!("Launched {} app(s) for domain '{dom}' (including dependencies).", started.len());
        }
        (_, Some(id)) => {
            let started = launcher.start_selected(std::slice::from_ref(id))?;
            println!("Launched '{id}' ({} app(s) including dependencies).", started.len());
        }
    }

    let infos = launcher.all_app_info();
    println!("\n{}", format::status_table(&infos));

    if supervise {
        println!("Supervising apps (restart on crash). Control socket: {}", server.path().display());
    } else {
        println!("Managing apps (no auto-restart; use --supervise). Control socket: {}", server.path().display());
    }
    println!("Manage with `kos status|stop|start|restart|suspend|resume|shutdown`, or press Ctrl+C.");
    launcher.supervise_until_signal_with(Some(&server))?;
    println!("All apps stopped.");

    Ok(())
}

fn send(command: &str) -> Result<String> {
    Ok(control::request(&control::default_socket_path(), command)?)
}

pub fn status(app_id: Option<String>) -> Result<()> {
    let command = match &app_id {
        Some(id) => format!("status {id}"),
        None => "status".to_string(),
    };
    let infos = control::parse_status(&send(&command)?)?;
    match (app_id, infos.first()) {
        (Some(_), Some(info)) => println!("{}", format::status_detail(info)),
        _ => println!("{}", format::status_table(&infos)),
    }
    Ok(())
}

pub fn stop(app_id: &str) -> Result<()> {
    send(&format!("stop {app_id}"))?;
    println!("Stopped '{app_id}'.");
    Ok(())
}

pub fn start(app_id: &str) -> Result<()> {
    send(&format!("start {app_id}"))?;
    println!("Started '{app_id}'.");
    Ok(())
}

pub fn restart(app_id: &str) -> Result<()> {
    send(&format!("restart {app_id}"))?;
    println!("Restarted '{app_id}'.");
    Ok(())
}

pub fn suspend(app_id: &str) -> Result<()> {
    send(&format!("suspend {app_id}"))?;
    println!("Suspended '{app_id}'.");
    Ok(())
}

pub fn resume(app_id: &str) -> Result<()> {
    send(&format!("resume {app_id}"))?;
    println!("Resumed '{app_id}'.");
    Ok(())
}

pub fn signal(name: &str) -> Result<()> {
    send(&format!("signal {name}"))?;
    println!("Signal '{name}' raised.");
    Ok(())
}

pub fn state(state: &str) -> Result<()> {
    send(&format!("state {state}"))?;
    println!("Vehicle state set to {}.", state.to_ascii_uppercase());
    Ok(())
}

pub fn shutdown() -> Result<()> {
    send("shutdown")?;
    println!("Shutdown requested.");
    Ok(())
}

fn topic_candidates(toml: Option<String>, extra: Vec<String>) -> Result<Vec<String>> {
    let mut candidates = extra;
    if let Ok(body) = control::request(&control::default_socket_path(), "topics") {
        candidates.extend(body.lines().filter(|l| !l.is_empty()).map(str::to_string));
    }
    if let Some(path) = toml {
        let content = fs::read_to_string(&path).map_err(|e| format!("cannot read {path}: {e}"))?;
        candidates.extend(Launcher::from_toml(&content)?.known_topics());
    }
    Ok(candidates)
}

pub fn shm_status(toml: Option<String>, extra: Vec<String>) -> Result<()> {
    let candidates = topic_candidates(toml, extra)?;
    let list = kos_exec::shm_inspect::list_topics(&candidates);
    if list.is_empty() {
        println!("(no KOS-comm SHM topics in /dev/shm)");
        return Ok(());
    }
    println!("{}", format::shm_table(&list));
    let unresolved = list.iter().filter(|s| s.topic.is_none()).count();
    if unresolved > 0 {
        println!("\n{unresolved} topic name(s) unknown — pass --toml <launch.toml> or --topic <name> to resolve.");
    }
    Ok(())
}

pub fn shm_cleanup(dry_run: bool, toml: Option<String>) -> Result<()> {
    let candidates = topic_candidates(toml, Vec::new())?;
    let stale = kos_exec::shm_inspect::cleanup_stale(&candidates, dry_run);
    if stale.is_empty() {
        println!("No stale SHM topics.");
        return Ok(());
    }
    println!("{}", format::shm_table(&stale));
    println!(
        "\n{} {} stale topic(s).",
        if dry_run { "Would remove" } else { "Removed" },
        stale.len()
    );
    Ok(())
}

pub fn shm_info(topic: &str) -> Result<()> {
    let detail = kos_exec::shm_inspect::topic_info(topic)?;
    println!("{}", format::shm_detail(&detail));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    const TEST_TOML: &str = r#"
[[domain]]
id = "test"
asil = "QM"
cores = [0]

[[app]]
id = "test.app1"
binary = "sleep"
args = ["999"]
domain = "test"

[app.schedule]
type = "periodic"
period_ms = 100

[[app]]
id = "test.app2"
binary = "sleep"
args = ["999"]
domain = "test"
depends_on = ["test.app1"]

[app.schedule]
type = "event"
trigger = "signal:start"
"#;

    fn write_temp_toml() -> String {
        let path = "/tmp/kos_cli_test.toml";
        let mut f = fs::File::create(path).unwrap();
        f.write_all(TEST_TOML.as_bytes()).unwrap();
        path.to_string()
    }

    #[test]
    fn launch_all_apps() {
        let path = write_temp_toml();
        let mut launcher = Launcher::from_toml(TEST_TOML).unwrap();
        launcher.start().unwrap();

        let infos = launcher.all_app_info();
        let state = |id: &str| {
            let info = infos.iter().find(|i| i.app_id == id).unwrap();
            format!("{}", info.state).to_uppercase()
        };
        let (s1, s2) = (state("test.app1"), state("test.app2"));
        launcher.raise_signal("start");
        let started = launcher.run_schedules();

        launcher.shutdown().unwrap();
        let _ = fs::remove_file(&path);
        assert_eq!(infos.len(), 2);
        assert_eq!((s1.as_str(), s2.as_str()), ("RUNNING", "INSTALLED"));
        assert_eq!(started, vec!["test.app2"]);
    }

    #[test]
    fn launch_domain_filter() {
        let mut launcher = Launcher::from_toml(TEST_TOML).unwrap();

        let apps: Vec<String> = launcher
            .app_configs()
            .iter()
            .filter(|c| c.domain == "test")
            .map(|c| c.id.clone())
            .collect();
        assert_eq!(apps.len(), 2);

        for id in &apps {
            launcher.start_app(id).unwrap();
        }

        let infos = launcher.all_app_info();
        assert!(infos.iter().all(|i| i.domain == "test"));

        launcher.shutdown().unwrap();
    }

    #[test]
    fn launch_single_app_filter() {
        let mut launcher = Launcher::from_toml(TEST_TOML).unwrap();
        launcher.start_app("test.app1").unwrap();

        let info = launcher.app_info("test.app1").unwrap();
        assert_eq!(format!("{}", info.state).to_uppercase(), "RUNNING");

        launcher.shutdown().unwrap();
    }

    #[test]
    fn stop_app_transitions_to_terminated() {
        let mut launcher = Launcher::from_toml(TEST_TOML).unwrap();
        launcher.start().unwrap();

        launcher.stop_app("test.app1").unwrap();
        let info = launcher.app_info("test.app1").unwrap();
        assert_eq!(format!("{}", info.state).to_uppercase(), "TERMINATED");

        launcher.shutdown().unwrap();
    }

    #[test]
    fn restart_app_recovers() {
        let mut launcher = Launcher::from_toml(TEST_TOML).unwrap();
        launcher.start().unwrap();

        launcher.restart_app("test.app1").unwrap();
        let info = launcher.app_info("test.app1").unwrap();
        assert_eq!(format!("{}", info.state).to_uppercase(), "RUNNING");

        launcher.shutdown().unwrap();
    }

    #[test]
    fn suspend_and_resume() {
        let mut launcher = Launcher::from_toml(TEST_TOML).unwrap();
        launcher.start().unwrap();

        launcher.suspend_app("test.app1").unwrap();
        let info = launcher.app_info("test.app1").unwrap();
        assert_eq!(format!("{}", info.state).to_uppercase(), "SUSPENDED");

        launcher.resume_app("test.app1").unwrap();
        let info = launcher.app_info("test.app1").unwrap();
        assert_eq!(format!("{}", info.state).to_uppercase(), "RUNNING");

        launcher.shutdown().unwrap();
    }

    #[test]
    fn shutdown_terminates_all() {
        let mut launcher = Launcher::from_toml(TEST_TOML).unwrap();
        launcher.start().unwrap();
        launcher.shutdown().unwrap();

        let infos = launcher.all_app_info();
        for info in &infos {
            assert_eq!(
                format!("{}", info.state).to_uppercase(),
                "TERMINATED"
            );
        }
    }
}

pub fn incidents(last: usize) -> Result<()> {
    let path = kos_exec::FileDiag::default_path();
    let list = match kos_exec::read_incidents(&path, last) {
        Ok(list) => list,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            println!("(no incidents recorded in {})", path.display());
            return Ok(());
        }
        Err(e) => return Err(format!("cannot read {}: {e}", path.display()).into()),
    };
    if list.is_empty() {
        println!("(no incidents recorded in {})", path.display());
    }
    for i in list {
        println!("{}  {:<20} {}", format::local_datetime(i.unix_secs), i.app_id, i.message);
    }
    Ok(())
}
