// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

const MAX_GROUPS: usize = 4096;
const GRACE_NS: libc::c_long = 300_000_000;
const MSG_CGROUP: i32 = 0;
const MAX_PATH: usize = 1024;

pub struct Reaper {
    tx: Option<OwnedFd>,
    pid: libc::pid_t,
}

impl Reaper {
    pub fn start() -> std::io::Result<Self> {
        let mut fds = [0 as libc::c_int; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let (rx, tx) = (fds[0], fds[1]);
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            let err = std::io::Error::last_os_error();
            unsafe {
                libc::close(rx);
                libc::close(tx);
            }
            return Err(err);
        }
        if pid == 0 {
            unsafe {
                libc::close(tx);
                reaper_main(rx)
            }
        }
        unsafe { libc::close(rx) };
        Ok(Self { tx: Some(unsafe { OwnedFd::from_raw_fd(tx) }), pid })
    }

    pub fn pid(&self) -> u32 {
        self.pid as u32
    }

    pub fn track(&self, pgid: u32) {
        self.send(pgid as i32);
    }

    pub fn untrack(&self, pgid: u32) {
        self.send(-(pgid as i32));
    }

    pub fn set_cgroup(&self, dir: &std::path::Path) {
        let mut path = dir.join("cgroup.kill").into_os_string().into_encoded_bytes();
        path.push(0);
        if path.len() > MAX_PATH {
            return;
        }
        if let Some(tx) = &self.tx {
            let mut msg = MSG_CGROUP.to_ne_bytes().to_vec();
            msg.extend_from_slice(&(path.len() as u32).to_ne_bytes());
            msg.extend_from_slice(&path);
            unsafe { libc::write(tx.as_raw_fd(), msg.as_ptr().cast(), msg.len()) };
        }
    }

    fn send(&self, msg: i32) {
        if let Some(tx) = &self.tx {
            let bytes = msg.to_ne_bytes();
            unsafe { libc::write(tx.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        }
    }
}

impl Drop for Reaper {
    fn drop(&mut self) {
        drop(self.tx.take());
        unsafe { libc::waitpid(self.pid, std::ptr::null_mut(), 0) };
    }
}

unsafe fn reaper_main(rx: libc::c_int) -> ! {
    libc::setpgid(0, 0);
    libc::signal(libc::SIGINT, libc::SIG_IGN);
    libc::signal(libc::SIGHUP, libc::SIG_IGN);
    libc::signal(libc::SIGPIPE, libc::SIG_IGN);

    let mut groups = [0 as libc::pid_t; MAX_GROUPS];
    let mut count = 0usize;
    let mut cgroup_kill = [0u8; MAX_PATH];
    let mut has_cgroup = false;
    let mut buf = [0u8; 4];
    loop {
        if !read_exact(rx, &mut buf) {
            break;
        }
        let msg = i32::from_ne_bytes(buf);
        if msg == MSG_CGROUP {
            if !read_exact(rx, &mut buf) {
                break;
            }
            let len = u32::from_ne_bytes(buf) as usize;
            if len == 0 || len > MAX_PATH || !read_exact(rx, &mut cgroup_kill[..len]) {
                break;
            }
            has_cgroup = cgroup_kill[len - 1] == 0;
        } else if msg > 0 {
            if count < MAX_GROUPS {
                groups[count] = msg;
                count += 1;
            }
        } else if msg < 0 {
            if let Some(i) = groups[..count].iter().position(|&g| g == -msg) {
                count -= 1;
                groups[i] = groups[count];
            }
        }
    }

    if has_cgroup {
        let fd = libc::open(cgroup_kill.as_ptr().cast(), libc::O_WRONLY);
        if fd >= 0 {
            libc::write(fd, b"1".as_ptr().cast(), 1);
            libc::close(fd);
        }
    }
    for &g in &groups[..count] {
        libc::kill(-g, libc::SIGTERM);
        libc::kill(-g, libc::SIGCONT);
    }
    if count > 0 {
        let ts = libc::timespec { tv_sec: 0, tv_nsec: GRACE_NS };
        libc::nanosleep(&ts, std::ptr::null_mut());
        for &g in &groups[..count] {
            libc::kill(-g, libc::SIGKILL);
        }
    }
    libc::_exit(0)
}

unsafe fn read_exact(fd: libc::c_int, buf: &mut [u8]) -> bool {
    let mut got = 0usize;
    while got < buf.len() {
        let n = libc::read(fd, buf.as_mut_ptr().add(got).cast(), buf.len() - got);
        if n > 0 {
            got += n as usize;
        } else if n < 0 && *libc::__errno_location() == libc::EINTR {
            continue;
        } else {
            return false;
        }
    }
    true
}
