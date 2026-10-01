// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::context::AppContext;
use crate::error::Result;
use crate::traits::{ErrorAction, KosApp};

pub struct RuntimeConfig {
    pub app_id: String,
    pub domain: String,
    pub params: HashMap<String, String>,
    pub max_cycles: u32,
}

impl RuntimeConfig {
    pub fn from_env(default_app_id: &str) -> Self {
        let env = crate::launch_env::LaunchEnv::current();
        Self {
            app_id: env.app_id.clone().unwrap_or_else(|| default_app_id.to_string()),
            domain: env.domain.clone().unwrap_or_else(|| "default".to_string()),
            params: env.params.clone(),
            max_cycles: 0,
        }
    }
}

static SUSPEND_REQUESTED: AtomicBool = AtomicBool::new(false);
static CONTINUED: AtomicU64 = AtomicU64::new(0);

extern "C" fn on_sigtstp(_: libc::c_int) {
    SUSPEND_REQUESTED.store(true, Ordering::SeqCst);
}

extern "C" fn on_sigcont(_: libc::c_int) {
    SUSPEND_REQUESTED.store(false, Ordering::SeqCst);
    CONTINUED.fetch_add(1, Ordering::SeqCst);
}

fn install_suspend_handlers() {
    use nix::sys::signal::{sigaction, SaFlags, SigAction, SigHandler, SigSet, Signal};
    let tstp = SigAction::new(SigHandler::Handler(on_sigtstp), SaFlags::SA_RESTART, SigSet::empty());
    let cont = SigAction::new(SigHandler::Handler(on_sigcont), SaFlags::SA_RESTART, SigSet::empty());
    unsafe {
        let _ = sigaction(Signal::SIGTSTP, &tstp);
        let _ = sigaction(Signal::SIGCONT, &cont);
    }
}

fn handle_suspend_request<A: KosApp>(app: &mut A, ctx: &mut AppContext) {
    if !SUSPEND_REQUESTED.swap(false, Ordering::SeqCst) {
        return;
    }
    let continued = CONTINUED.load(Ordering::SeqCst);
    if let Err(e) = app.on_suspend(ctx) {
        ctx.log_warn(&format!("on_suspend failed: {e}"));
    }
    if CONTINUED.load(Ordering::SeqCst) == continued {
        unsafe { libc::raise(libc::SIGSTOP) };
    }
    if let Err(e) = app.on_resume(ctx) {
        ctx.log_warn(&format!("on_resume failed: {e}"));
    }
}

pub fn run<A: KosApp>(mut app: A, config: RuntimeConfig) -> Result<()> {
    let mut ctx = AppContext::new(&config.app_id, &config.domain, config.params);
    install_suspend_handlers();

    app.on_init(&mut ctx)?;

    let mut cycle = 0u32;
    loop {
        if config.max_cycles > 0 && cycle >= config.max_cycles {
            break;
        }

        crate::heartbeat::beat();
        handle_suspend_request(&mut app, &mut ctx);
        match app.on_run(&mut ctx) {
            Ok(()) => {}
            Err(e) => {
                let action = app.on_error(&mut ctx, &e);
                match action {
                    ErrorAction::Restart => {
                        app.on_init(&mut ctx)?;
                    }
                    ErrorAction::Terminate => {
                        break;
                    }
                    ErrorAction::Ignore => {}
                }
            }
        }

        cycle += 1;
    }

    app.on_terminate(&mut ctx)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::KosError;
    use crate::traits::ErrorAction;
    use std::sync::{Arc, Mutex};

    struct TrackerApp {
        log: Arc<Mutex<Vec<String>>>,
    }

    impl KosApp for TrackerApp {
        fn on_init(&mut self, _ctx: &mut AppContext) -> Result<()> {
            self.log.lock().unwrap().push("init".into());
            Ok(())
        }

        fn on_run(&mut self, _ctx: &mut AppContext) -> Result<()> {
            self.log.lock().unwrap().push("run".into());
            Ok(())
        }

        fn on_terminate(&mut self, _ctx: &mut AppContext) -> Result<()> {
            self.log.lock().unwrap().push("terminate".into());
            Ok(())
        }
    }

    #[test]
    fn normal_lifecycle_init_run_terminate() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let app = TrackerApp { log: log.clone() };

        let config = RuntimeConfig {
            app_id: "test".into(),
            domain: "default".into(),
            params: HashMap::new(),
            max_cycles: 3,
        };

        run(app, config).unwrap();

        let entries = log.lock().unwrap();
        assert_eq!(
            *entries,
            vec!["init", "run", "run", "run", "terminate"]
        );
    }

    struct FailOnceApp {
        run_count: u32,
        init_count: u32,
    }

    impl KosApp for FailOnceApp {
        fn on_init(&mut self, _ctx: &mut AppContext) -> Result<()> {
            self.init_count += 1;
            Ok(())
        }

        fn on_run(&mut self, _ctx: &mut AppContext) -> Result<()> {
            self.run_count += 1;
            if self.run_count == 2 {
                return Err(KosError::InvalidConfig("boom".into()));
            }
            Ok(())
        }

        fn on_error(
            &mut self,
            _ctx: &mut AppContext,
            _error: &KosError,
        ) -> ErrorAction {
            ErrorAction::Restart
        }
    }

    #[test]
    fn error_triggers_restart() {
        let app = FailOnceApp {
            run_count: 0,
            init_count: 0,
        };

        let config = RuntimeConfig {
            app_id: "test".into(),
            domain: "default".into(),
            params: HashMap::new(),
            max_cycles: 4,
        };

        run(app, config).unwrap();
    }

    struct TermOnErrorApp {
        run_count: u32,
    }

    impl KosApp for TermOnErrorApp {
        fn on_init(&mut self, _ctx: &mut AppContext) -> Result<()> {
            Ok(())
        }

        fn on_run(&mut self, _ctx: &mut AppContext) -> Result<()> {
            self.run_count += 1;
            if self.run_count == 2 {
                return Err(KosError::InvalidConfig("fatal".into()));
            }
            Ok(())
        }

        fn on_error(
            &mut self,
            _ctx: &mut AppContext,
            _error: &KosError,
        ) -> ErrorAction {
            ErrorAction::Terminate
        }
    }

    #[test]
    fn error_action_terminate_stops_loop() {
        let app = TermOnErrorApp { run_count: 0 };

        let config = RuntimeConfig {
            app_id: "test".into(),
            domain: "default".into(),
            params: HashMap::new(),
            max_cycles: 10,
        };

        run(app, config).unwrap();
    }
}
