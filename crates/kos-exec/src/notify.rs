// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::io;
use std::os::unix::io::{AsRawFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::collections::HashMap;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum NotifyCommand {
    ReportStatus = 1,
    PrepareMigrate = 2,
    Degrade = 3,
    Restore = 4,
    Shutdown = 5,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum NotifyStatus {
    Utilization = 100,
    ReadyToMigrate = 101,
    Ack = 102,
    Heartbeat = 103,
}

#[derive(Debug, Clone, Copy)]
#[repr(C)]
pub struct NotifyMessage {
    pub msg_type: u32,
    pub payload: u32,
}

impl NotifyMessage {
    pub fn command(cmd: NotifyCommand, payload: u32) -> Self {
        Self { msg_type: cmd as u32, payload }
    }

    pub fn status(status: NotifyStatus, payload: u32) -> Self {
        Self { msg_type: status as u32, payload }
    }

    fn as_bytes(&self) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                self as *const Self as *const u8,
                std::mem::size_of::<Self>(),
            )
        }
    }

    fn from_bytes(buf: &[u8; 8]) -> Self {
        unsafe { std::ptr::read_unaligned(buf.as_ptr() as *const Self) }
    }

    pub fn as_command(&self) -> Option<NotifyCommand> {
        match self.msg_type {
            1 => Some(NotifyCommand::ReportStatus),
            2 => Some(NotifyCommand::PrepareMigrate),
            3 => Some(NotifyCommand::Degrade),
            4 => Some(NotifyCommand::Restore),
            5 => Some(NotifyCommand::Shutdown),
            _ => None,
        }
    }

    pub fn as_status(&self) -> Option<NotifyStatus> {
        match self.msg_type {
            100 => Some(NotifyStatus::Utilization),
            101 => Some(NotifyStatus::ReadyToMigrate),
            102 => Some(NotifyStatus::Ack),
            103 => Some(NotifyStatus::Heartbeat),
            _ => None,
        }
    }
}

struct Epoll {
    epfd: RawFd,
}

impl Epoll {
    fn new() -> io::Result<Self> {
        let epfd = unsafe { libc::epoll_create1(0) };
        if epfd < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { epfd })
    }

    fn add(&self, fd: RawFd, data: u64) -> io::Result<()> {
        let mut event = libc::epoll_event {
            events: libc::EPOLLIN as u32,
            u64: data,
        };
        let ret = unsafe {
            libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_ADD, fd, &mut event)
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn remove(&self, fd: RawFd) -> io::Result<()> {
        let ret = unsafe {
            libc::epoll_ctl(self.epfd, libc::EPOLL_CTL_DEL, fd, std::ptr::null_mut())
        };
        if ret < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    fn wait(&self, timeout_ms: i32) -> io::Result<Vec<u64>> {
        let mut events = [libc::epoll_event { events: 0, u64: 0 }; 64];
        let n = unsafe {
            libc::epoll_wait(self.epfd, events.as_mut_ptr(), events.len() as i32, timeout_ms)
        };
        if n < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                return Ok(Vec::new());
            }
            return Err(err);
        }
        Ok(events[..n as usize].iter().map(|e| e.u64).collect())
    }
}

impl Drop for Epoll {
    fn drop(&mut self) {
        unsafe { libc::close(self.epfd) };
    }
}

struct ClientConn {
    stream: UnixStream,
    pid: u32,
    app_id: String,
}

pub struct NotifyServer {
    listener: UnixListener,
    socket_path: PathBuf,
    epoll: Epoll,
    clients: HashMap<u32, ClientConn>,
    listener_data: u64,
}

impl NotifyServer {
    pub fn new(socket_path: &Path) -> io::Result<Self> {
        let _ = std::fs::remove_file(socket_path);

        let listener = UnixListener::bind(socket_path)?;
        listener.set_nonblocking(true)?;

        let epoll = Epoll::new()?;
        let listener_data = u64::MAX;
        epoll.add(listener.as_raw_fd(), listener_data)?;

        Ok(Self {
            listener,
            socket_path: socket_path.to_path_buf(),
            epoll,
            clients: HashMap::new(),
            listener_data,
        })
    }

