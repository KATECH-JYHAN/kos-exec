// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::{KosError, Result};
use crate::launch::{AppInfo, Launcher};

pub fn default_socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("KOS_CONTROL_SOCKET") {
        return PathBuf::from(p);
    }
    if let Ok(d) = std::env::var("KOS_DATA_DIR") {
        return PathBuf::from(d).join("control.sock");
    }
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        return PathBuf::from("/run/kos/control.sock");
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("kos/control.sock");
    }
    PathBuf::from(format!("/tmp/kos-{uid}/control.sock"))
}

#[derive(Serialize, Deserialize, Default)]
struct StatusBody {
    #[serde(default)]
    app: Vec<AppInfo>,
}

pub struct ControlServer {
    listener: UnixListener,
    path: PathBuf,
    shutdown_requested: AtomicBool,
}

impl ControlServer {
    pub fn bind(path: &Path) -> Result<Self> {
        if path.exists() {
            if UnixStream::connect(path).is_ok() {
                return Err(KosError::AlreadyExists(format!(
                    "another supervisor is running at {}",
                    path.display()
                )));
            }
            let _ = std::fs::remove_file(path);
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| KosError::InvalidConfig(format!("create {}: {e}", dir.display())))?;
        }
        if path.as_os_str().len() >= 108 {
            return Err(KosError::InvalidConfig(format!(
                "control socket path is too long ({} bytes, max 107): {} — set KOS_CONTROL_SOCKET to a shorter path",
                path.as_os_str().len(),
                path.display()
            )));
        }
        let listener = UnixListener::bind(path)
            .map_err(|e| KosError::InvalidConfig(format!("bind {}: {e}", path.display())))?;
        listener
            .set_nonblocking(true)
            .map_err(|e| KosError::InvalidConfig(format!("nonblocking: {e}")))?;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        Ok(Self { listener, path: path.to_path_buf(), shutdown_requested: AtomicBool::new(false) })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn shutdown_requested(&self) -> bool {
        self.shutdown_requested.load(Ordering::Acquire)
    }

    pub fn poll(&self, launcher: &mut Launcher) {
        while let Ok((stream, _)) = self.listener.accept() {
            if let Err(e) = self.handle(stream, launcher) {
                eprintln!("[kos-exec] control: {e}");
            }
        }
    }

    fn handle(&self, stream: UnixStream, launcher: &mut Launcher) -> std::io::Result<()> {
        stream.set_nonblocking(false)?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        let mut words = line.split_whitespace();
        let cmd = words.next().unwrap_or("");
        let arg = words.next();

        let result: Result<String> = match (cmd, arg) {
            ("status", None) => Self::status_body(launcher.all_app_info()),
            ("status", Some(id)) => launcher.app_info(id).and_then(|i| Self::status_body(vec![i])),
            ("start", Some(id)) => launcher.start_app(id).map(|_| String::new()),
            ("stop", Some(id)) => launcher.stop_app(id).map(|_| String::new()),
            ("restart", Some(id)) => launcher.restart_app(id).map(|_| String::new()),
            ("suspend", Some(id)) => launcher.suspend_app(id).map(|_| String::new()),
            ("resume", Some(id)) => launcher.resume_app(id).map(|_| String::new()),
            ("topics", None) => Ok(launcher.known_topics().join("\n")),
            ("signal", Some(name)) => {
                launcher.raise_signal(name);
                Ok(String::new())
            }
            ("state", Some(state)) => crate::app_scheduler::VehicleState::parse(state).map(|s| {
                launcher.set_vehicle_state(s);
                String::new()
            }),
            ("shutdown", None) => {
                self.shutdown_requested.store(true, Ordering::Release);
                Ok(String::new())
            }
            _ => Err(KosError::InvalidConfig(format!("unknown command: {}", line.trim()))),
        };

        let mut out = &stream;
        match result {
            Ok(body) => write!(out, "OK\n{body}"),
            Err(e) => writeln!(out, "ERR {}", encode_error(&e)),
        }
    }

    fn status_body(apps: Vec<AppInfo>) -> Result<String> {
        toml::to_string(&StatusBody { app: apps })
            .map_err(|e| KosError::InvalidConfig(format!("encode status: {e}")))
    }
}

impl Drop for ControlServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub fn request(path: &Path, command: &str) -> Result<String> {
    let mut stream = UnixStream::connect(path).map_err(|e| {
        KosError::NotFound(format!(
            "no running supervisor at {} ({e}); start one with `kos launch <toml> --supervise`",
            path.display()
        ))
    })?;
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .map_err(|e| KosError::InvalidConfig(e.to_string()))?;
    writeln!(stream, "{command}").map_err(|e| KosError::InvalidConfig(format!("send: {e}")))?;

    let mut response = String::new();
    stream
        .read_to_string(&mut response)
        .map_err(|e| KosError::InvalidConfig(format!("receive: {e}")))?;
    let (status, body) = response.split_once('\n').unwrap_or((response.as_str(), ""));
    if status == "OK" {
        Ok(body.to_string())
    } else if let Some(err) = status.strip_prefix("ERR ") {
        Err(decode_error(err))
    } else {
        Err(KosError::InvalidConfig(format!("invalid response: {status}")))
    }
}

