// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::domain::DomainController;
use crate::error::{KosError, Result};

pub trait TopicPublisher: Send {
    fn publish(&mut self, data: &[u8]) -> Result<()>;

    fn topic(&self) -> &str;
}

pub struct RecvGuard {
    data_ptr: *const u8,
    data_len: usize,
    _inner: Box<dyn Send>,
}

unsafe impl Sync for RecvGuard {}

impl RecvGuard {
    #[allow(dead_code)]
    pub(crate) fn from_parts(data_ptr: *const u8, data_len: usize, inner: Box<dyn Send>) -> Self {
        Self { data_ptr, data_len, _inner: inner }
    }

    pub fn data(&self) -> &[u8] {
        unsafe { std::slice::from_raw_parts(self.data_ptr, self.data_len) }
    }

    pub fn release(self) {
        drop(self);
    }
}

pub trait TopicSubscriber: Send {
    fn recv(&mut self) -> Result<RecvGuard>;

    fn recv_copy(&mut self) -> Result<Vec<u8>>;

    fn topic(&self) -> &str;

    fn try_recv_latest(&mut self) -> Result<Option<Vec<u8>>> {
        let mut latest = None;
        while let Ok(data) = self.recv_copy() {
            latest = Some(data);
        }
        Ok(latest)
    }

    fn read_history(&mut self, _n: usize) -> Result<Option<Vec<u8>>> {
        Ok(None)
    }

    fn wait_latest(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(data) = self.try_recv_latest()? {
                return Ok(Some(data));
            }
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

pub trait Transport: Send + Sync {
    fn publisher(&self, topic: &str) -> Result<Box<dyn TopicPublisher>>;
    fn subscriber(&self, topic: &str) -> Result<Box<dyn TopicSubscriber>>;
    fn name(&self) -> &'static str;
}

pub struct CheckedTransport<T: Transport> {
    inner: T,
    app_id: String,
    domains: Arc<DomainController>,
}

impl<T: Transport> CheckedTransport<T> {
    pub fn new(inner: T, app_id: &str, domains: Arc<DomainController>) -> Self {
        Self { inner, app_id: app_id.to_string(), domains }
    }
}

impl<T: Transport> Transport for CheckedTransport<T> {
    fn publisher(&self, topic: &str) -> Result<Box<dyn TopicPublisher>> {
        check_publish_access(&self.domains, &self.app_id, topic)?;
        self.inner.publisher(topic)
    }

    fn subscriber(&self, topic: &str) -> Result<Box<dyn TopicSubscriber>> {
        check_subscribe_access(&self.domains, &self.app_id, topic)?;
        self.inner.subscriber(topic)
    }

    fn name(&self) -> &'static str {
        self.inner.name()
    }
}

pub fn topic_domain(topic: &str) -> Option<&str> {
    let trimmed = topic.trim_start_matches('/');
    trimmed.split('/').next().filter(|s| !s.is_empty())
}

pub fn check_publish_access(
    domains: &DomainController,
    app_id: &str,
    topic: &str,
) -> Result<()> {
    let app_asil = domains.asil_for(app_id)?;

    let Some(target_domain) = topic_domain(topic) else {
        return Ok(());
    };

    let target_asil = match domains.domain_asil(target_domain) {
        Ok(asil) => asil,
        Err(_) => return Ok(()),
    };

    if kos_safety::can_access(app_asil, target_asil, true) {
        Ok(())
    } else {
        Err(KosError::PermissionDenied(format!(
            "app '{app_id}' (ASIL {:?}) cannot publish to topic '{topic}' (domain '{target_domain}', ASIL {:?})",
            app_asil, target_asil
        )))
    }
}

pub fn check_subscribe_access(
    _domains: &DomainController,
    _app_id: &str,
    _topic: &str,
) -> Result<()> {
    Ok(())
}

#[derive(Clone, Default)]
pub struct MockTransport {
    channels: Arc<Mutex<HashMap<String, Vec<Vec<u8>>>>>,
}

impl MockTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn publisher(&self, topic: &str) -> MockPublisher {
        MockPublisher {
            topic: topic.to_string(),
            channels: Arc::clone(&self.channels),
        }
    }

