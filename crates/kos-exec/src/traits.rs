// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use crate::context::AppContext;
use crate::error::{KosError, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorAction {
    Restart,
    Terminate,
    Ignore,
}

pub trait KosApp: Send + 'static {
    fn on_init(&mut self, ctx: &mut AppContext) -> Result<()>;

    fn on_run(&mut self, ctx: &mut AppContext) -> Result<()>;

    fn on_suspend(&mut self, ctx: &mut AppContext) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    fn on_resume(&mut self, ctx: &mut AppContext) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    fn on_terminate(&mut self, ctx: &mut AppContext) -> Result<()> {
        let _ = ctx;
        Ok(())
    }

    fn on_error(&mut self, ctx: &mut AppContext, error: &KosError) -> ErrorAction {
        let _ = (ctx, error);
        ErrorAction::Restart
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct CounterApp {
        count: u32,
    }

    impl KosApp for CounterApp {
        fn on_init(&mut self, ctx: &mut AppContext) -> Result<()> {
            ctx.log_info("init");
            Ok(())
        }

        fn on_run(&mut self, ctx: &mut AppContext) -> Result<()> {
            self.count += 1;
            ctx.log_info(&format!("run {}", self.count));
            Ok(())
        }
    }

    #[test]
    fn trait_default_callbacks() {
        let mut app = CounterApp { count: 0 };
        let mut ctx = AppContext::new("test", "default", Default::default());

        assert!(app.on_suspend(&mut ctx).is_ok());
        assert!(app.on_resume(&mut ctx).is_ok());
        assert!(app.on_terminate(&mut ctx).is_ok());

        let action = app.on_error(
            &mut ctx,
            &KosError::NotFound("x".into()),
        );
        assert_eq!(action, ErrorAction::Restart);
    }

    #[test]
    fn trait_init_and_run() {
        let mut app = CounterApp { count: 0 };
        let mut ctx = AppContext::new("test", "default", Default::default());

        app.on_init(&mut ctx).unwrap();
        app.on_run(&mut ctx).unwrap();
        app.on_run(&mut ctx).unwrap();

        assert_eq!(app.count, 2);
        let logs = ctx.drain_logs();
        assert_eq!(logs.len(), 3);
    }
}
