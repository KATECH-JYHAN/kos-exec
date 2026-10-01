// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;
use std::time::Instant;

use crate::error::{KosError, Result};

#[derive(Debug, Clone)]
pub struct HealthConfig {
    pub heartbeat_interval_ms: u32,
    pub deadline_ms: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthStatus {
    Healthy,
    Unhealthy,
    Recovering,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthEvent {
    HeartbeatTimeout(String),
    DeadlineTimeout(String),
    Recovered(String),
}

struct HealthState {
    heartbeat_interval_ms: u32,
    deadline_ms: Option<u32>,
    last_heartbeat: Instant,
    deadline_started: Option<Instant>,
    status: HealthStatus,
}

pub struct HealthMonitor {
    checks: HashMap<String, HealthState>,
}

impl HealthMonitor {
    pub fn new() -> Self {
        Self {
            checks: HashMap::new(),
        }
    }

    pub fn register(&mut self, app_id: &str, config: &HealthConfig) -> Result<()> {
        if self.checks.contains_key(app_id) {
            return Err(KosError::AlreadyExists(format!("health {app_id}")));
        }
        self.checks.insert(
            app_id.to_string(),
            HealthState {
                heartbeat_interval_ms: config.heartbeat_interval_ms,
                deadline_ms: config.deadline_ms,
                last_heartbeat: Instant::now(),
                deadline_started: None,
                status: HealthStatus::Healthy,
            },
        );
        Ok(())
    }

    pub fn unregister(&mut self, app_id: &str) {
        self.checks.remove(app_id);
    }

    pub fn is_registered(&self, app_id: &str) -> bool {
        self.checks.contains_key(app_id)
    }

    pub fn heartbeat(&mut self, app_id: &str) {
        if let Some(state) = self.checks.get_mut(app_id) {
            state.last_heartbeat = Instant::now();
        }
    }

    pub fn deadline_start(&mut self, app_id: &str) {
        if let Some(state) = self.checks.get_mut(app_id) {
            state.deadline_started = Some(Instant::now());
        }
    }

    pub fn deadline_reset(&mut self, app_id: &str) {
        if let Some(state) = self.checks.get_mut(app_id) {
            state.deadline_started = None;
        }
    }

    pub fn status(&self, app_id: &str) -> Result<HealthStatus> {
        self.checks
            .get(app_id)
            .map(|s| s.status)
            .ok_or_else(|| KosError::NotFound(format!("health {app_id}")))
    }

    pub fn tick(&mut self) -> Vec<HealthEvent> {
        let now = Instant::now();
        let mut events = Vec::new();

        for (app_id, state) in &mut self.checks {
            let elapsed_ms = now.duration_since(state.last_heartbeat).as_millis() as u32;
            let hb_timeout = elapsed_ms > state.heartbeat_interval_ms * 2;

            let mut deadline_fired = false;
            if let (Some(deadline_ms), Some(started)) = (state.deadline_ms, state.deadline_started)
            {
                let deadline_elapsed = now.duration_since(started).as_millis() as u32;
                if deadline_elapsed > deadline_ms {
                    events.push(HealthEvent::DeadlineTimeout(app_id.clone()));
                    state.deadline_started = None;
                    state.status = HealthStatus::Unhealthy;
                    deadline_fired = true;
                }
            }

            if !deadline_fired {
                match state.status {
                    HealthStatus::Healthy | HealthStatus::Recovering => {
                        if hb_timeout {
                            events.push(HealthEvent::HeartbeatTimeout(app_id.clone()));
                            state.status = HealthStatus::Unhealthy;
                        }
                    }
                    HealthStatus::Unhealthy => {
                        if !hb_timeout {
                            events.push(HealthEvent::Recovered(app_id.clone()));
                            state.status = HealthStatus::Recovering;
                        }
                    }
                }
            }
        }

        events
    }

    #[cfg(test)]
    fn tick_at(&mut self, now: Instant) -> Vec<HealthEvent> {
        let mut events = Vec::new();

        for (app_id, state) in &mut self.checks {
            let elapsed_ms = now.duration_since(state.last_heartbeat).as_millis() as u32;
            let hb_timeout = elapsed_ms > state.heartbeat_interval_ms * 2;

            let mut deadline_fired = false;
            if let (Some(deadline_ms), Some(started)) = (state.deadline_ms, state.deadline_started)
            {
                let deadline_elapsed = now.duration_since(started).as_millis() as u32;
                if deadline_elapsed > deadline_ms {
                    events.push(HealthEvent::DeadlineTimeout(app_id.clone()));
                    state.deadline_started = None;
                    state.status = HealthStatus::Unhealthy;
                    deadline_fired = true;
                }
            }

            if !deadline_fired {
                match state.status {
                    HealthStatus::Healthy | HealthStatus::Recovering => {
                        if hb_timeout {
                            events.push(HealthEvent::HeartbeatTimeout(app_id.clone()));
                            state.status = HealthStatus::Unhealthy;
                        }
                    }
                    HealthStatus::Unhealthy => {
                        if !hb_timeout {
                            events.push(HealthEvent::Recovered(app_id.clone()));
                            state.status = HealthStatus::Recovering;
                        }
                    }
                }
            }
        }

        events
    }

    #[cfg(test)]
    fn set_last_heartbeat(&mut self, app_id: &str, at: Instant) {
        if let Some(state) = self.checks.get_mut(app_id) {
            state.last_heartbeat = at;
        }
    }

    #[cfg(test)]
    fn set_deadline_started(&mut self, app_id: &str, at: Instant) {
        if let Some(state) = self.checks.get_mut(app_id) {
            state.deadline_started = Some(at);
        }
    }
}

impl Default for HealthMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn heartbeat_normal_stays_healthy() {
        let mut mon = HealthMonitor::new();
        mon.register(
            "app-1",
            &HealthConfig {
                heartbeat_interval_ms: 500,
                deadline_ms: None,
            },
        )
        .unwrap();

        let now = Instant::now();
        mon.set_last_heartbeat("app-1", now);

        let future = now + Duration::from_millis(100);
        let events = mon.tick_at(future);
        assert!(events.is_empty());
        assert_eq!(mon.status("app-1").unwrap(), HealthStatus::Healthy);
    }

    #[test]
    fn heartbeat_timeout_emits_event() {
        let mut mon = HealthMonitor::new();
        mon.register(
            "app-1",
            &HealthConfig {
                heartbeat_interval_ms: 500,
                deadline_ms: None,
            },
        )
        .unwrap();

        let now = Instant::now();
        mon.set_last_heartbeat("app-1", now);

        let future = now + Duration::from_millis(1100);
        let events = mon.tick_at(future);
        assert_eq!(events, vec![HealthEvent::HeartbeatTimeout("app-1".into())]);
        assert_eq!(mon.status("app-1").unwrap(), HealthStatus::Unhealthy);
    }

    #[test]
    fn deadline_within_no_event() {
        let mut mon = HealthMonitor::new();
        mon.register(
            "app-1",
            &HealthConfig {
                heartbeat_interval_ms: 500,
                deadline_ms: Some(20),
            },
        )
        .unwrap();

        let now = Instant::now();
        mon.set_last_heartbeat("app-1", now);
        mon.set_deadline_started("app-1", now);

        let future = now + Duration::from_millis(10);
        let events = mon.tick_at(future);
        assert!(events.is_empty());
    }

    #[test]
    fn deadline_exceeded_emits_event() {
        let mut mon = HealthMonitor::new();
        mon.register(
            "app-1",
            &HealthConfig {
                heartbeat_interval_ms: 500,
                deadline_ms: Some(20),
            },
        )
        .unwrap();

        let now = Instant::now();
        mon.set_last_heartbeat("app-1", now);
        mon.set_deadline_started("app-1", now);

        let future = now + Duration::from_millis(25);
        let events = mon.tick_at(future);
        assert_eq!(events, vec![HealthEvent::DeadlineTimeout("app-1".into())]);
        assert_eq!(mon.status("app-1").unwrap(), HealthStatus::Unhealthy);
    }

    #[test]
    fn recovery_after_heartbeat_resumes() {
        let mut mon = HealthMonitor::new();
        mon.register(
            "app-1",
            &HealthConfig {
                heartbeat_interval_ms: 500,
                deadline_ms: None,
            },
        )
        .unwrap();

        let now = Instant::now();
        mon.set_last_heartbeat("app-1", now);

        let timeout = now + Duration::from_millis(1100);
        mon.tick_at(timeout);
        assert_eq!(mon.status("app-1").unwrap(), HealthStatus::Unhealthy);

        let resume = now + Duration::from_millis(1200);
        mon.set_last_heartbeat("app-1", resume);

        let check = resume + Duration::from_millis(100);
        let events = mon.tick_at(check);
        assert_eq!(events, vec![HealthEvent::Recovered("app-1".into())]);
        assert_eq!(mon.status("app-1").unwrap(), HealthStatus::Recovering);
    }

    #[test]
    fn unregistered_app_heartbeat_ignored() {
        let mut mon = HealthMonitor::new();
        mon.heartbeat("ghost");
        mon.deadline_start("ghost");
        mon.deadline_reset("ghost");
        assert!(matches!(
            mon.status("ghost"),
            Err(KosError::NotFound(_))
        ));
    }
}