    pub fn default_path() -> PathBuf {
        PathBuf::from(format!("/tmp/kos_notify_{}.sock", std::process::id()))
    }

    pub fn path(&self) -> &Path {
        &self.socket_path
    }

    pub fn accept_pending(&mut self) -> io::Result<Vec<u32>> {
        let mut accepted = Vec::new();

        loop {
            match self.listener.accept() {
                Ok((stream, _addr)) => {
                    stream.set_nonblocking(true)?;

                    let fd = stream.as_raw_fd();
                    let temp_pid = fd as u32;
                    self.epoll.add(fd, temp_pid as u64)?;
                    self.clients.insert(temp_pid, ClientConn {
                        stream,
                        pid: 0,
                        app_id: String::new(),
                    });
                    accepted.push(temp_pid);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }

        Ok(accepted)
    }

    pub fn register_client(&mut self, temp_id: u32, pid: u32, app_id: String) {
        if let Some(mut conn) = self.clients.remove(&temp_id) {
            let fd = conn.stream.as_raw_fd();
            let _ = self.epoll.remove(fd);
            let _ = self.epoll.add(fd, pid as u64);
            conn.pid = pid;
            conn.app_id = app_id;
            self.clients.insert(pid, conn);
        }
    }

    pub fn send_command(&self, pid: u32, cmd: NotifyCommand, payload: u32) -> io::Result<()> {
        let conn = self.clients.get(&pid)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "pid not connected"))?;
        let msg = NotifyMessage::command(cmd, payload);
        send_msg(&conn.stream, &msg)
    }

    pub fn broadcast(&self, cmd: NotifyCommand, payload: u32) -> usize {
        let msg = NotifyMessage::command(cmd, payload);
        let mut sent = 0;
        for conn in self.clients.values() {
            if send_msg(&conn.stream, &msg).is_ok() {
                sent += 1;
            }
        }
        sent
    }

    pub fn poll(&mut self, timeout_ms: i32) -> io::Result<Vec<(u32, NotifyMessage)>> {
        let _ = self.accept_pending();

        let ready = self.epoll.wait(timeout_ms)?;
        let mut messages = Vec::new();

        for data in ready {
            if data == self.listener_data {
                let _ = self.accept_pending();
                continue;
            }

            let pid = data as u32;
            if let Some(conn) = self.clients.get(&pid) {
                match recv_msg(&conn.stream) {
                    Ok(msg) => messages.push((pid, msg)),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(_) => {
                        if let Some(conn) = self.clients.get(&pid) {
                            let _ = self.epoll.remove(conn.stream.as_raw_fd());
                        }
                        self.clients.remove(&pid);
                    }
                }
            }
        }

        Ok(messages)
    }

    pub fn client_count(&self) -> usize {
        self.clients.len()
    }

    pub fn disconnect(&mut self, pid: u32) {
        if let Some(conn) = self.clients.remove(&pid) {
            let _ = self.epoll.remove(conn.stream.as_raw_fd());
        }
    }
}

impl Drop for NotifyServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.socket_path);
    }
}

pub struct NotifyClient {
    stream: UnixStream,
    epoll: Epoll,
}

impl NotifyClient {
    pub fn connect(socket_path: &Path) -> io::Result<Self> {
        let stream = UnixStream::connect(socket_path)?;
        stream.set_nonblocking(true)?;

        let epoll = Epoll::new()?;
        epoll.add(stream.as_raw_fd(), 0)?;

        Ok(Self { stream, epoll })
    }

