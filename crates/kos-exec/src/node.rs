// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::time::Duration;

use crate::error::{KosError, Result};
use crate::thread_context::ThreadContext;
use crate::thread_manager::{Priority, ThreadCallbacks, ThreadConfig, ThreadManager, Trigger};

#[derive(Debug, Clone)]
pub struct NodeConfig {
    pub trigger: Trigger,
    pub priority: Priority,
    pub cpu_affinity: Option<usize>,
    pub subs: Vec<String>,
    pub pubs: Vec<String>,
}

impl NodeConfig {
    pub fn periodic(period: Duration) -> Self {
        Self {
            trigger: Trigger::Periodic(period),
            priority: Priority::Normal,
            cpu_affinity: None,
            subs: Vec::new(),
            pubs: Vec::new(),
        }
    }

    pub fn event(event: &str) -> Self {
        Self {
            trigger: Trigger::Event(event.to_string()),
            priority: Priority::Normal,
            cpu_affinity: None,
            subs: Vec::new(),
            pubs: Vec::new(),
        }
    }

    pub fn with_priority(mut self, priority: Priority) -> Self {
        self.priority = priority;
        self
    }

    pub fn with_cpu_affinity(mut self, core: usize) -> Self {
        self.cpu_affinity = Some(core);
        self
    }

    pub fn with_subs(mut self, subs: Vec<&str>) -> Self {
        self.subs = subs.into_iter().map(|s| s.to_string()).collect();
        self
    }

    pub fn with_pubs(mut self, pubs: Vec<&str>) -> Self {
        self.pubs = pubs.into_iter().map(|s| s.to_string()).collect();
        self
    }
}

pub struct Node {
    name: String,
    config: NodeConfig,
    manager: ThreadManager,
    on_init: Option<Box<dyn FnOnce(&mut Node) + Send + 'static>>,
    on_shutdown: Option<Box<dyn FnOnce() + Send + 'static>>,
    main_task: Option<Box<dyn Fn(&ThreadContext) + Send + Sync + 'static>>,
    running: bool,
}

