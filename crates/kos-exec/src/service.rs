// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::error::{KosError, Result};

pub const MAX_SERVICE_MESSAGE: usize = 1 << 20;
pub const DEFAULT_CALL_TIMEOUT: Duration = Duration::from_secs(1);

const STATUS_OK: u8 = 0;
const STATUS_ERR: u8 = 1;
const SOCKET_PATH_MAX: usize = 107;

pub type ServiceHandler = dyn Fn(&[u8]) -> std::result::Result<Vec<u8>, String> + Send + Sync;

pub fn service_dir() -> PathBuf {
    if let Ok(d) = std::env::var("KOS_SERVICE_DIR") {
        return PathBuf::from(d);
    }
    if let Ok(d) = std::env::var("KOS_DATA_DIR") {
        return PathBuf::from(d).join("svc");
    }
    let uid = unsafe { libc::getuid() };
    if uid == 0 {
        return PathBuf::from("/run/kos/svc");
    }
    if let Ok(d) = std::env::var("XDG_RUNTIME_DIR") {
        return PathBuf::from(d).join("kos/svc");
    }
    PathBuf::from(format!("/tmp/kos-{uid}/svc"))
}

pub fn service_path(name: &str) -> Result<PathBuf> {
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/'))
        || name.starts_with('/')
        || name.contains("..")
    {
        return Err(KosError::InvalidConfig(format!("invalid service name '{name}'")));
    }
    let path = service_dir().join(format!("{}.sock", name.replace('/', "__")));
    if path.as_os_str().len() > SOCKET_PATH_MAX {
        return Err(KosError::InvalidConfig(format!(
            "service socket path too long ({} bytes): {} — set KOS_SERVICE_DIR to a shorter directory",
            path.as_os_str().len(),
            path.display()
        )));
    }
    Ok(path)
}

fn io_err(ctx: &str, e: std::io::Error) -> KosError {
    match e.kind() {
        ErrorKind::WouldBlock | ErrorKind::TimedOut => KosError::Timeout(ctx.to_string()),
        _ => KosError::Io(format!("{ctx}: {e}")),
    }
}

fn write_frame(stream: &mut UnixStream, parts: &[&[u8]]) -> std::io::Result<()> {
    let len: usize = parts.iter().map(|p| p.len()).sum();
    let mut buf = Vec::with_capacity(4 + len);
    buf.extend_from_slice(&(len as u32).to_le_bytes());
    for p in parts {
        buf.extend_from_slice(p);
    }
    stream.write_all(&buf)
}

fn read_frame(stream: &mut UnixStream) -> std::io::Result<Vec<u8>> {
    let mut len = [0u8; 4];
    stream.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_SERVICE_MESSAGE + 1 {
        return Err(std::io::Error::new(ErrorKind::InvalidData, format!("frame too large ({len} bytes)")));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    Ok(buf)
}

pub struct ServiceServer {
    name: String,
    path: PathBuf,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<UnixStream>>>,
    accept_thread: Option<JoinHandle<()>>,
}

impl ServiceServer {
    pub fn advertise<F>(name: &str, handler: F) -> Result<Self>
    where
        F: Fn(&[u8]) -> std::result::Result<Vec<u8>, String> + Send + Sync + 'static,
    {
        let path = service_path(name)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| io_err("create service dir", e))?;
        }
        if path.exists() {
            if UnixStream::connect(&path).is_ok() {
                return Err(KosError::AlreadyExists(format!("service '{name}' is already advertised")));
            }
            let _ = std::fs::remove_file(&path);
        }
        let listener = UnixListener::bind(&path).map_err(|e| io_err("bind service socket", e))?;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
        listener.set_nonblocking(true).map_err(|e| io_err("service socket", e))?;

        let handler: Arc<ServiceHandler> = Arc::new(handler);
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(Mutex::new(Vec::new()));
        let accept_thread = {
            let stop = stop.clone();
            let connections = connections.clone();
            std::thread::Builder::new()
                .name(format!("svc:{name}"))
                .spawn(move || accept_loop(listener, handler, stop, connections))
                .map_err(|e| io_err("spawn service thread", e))?
        };
        Ok(Self { name: name.to_string(), path, stop, connections, accept_thread: Some(accept_thread) })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for ServiceServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        for conn in self.connections.lock().unwrap().drain(..) {
            let _ = conn.shutdown(std::net::Shutdown::Both);
        }
        if let Some(t) = self.accept_thread.take() {
            let _ = t.join();
        }
        let _ = std::fs::remove_file(&self.path);
    }
}