    pub fn subscriber(&self, topic: &str) -> MockSubscriber {
        MockSubscriber {
            topic: topic.to_string(),
            channels: Arc::clone(&self.channels),
            cursor: 0,
        }
    }

    pub fn messages(&self, topic: &str) -> Vec<Vec<u8>> {
        self.channels
            .lock()
            .unwrap()
            .get(topic)
            .cloned()
            .unwrap_or_default()
    }
}

#[derive(Debug)]
pub struct MockPublisher {
    topic: String,
    channels: Arc<Mutex<HashMap<String, Vec<Vec<u8>>>>>,
}

impl TopicPublisher for MockPublisher {
    fn publish(&mut self, data: &[u8]) -> Result<()> {
        let mut map = self.channels.lock().unwrap();
        map.entry(self.topic.clone())
            .or_default()
            .push(data.to_vec());
        Ok(())
    }

    fn topic(&self) -> &str {
        &self.topic
    }
}

pub struct MockSubscriber {
    topic: String,
    channels: Arc<Mutex<HashMap<String, Vec<Vec<u8>>>>>,
    cursor: usize,
}

impl TopicSubscriber for MockSubscriber {
    fn recv(&mut self) -> Result<RecvGuard> {
        let data = self.recv_copy()?;
        let data_ptr = data.as_ptr();
        let data_len = data.len();
        Ok(RecvGuard {
            data_ptr,
            data_len,
            _inner: Box::new(data),
        })
    }

    fn recv_copy(&mut self) -> Result<Vec<u8>> {
        let map = self.channels.lock().unwrap();
        let msgs = map.get(&self.topic);
        match msgs.and_then(|m| m.get(self.cursor)) {
            Some(data) => {
                self.cursor += 1;
                Ok(data.clone())
            }
            None => Err(KosError::NotFound("no data available".into())),
        }
    }

    fn read_history(&mut self, n: usize) -> Result<Option<Vec<u8>>> {
        let map = self.channels.lock().unwrap();
        Ok(map
            .get(&self.topic)
            .and_then(|m| m.len().checked_sub(n + 1).map(|i| m[i].clone())))
    }

    fn topic(&self) -> &str {
        &self.topic
    }
}

impl Transport for MockTransport {
    fn publisher(&self, topic: &str) -> Result<Box<dyn TopicPublisher>> {
        Ok(Box::new(MockTransport::publisher(self, topic)))
    }

    fn subscriber(&self, topic: &str) -> Result<Box<dyn TopicSubscriber>> {
        Ok(Box::new(MockTransport::subscriber(self, topic)))
    }

    fn name(&self) -> &'static str {
        "mock"
    }
}

pub const SHM_MSG_SIZE: usize = 4096;

#[derive(Clone, Copy)]
#[repr(C)]
pub struct ShmMessage {
    pub len: u32,
    pub data: [u8; SHM_MSG_SIZE],
}

impl Default for ShmMessage {
    fn default() -> Self {
        Self {
            len: 0,
            data: [0u8; SHM_MSG_SIZE],
        }
    }
}

const SHM_HISTORY: u32 = 8;

fn shm_err(ctx: &str, topic: &str, e: impl std::fmt::Debug) -> KosError {
    KosError::InvalidConfig(format!("shm {ctx} '{topic}': {e:?}"))
}

pub struct ShmPublisher {
    topic: String,
    inner: comm_core::Publisher<ShmMessage>,
}

impl TopicPublisher for ShmPublisher {
    fn publish(&mut self, data: &[u8]) -> Result<()> {
        let slot = self
            .inner
            .borrow(comm_core::WriteMode::Fresh)
            .map_err(|e| shm_err("borrow", &self.topic, e))?;

        let n = data.len().min(SHM_MSG_SIZE);
        slot.data[..n].copy_from_slice(&data[..n]);
        slot.len = n as u32;

        self.inner
            .publish()
            .map_err(|e| shm_err("publish", &self.topic, e))
    }

