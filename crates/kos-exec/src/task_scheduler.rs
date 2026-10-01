// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use crate::app_scheduler::AppTrigger;
use crate::error::{KosError, Result};

pub type TaskPriority = kos_safety::Priority;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverrunPolicy {
    Skip,
    Queue,
    Abort,
}

struct PeriodicEntry {
    app_id: String,
    period_ns: u64,
    next_run_ns: u64,
    priority: TaskPriority,
    overrun: OverrunPolicy,
}

struct EventEntry {
    app_id: String,
    #[allow(dead_code)]
    trigger: AppTrigger,
    #[allow(dead_code)]
    priority: TaskPriority,
}

pub struct TaskScheduler {
    periodic: Vec<PeriodicEntry>,
    events: Vec<EventEntry>,
}

impl TaskScheduler {
    pub fn new() -> Self {
        Self {
            periodic: Vec::new(),
            events: Vec::new(),
        }
    }

    pub fn register_periodic(
        &mut self,
        app_id: &str,
        period_ms: u32,
        priority: TaskPriority,
        overrun: OverrunPolicy,
    ) -> Result<()> {
        if self.find_any(app_id) {
            return Err(KosError::AlreadyExists(format!("task {app_id}")));
        }
        let period_ns = u64::from(period_ms) * 1_000_000;
        self.periodic.push(PeriodicEntry {
            app_id: app_id.to_string(),
            period_ns,
            next_run_ns: 0,
            priority,
            overrun,
        });
        Ok(())
    }

    pub fn register_event(
        &mut self,
        app_id: &str,
        trigger: AppTrigger,
        priority: TaskPriority,
    ) -> Result<()> {
        if self.find_any(app_id) {
            return Err(KosError::AlreadyExists(format!("task {app_id}")));
        }
        self.events.push(EventEntry {
            app_id: app_id.to_string(),
            trigger,
            priority,
        });
        Ok(())
    }

    pub fn unregister(&mut self, app_id: &str) -> Result<()> {
        let before = self.periodic.len() + self.events.len();
        self.periodic.retain(|e| e.app_id != app_id);
        self.events.retain(|e| e.app_id != app_id);
        let after = self.periodic.len() + self.events.len();

        if before == after {
            return Err(KosError::NotFound(format!("task {app_id}")));
        }
        Ok(())
    }

    pub fn tick(&mut self, now_ns: u64) -> Vec<String> {
        let mut ready: Vec<(TaskPriority, String)> = Vec::new();

        for entry in &mut self.periodic {
            if now_ns < entry.next_run_ns {
                continue;
            }

            match entry.overrun {
                OverrunPolicy::Skip => {
                    ready.push((entry.priority, entry.app_id.clone()));
                    if entry.period_ns > 0 {
                        let missed = (now_ns - entry.next_run_ns) / entry.period_ns;
                        entry.next_run_ns += (missed + 1) * entry.period_ns;
                    }
                }
                OverrunPolicy::Queue => {
                    while entry.next_run_ns <= now_ns {
                        ready.push((entry.priority, entry.app_id.clone()));
                        entry.next_run_ns += entry.period_ns;
                    }
                }
                OverrunPolicy::Abort => {
                    ready.push((entry.priority, entry.app_id.clone()));
                    if entry.period_ns > 0 {
                        let missed = (now_ns - entry.next_run_ns) / entry.period_ns;
                        entry.next_run_ns += (missed + 1) * entry.period_ns;
                    }
                }
            }
        }

        ready.sort_by_key(|(prio, _)| *prio);
        ready.into_iter().map(|(_, id)| id).collect()
    }

    fn find_any(&self, app_id: &str) -> bool {
        self.periodic.iter().any(|e| e.app_id == app_id)
            || self.events.iter().any(|e| e.app_id == app_id)
    }
}

impl Default for TaskScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn periodic_tick_correct_timing() {
        let mut sched = TaskScheduler::new();
        sched
            .register_periodic("app-1", 10, TaskPriority::Normal, OverrunPolicy::Skip)
            .unwrap();

        let ready = sched.tick(0);
        assert_eq!(ready, vec!["app-1"]);

        let ready = sched.tick(5_000_000);
        assert!(ready.is_empty());

        let ready = sched.tick(10_000_000);
        assert_eq!(ready, vec!["app-1"]);
    }

    #[test]
    fn priority_ordering_critical_before_low() {
        let mut sched = TaskScheduler::new();
        sched
            .register_periodic("low-app", 10, TaskPriority::Low, OverrunPolicy::Skip)
            .unwrap();
        sched
            .register_periodic("crit-app", 10, TaskPriority::Critical, OverrunPolicy::Skip)
            .unwrap();
        sched
            .register_periodic("high-app", 10, TaskPriority::High, OverrunPolicy::Skip)
            .unwrap();

        let ready = sched.tick(0);
        assert_eq!(ready, vec!["crit-app", "high-app", "low-app"]);
    }

    #[test]
    fn overrun_skip_ignores_missed_periods() {
        let mut sched = TaskScheduler::new();
        sched
            .register_periodic("app-1", 10, TaskPriority::Normal, OverrunPolicy::Skip)
            .unwrap();

        sched.tick(0);

        let ready = sched.tick(35_000_000);
        assert_eq!(ready.len(), 1);
        assert_eq!(ready[0], "app-1");

        assert!(sched.tick(39_000_000).is_empty());
        assert_eq!(sched.tick(40_000_000), vec!["app-1"]);
    }

    #[test]
    fn overrun_queue_executes_all_missed() {
        let mut sched = TaskScheduler::new();
        sched
            .register_periodic("app-1", 10, TaskPriority::Normal, OverrunPolicy::Queue)
            .unwrap();

        sched.tick(0);

        let ready = sched.tick(35_000_000);
        assert_eq!(ready.len(), 3);
        assert!(ready.iter().all(|id| id == "app-1"));
    }

    #[test]
    fn unregister_removes_from_tick() {
        let mut sched = TaskScheduler::new();
        sched
            .register_periodic("app-1", 10, TaskPriority::Normal, OverrunPolicy::Skip)
            .unwrap();

        assert_eq!(sched.tick(0), vec!["app-1"]);

        sched.unregister("app-1").unwrap();
        assert!(sched.tick(10_000_000).is_empty());
    }

    #[test]
    fn duplicate_register_rejected() {
        let mut sched = TaskScheduler::new();
        sched
            .register_periodic("app-1", 10, TaskPriority::Normal, OverrunPolicy::Skip)
            .unwrap();
        let err = sched
            .register_periodic("app-1", 20, TaskPriority::High, OverrunPolicy::Skip)
            .unwrap_err();
        assert!(matches!(err, KosError::AlreadyExists(_)));
    }
}