impl Node {
    pub fn new(name: &str, config: NodeConfig) -> Self {
        Self {
            name: name.to_string(),
            config,
            manager: ThreadManager::new(),
            on_init: None,
            on_shutdown: None,
            main_task: None,
            running: false,
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn set_on_init<F>(&mut self, f: F)
    where
        F: FnOnce(&mut Node) + Send + 'static,
    {
        self.on_init = Some(Box::new(f));
    }

    pub fn set_on_shutdown<F>(&mut self, f: F)
    where
        F: FnOnce() + Send + 'static,
    {
        self.on_shutdown = Some(Box::new(f));
    }

    pub fn on_run<F>(&mut self, f: F)
    where
        F: Fn(&ThreadContext) + Send + Sync + 'static,
    {
        self.main_task = Some(Box::new(f));
    }

    pub fn set_transport(&mut self, transport: std::sync::Arc<dyn crate::comm::Transport>) {
        self.manager.set_transport(transport);
    }

    pub fn create_thread(
        &mut self,
        config: ThreadConfig,
        callbacks: ThreadCallbacks,
    ) -> Result<()> {
        self.manager.register(config, callbacks)
    }

    pub fn thread_count(&self) -> usize {
        self.manager.thread_count()
    }

    pub fn spin(&mut self) -> Result<()> {
        if let Some(init) = self.on_init.take() {
            init(self);
        }

        if let Some(task) = self.main_task.take() {
            let thread_config = ThreadConfig {
                name: format!("{}_main", self.name),
                trigger: self.config.trigger.clone(),
                priority: self.config.priority,
                cpu_affinity: self.config.cpu_affinity,
                subs: self.config.subs.clone(),
                pubs: self.config.pubs.clone(),
                threshold: None,
            };
            let callbacks = ThreadCallbacks::new(task);
            self.manager.register(thread_config, callbacks)?;
        }

        if self.manager.thread_count() == 0 {
            return Err(KosError::InvalidConfig(
                "node has no threads or on_run callback".into(),
            ));
        }

        self.running = true;
        self.manager.start_all()?;

        Ok(())
    }

    pub fn spin_for(&mut self, duration: Duration) -> Result<()> {
        self.spin()?;
        std::thread::sleep(duration);
        self.shutdown();
        Ok(())
    }

    pub fn check_health(&mut self) -> Vec<String> {
        self.manager.check_health()
    }

    pub fn restart_thread(&mut self, name: &str) -> Result<()> {
        self.manager.restart_thread(name)
    }

    pub fn shutdown(&mut self) {
        self.running = false;
        self.manager.shutdown_all();

        if let Some(shutdown) = self.on_shutdown.take() {
            shutdown();
        }
    }

    pub fn is_running(&self) -> bool {
        self.running
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn simple_node_on_run() {
        let counter = Arc::new(AtomicU32::new(0));
        let counter_clone = counter.clone();

        let config = NodeConfig::periodic(Duration::from_millis(5))
            .with_subs(vec!["input"])
            .with_pubs(vec!["output"]);

        let mut node = Node::new("test_sensor", config);
        node.on_run(move |_ctx| {
            counter_clone.fetch_add(1, Ordering::Relaxed);
        });

        node.spin_for(Duration::from_millis(30)).unwrap();

        assert!(counter.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn multi_thread_node() {
        let fusion_count = Arc::new(AtomicU32::new(0));
        let diag_count = Arc::new(AtomicU32::new(0));
        let fc = fusion_count.clone();
        let dc = diag_count.clone();

        let config = NodeConfig::periodic(Duration::from_millis(10));
        let mut node = Node::new("multi", config);

        let fusion_cfg = ThreadConfig::periodic("fusion", Duration::from_millis(5))
            .with_priority(Priority::Critical)
            .with_subs(vec!["lidar", "radar"])
            .with_pubs(vec!["fused"]);
        node.create_thread(fusion_cfg, ThreadCallbacks::new(move |_ctx| {
            fc.fetch_add(1, Ordering::Relaxed);
        })).unwrap();

        let diag_cfg = ThreadConfig::periodic("diag", Duration::from_millis(10))
            .with_priority(Priority::Low);
        node.create_thread(diag_cfg, ThreadCallbacks::new(move |_ctx| {
            dc.fetch_add(1, Ordering::Relaxed);
        })).unwrap();

        node.spin_for(Duration::from_millis(50)).unwrap();

        assert!(fusion_count.load(Ordering::Relaxed) > 0);
        assert!(diag_count.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn node_with_init_callback() {
        let init_called = Arc::new(AtomicU32::new(0));
        let ic = init_called.clone();

        let config = NodeConfig::periodic(Duration::from_millis(5));
        let mut node = Node::new("init_test", config);

        node.set_on_init(move |node| {
            ic.fetch_add(1, Ordering::Relaxed);
            let cfg = ThreadConfig::periodic("worker", Duration::from_millis(5));
            node.create_thread(cfg, ThreadCallbacks::new(|_| {})).unwrap();
        });

        node.spin_for(Duration::from_millis(20)).unwrap();

        assert_eq!(init_called.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn node_with_shutdown_callback() {
        let shutdown_called = Arc::new(AtomicU32::new(0));
        let sc = shutdown_called.clone();

        let config = NodeConfig::periodic(Duration::from_millis(5));
        let mut node = Node::new("shutdown_test", config);
        node.on_run(|_ctx| {});
        node.set_on_shutdown(move || {
            sc.fetch_add(1, Ordering::Relaxed);
        });

        node.spin_for(Duration::from_millis(20)).unwrap();

        assert_eq!(shutdown_called.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn node_no_threads_error() {
        let config = NodeConfig::periodic(Duration::from_millis(10));
        let mut node = Node::new("empty", config);

        let err = node.spin().unwrap_err();
        assert!(matches!(err, KosError::InvalidConfig(_)));
    }

    #[test]
    fn node_config_builders() {
        let config = NodeConfig::periodic(Duration::from_millis(20))
            .with_priority(Priority::High)
            .with_cpu_affinity(2)
            .with_subs(vec!["a", "b"])
            .with_pubs(vec!["c"]);

        assert_eq!(config.trigger, Trigger::Periodic(Duration::from_millis(20)));
        assert_eq!(config.priority, Priority::High);
        assert_eq!(config.cpu_affinity, Some(2));
        assert_eq!(config.subs, vec!["a", "b"]);
        assert_eq!(config.pubs, vec!["c"]);
    }

    #[test]
    fn node_event_config() {
        let config = NodeConfig::event("data_ready");
        assert_eq!(
            config.trigger,
            Trigger::Event("data_ready".to_string())
        );
    }
}