    fn topic(&self) -> &str {
        &self.topic
    }
}

pub struct ShmSubscriber {
    topic: String,
    inner: Option<comm_core::Subscriber<ShmMessage>>,
    last_attach_try: Option<Instant>,
    last_liveness_check: Option<Instant>,
}

impl ShmSubscriber {
    fn new(topic: &str) -> Self {
        let mut s = Self { topic: topic.to_string(), inner: None, last_attach_try: None, last_liveness_check: None };
        s.connect();
        s
    }

    fn connect(&mut self) -> bool {
        if self.inner.is_some() {
            return true;
        }
        let now = Instant::now();
        if self.last_attach_try.is_some_and(|t| now.duration_since(t) < Duration::from_millis(10)) {
            return false;
        }
        self.last_attach_try = Some(now);
        self.inner = comm_core::Subscriber::<ShmMessage>::new(&self.topic).ok();
        self.inner.is_some()
    }

    fn follow_publisher_restart(&mut self) {
        let Some(sub) = &self.inner else { return };
        let now = Instant::now();
        if self.last_liveness_check.is_some_and(|t| now.duration_since(t) < Duration::from_millis(50)) {
            return;
        }
        self.last_liveness_check = Some(now);
        if sub.is_publisher_alive() {
            return;
        }
        if let Ok(fresh) = comm_core::Subscriber::<ShmMessage>::new(&self.topic) {
            if fresh.is_publisher_alive() {
                self.inner = Some(fresh);
            }
        }
    }

    fn copy_latest_new(&self) -> Result<Option<Vec<u8>>> {
        let Some(sub) = &self.inner else { return Ok(None) };
        match sub.lock_latest_new() {
            Ok(guard) => {
                let msg = guard.latest().ok_or_else(|| shm_err("recv", &self.topic, "empty guard"))?;
                Ok(Some(msg.data[..msg.len as usize].to_vec()))
            }
            Err(comm_core::Error::NoData) => Ok(None),
            Err(e) => Err(shm_err("recv", &self.topic, e)),
        }
    }
}

impl TopicSubscriber for ShmSubscriber {
    fn recv(&mut self) -> Result<RecvGuard> {
        if !self.connect() {
            return Err(KosError::NotFound(format!("shm '{}': no publisher", self.topic)));
        }
        let sub = self.inner.as_ref().expect("connected");
        let guard = sub
            .lock_latest()
            .map_err(|e| KosError::NotFound(format!("shm recv '{}': {e:?}", self.topic)))?;
        let msg = guard
            .latest()
            .ok_or_else(|| KosError::NotFound("shm recv: empty guard".into()))?;
        let data_ptr = msg.data.as_ptr();
        let data_len = msg.len as usize;

        let erased: Box<dyn Send> = unsafe {
            Box::new(std::mem::transmute::<
                comm_core::QueueLockGuard<'_, ShmMessage>,
                comm_core::QueueLockGuard<'static, ShmMessage>,
            >(guard))
        };

        Ok(RecvGuard {
            data_ptr,
            data_len,
            _inner: erased,
        })
    }

    fn recv_copy(&mut self) -> Result<Vec<u8>> {
        if !self.connect() {
            return Err(KosError::NotFound(format!("shm '{}': no publisher", self.topic)));
        }
        let sub = self.inner.as_ref().expect("connected");
        let guard = sub
            .lock_latest()
            .map_err(|e| KosError::NotFound(format!("shm recv_copy '{}': {e:?}", self.topic)))?;
        let msg = guard.latest().ok_or_else(|| KosError::NotFound("empty guard".into()))?;
        Ok(msg.data[..msg.len as usize].to_vec())
    }

    fn topic(&self) -> &str {
        &self.topic
    }

