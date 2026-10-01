// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;
use std::str::FromStr;

pub struct AppContext {
    app_id: String,
    domain: String,
    params: HashMap<String, String>,
    log_buf: std::collections::VecDeque<String>,
}

const LOG_KEEP: usize = 256;

impl AppContext {
    pub fn new(app_id: &str, domain: &str, params: HashMap<String, String>) -> Self {
        Self {
            app_id: app_id.to_string(),
            domain: domain.to_string(),
            params,
            log_buf: std::collections::VecDeque::new(),
        }
    }

    pub fn app_id(&self) -> &str {
        &self.app_id
    }

    pub fn domain(&self) -> &str {
        &self.domain
    }

    pub fn params(&self) -> &HashMap<String, String> {
        &self.params
    }

    pub fn param<T: FromStr>(&self, key: &str) -> Option<T> {
        self.params.get(key).and_then(|v| v.parse().ok())
    }

    pub fn log_info(&mut self, msg: &str) {
        self.log("INFO", msg);
    }

    pub fn log_warn(&mut self, msg: &str) {
        self.log("WARN", msg);
    }

    pub fn log_error(&mut self, msg: &str) {
        self.log("ERROR", msg);
    }

    fn log(&mut self, level: &str, msg: &str) {
        let line = format!("[{level}] {}: {msg}", self.app_id);
        eprintln!("{line}");
        if self.log_buf.len() == LOG_KEEP {
            self.log_buf.pop_front();
        }
        self.log_buf.push_back(line);
    }

    pub fn drain_logs(&mut self) -> Vec<String> {
        self.log_buf.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_parsing() {
        let mut params = HashMap::new();
        params.insert("gain".into(), "1.5".into());
        params.insert("count".into(), "42".into());
        params.insert("name".into(), "lka".into());

        let ctx = AppContext::new("app-1", "adas", params);

        assert_eq!(ctx.param::<f64>("gain"), Some(1.5));
        assert_eq!(ctx.param::<u32>("count"), Some(42));
        assert_eq!(ctx.param::<String>("name"), Some("lka".into()));
        assert_eq!(ctx.param::<f64>("missing"), None);
    }

    #[test]
    fn identity_accessors() {
        let ctx = AppContext::new("app-1", "adas", HashMap::new());
        assert_eq!(ctx.app_id(), "app-1");
        assert_eq!(ctx.domain(), "adas");
    }

    #[test]
    fn log_buffer() {
        let mut ctx = AppContext::new("app-1", "adas", HashMap::new());
        ctx.log_info("hello");
        ctx.log_info("world");

        let logs = ctx.drain_logs();
        assert_eq!(logs.len(), 2);
        assert!(logs[0].contains("hello"));
        assert!(ctx.drain_logs().is_empty());
    }

    #[test]
    fn log_buffer_is_bounded() {
        let mut ctx = AppContext::new("app-1", "adas", HashMap::new());
        for i in 0..(LOG_KEEP + 10) {
            ctx.log_warn(&format!("m{i}"));
        }
        let logs = ctx.drain_logs();
        assert_eq!(logs.len(), LOG_KEEP);
        assert!(logs[0].ends_with("m10"));
        assert!(logs.last().unwrap().starts_with("[WARN] app-1"));
    }
}
