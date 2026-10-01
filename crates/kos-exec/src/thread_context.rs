// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

use crate::comm::{TopicPublisher, TopicSubscriber, Transport};
use crate::error::Result;

pub struct ThreadContext {
    thread_name: String,
    subs: Vec<String>,
    pubs: Vec<String>,
    locked: bool,
    topic_data: HashMap<String, Vec<u8>>,
    fresh: HashSet<String>,
    write_buf: RefCell<HashMap<String, Vec<u8>>>,
    readers: RefCell<HashMap<String, Box<dyn TopicSubscriber>>>,
    history_cache: RefCell<Vec<Box<[u8]>>>,
    writers: HashMap<String, Box<dyn TopicPublisher>>,
    log_buf: RefCell<Vec<String>>,
}

impl ThreadContext {
    pub fn new(thread_name: &str, subs: Vec<String>, pubs: Vec<String>) -> Self {
        Self {
            thread_name: thread_name.to_string(),
            subs,
            pubs,
            locked: false,
            topic_data: HashMap::new(),
            fresh: HashSet::new(),
            write_buf: RefCell::new(HashMap::new()),
            readers: RefCell::new(HashMap::new()),
            history_cache: RefCell::new(Vec::new()),
            writers: HashMap::new(),
            log_buf: RefCell::new(Vec::new()),
        }
    }

    pub fn attach(&mut self, transport: &dyn Transport) -> Result<()> {
        let readers = self.readers.get_mut();
        for topic in &self.subs {
            if !readers.contains_key(topic) {
                readers.insert(topic.clone(), transport.subscriber(topic)?);
            }
        }
        for topic in &self.pubs {
            if !self.writers.contains_key(topic) {
                self.writers.insert(topic.clone(), transport.publisher(topic)?);
            }
        }
        Ok(())
    }

    pub fn is_attached(&self) -> bool {
        !self.readers.borrow().is_empty() || !self.writers.is_empty()
    }

    pub fn thread_name(&self) -> &str {
        &self.thread_name
    }

    pub fn subscribed_topics(&self) -> &[String] {
        &self.subs
    }

