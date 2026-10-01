// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;
use std::sync::atomic::Ordering;

use comm_core::queue_shm::{queue_shm_name, QUEUE_SHM_MAGIC, QUEUE_SHM_VERSION};
use comm_core::{QueueShm, QueueShmHeader, MAX_DATA_SHMS};

use crate::error::{KosError, Result};

#[derive(Debug, Clone)]
pub struct ShmTopicSummary {
    pub shm_name: String,
    pub topic: Option<String>,
    pub size_bytes: u64,
    pub data_size: u32,
    pub history: u32,
    pub margin: u32,
    pub slots: u32,
    pub queue_len: u32,
    pub publish_count: u64,
    pub publisher_pid: u32,
    pub publisher_alive: bool,
    pub readers: u32,
    pub overflow: u64,
    pub starvation: u64,
    pub sub_miss: u64,
    pub deadline_us: u32,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ShmSlotInfo {
    pub index: u32,
    pub queue_pos: Option<u32>,
    pub sequence: u64,
    pub size: u32,
    pub readers: u32,
    pub pending: bool,
    pub read_count: u64,
    pub timestamp_ns: u64,
}

#[derive(Debug, Clone)]
pub struct ShmTopicDetail {
    pub summary: ShmTopicSummary,
    pub slots: Vec<ShmSlotInfo>,
}

fn pid_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    let r = unsafe { libc::kill(pid as i32, 0) };
    r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

fn summarize(shm_name: &str, topic: Option<String>, size_bytes: u64, h: &QueueShmHeader) -> ShmTopicSummary {
    let pid = h.publisher_pid.load(Ordering::Acquire);
    ShmTopicSummary {
        shm_name: shm_name.to_string(),
        topic,
        size_bytes,
        data_size: h.data_size,
        history: h.history,
        margin: h.margin,
        slots: h.num.load(Ordering::Acquire),
        queue_len: h.queue_len.load(Ordering::Acquire),
        publish_count: h.publish_count.load(Ordering::Acquire),
        publisher_pid: pid,
        publisher_alive: pid_alive(pid),
        readers: h.next_reader_id.load(Ordering::Acquire),
        overflow: h.pub_overflow_count.load(Ordering::Relaxed),
        starvation: h.pub_starvation_count.load(Ordering::Relaxed),
        sub_miss: h.sub_total_miss.load(Ordering::Relaxed),
        deadline_us: h.deadline_us,
        error: None,
    }
}

struct HeaderMap {
    ptr: *mut libc::c_void,
    len: usize,
}

impl HeaderMap {
    fn open(shm_name: &str) -> std::result::Result<(Self, u64), String> {
        let path = format!("/dev/shm{shm_name}");
        let c_path = std::ffi::CString::new(path.clone()).map_err(|e| e.to_string())?;
        let fd = unsafe { libc::open(c_path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(format!("open {path}: {}", std::io::Error::last_os_error()));
        }
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let size = if unsafe { libc::fstat(fd, &mut st) } == 0 { st.st_size as u64 } else { 0 };
        let len = std::mem::size_of::<QueueShmHeader>();
        if (size as usize) < len {
            unsafe { libc::close(fd) };
            return Err(format!("too small ({size} bytes)"));
        }
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ, libc::MAP_SHARED, fd, 0) };
        unsafe { libc::close(fd) };
        if ptr == libc::MAP_FAILED {
            return Err(format!("mmap: {}", std::io::Error::last_os_error()));
        }
        Ok((Self { ptr, len }, size))
    }

    fn header(&self) -> &QueueShmHeader {
        unsafe { &*(self.ptr as *const QueueShmHeader) }
    }
}

impl Drop for HeaderMap {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr, self.len) };
    }
}

