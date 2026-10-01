// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::time::{Duration, Instant};

use crate::error::{KosError, Result};
use crate::node::{Node, NodeConfig};

pub struct MainTaskConfig {
    pub name: String,
    pub node_config: NodeConfig,
    pub health_check_interval: Duration,
    pub max_thread_failures: u32,
}

impl MainTaskConfig {
    pub fn new(name: &str, node_config: NodeConfig) -> Self {
        Self {
            name: name.to_string(),
            node_config,
            health_check_interval: Duration::from_secs(1),
            max_thread_failures: 3,
        }
    }
}

pub struct MainTask {
    config: MainTaskConfig,
    node: Node,
    failure_counts: std::collections::HashMap<String, u32>,
}

impl MainTask {
    pub fn new(config: MainTaskConfig) -> Self {
        let node = Node::new(&config.name, config.node_config.clone());
        Self {
            config,
            node,
            failure_counts: std::collections::HashMap::new(),
        }
    }

    pub fn node_mut(&mut self) -> &mut Node {
        &mut self.node
    }

    pub fn node(&self) -> &Node {
        &self.node
    }

    pub fn run(&mut self) -> Result<()> {
        self.node.spin()?;

        loop {
            std::thread::sleep(self.config.health_check_interval);
            if !self.node.is_running() {
                break;
            }
            self.handle_failures()?;
        }

        Ok(())
    }

    pub fn run_for(&mut self, duration: Duration) -> Result<()> {
        self.node.spin()?;

        let start = Instant::now();
        while start.elapsed() < duration {
            let check_sleep = std::cmp::min(
                self.config.health_check_interval,
                duration.saturating_sub(start.elapsed()),
            );
            std::thread::sleep(check_sleep);
            self.handle_failures()?;
        }

        self.node.shutdown();
        Ok(())
    }

    fn handle_failures(&mut self) -> Result<()> {
        for name in self.node.check_health() {
            let count = self.failure_counts.entry(name.clone()).or_insert(0);
            *count += 1;
            let count = *count;

            if count >= self.config.max_thread_failures {
                eprintln!("[kos-exec] thread '{name}' failed {count} times; shutting down node");
                self.node.shutdown();
                return Err(KosError::InvalidConfig(format!(
                    "thread '{name}' failed {count} times, node shutting down"
                )));
            }

            eprintln!(
                "[kos-exec] thread '{name}' failed ({count}/{}); restarting",
                self.config.max_thread_failures
            );
            if let Err(e) = self.node.restart_thread(&name) {
                eprintln!("[kos-exec] thread '{name}' restart failed: {e}");
            }
        }
        Ok(())
    }

    pub fn failure_counts(&self) -> &std::collections::HashMap<String, u32> {
        &self.failure_counts
    }