    fn try_recv_latest(&mut self) -> Result<Option<Vec<u8>>> {
        if !self.connect() {
            return Ok(None);
        }
        match self.copy_latest_new()? {
            Some(data) => Ok(Some(data)),
            None => {
                self.follow_publisher_restart();
                self.copy_latest_new()
            }
        }
    }

    fn read_history(&mut self, n: usize) -> Result<Option<Vec<u8>>> {
        if !self.connect() {
            return Ok(None);
        }
        let sub = self.inner.as_ref().expect("connected");
        let want = (n + 1).min(SHM_HISTORY as usize) as u32;
        if (n + 1) as u32 > want {
            return Ok(None);
        }
        match sub.lock_latest_n(want) {
            Ok(guard) if guard.len() == want as usize => {
                let msg = guard.get(0).ok_or_else(|| shm_err("history", &self.topic, "empty"))?;
                Ok(Some(msg.data[..msg.len as usize].to_vec()))
            }
            Ok(_) | Err(comm_core::Error::NoData) => Ok(None),
            Err(e) => Err(shm_err("history", &self.topic, e)),
        }
    }

    fn wait_latest(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        let deadline = Instant::now() + timeout;
        while !self.connect() {
            if Instant::now() >= deadline {
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        if let Some(data) = self.copy_latest_new()? {
            return Ok(Some(data));
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let ms = remaining.as_millis().clamp(1, i32::MAX as u128) as i32;
        let sub = self.inner.as_ref().expect("connected");
        if sub.wait_for_publish(ms) {
            self.copy_latest_new()
        } else {
            self.follow_publisher_restart();
            self.copy_latest_new()
        }
    }
}

#[derive(Default)]
pub struct ShmTransport {
    _name: String,
}

impl ShmTransport {
    pub fn new(name: &str) -> Self {
        Self { _name: name.to_string() }
    }

    pub fn publisher(&self, topic: &str) -> Result<ShmPublisher> {
        let inner = comm_core::Publisher::<ShmMessage>::new(topic, SHM_HISTORY, 2, 5, 0)
            .map_err(|e| shm_err("advertise", topic, e))?;
        Ok(ShmPublisher { topic: topic.to_string(), inner })
    }

    pub fn subscriber(&self, topic: &str) -> Result<ShmSubscriber> {
        Ok(ShmSubscriber::new(topic))
    }
}

impl Transport for ShmTransport {
    fn publisher(&self, topic: &str) -> Result<Box<dyn TopicPublisher>> {
        Ok(Box::new(ShmTransport::publisher(self, topic)?))
    }

    fn subscriber(&self, topic: &str) -> Result<Box<dyn TopicSubscriber>> {
        Ok(Box::new(ShmTransport::subscriber(self, topic)?))
    }

    fn name(&self) -> &'static str {
        "kos-comm"
    }
}

pub struct CommHandle {
    mock: Option<MockTransport>,
    shm: Option<ShmTransport>,
    domains: Arc<DomainController>,
    app_id: String,
}

impl CommHandle {
    pub fn new(app_id: &str, domains: Arc<DomainController>, transport: MockTransport) -> Self {
        Self {
            mock: Some(transport),
            shm: None,
            domains,
            app_id: app_id.to_string(),
        }
    }

    pub fn with_shm(app_id: &str, domains: Arc<DomainController>, transport: ShmTransport) -> Self {
        Self {
            mock: None,
            shm: Some(transport),
            domains,
            app_id: app_id.to_string(),
        }
    }

    pub fn advertise(&mut self, topic: &str) -> Result<Box<dyn TopicPublisher>> {
        check_publish_access(&self.domains, &self.app_id, topic)?;
        if let Some(ref mock) = self.mock {
            Ok(Box::new(mock.publisher(topic)))
        } else if let Some(ref mut shm) = self.shm {
            Ok(Box::new(shm.publisher(topic)?))
        } else {
            Err(KosError::InvalidConfig("no transport configured".into()))
        }
    }

    pub fn subscribe(&mut self, topic: &str) -> Result<Box<dyn TopicSubscriber>> {
        check_subscribe_access(&self.domains, &self.app_id, topic)?;
        if let Some(ref mock) = self.mock {
            Ok(Box::new(mock.subscriber(topic)))
        } else if let Some(ref mut shm) = self.shm {
            Ok(Box::new(shm.subscriber(topic)?))
        } else {
            Err(KosError::InvalidConfig("no transport configured".into()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{AsilLevel, DomainConfig};

    fn setup_domains() -> DomainController {
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
        dc
    }

    #[test]
    fn topic_domain_extraction() {
        assert_eq!(topic_domain("adas/camera/frame"), Some("adas"));
        assert_eq!(topic_domain("ivi/media/track"), Some("ivi"));
        assert_eq!(topic_domain("/adas/camera"), Some("adas"));
        assert_eq!(topic_domain("simple"), Some("simple"));
        assert_eq!(topic_domain(""), None);
    }

    #[test]
    fn qm_cannot_publish_to_asil_d_topic() {
        let dc = setup_domains();
        let result = check_publish_access(&dc, "ivi.media", "adas/camera/frame");
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), KosError::PermissionDenied(_)));
    }

    #[test]
    fn asil_d_can_publish_to_qm_topic() {
        let dc = setup_domains();
        let result = check_publish_access(&dc, "adas.camera", "ivi/media/notification");
        assert!(result.is_ok());
    }

    #[test]
    fn same_domain_publish_allowed() {
        let dc = setup_domains();
        let result = check_publish_access(&dc, "adas.camera", "adas/camera/frame");
        assert!(result.is_ok());
    }

    #[test]
    fn subscribe_always_allowed() {
        let dc = setup_domains();
        assert!(check_subscribe_access(&dc, "ivi.media", "adas/camera/frame").is_ok());
    }

    #[test]
    fn mock_pub_sub_roundtrip() {
        let transport = MockTransport::new();
        let mut publisher = transport.publisher("test/topic");
        let mut subscriber = transport.subscriber("test/topic");

        publisher.publish(b"hello").unwrap();
        publisher.publish(b"world").unwrap();

        let guard = subscriber.recv().unwrap();
        assert_eq!(guard.data(), b"hello");
        guard.release();

        let msg2 = subscriber.recv_copy().unwrap();
        assert_eq!(msg2, b"world");

        assert!(subscriber.recv().is_err());
    }

    #[test]
    fn comm_handle_enforces_access() {
        let dc = Arc::new(setup_domains());
        let transport = MockTransport::new();

        let mut qm_handle = CommHandle::new("ivi.media", Arc::clone(&dc), transport.clone());

        assert!(qm_handle.advertise("adas/camera/frame").is_err());

        let mut pub_ = qm_handle.advertise("ivi/media/track").unwrap();
        pub_.publish(b"data").unwrap();

        let _sub = qm_handle.subscribe("adas/camera/frame").unwrap();

        let mut d_handle = CommHandle::new("adas.camera", Arc::clone(&dc), transport);
        assert!(d_handle.advertise("ivi/media/notification").is_ok());
    }

    #[test]
    fn asil_b_cannot_publish_to_asil_d() {
        let dc = setup_domains();
        let result = check_publish_access(&dc, "body.lights", "adas/safety/alert");
        assert!(result.is_err());
    }

    #[test]
    fn asil_b_can_publish_to_qm() {
        let dc = setup_domains();
        let result = check_publish_access(&dc, "body.lights", "ivi/status/lights");
        assert!(result.is_ok());
    }

    fn unique_topic(name: &str) -> String {
        format!("kosexec_test/{name}_{}", std::process::id())
    }

    #[test]
    fn shm_subscriber_before_publisher_connects_lazily() {
        let t = ShmTransport::new("test");
        let topic = unique_topic("lazy");
        let mut sub = Transport::subscriber(&t, &topic).unwrap();
        assert_eq!(sub.try_recv_latest().unwrap(), None);

        let mut publ = Transport::publisher(&t, &topic).unwrap();
        publ.publish(b"one").unwrap();
        std::thread::sleep(Duration::from_millis(15));
        assert_eq!(sub.try_recv_latest().unwrap().as_deref(), Some(&b"one"[..]));
    }

    #[test]
    fn shm_try_recv_latest_returns_only_new_latest() {
        let t = ShmTransport::new("test");
        let topic = unique_topic("latest");
        let mut publ = Transport::publisher(&t, &topic).unwrap();
        let mut sub = Transport::subscriber(&t, &topic).unwrap();

        publ.publish(b"a").unwrap();
        publ.publish(b"b").unwrap();
        assert_eq!(sub.try_recv_latest().unwrap().as_deref(), Some(&b"b"[..]));
        assert_eq!(sub.try_recv_latest().unwrap(), None);
        publ.publish(b"c").unwrap();
        assert_eq!(sub.try_recv_latest().unwrap().as_deref(), Some(&b"c"[..]));
    }

    #[test]
    fn shm_wait_latest_wakes_on_publish() {
        let t = ShmTransport::new("test");
        let topic = unique_topic("wait");
        let mut publ = Transport::publisher(&t, &topic).unwrap();
        let mut sub = Transport::subscriber(&t, &topic).unwrap();

        assert_eq!(sub.wait_latest(Duration::from_millis(20)).unwrap(), None);

        let waiter = std::thread::spawn(move || {
            let start = Instant::now();
            let got = sub.wait_latest(Duration::from_secs(2)).unwrap();
            (got, start.elapsed())
        });
        std::thread::sleep(Duration::from_millis(50));
        publ.publish(b"wake").unwrap();
        let (got, waited) = waiter.join().unwrap();
        assert_eq!(got.as_deref(), Some(&b"wake"[..]));
        assert!(waited < Duration::from_millis(500), "woke after {waited:?}");
    }

    #[test]
    fn shm_read_history() {
        let t = ShmTransport::new("test");
        let topic = unique_topic("history");
        let mut publ = Transport::publisher(&t, &topic).unwrap();
        let mut sub = Transport::subscriber(&t, &topic).unwrap();
        assert_eq!(sub.read_history(0).unwrap(), None);
        for v in [b"v1", b"v2", b"v3"] {
            publ.publish(v).unwrap();
        }
        assert_eq!(sub.read_history(0).unwrap().as_deref(), Some(&b"v3"[..]));
        assert_eq!(sub.read_history(2).unwrap().as_deref(), Some(&b"v1"[..]));
        assert_eq!(sub.read_history(3).unwrap(), None);
        assert_eq!(sub.read_history(100).unwrap(), None);
    }

    #[test]
    fn mock_read_history() {
        let t = MockTransport::new();
        let mut publ = Transport::publisher(&t, "m/h").unwrap();
        let mut sub = Transport::subscriber(&t, "m/h").unwrap();
        publ.publish(b"a").unwrap();
        publ.publish(b"b").unwrap();
        assert_eq!(sub.read_history(1).unwrap().as_deref(), Some(&b"a"[..]));
        assert_eq!(sub.read_history(2).unwrap(), None);
    }

    #[test]
    fn shm_subscriber_follows_publisher_restart() {
        let t = ShmTransport::new("test");
        let topic = unique_topic("restart");
        let mut sub = Transport::subscriber(&t, &topic).unwrap();
        {
            let mut first = Transport::publisher(&t, &topic).unwrap();
            first.publish(b"from-first").unwrap();
            std::thread::sleep(Duration::from_millis(15));
            assert_eq!(sub.try_recv_latest().unwrap().as_deref(), Some(&b"from-first"[..]));
        }

        let mut second = Transport::publisher(&t, &topic).unwrap();
        second.publish(b"from-second").unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let got = loop {
            if let Some(d) = sub.try_recv_latest().unwrap() {
                break d;
            }
            assert!(Instant::now() < deadline, "subscriber did not follow the restarted publisher");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert_eq!(got, b"from-second");
    }
}