pub fn list_topics(candidates: &[String]) -> Vec<ShmTopicSummary> {
    let names: HashMap<String, String> =
        candidates.iter().map(|t| (queue_shm_name(t), t.clone())).collect();

    let mut regions: Vec<String> = comm_core::list_shm_regions()
        .into_iter()
        .filter(|n| n.starts_with("/kos_q_"))
        .collect();
    regions.sort();

    let mut out: Vec<ShmTopicSummary> = regions
        .into_iter()
        .map(|shm_name| {
            let topic = names.get(&shm_name).cloned();
            match HeaderMap::open(&shm_name) {
                Ok((map, size)) => {
                    let h = map.header();
                    if h.magic != QUEUE_SHM_MAGIC || h.version != QUEUE_SHM_VERSION {
                        bad(&shm_name, topic, format!("unknown header (magic {:#x}, version {})", h.magic, h.version))
                    } else {
                        summarize(&shm_name, topic, size, h)
                    }
                }
                Err(e) => bad(&shm_name, topic, e),
            }
        })
        .collect();
    out.sort_by(|a, b| (a.topic.is_none(), &a.topic, &a.shm_name).cmp(&(b.topic.is_none(), &b.topic, &b.shm_name)));
    out
}

fn bad(shm_name: &str, topic: Option<String>, error: String) -> ShmTopicSummary {
    ShmTopicSummary {
        shm_name: shm_name.to_string(),
        topic,
        size_bytes: 0,
        data_size: 0,
        history: 0,
        margin: 0,
        slots: 0,
        queue_len: 0,
        publish_count: 0,
        publisher_pid: 0,
        publisher_alive: false,
        readers: 0,
        overflow: 0,
        starvation: 0,
        sub_miss: 0,
        deadline_us: 0,
        error: Some(error),
    }
}

pub fn cleanup_stale(candidates: &[String], dry_run: bool) -> Vec<ShmTopicSummary> {
    cleanup_stale_where(candidates, dry_run, |_| true)
}

pub fn cleanup_stale_topics(topics: &[String], dry_run: bool) -> Vec<ShmTopicSummary> {
    cleanup_stale_where(topics, dry_run, |s| s.topic.as_ref().is_some_and(|t| topics.contains(t)))
}

fn cleanup_stale_where(
    candidates: &[String],
    dry_run: bool,
    select: impl Fn(&ShmTopicSummary) -> bool,
) -> Vec<ShmTopicSummary> {
    let stale: Vec<ShmTopicSummary> = list_topics(candidates)
        .into_iter()
        .filter(|s| s.error.is_some() || (s.publisher_pid != 0 && !s.publisher_alive))
        .filter(|s| select(s))
        .collect();
    if !dry_run {
        for s in &stale {
            if let Ok(name) = std::ffi::CString::new(s.shm_name.clone()) {
                unsafe { libc::shm_unlink(name.as_ptr()) };
            }
        }
    }
    stale
}

