// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::{Deserialize, Serialize};

use crate::config::{AppConfig, ThreadConfigToml};

pub const ENV_APP_ID: &str = "KOS_APP_ID";
pub const ENV_DOMAIN: &str = "KOS_DOMAIN";
pub const ENV_PARAM_PREFIX: &str = "KOS_PARAM_";
pub const ENV_THREADS: &str = "KOS_APP_THREADS";

#[derive(Serialize, Deserialize, Default)]
struct ThreadList {
    #[serde(default)]
    thread: Vec<ThreadConfigToml>,
}

#[derive(Debug, Clone, Default)]
pub struct LaunchEnv {
    pub app_id: Option<String>,
    pub domain: Option<String>,
    pub params: HashMap<String, String>,
    pub threads: Vec<ThreadConfigToml>,
}

impl LaunchEnv {
    pub fn from_env() -> Self {
        Self::from_vars(std::env::vars())
    }

    pub fn current() -> &'static LaunchEnv {
        static ENV: OnceLock<LaunchEnv> = OnceLock::new();
        ENV.get_or_init(Self::from_env)
    }

    pub fn from_vars(vars: impl IntoIterator<Item = (String, String)>) -> Self {
        let mut env = LaunchEnv::default();
        for (k, v) in vars {
            if k == ENV_APP_ID {
                env.app_id = Some(v);
            } else if k == ENV_DOMAIN {
                env.domain = Some(v);
            } else if k == ENV_THREADS {
                match toml::from_str::<ThreadList>(&v) {
                    Ok(list) => env.threads = list.thread,
                    Err(e) => eprintln!("[kos-exec] WARNING: invalid {ENV_THREADS}: {e}"),
                }
            } else if let Some(key) = k.strip_prefix(ENV_PARAM_PREFIX) {
                env.params.insert(key.to_string(), v);
            }
        }
        env
    }

    pub fn vars_for(config: &AppConfig) -> Vec<(String, String)> {
        let mut vars = vec![
            (ENV_APP_ID.to_string(), config.id.clone()),
            (ENV_DOMAIN.to_string(), config.domain.clone()),
        ];
        for (k, v) in &config.params {
            if k.is_empty() || k.contains('=') || k.contains('\0') || v.contains('\0') {
                eprintln!("[kos-exec] WARNING: app '{}': skipping param '{k}' (invalid for env)", config.id);
                continue;
            }
            vars.push((format!("{ENV_PARAM_PREFIX}{k}"), v.clone()));
        }
        if !config.threads.is_empty() {
            let list = ThreadList { thread: config.threads.clone() };
            match toml::to_string(&list) {
                Ok(text) => vars.push((ENV_THREADS.to_string(), text)),
                Err(e) => eprintln!("[kos-exec] WARNING: app '{}': cannot encode threads: {e}", config.id),
            }
        }
        vars
    }

    pub fn thread(&self, name: &str) -> Option<&ThreadConfigToml> {
        self.threads.iter().find(|t| t.name == name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{RestartPolicy, ScheduleConfig};

    fn app() -> AppConfig {
        AppConfig {
            id: "adas.fusion".into(),
            binary: "fusion".into(),
            args: vec![],
            domain: "adas".into(),
            depends_on: vec![],
            restart: RestartPolicy::default(),
            schedule: ScheduleConfig::default(),
            priority: "normal".into(),
            params: HashMap::from([
                ("gain".to_string(), "0.5".to_string()),
                ("mode".to_string(), "fast lane".to_string()),
            ]),
            threads: vec![ThreadConfigToml {
                name: "control".into(),
                trigger: "periodic".into(),
                period_ms: Some(5),
                event_topic: None,
                priority: "critical".into(),
                cpu_affinity: Some(1),
                subs: vec!["adas/obj".into()],
                pubs: vec!["adas/cmd".into()],
            }],
        }
    }

    #[test]
    fn roundtrip_through_env_vars() {
        let vars = LaunchEnv::vars_for(&app());
        let env = LaunchEnv::from_vars(vars);
        assert_eq!(env.app_id.as_deref(), Some("adas.fusion"));
        assert_eq!(env.domain.as_deref(), Some("adas"));
        assert_eq!(env.params.get("gain").map(String::as_str), Some("0.5"));
        assert_eq!(env.params.get("mode").map(String::as_str), Some("fast lane"));
        let t = env.thread("control").expect("thread");
        assert_eq!(t.period_ms, Some(5));
        assert_eq!(t.priority, "critical");
        assert_eq!(t.cpu_affinity, Some(1));
        assert_eq!(t.subs, vec!["adas/obj"]);
    }

    #[test]
    fn unrelated_vars_are_ignored() {
        let env = LaunchEnv::from_vars([("PATH".to_string(), "/bin".to_string())]);
        assert!(env.app_id.is_none() && env.params.is_empty() && env.threads.is_empty());
    }
}
