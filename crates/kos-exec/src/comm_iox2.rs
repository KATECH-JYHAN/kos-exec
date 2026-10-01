// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::time::Duration;

use iceoryx2::port::listener::Listener;
use iceoryx2::port::notifier::Notifier;
use iceoryx2::port::publisher::Publisher;
use iceoryx2::port::subscriber::Subscriber;
use iceoryx2::prelude::*;
use iceoryx2::sample::Sample;

use crate::comm::{RecvGuard, TopicPublisher, TopicSubscriber, Transport, SHM_MSG_SIZE};
use crate::error::{KosError, Result};

type Svc = ipc_threadsafe::Service;

fn iox_err(ctx: &str, topic: &str, e: impl std::fmt::Debug) -> KosError {
    KosError::InvalidConfig(format!("iceoryx2 {ctx} '{topic}': {e:?}"))
}

pub struct Iox2Transport {
    node: Node<Svc>,
}

impl Iox2Transport {
    pub fn new(name: &str) -> Result<Self> {
        let node_name: NodeName = name.try_into().map_err(|e| iox_err("node name", name, e))?;
        let node = NodeBuilder::new()
            .name(&node_name)
            .create::<Svc>()
            .map_err(|e| iox_err("node", name, e))?;
        Ok(Self { node })
    }

    fn service_name(topic: &str) -> Result<ServiceName> {
        topic.try_into().map_err(|e| iox_err("service name", topic, e))
    }

    fn pubsub(
        &self,
        topic: &str,
    ) -> Result<iceoryx2::service::port_factory::publish_subscribe::PortFactory<Svc, [u8], ()>> {
        self.node
            .service_builder(&Self::service_name(topic)?)
            .publish_subscribe::<[u8]>()
            .history_size(1)
            .subscriber_max_buffer_size(4)
            .enable_safe_overflow(true)
            .max_publishers(4)
            .max_subscribers(32)
            .open_or_create()
            .map_err(|e| iox_err("pubsub service", topic, e))
    }

    fn event(&self, topic: &str) -> Result<iceoryx2::service::port_factory::event::PortFactory<Svc>> {
        self.node
            .service_builder(&Self::service_name(topic)?)
            .event()
            .max_notifiers(4)
            .max_listeners(32)
            .open_or_create()
            .map_err(|e| iox_err("event service", topic, e))
    }
}

impl Transport for Iox2Transport {
    fn publisher(&self, topic: &str) -> Result<Box<dyn TopicPublisher>> {
        let publisher = self
            .pubsub(topic)?
            .publisher_builder()
            .initial_max_slice_len(SHM_MSG_SIZE)
            .create()
            .map_err(|e| iox_err("publisher", topic, e))?;
        let notifier = self
            .event(topic)?
            .notifier_builder()
            .create()
            .map_err(|e| iox_err("notifier", topic, e))?;
        Ok(Box::new(Iox2Publisher { topic: topic.to_string(), publisher, notifier }))
    }

    fn subscriber(&self, topic: &str) -> Result<Box<dyn TopicSubscriber>> {
        let subscriber = self
            .pubsub(topic)?
            .subscriber_builder()
            .create()
            .map_err(|e| iox_err("subscriber", topic, e))?;
        let listener = self
            .event(topic)?
            .listener_builder()
            .create()
            .map_err(|e| iox_err("listener", topic, e))?;
        Ok(Box::new(Iox2Subscriber { topic: topic.to_string(), subscriber, listener }))
    }

    fn name(&self) -> &'static str {
        "iceoryx2"
    }
}

pub struct Iox2Publisher {
    topic: String,
    publisher: Publisher<Svc, [u8], ()>,
    notifier: Notifier<Svc>,
}

impl TopicPublisher for Iox2Publisher {
    fn publish(&mut self, data: &[u8]) -> Result<()> {
        let data = &data[..data.len().min(SHM_MSG_SIZE)];
        let sample = self
            .publisher
            .loan_slice_uninit(data.len())
            .map_err(|e| iox_err("loan", &self.topic, e))?;
        let sample = sample.write_from_slice(data);
        sample.send().map_err(|e| iox_err("send", &self.topic, e))?;
        self.notifier.notify().map_err(|e| iox_err("notify", &self.topic, e))?;
        Ok(())
    }

    fn topic(&self) -> &str {
        &self.topic
    }
}