pub fn topic_info(topic: &str) -> Result<ShmTopicDetail> {
    let shm = QueueShm::attach_readonly(topic)
        .map_err(|e| KosError::NotFound(format!("SHM for topic '{topic}' ({}): {e:?}", queue_shm_name(topic))))?;
    let h = shm.header();
    let shm_name = queue_shm_name(topic);
    let size = std::fs::metadata(format!("/dev/shm{shm_name}")).map(|m| m.len()).unwrap_or(0);
    let summary = summarize(&shm_name, Some(topic.to_string()), size, h);

    let head = h.queue_head.load(Ordering::Acquire);
    let qlen = h.queue_len.load(Ordering::Acquire);
    let mut queue_pos: HashMap<u32, u32> = HashMap::new();
    for i in 0..qlen {
        let pos = (head + i) as usize % MAX_DATA_SHMS;
        queue_pos.insert(h.queue[pos].load(Ordering::Acquire), i);
    }

    let mut slots = Vec::new();
    for idx in 0..shm.data_count() {
        let sh = shm.slot_header(idx).map_err(|e| KosError::InvalidConfig(format!("slot {idx}: {e:?}")))?;
        let bitmask_readers = h
            .reader_bitmasks
            .iter()
            .filter(|b| b.slots.load(Ordering::Acquire) & (1u64 << idx) != 0)
            .count() as u32;
        slots.push(ShmSlotInfo {
            index: idx,
            queue_pos: queue_pos.get(&idx).copied(),
            sequence: sh.sequence.load(Ordering::Acquire),
            size: sh.size.load(Ordering::Acquire),
            readers: sh.rx_count.load(Ordering::Acquire) + bitmask_readers,
            pending: h.pending_flags[idx as usize].load(Ordering::Acquire) != 0,
            read_count: sh.cnt_read.load(Ordering::Relaxed),
            timestamp_ns: sh.timestamp.load(Ordering::Acquire),
        });
    }
    Ok(ShmTopicDetail { summary, slots })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::comm::{ShmTransport, Transport};

    #[test]
    fn list_and_info_for_live_topic() {
        let topic = format!("kosexec_test/inspect_{}", std::process::id());
        let t = ShmTransport::new("test");
        let mut publ = Transport::publisher(&t, &topic).unwrap();
        let _sub = Transport::subscriber(&t, &topic).unwrap();
        for v in [b"a".as_slice(), b"bb", b"ccc"] {
            publ.publish(v).unwrap();
        }

        let list = list_topics(std::slice::from_ref(&topic));
        let s = list.iter().find(|s| s.topic.as_deref() == Some(topic.as_str())).expect("topic listed");
        assert!(s.error.is_none(), "{:?}", s.error);
        assert_eq!(s.publish_count, 3);
        assert_eq!(s.queue_len, 3);
        assert_eq!(s.publisher_pid, std::process::id());
        assert!(s.publisher_alive);
        assert!(s.readers >= 1);

        let d = topic_info(&topic).unwrap();
        let mut queued: Vec<&ShmSlotInfo> = d.slots.iter().filter(|s| s.queue_pos.is_some()).collect();
        queued.sort_by_key(|s| s.queue_pos);
        let sizes: Vec<u32> = queued.iter().map(|s| s.size).collect();
        let seqs: Vec<u64> = queued.iter().map(|s| s.sequence).collect();
        assert_eq!(seqs, vec![0, 1, 2]);
        assert_eq!(sizes.len(), 3);
    }

    #[test]
    fn cleanup_removes_only_dead_publisher_regions() {
        let pid = std::process::id();
        let live = format!("kosexec_test/cleanup_live_{pid}");
        let dead = format!("kosexec_test/cleanup_dead_{pid}");
        let t = ShmTransport::new("test");
        let mut live_pub = Transport::publisher(&t, &live).unwrap();
        live_pub.publish(b"x").unwrap();
        {
            let mut p = Transport::publisher(&t, &dead).unwrap();
            p.publish(b"x").unwrap();
            let mut child = std::process::Command::new("true").spawn().unwrap();
            let dead_pid = child.id();
            child.wait().unwrap();
            let shm = QueueShm::attach(&dead).unwrap();
            shm.header().publisher_pid.store(dead_pid, Ordering::Release);
            std::mem::forget(p);
        }
        let names = [live.clone(), dead.clone()];
        let ours = |s: &ShmTopicSummary| s.topic.as_ref().is_some_and(|t| names.contains(t));

        let planned = cleanup_stale_where(&names, true, ours);
        assert!(planned.iter().any(|s| s.topic.as_deref() == Some(dead.as_str())));
        assert!(!planned.iter().any(|s| s.topic.as_deref() == Some(live.as_str())));
        assert!(list_topics(&names).iter().any(|s| s.topic.as_deref() == Some(dead.as_str())), "dry run must not remove");

        cleanup_stale_where(&names, false, ours);
        let after = list_topics(&names);
        assert!(!after.iter().any(|s| s.topic.as_deref() == Some(dead.as_str())));
        assert!(after.iter().any(|s| s.topic.as_deref() == Some(live.as_str())));
    }

    #[test]
    fn unknown_topic_info_is_not_found() {
        assert!(matches!(topic_info("kosexec_test/no_such_topic_xyz"), Err(KosError::NotFound(_))));
    }
}
