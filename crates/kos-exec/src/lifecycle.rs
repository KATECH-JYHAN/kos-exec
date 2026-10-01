// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;

use crate::error::{KosError, Result};

type StateChangeCallback = Box<dyn Fn(&str, AppState, AppState) + Send>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum AppState {
    Installed,
    Init,
    Running,
    Suspended,
    Error,
    Terminated,
}

impl core::fmt::Display for AppState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let s = match self {
            AppState::Installed => "Installed",
            AppState::Init => "Init",
            AppState::Running => "Running",
            AppState::Suspended => "Suspended",
            AppState::Error => "Error",
            AppState::Terminated => "Terminated",
        };
        f.write_str(s)
    }
}

impl AppState {
    fn can_transition_to(self, to: AppState) -> bool {
        if to == AppState::Terminated {
            return true;
        }
        matches!(
            (self, to),
            (AppState::Installed, AppState::Init)
                | (AppState::Init, AppState::Running)
                | (AppState::Init, AppState::Error)
                | (AppState::Running, AppState::Suspended)
                | (AppState::Running, AppState::Error)
                | (AppState::Suspended, AppState::Running)
                | (AppState::Error, AppState::Init)
        )
    }
}

pub struct Lifecycle {
    states: HashMap<String, AppState>,
    callbacks: Vec<StateChangeCallback>,
}

impl Lifecycle {
    pub fn new() -> Self {
        Self {
            states: HashMap::new(),
            callbacks: Vec::new(),
        }
    }

    pub fn register(&mut self, app_id: &str) -> Result<()> {
        if self.states.contains_key(app_id) {
            return Err(KosError::AlreadyExists(format!("app {app_id}")));
        }
        self.states.insert(app_id.to_string(), AppState::Installed);
        Ok(())
    }

    pub fn transition(&mut self, app_id: &str, to: AppState) -> Result<()> {
        let from = *self
            .states
            .get(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;

        if !from.can_transition_to(to) {
            return Err(KosError::InvalidTransition(format!("{from} -> {to}")));
        }

        self.states.insert(app_id.to_string(), to);

        for cb in &self.callbacks {
            cb(app_id, from, to);
        }

        Ok(())
    }

    pub fn reset(&mut self, app_id: &str) -> Result<()> {
        let state = self.get_state(app_id)?;
        if state != AppState::Terminated {
            return Err(KosError::InvalidTransition(format!(
                "reset requires Terminated state, got {state}"
            )));
        }
        self.states.insert(app_id.to_string(), AppState::Installed);
        Ok(())
    }

    pub fn get_state(&self, app_id: &str) -> Result<AppState> {
        self.states
            .get(app_id)
            .copied()
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))
    }

    pub fn on_change(&mut self, cb: impl Fn(&str, AppState, AppState) + Send + 'static) {
        self.callbacks.push(Box::new(cb));
    }
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn valid_transitions() {
        let mut lc = Lifecycle::new();
        lc.register("app-1").unwrap();

        lc.transition("app-1", AppState::Init).unwrap();
        lc.transition("app-1", AppState::Running).unwrap();
        lc.transition("app-1", AppState::Suspended).unwrap();
        lc.transition("app-1", AppState::Running).unwrap();
        lc.transition("app-1", AppState::Terminated).unwrap();

        lc.register("app-2").unwrap();
        lc.transition("app-2", AppState::Init).unwrap();
        lc.transition("app-2", AppState::Error).unwrap();
        lc.transition("app-2", AppState::Init).unwrap();
        lc.transition("app-2", AppState::Running).unwrap();
        lc.transition("app-2", AppState::Error).unwrap();
        lc.transition("app-2", AppState::Terminated).unwrap();
    }

    #[test]
    fn invalid_transition_rejected() {
        let mut lc = Lifecycle::new();
        lc.register("app-1").unwrap();

        let err = lc.transition("app-1", AppState::Running).unwrap_err();
        assert!(matches!(err, KosError::InvalidTransition(_)));
        assert_eq!(lc.get_state("app-1").unwrap(), AppState::Installed);
    }

    #[test]
    fn callback_invoked() {
        let log: Arc<Mutex<Vec<(String, AppState, AppState)>>> =
            Arc::new(Mutex::new(Vec::new()));
        let log_clone = Arc::clone(&log);

        let mut lc = Lifecycle::new();
        lc.on_change(move |id, from, to| {
            log_clone.lock().unwrap().push((id.to_string(), from, to));
        });

        lc.register("app-1").unwrap();
        lc.transition("app-1", AppState::Init).unwrap();
        lc.transition("app-1", AppState::Running).unwrap();

        let entries = log.lock().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0], ("app-1".into(), AppState::Installed, AppState::Init));
        assert_eq!(entries[1], ("app-1".into(), AppState::Init, AppState::Running));
    }

    #[test]
    fn duplicate_register_rejected() {
        let mut lc = Lifecycle::new();
        lc.register("app-1").unwrap();
        assert!(matches!(
            lc.register("app-1"),
            Err(KosError::AlreadyExists(_))
        ));
    }

    #[test]
    fn unregistered_app_error() {
        let mut lc = Lifecycle::new();
        assert!(matches!(
            lc.transition("ghost", AppState::Init),
            Err(KosError::NotFound(_))
        ));
        assert!(matches!(
            lc.get_state("ghost"),
            Err(KosError::NotFound(_))
        ));
    }
}
