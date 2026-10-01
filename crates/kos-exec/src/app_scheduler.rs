// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;
use std::time::SystemTime;

use crate::error::{KosError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VehicleState {
    #[default]
    Parked,
    Driving,
    Charging,
    Emergency,
}

impl VehicleState {
    pub fn parse(s: &str) -> crate::error::Result<Self> {
        match s.to_ascii_uppercase().as_str() {
            "PARKED" => Ok(Self::Parked),
            "DRIVING" => Ok(Self::Driving),
            "CHARGING" => Ok(Self::Charging),
            "EMERGENCY" => Ok(Self::Emergency),
            other => Err(crate::error::KosError::InvalidConfig(format!(
                "unknown vehicle state '{other}' (PARKED|DRIVING|CHARGING|EMERGENCY)"
            ))),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AppTrigger {
    Signal(String),
    Topic(String),
    VehicleState(VehicleState),
}

#[derive(Debug, Clone)]
pub enum AppSchedule {
    Boot,
    Periodic { period_ms: u32 },
    Event { trigger: AppTrigger },
    Cron { expr: String },
}

pub struct SystemContext {
    pub now: SystemTime,
    pub vehicle_state: VehicleState,
    pub signals: Vec<String>,
    pub topics_received: Vec<String>,
    pub hour: u32,
    pub minute: u32,
}

pub struct AppScheduler {
    schedules: HashMap<String, AppSchedule>,
    boot_consumed: bool,
}

impl AppScheduler {
    pub fn new() -> Self {
        Self {
            schedules: HashMap::new(),
            boot_consumed: false,
        }
    }

    pub fn register(&mut self, app_id: &str, schedule: AppSchedule) -> Result<()> {
        if self.schedules.contains_key(app_id) {
            return Err(KosError::AlreadyExists(format!("schedule {app_id}")));
        }
        self.schedules.insert(app_id.to_string(), schedule);
        Ok(())
    }

    pub fn unregister(&mut self, app_id: &str) -> Result<()> {
        self.schedules
            .remove(app_id)
            .map(|_| ())
            .ok_or_else(|| KosError::NotFound(format!("schedule {app_id}")))
    }

    pub fn tick(&mut self, ctx: &SystemContext) -> Vec<String> {
        let mut ready = Vec::new();

        for (app_id, schedule) in &self.schedules {
            match schedule {
                AppSchedule::Boot => {
                    if !self.boot_consumed {
                        ready.push(app_id.clone());
                    }
                }
                AppSchedule::Periodic { .. } => {
                    if !self.boot_consumed {
                        ready.push(app_id.clone());
                    }
                }
                AppSchedule::Event { trigger } => {
                    if event_matches(trigger, ctx) {
                        ready.push(app_id.clone());
                    }
                }
                AppSchedule::Cron { expr } => {
                    if cron_matches(expr, ctx) {
                        ready.push(app_id.clone());
                    }
                }
            }
        }

        self.boot_consumed = true;
        ready.sort();
        ready
    }
}

impl Default for AppScheduler {
    fn default() -> Self {
        Self::new()
    }
}

fn event_matches(trigger: &AppTrigger, ctx: &SystemContext) -> bool {
    match trigger {
        AppTrigger::Signal(name) => ctx.signals.contains(name),
        AppTrigger::Topic(topic) => ctx.topics_received.contains(topic),
        AppTrigger::VehicleState(state) => ctx.vehicle_state == *state,
    }
}

fn cron_matches(expr: &str, ctx: &SystemContext) -> bool {
    cron_matches_hm(expr, ctx.hour, ctx.minute)
}

pub(crate) fn cron_matches_hm(expr: &str, hour: u32, minute: u32) -> bool {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() < 2 {
        return false;
    }

    let min_match = parts[0] == "*" || parts[0].parse::<u32>().ok() == Some(minute);
    let hour_match = parts[1] == "*" || parts[1].parse::<u32>().ok() == Some(hour);

    min_match && hour_match
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_ctx() -> SystemContext {
        SystemContext {
            now: SystemTime::now(),
            vehicle_state: VehicleState::Parked,
            signals: vec![],
            topics_received: vec![],
            hour: 12,
            minute: 0,
        }
    }

    #[test]
    fn boot_schedule_returned_immediately() {
        let mut sched = AppScheduler::new();
        sched
            .register("boot-app", AppSchedule::Boot)
            .unwrap();

        let ctx = base_ctx();
        let ready = sched.tick(&ctx);
        assert_eq!(ready, vec!["boot-app"]);

        let ready = sched.tick(&ctx);
        assert!(ready.is_empty());
    }

    #[test]
    fn periodic_returned_on_first_tick_only() {
        let mut sched = AppScheduler::new();
        sched
            .register("sensor", AppSchedule::Periodic { period_ms: 10 })
            .unwrap();

        let ctx = base_ctx();
        let ready = sched.tick(&ctx);
        assert_eq!(ready, vec!["sensor"]);

        let ready = sched.tick(&ctx);
        assert!(ready.is_empty());
    }

    #[test]
    fn event_signal_trigger() {
        let mut sched = AppScheduler::new();
        sched
            .register(
                "media",
                AppSchedule::Event {
                    trigger: AppTrigger::Signal("media_play".into()),
                },
            )
            .unwrap();

        let ctx = base_ctx();
        assert!(sched.tick(&ctx).is_empty());

        let mut ctx2 = base_ctx();
        ctx2.signals = vec!["media_play".into()];
        assert_eq!(sched.tick(&ctx2), vec!["media"]);
    }

    #[test]
    fn event_vehicle_state_trigger() {
        let mut sched = AppScheduler::new();
        sched
            .register(
                "charger",
                AppSchedule::Event {
                    trigger: AppTrigger::VehicleState(VehicleState::Charging),
                },
            )
            .unwrap();

        let ctx = base_ctx();
        assert!(sched.tick(&ctx).is_empty());

        let mut ctx2 = base_ctx();
        ctx2.vehicle_state = VehicleState::Charging;
        assert_eq!(sched.tick(&ctx2), vec!["charger"]);
    }

    #[test]
    fn cron_schedule_matches_at_correct_time() {
        let mut sched = AppScheduler::new();
        sched
            .register(
                "backup",
                AppSchedule::Cron {
                    expr: "0 3 * * *".into(),
                },
            )
            .unwrap();

        let ctx = base_ctx();
        sched.boot_consumed = true;
        assert!(sched.tick(&ctx).is_empty());

        let mut ctx2 = base_ctx();
        ctx2.hour = 3;
        ctx2.minute = 0;
        assert_eq!(sched.tick(&ctx2), vec!["backup"]);
    }

    #[test]
    fn unregister_removes_from_tick() {
        let mut sched = AppScheduler::new();
        sched
            .register(
                "media",
                AppSchedule::Event {
                    trigger: AppTrigger::Signal("play".into()),
                },
            )
            .unwrap();

        sched.unregister("media").unwrap();

        let mut ctx = base_ctx();
        ctx.signals = vec!["play".into()];
        assert!(sched.tick(&ctx).is_empty());
    }
}