    pub fn published_topics(&self) -> &[String] {
        &self.pubs
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    pub fn lock_topics(&mut self) {
        self.fresh.clear();
        self.history_cache.get_mut().clear();
        for (topic, reader) in self.readers.get_mut().iter_mut() {
            match reader.try_recv_latest() {
                Ok(Some(data)) => {
                    self.topic_data.insert(topic.clone(), data);
                    self.fresh.insert(topic.clone());
                }
                Ok(None) => {}
                Err(e) => eprintln!("[kos-exec] thread '{}': recv '{topic}': {e}", self.thread_name),
            }
        }
        self.locked = true;
    }

    pub fn unlock_topics(&mut self) {
        let writes = std::mem::take(&mut *self.write_buf.borrow_mut());
        for (topic, data) in writes {
            match self.writers.get_mut(&topic) {
                Some(w) => {
                    if let Err(e) = w.publish(&data) {
                        eprintln!("[kos-exec] thread '{}': publish '{topic}': {e}", self.thread_name);
                    }
                }
                None => {
                    self.write_buf.borrow_mut().insert(topic, data);
                }
            }
        }
        self.locked = false;
    }

    pub(crate) fn wait_topic(&mut self, topic: &str, timeout: Duration) -> bool {
        let Some(reader) = self.readers.get_mut().get_mut(topic) else {
            std::thread::sleep(timeout);
            return false;
        };
        match reader.wait_latest(timeout) {
            Ok(Some(data)) => {
                self.topic_data.insert(topic.to_string(), data);
                self.fresh.insert(topic.to_string());
                true
            }
            Ok(None) => false,
            Err(e) => {
                eprintln!("[kos-exec] thread '{}': wait '{topic}': {e}", self.thread_name);
                std::thread::sleep(timeout);
                false
            }
        }
    }

    pub(crate) fn mark_fresh(&mut self, topic: &str) {
        self.fresh.insert(topic.to_string());
    }

    pub fn is_fresh(&self, topic: &str) -> bool {
        self.fresh.contains(topic)
    }

    fn assert_subscribed(&self, topic: &str) {
        assert!(
            self.subs.iter().any(|s| s == topic),
            "topic '{topic}' not in subscription list for thread '{}'",
            self.thread_name,
        );
    }

    fn assert_published(&self, topic: &str) {
        assert!(
            self.pubs.iter().any(|s| s == topic),
            "topic '{topic}' not in publish list for thread '{}'",
            self.thread_name,
        );
    }

    pub fn read_bytes(&self, topic: &str) -> &[u8] {
        self.assert_subscribed(topic);
        self.topic_data.get(topic).map(|v| v.as_slice()).unwrap_or(&[])
    }

    pub fn read<T: Default + Copy>(&self, topic: &str) -> T {
        let bytes = self.read_bytes(topic);
        if bytes.len() < std::mem::size_of::<T>() {
            return T::default();
        }
        unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const T) }
    }

    pub fn try_read<T: Copy>(&self, topic: &str) -> Option<T> {
        self.assert_subscribed(topic);
        let bytes = self.topic_data.get(topic)?;
        if bytes.len() < std::mem::size_of::<T>() {
            return None;
        }
        Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const T) })
    }

    pub fn read_ago<T: Default + Copy>(&self, topic: &str, n: usize) -> T {
        self.try_read_ago(topic, n).unwrap_or_default()
    }

    pub fn try_read_ago<T: Copy>(&self, topic: &str, n: usize) -> Option<T> {
        let bytes = self.read_ago_bytes(topic, n)?;
        if bytes.len() < std::mem::size_of::<T>() {
            return None;
        }
        Some(unsafe { std::ptr::read_unaligned(bytes.as_ptr() as *const T) })
    }

    pub fn read_ago_bytes(&self, topic: &str, n: usize) -> Option<&[u8]> {
        self.assert_subscribed(topic);
        if n == 0 {
            return self.topic_data.get(topic).map(|v| v.as_slice());
        }
        let data = self.readers.borrow_mut().get_mut(topic)?.read_history(n).ok()??;
        let boxed: Box<[u8]> = data.into_boxed_slice();
        let ptr: *const [u8] = &*boxed;
        self.history_cache.borrow_mut().push(boxed);
        Some(unsafe { &*ptr })
    }

    pub fn write<T: Copy>(&self, topic: &str, data: &T) {
        let bytes = unsafe {
            std::slice::from_raw_parts(data as *const T as *const u8, std::mem::size_of::<T>())
        };
        self.write_bytes(topic, bytes);
    }

    pub fn write_bytes(&self, topic: &str, data: &[u8]) {
        self.assert_published(topic);
        self.write_buf.borrow_mut().insert(topic.to_string(), data.to_vec());
    }

    pub fn drain_writes(&mut self) -> HashMap<String, Vec<u8>> {
        std::mem::take(&mut *self.write_buf.borrow_mut())
    }

    pub fn inject_topic_data(&mut self, topic: &str, data: Vec<u8>) {
        self.topic_data.insert(topic.to_string(), data);
    }

    pub fn log_info(&self, msg: &str) {
        self.log_buf.borrow_mut().push(format!("[{}] {msg}", self.thread_name));
    }

    pub fn drain_logs(&mut self) -> Vec<String> {
        std::mem::take(&mut *self.log_buf.borrow_mut())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lock_unlock_toggle() {
        let mut ctx = ThreadContext::new("test", vec!["topic_a".into()], vec![]);
        assert!(!ctx.is_locked());

        ctx.lock_topics();
        assert!(ctx.is_locked());

        ctx.unlock_topics();
        assert!(!ctx.is_locked());
    }

    #[test]
    fn read_default_when_no_data() {
        let ctx = ThreadContext::new("test", vec!["sensor".into()], vec![]);
        let val: f32 = ctx.read("sensor");
        assert_eq!(val, 0.0);
    }

    #[test]
    fn read_injected_data() {
        let mut ctx = ThreadContext::new("test", vec!["sensor".into()], vec![]);

        let value: f32 = 42.5;
        let bytes = unsafe {
            std::slice::from_raw_parts(
                &value as *const f32 as *const u8,
                std::mem::size_of::<f32>(),
            )
        };
        ctx.inject_topic_data("sensor", bytes.to_vec());

        let result: f32 = ctx.read("sensor");
        assert!((result - 42.5).abs() < f32::EPSILON);
    }

    #[test]
    fn try_read_none_when_no_data() {
        let ctx = ThreadContext::new("test", vec!["sensor".into()], vec![]);
        let result: Option<f32> = ctx.try_read("sensor");
        assert!(result.is_none());
    }

    #[test]
    fn try_read_some_with_data() {
        let mut ctx = ThreadContext::new("test", vec!["sensor".into()], vec![]);

        let value: u32 = 123;
        let bytes = value.to_ne_bytes().to_vec();
        ctx.inject_topic_data("sensor", bytes);

        let result: Option<u32> = ctx.try_read("sensor");
        assert_eq!(result, Some(123));
    }

    #[test]
    fn write_and_drain() {
        let mut ctx = ThreadContext::new("test", vec![], vec!["output".into()]);

        let value: u32 = 99;
        ctx.write("output", &value);

        let writes = ctx.drain_writes();
        assert_eq!(writes.len(), 1);
        assert!(writes.contains_key("output"));

        let stored_bytes = &writes["output"];
        let stored_value = u32::from_ne_bytes(stored_bytes[..4].try_into().unwrap());
        assert_eq!(stored_value, 99);

        assert!(ctx.drain_writes().is_empty());
    }

    #[test]
    #[should_panic(expected = "not in subscription list")]
    fn read_unsubscribed_topic_panics() {
        let ctx = ThreadContext::new("test", vec![], vec![]);
        let _: f32 = ctx.read("nonexistent");
    }

    #[test]
    #[should_panic(expected = "not in publish list")]
    fn write_unpublished_topic_panics() {
        let ctx = ThreadContext::new("test", vec![], vec![]);
        let val: u32 = 1;
        ctx.write("nonexistent", &val);
    }

    #[test]
    fn log_buffer() {
        let mut ctx = ThreadContext::new("fusion", vec![], vec![]);
        ctx.log_info("started");
        ctx.log_info("done");

        let logs = ctx.drain_logs();
        assert_eq!(logs.len(), 2);
        assert!(logs[0].contains("fusion"));
        assert!(logs[0].contains("started"));
    }

    #[test]
    fn accessors() {
        let ctx = ThreadContext::new(
            "worker",
            vec!["in1".into(), "in2".into()],
            vec!["out1".into()],
        );
        assert_eq!(ctx.thread_name(), "worker");
        assert_eq!(ctx.subscribed_topics(), &["in1", "in2"]);
        assert_eq!(ctx.published_topics(), &["out1"]);
    }

    #[test]
    fn read_ago_through_transport() {
        use crate::comm::{MockTransport, Transport};
        let t = MockTransport::new();
        let mut publ = Transport::publisher(&t, "t/v").unwrap();
        let mut ctx = ThreadContext::new("th", vec!["t/v".into()], vec![]);
        ctx.attach(&t).unwrap();
        for v in [10u32, 20, 30] {
            publ.publish(&v.to_ne_bytes()).unwrap();
        }
        ctx.lock_topics();
        assert_eq!(ctx.read_ago::<u32>("t/v", 0), 30);
        assert_eq!(ctx.read_ago::<u32>("t/v", 1), 20);
        assert_eq!(ctx.read_ago::<u32>("t/v", 2), 10);
        assert_eq!(ctx.try_read_ago::<u32>("t/v", 3), None);
        let b1 = ctx.read_ago_bytes("t/v", 1).unwrap();
        let b2 = ctx.read_ago_bytes("t/v", 2).unwrap();
        assert_eq!((b1, b2), (&20u32.to_ne_bytes()[..], &10u32.to_ne_bytes()[..]));
        ctx.unlock_topics();
    }
}