    pub fn shutdown(&mut self) {
        self.node.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::thread_manager::{Priority, ThreadCallbacks, ThreadConfig};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::Arc;

    #[test]
    fn main_task_runs_threads() {
        let counter = Arc::new(AtomicU32::new(0));
        let cc = counter.clone();

        let node_config = NodeConfig::periodic(Duration::from_millis(5));
        let config = MainTaskConfig::new("test_node", node_config);
        let mut main_task = MainTask::new(config);

        let thread_cfg = ThreadConfig::periodic("worker", Duration::from_millis(5));
        main_task.node_mut().create_thread(
            thread_cfg,
            ThreadCallbacks::new(move |_ctx| {
                cc.fetch_add(1, Ordering::Relaxed);
            }),
        ).unwrap();

        main_task.run_for(Duration::from_millis(50)).unwrap();

        assert!(counter.load(Ordering::Relaxed) > 0);
    }

    #[test]
    fn main_task_with_multiple_threads() {
        let fast_count = Arc::new(AtomicU32::new(0));
        let slow_count = Arc::new(AtomicU32::new(0));
        let fc = fast_count.clone();
        let sc = slow_count.clone();

        let node_config = NodeConfig::periodic(Duration::from_millis(5));
        let config = MainTaskConfig::new("multi", node_config);
        let mut main_task = MainTask::new(config);

        let fast_cfg = ThreadConfig::periodic("fast", Duration::from_millis(5))
            .with_priority(Priority::Critical);
        main_task.node_mut().create_thread(
            fast_cfg,
            ThreadCallbacks::new(move |_ctx| {
                fc.fetch_add(1, Ordering::Relaxed);
            }),
        ).unwrap();

        let slow_cfg = ThreadConfig::periodic("slow", Duration::from_millis(20))
            .with_priority(Priority::Low);
        main_task.node_mut().create_thread(
            slow_cfg,
            ThreadCallbacks::new(move |_ctx| {
                sc.fetch_add(1, Ordering::Relaxed);
            }),
        ).unwrap();

        main_task.run_for(Duration::from_millis(100)).unwrap();

        let fast = fast_count.load(Ordering::Relaxed);
        let slow = slow_count.load(Ordering::Relaxed);

        assert!(fast > slow);
        assert!(fast > 0);
        assert!(slow > 0);
    }

    #[test]
    fn main_task_config_defaults() {
        let node_config = NodeConfig::periodic(Duration::from_millis(10));
        let config = MainTaskConfig::new("test", node_config);

        assert_eq!(config.health_check_interval, Duration::from_secs(1));
        assert_eq!(config.max_thread_failures, 3);
    }

    fn quiet_panics() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            let default = std::panic::take_hook();
            std::panic::set_hook(Box::new(move |info| {
                if !info.to_string().contains("intentional") {
                    default(info);
                }
            }));
        });
    }

    fn fast_health(name: &str, max: u32) -> MainTaskConfig {
        let mut c = MainTaskConfig::new(name, NodeConfig::periodic(Duration::from_millis(5)));
        c.health_check_interval = Duration::from_millis(10);
        c.max_thread_failures = max;
        c
    }

    #[test]
    fn panicking_thread_is_restarted() {
        quiet_panics();
        let runs = Arc::new(AtomicU32::new(0));
        let r = runs.clone();
        let mut mt = MainTask::new(fast_health("restart", 3));
        mt.node_mut().create_thread(
            ThreadConfig::periodic("flaky", Duration::from_millis(2)),
            ThreadCallbacks::new(move |_ctx| {
                if r.fetch_add(1, Ordering::Relaxed) < 2 {
                    panic!("intentional test panic");
                }
            }),
        ).unwrap();

        mt.run_for(Duration::from_millis(200)).unwrap();
        assert_eq!(mt.failure_counts().get("flaky"), Some(&2));
        assert!(runs.load(Ordering::Relaxed) > 5, "thread should keep running after restarts");
    }

    #[test]
    fn thread_failing_too_often_shuts_down_node() {
        quiet_panics();
        let mut mt = MainTask::new(fast_health("giveup", 3));
        mt.node_mut().create_thread(
            ThreadConfig::periodic("broken", Duration::from_millis(2)),
            ThreadCallbacks::new(|_ctx| panic!("intentional test panic")),
        ).unwrap();

        let err = mt.run_for(Duration::from_millis(500)).unwrap_err();
        assert!(err.to_string().contains("broken"), "{err}");
        assert_eq!(mt.failure_counts().get("broken"), Some(&3));
    }

    #[test]
    fn init_failure_is_reported_and_restarted() {
        let inits = Arc::new(AtomicU32::new(0));
        let i = inits.clone();
        let runs = Arc::new(AtomicU32::new(0));
        let r = runs.clone();
        let mut mt = MainTask::new(fast_health("initfail", 3));
        mt.node_mut().create_thread(
            ThreadConfig::periodic("init_once_fails", Duration::from_millis(2)),
            ThreadCallbacks::new(move |_ctx| {
                r.fetch_add(1, Ordering::Relaxed);
            })
            .with_init(move |_ctx| {
                if i.fetch_add(1, Ordering::Relaxed) == 0 {
                    Err(KosError::InvalidConfig("intentional init failure".into()))
                } else {
                    Ok(())
                }
            }),
        ).unwrap();

        mt.run_for(Duration::from_millis(200)).unwrap();
        assert_eq!(mt.failure_counts().get("init_once_fails"), Some(&1));
        assert_eq!(inits.load(Ordering::Relaxed), 2);
        assert!(runs.load(Ordering::Relaxed) > 0);
    }
}