fn accept_loop(
    listener: UnixListener,
    handler: Arc<ServiceHandler>,
    stop: Arc<AtomicBool>,
    connections: Arc<Mutex<Vec<UnixStream>>>,
) {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, _)) => {
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                if let Ok(clone) = stream.try_clone() {
                    let mut conns = connections.lock().unwrap();
                    conns.retain(|c| c.peer_addr().is_ok());
                    conns.push(clone);
                }
                let handler = handler.clone();
                let _ = std::thread::Builder::new()
                    .name("svc-conn".into())
                    .spawn(move || serve_connection(stream, handler));
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => std::thread::sleep(Duration::from_millis(5)),
            Err(_) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
}

fn serve_connection(mut stream: UnixStream, handler: Arc<ServiceHandler>) {
    while let Ok(request) = read_frame(&mut stream) {
        let reply = match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handler(&request))) {
            Ok(Ok(resp)) if resp.len() <= MAX_SERVICE_MESSAGE => write_frame(&mut stream, &[&[STATUS_OK], &resp]),
            Ok(Ok(resp)) => {
                let msg = format!("response too large ({} bytes)", resp.len());
                write_frame(&mut stream, &[&[STATUS_ERR], msg.as_bytes()])
            }
            Ok(Err(msg)) => write_frame(&mut stream, &[&[STATUS_ERR], msg.as_bytes()]),
            Err(_) => write_frame(&mut stream, &[&[STATUS_ERR], b"service handler panicked"]),
        };
        if reply.is_err() {
            break;
        }
    }
}

pub struct ServiceClient {
    name: String,
    path: PathBuf,
    timeout: Duration,
    stream: Option<UnixStream>,
}

impl ServiceClient {
    pub fn new(name: &str) -> Result<Self> {
        Ok(Self { name: name.to_string(), path: service_path(name)?, timeout: DEFAULT_CALL_TIMEOUT, stream: None })
    }

    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout.max(Duration::from_millis(1));
        self.stream = None;
    }

    pub fn call(&mut self, request: &[u8]) -> Result<Vec<u8>> {
        if request.len() > MAX_SERVICE_MESSAGE {
            return Err(KosError::InvalidConfig(format!("request too large ({} bytes)", request.len())));
        }
        let reused = self.stream.is_some();
        match self.call_once(request) {
            Err(KosError::Io(_)) if reused => {
                self.stream = None;
                self.call_once(request)
            }
            other => other,
        }
    }

    fn connect(&mut self) -> Result<&mut UnixStream> {
        if self.stream.is_none() {
            let stream = UnixStream::connect(&self.path).map_err(|e| match e.kind() {
                ErrorKind::NotFound | ErrorKind::ConnectionRefused => {
                    KosError::NotFound(format!("service '{}' is not advertised", self.name))
                }
                _ => io_err("connect service", e),
            })?;
            stream.set_read_timeout(Some(self.timeout)).map_err(|e| io_err("service socket", e))?;
            stream.set_write_timeout(Some(self.timeout)).map_err(|e| io_err("service socket", e))?;
            self.stream = Some(stream);
        }
        Ok(self.stream.as_mut().unwrap())
    }

    fn call_once(&mut self, request: &[u8]) -> Result<Vec<u8>> {
        let name = self.name.clone();
        let stream = self.connect()?;
        let result = write_frame(stream, &[request])
            .and_then(|_| read_frame(stream))
            .map_err(|e| match e.kind() {
                ErrorKind::WouldBlock | ErrorKind::TimedOut => KosError::Timeout(format!("service '{name}'")),
                _ => KosError::Io(format!("service '{name}': {e}")),
            });
        let reply = match result {
            Ok(r) => r,
            Err(e) => {
                self.stream = None;
                return Err(e);
            }
        };
        match reply.split_first() {
            Some((&STATUS_OK, payload)) => Ok(payload.to_vec()),
            Some((_, msg)) => Err(KosError::Remote(String::from_utf8_lossy(msg).into_owned())),
            None => Err(KosError::Io(format!("service '{name}': empty reply"))),
        }
    }
}

