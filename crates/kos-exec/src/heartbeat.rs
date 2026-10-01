// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::Duration;

pub const ENV_HEARTBEAT: &str = "KOS_HEARTBEAT";

const MEMFD_NAME: &[u8] = b"kos_heartbeat\0";
const CELL_SIZE: usize = std::mem::size_of::<AtomicU64>();

pub struct HeartbeatCell {
    ptr: NonNull<AtomicU64>,
}

unsafe impl Send for HeartbeatCell {}
unsafe impl Sync for HeartbeatCell {}

impl HeartbeatCell {
    pub fn create() -> std::io::Result<(Self, OwnedFd)> {
        let raw = unsafe { libc::memfd_create(MEMFD_NAME.as_ptr().cast(), libc::MFD_CLOEXEC) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if unsafe { libc::ftruncate(raw, CELL_SIZE as libc::off_t) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        let ptr = map_cell(raw).ok_or_else(std::io::Error::last_os_error)?;
        Ok((Self { ptr }, fd))
    }

    pub fn count(&self) -> u64 {
        unsafe { self.ptr.as_ref() }.load(Ordering::Acquire)
    }
}

impl Drop for HeartbeatCell {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), CELL_SIZE) };
    }
}

fn map_cell(fd: RawFd) -> Option<NonNull<AtomicU64>> {
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            CELL_SIZE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        return None;
    }
    NonNull::new(p.cast())
}

pub fn env_value(fd: &OwnedFd, timeout_ms: u64) -> String {
    format!("{}:{timeout_ms}", fd.as_raw_fd())
}

pub fn inherit_in_child(fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

struct AppSide {
    cell: &'static AtomicU64,
    timeout: Duration,
}

fn app_side() -> Option<&'static AppSide> {
    static SIDE: OnceLock<Option<AppSide>> = OnceLock::new();
    SIDE.get_or_init(|| {
        let value = std::env::var(ENV_HEARTBEAT).ok()?;
        let (fd, timeout_ms) = value.split_once(':')?;
        let fd: RawFd = fd.parse().ok()?;
        let timeout_ms: u64 = timeout_ms.parse().ok()?;
        if unsafe { libc::getpgrp() } != unsafe { libc::getpid() } {
            return None;
        }
        let link = std::fs::read_link(format!("/proc/self/fd/{fd}")).ok()?;
        if !link.to_string_lossy().starts_with("/memfd:kos_heartbeat") {
            return None;
        }
        let ptr = map_cell(fd)?;
        unsafe { libc::close(fd) };
        Some(AppSide {
            cell: unsafe { &*ptr.as_ptr() },
            timeout: Duration::from_millis(timeout_ms.max(1)),
        })
    })
    .as_ref()
}

pub fn enabled() -> bool {
    app_side().is_some()
}

pub fn timeout() -> Option<Duration> {
    app_side().map(|s| s.timeout)
}

pub fn beat() {
    if let Some(side) = app_side() {
        side.cell.fetch_add(1, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cell_is_shared_through_the_fd() {
        let (cell, fd) = HeartbeatCell::create().unwrap();
        assert_eq!(cell.count(), 0);
        let other = map_cell(fd.as_raw_fd()).unwrap();
        unsafe { other.as_ref() }.fetch_add(3, Ordering::Release);
        assert_eq!(cell.count(), 3);
        unsafe { libc::munmap(other.as_ptr().cast(), CELL_SIZE) };
    }

    #[test]
    fn env_value_carries_fd_and_timeout() {
        let (_cell, fd) = HeartbeatCell::create().unwrap();
        assert_eq!(env_value(&fd, 500), format!("{}:500", fd.as_raw_fd()));
    }
}
