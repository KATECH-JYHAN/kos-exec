// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStrategy {
    Cold,
    Warm,
}

#[derive(Debug, Clone)]
pub struct RestartPolicy {
    pub strategy: RestartStrategy,
    pub max_retries: u32,
    pub backoff_ms: u64,
    pub backoff_max_ms: u64,
    pub watchdog_ms: Option<u64>,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            strategy: RestartStrategy::Cold,
            max_retries: 3,
            backoff_ms: 100,
            backoff_max_ms: 5000,
            watchdog_ms: None,
        }
    }
}

impl RestartPolicy {
    pub fn backoff(&self, restarts: u32) -> std::time::Duration {
        let ms = self
            .backoff_ms
            .saturating_mul(1u64 << restarts.min(32))
            .min(self.backoff_max_ms.max(self.backoff_ms));
        std::time::Duration::from_millis(ms)
    }
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ThreadConfigToml {
    pub name: String,
    #[serde(default = "default_thread_trigger")]
    pub trigger: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub period_ms: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event_topic: Option<String>,
    #[serde(default = "default_thread_priority")]
    pub priority: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_affinity: Option<u32>,
    #[serde(default)]
    pub subs: Vec<String>,
    #[serde(default)]
    pub pubs: Vec<String>,
}

fn default_thread_trigger() -> String {
    "periodic".into()
}

fn default_thread_priority() -> String {
    "normal".into()
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub id: String,
    pub binary: String,
    pub args: Vec<String>,
    pub domain: String,
    pub depends_on: Vec<String>,
    pub restart: RestartPolicy,
    pub schedule: ScheduleConfig,
    pub priority: String,
    pub params: HashMap<String, String>,
    pub threads: Vec<ThreadConfigToml>,
}

#[derive(Debug, Clone)]
pub enum ScheduleConfig {
    Boot,
    Periodic { period_ms: u32 },
    Event { trigger: String },
    Cron { expr: String },
}

impl Default for ScheduleConfig {
    fn default() -> Self {
        Self::Boot
    }
}