    pub fn poll(&self, timeout: Option<Duration>) -> io::Result<Option<NotifyMessage>> {
        let timeout_ms = timeout
            .map(|d| d.as_millis() as i32)
            .unwrap_or(-1);

        let ready = self.epoll.wait(timeout_ms)?;
        if ready.is_empty() {
            return Ok(None);
        }

        match recv_msg(&self.stream) {
            Ok(msg) => Ok(Some(msg)),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn try_recv(&self) -> io::Result<Option<NotifyMessage>> {
        self.poll(Some(Duration::ZERO))
    }

    pub fn send_status(&self, status: NotifyStatus, payload: u32) -> io::Result<()> {
        let msg = NotifyMessage::status(status, payload);
        send_msg(&self.stream, &msg)
    }

    pub fn report_utilization(&self, util_pct: f64) -> io::Result<()> {
        let payload = (util_pct * 100.0) as u32;
        self.send_status(NotifyStatus::Utilization, payload)
    }

    pub fn report_ready_to_migrate(&self) -> io::Result<()> {
        self.send_status(NotifyStatus::ReadyToMigrate, 0)
    }

    pub fn heartbeat(&self) -> io::Result<()> {
        self.send_status(NotifyStatus::Heartbeat, 0)
    }

    pub fn ack(&self) -> io::Result<()> {
        self.send_status(NotifyStatus::Ack, 0)
    }
}

fn send_msg(stream: &UnixStream, msg: &NotifyMessage) -> io::Result<()> {
    use std::io::Write;
    let bytes = msg.as_bytes();
    (&*stream).write_all(bytes)?;
    Ok(())
}

fn recv_msg(stream: &UnixStream) -> io::Result<NotifyMessage> {
    use std::io::Read;
    let mut buf = [0u8; 8];
    (&*stream).read_exact(&mut buf)?;
    Ok(NotifyMessage::from_bytes(&buf))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;

    #[test]
    fn test_message_roundtrip() {
        let msg = NotifyMessage::command(NotifyCommand::PrepareMigrate, 2);
        let bytes = msg.as_bytes();
        let mut buf = [0u8; 8];
        buf.copy_from_slice(bytes);
        let decoded = NotifyMessage::from_bytes(&buf);
        assert_eq!(decoded.msg_type, NotifyCommand::PrepareMigrate as u32);
        assert_eq!(decoded.payload, 2);
        assert_eq!(decoded.as_command(), Some(NotifyCommand::PrepareMigrate));
    }

    #[test]
    fn test_server_client_communication() {
        let path = PathBuf::from(format!("/tmp/kos_notify_test_{}.sock", std::process::id()));
        let mut server = NotifyServer::new(&path).expect("server bind");

        let path_clone = path.clone();
        let client_thread = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let client = NotifyClient::connect(&path_clone).expect("client connect");

            let msg = client.poll(Some(Duration::from_secs(2)))
                .expect("poll")
                .expect("should receive command");
            assert_eq!(msg.as_command(), Some(NotifyCommand::ReportStatus));

            client.report_utilization(23.5).expect("send status");
        });

        thread::sleep(Duration::from_millis(100));
        let accepted = server.accept_pending().expect("accept");
        assert!(!accepted.is_empty());
        let temp_id = accepted[0];
        server.register_client(temp_id, 12345, "test_app".into());

        server.send_command(12345, NotifyCommand::ReportStatus, 0).expect("send cmd");

        thread::sleep(Duration::from_millis(100));
        let msgs = server.poll(1000).expect("poll");
        assert!(!msgs.is_empty());
        let (pid, msg) = &msgs[0];
        assert_eq!(*pid, 12345);
        assert_eq!(msg.as_status(), Some(NotifyStatus::Utilization));
        assert_eq!(msg.payload, 2350);

        client_thread.join().unwrap();
    }

    #[test]
    fn test_broadcast() {
        let path = PathBuf::from(format!("/tmp/kos_notify_bcast_{}.sock", std::process::id()));
        let mut server = NotifyServer::new(&path).expect("server");

        let path_c = path.clone();
        let t1 = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let client = NotifyClient::connect(&path_c).expect("connect");
            let msg = client.poll(Some(Duration::from_secs(2))).unwrap();
            msg.is_some()
        });

        let path_c2 = path.clone();
        let t2 = thread::spawn(move || {
            thread::sleep(Duration::from_millis(50));
            let client = NotifyClient::connect(&path_c2).expect("connect");
            let msg = client.poll(Some(Duration::from_secs(2))).unwrap();
            msg.is_some()
        });

        thread::sleep(Duration::from_millis(200));
        let _ = server.accept_pending();

        let sent = server.broadcast(NotifyCommand::Shutdown, 0);
        assert!(sent >= 1);

        assert!(t1.join().unwrap());
        assert!(t2.join().unwrap());
    }
}