pub fn call(name: &str, request: &[u8], timeout: Duration) -> Result<Vec<u8>> {
    let mut client = ServiceClient::new(name)?;
    client.set_timeout(timeout);
    client.call(request)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique(name: &str) -> String {
        format!("test/{name}_{}", std::process::id())
    }

    #[test]
    fn call_returns_handler_reply() {
        let name = unique("echo");
        let _srv = ServiceServer::advertise(&name, |req| Ok([b"re:", req].concat())).unwrap();
        assert_eq!(call(&name, b"hi", DEFAULT_CALL_TIMEOUT).unwrap(), b"re:hi");
        let mut client = ServiceClient::new(&name).unwrap();
        for i in 0..20u8 {
            assert_eq!(client.call(&[i]).unwrap(), [b'r', b'e', b':', i]);
        }
    }

    #[test]
    fn handler_error_and_panic_are_reported() {
        let name = unique("err");
        let _srv = ServiceServer::advertise(&name, |req| match req {
            b"panic" => panic!("boom"),
            _ => Err("bad request".to_string()),
        })
        .unwrap();
        assert_eq!(call(&name, b"x", DEFAULT_CALL_TIMEOUT), Err(KosError::Remote("bad request".into())));
        assert!(matches!(call(&name, b"panic", DEFAULT_CALL_TIMEOUT), Err(KosError::Remote(_))));
        assert_eq!(call(&name, b"x", DEFAULT_CALL_TIMEOUT), Err(KosError::Remote("bad request".into())));
    }

    #[test]
    fn missing_service_is_not_found() {
        assert!(matches!(call(&unique("none"), b"x", DEFAULT_CALL_TIMEOUT), Err(KosError::NotFound(_))));
    }

    #[test]
    fn slow_handler_times_out() {
        let name = unique("slow");
        let _srv = ServiceServer::advertise(&name, |_| {
            std::thread::sleep(Duration::from_millis(300));
            Ok(vec![])
        })
        .unwrap();
        assert!(matches!(call(&name, b"x", Duration::from_millis(50)), Err(KosError::Timeout(_))));
    }

    #[test]
    fn concurrent_clients_get_their_own_replies() {
        let name = unique("concurrent");
        let _srv = ServiceServer::advertise(&name, |req| {
            std::thread::sleep(Duration::from_millis(2));
            Ok(req.to_vec())
        })
        .unwrap();
        let workers: Vec<_> = (0..8u8)
            .map(|id| {
                let name = name.clone();
                std::thread::spawn(move || {
                    let mut c = ServiceClient::new(&name).unwrap();
                    for n in 0..25u8 {
                        assert_eq!(c.call(&[id, n]).unwrap(), [id, n]);
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
    }

    #[test]
    fn second_server_is_rejected_and_socket_removed_on_drop() {
        let name = unique("dup");
        let srv = ServiceServer::advertise(&name, |_| Ok(vec![])).unwrap();
        assert!(matches!(ServiceServer::advertise(&name, |_| Ok(vec![])), Err(KosError::AlreadyExists(_))));
        let path = srv.path().to_path_buf();
        drop(srv);
        assert!(!path.exists());
        assert!(matches!(call(&name, b"x", DEFAULT_CALL_TIMEOUT), Err(KosError::NotFound(_))));
    }

    #[test]
    fn invalid_names_are_rejected() {
        for bad in ["", "/abs", "a/../b", "sp ace"] {
            assert!(service_path(bad).is_err(), "{bad}");
        }
    }
}