pub struct Iox2Subscriber {
    topic: String,
    subscriber: Subscriber<Svc, [u8], ()>,
    listener: Listener<Svc>,
}

impl Iox2Subscriber {
    fn latest_sample(&self) -> Result<Option<Sample<Svc, [u8], ()>>> {
        let mut latest = None;
        while let Some(sample) = self
            .subscriber
            .receive()
            .map_err(|e| iox_err("receive", &self.topic, e))?
        {
            latest = Some(sample);
        }
        Ok(latest)
    }

    fn drain_events(&self) {
        let _ = self.listener.try_wait(|_| {});
    }
}

impl TopicSubscriber for Iox2Subscriber {
    fn recv(&mut self) -> Result<RecvGuard> {
        let sample = self
            .latest_sample()?
            .ok_or_else(|| KosError::NotFound(format!("iceoryx2 '{}': no data", self.topic)))?;
        let payload = sample.payload();
        let (data_ptr, data_len) = (payload.as_ptr(), payload.len());
        Ok(RecvGuard::from_parts(data_ptr, data_len, Box::new(sample)))
    }

    fn recv_copy(&mut self) -> Result<Vec<u8>> {
        self.latest_sample()?
            .map(|s| s.payload().to_vec())
            .ok_or_else(|| KosError::NotFound(format!("iceoryx2 '{}': no data", self.topic)))
    }

    fn topic(&self) -> &str {
        &self.topic
    }

    fn try_recv_latest(&mut self) -> Result<Option<Vec<u8>>> {
        self.drain_events();
        Ok(self.latest_sample()?.map(|s| s.payload().to_vec()))
    }

    fn wait_latest(&mut self, timeout: Duration) -> Result<Option<Vec<u8>>> {
        if let Some(data) = self.try_recv_latest()? {
            return Ok(Some(data));
        }
        self.listener
            .timed_wait(|_| {}, timeout)
            .map_err(|e| iox_err("wait", &self.topic, e))?;
        Ok(self.latest_sample()?.map(|s| s.payload().to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn topic(name: &str) -> String {
        format!("kosexec_test/iox2_{name}_{}", std::process::id())
    }

    #[test]
    fn publish_and_receive_latest() {
        let t = Iox2Transport::new("test").unwrap();
        let topic = topic("latest");
        let mut publ = t.publisher(&topic).unwrap();
        let mut sub = t.subscriber(&topic).unwrap();

        assert_eq!(sub.try_recv_latest().unwrap(), None);
        publ.publish(b"a").unwrap();
        publ.publish(b"bb").unwrap();
        assert_eq!(sub.try_recv_latest().unwrap().as_deref(), Some(&b"bb"[..]));
        assert_eq!(sub.try_recv_latest().unwrap(), None);
    }

    #[test]
    fn subscriber_created_before_publisher_gets_data() {
        let t = Iox2Transport::new("test").unwrap();
        let topic = topic("order");
        let mut sub = t.subscriber(&topic).unwrap();
        let mut publ = t.publisher(&topic).unwrap();
        publ.publish(b"hi").unwrap();
        assert_eq!(sub.try_recv_latest().unwrap().as_deref(), Some(&b"hi"[..]));
    }

    #[test]
    fn wait_latest_wakes_on_publish() {
        let t = Iox2Transport::new("test").unwrap();
        let topic = topic("wait");
        let mut publ = t.publisher(&topic).unwrap();
        let mut sub = t.subscriber(&topic).unwrap();
        assert_eq!(sub.wait_latest(Duration::from_millis(20)).unwrap(), None);

        let waiter = std::thread::spawn(move || {
            let start = Instant::now();
            (sub.wait_latest(Duration::from_secs(2)).unwrap(), start.elapsed())
        });
        std::thread::sleep(Duration::from_millis(50));
        publ.publish(b"wake").unwrap();
        let (got, waited) = waiter.join().unwrap();
        assert_eq!(got.as_deref(), Some(&b"wake"[..]));
        assert!(waited < Duration::from_millis(500), "woke after {waited:?}");
    }

    #[test]
    fn zero_copy_recv() {
        let t = Iox2Transport::new("test").unwrap();
        let topic = topic("zc");
        let mut publ = t.publisher(&topic).unwrap();
        let mut sub = t.subscriber(&topic).unwrap();
        publ.publish(b"zero-copy").unwrap();
        let guard = sub.recv().unwrap();
        assert_eq!(guard.data(), b"zero-copy");
    }
}