fn encode_error(e: &KosError) -> String {
    let (kind, msg) = match e {
        KosError::NotFound(m) => ("NotFound", m),
        KosError::InvalidTransition(m) => ("InvalidTransition", m),
        KosError::PermissionDenied(m) => ("PermissionDenied", m),
        KosError::AlreadyExists(m) => ("AlreadyExists", m),
        KosError::InvalidConfig(m) => ("InvalidConfig", m),
        KosError::Timeout(m) => ("Timeout", m),
        KosError::Remote(m) => ("Remote", m),
        KosError::Io(m) => ("Io", m),
        KosError::Failed(m) => ("Failed", m),
    };
    format!("{kind} {}", msg.replace('\n', " "))
}

fn decode_error(s: &str) -> KosError {
    let (kind, msg) = s.split_once(' ').unwrap_or((s, ""));
    let msg = msg.to_string();
    match kind {
        "NotFound" => KosError::NotFound(msg),
        "InvalidTransition" => KosError::InvalidTransition(msg),
        "PermissionDenied" => KosError::PermissionDenied(msg),
        "AlreadyExists" => KosError::AlreadyExists(msg),
        "Timeout" => KosError::Timeout(msg),
        "Remote" => KosError::Remote(msg),
        "Io" => KosError::Io(msg),
        "Failed" => KosError::Failed(msg),
        _ => KosError::InvalidConfig(msg),
    }
}

pub fn parse_status(body: &str) -> Result<Vec<AppInfo>> {
    toml::from_str::<StatusBody>(body)
        .map(|b| b.app)
        .map_err(|e| KosError::InvalidConfig(format!("decode status: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lifecycle::AppState;

    const TOML: &str = r#"
[[domain]]
id = "d"
asil = "QM"
cores = [0]

[[app]]
id = "a"
binary = "sleep"
args = ["30"]
domain = "d"
"#;

    fn sock(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("kos_ctl_{name}_{}.sock", std::process::id()))
    }

    fn with_server(name: &str, f: impl FnOnce(&Path)) -> Launcher {
        let path = sock(name);
        let server = ControlServer::bind(&path).unwrap();
        let mut launcher = Launcher::from_toml(TOML).unwrap();
        launcher.start_app("a").unwrap();
        let done = AtomicBool::new(false);
        let result = std::thread::scope(|scope| {
            scope.spawn(|| {
                while !done.load(Ordering::Acquire) {
                    server.poll(&mut launcher);
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&path)));
            done.store(true, Ordering::Release);
            result
        });
        if let Err(panic) = result {
            let _ = launcher.shutdown();
            std::panic::resume_unwind(panic);
        }
        drop(server);
        assert!(!path.exists(), "socket file must be removed on drop");
        launcher
    }

    #[test]
    fn status_stop_start_over_socket() {
        let mut l = with_server("ops", |path| {
            let apps = parse_status(&request(path, "status").unwrap()).unwrap();
            assert_eq!(apps.len(), 1);
            assert_eq!(apps[0].state, AppState::Running);
            let pid = apps[0].pid.unwrap();

            request(path, "stop a").unwrap();
            let apps = parse_status(&request(path, "status a").unwrap()).unwrap();
            assert_eq!(apps[0].state, AppState::Terminated);

            request(path, "start a").unwrap();
            let apps = parse_status(&request(path, "status a").unwrap()).unwrap();
            assert_eq!(apps[0].state, AppState::Running);
            assert_ne!(apps[0].pid.unwrap(), pid);
        });
        l.shutdown().unwrap();
    }

    #[test]
    fn suspend_and_resume_send_signals() {
        let mut l = with_server("susp", |path| {
            let apps = parse_status(&request(path, "status a").unwrap()).unwrap();
            let pid = apps[0].pid.unwrap();
            let proc_state = || {
                std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .ok()
                    .and_then(|s| s.rsplit(')').next().and_then(|r| r.split_whitespace().next()).map(str::to_string))
                    .unwrap_or_default()
            };

            let wait_until = |want: &dyn Fn(&str) -> bool| {
                let deadline = std::time::Instant::now() + Duration::from_secs(2);
                while std::time::Instant::now() < deadline && !want(&proc_state()) {
                    std::thread::sleep(Duration::from_millis(10));
                }
                proc_state()
            };

            request(path, "suspend a").unwrap();
            assert_eq!(wait_until(&|s| s == "T"), "T", "process should be stopped");
            request(path, "resume a").unwrap();
            assert_ne!(wait_until(&|s| s != "T"), "T", "process should be running again");
        });
        l.shutdown().unwrap();
    }

    #[test]
    fn errors_and_shutdown_request() {
        let path = sock("err");
        let server = ControlServer::bind(&path).unwrap();
        assert!(ControlServer::bind(&path).is_err(), "second supervisor must be rejected");
        let mut launcher = Launcher::from_toml(TOML).unwrap();
        std::thread::scope(|scope| {
            scope.spawn(|| {
                while !server.shutdown_requested() {
                    server.poll(&mut launcher);
                    std::thread::sleep(Duration::from_millis(2));
                }
            });
            assert_eq!(request(&path, "stop nope"), Err(KosError::NotFound("app nope".into())));
            assert!(request(&path, "bogus").is_err());
            request(&path, "shutdown").unwrap();
        });
        assert!(server.shutdown_requested());
    }

    #[test]
    fn too_long_socket_path_is_rejected_with_hint() {
        let long = PathBuf::from(format!("/tmp/{}/control.sock", "x".repeat(110)));
        let err = ControlServer::bind(&long).err().expect("must fail").to_string();
        assert!(err.contains("KOS_CONTROL_SOCKET"), "{err}");
    }

    #[test]
    fn request_without_server_explains() {
        let err = request(&sock("none"), "status").unwrap_err().to_string();
        assert!(err.contains("--supervise"), "{err}");
    }
}
