// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

#[macro_export]
macro_rules! kos_app {
    (
        name: $name:ident,
        data: { $($field:ident : $type:ty = $default:expr),* $(,)? },
        on_init: |$si:ident, $ci:ident| $init:block,
        on_run:  |$sr:ident, $cr:ident| $run:block,
        on_suspend:   |$ss:ident, $cs:ident| $sus:block,
        on_resume:    |$sres:ident, $cres:ident| $res:block,
        on_terminate: |$st:ident, $ct:ident| $term:block,
        on_error:     |$se:ident, $ce:ident, $err:ident| $ebody:block
        $(,)?
    ) => {
        struct $name {
            $( $field: $type, )*
        }

        impl $crate::KosApp for $name {
            fn on_init(&mut $si, $ci: &mut $crate::AppContext) -> $crate::Result<()>
                $init

            fn on_run(&mut $sr, $cr: &mut $crate::AppContext) -> $crate::Result<()>
                $run

            fn on_suspend(&mut $ss, $cs: &mut $crate::AppContext) -> $crate::Result<()>
                $sus

            fn on_resume(&mut $sres, $cres: &mut $crate::AppContext) -> $crate::Result<()>
                $res

            fn on_terminate(&mut $st, $ct: &mut $crate::AppContext) -> $crate::Result<()>
                $term

            fn on_error(
                &mut $se,
                $ce: &mut $crate::AppContext,
                $err: &$crate::KosError,
            ) -> $crate::ErrorAction
                $ebody
        }

        impl $name {
            #[allow(dead_code)]
            fn new() -> Self {
                Self {
                    $( $field: $default, )*
                }
            }
        }
    };

    (
        name: $name:ident,
        data: { $($field:ident : $type:ty = $default:expr),* $(,)? },
        on_init: |$si:ident, $ci:ident| $init:block,
        on_run:  |$sr:ident, $cr:ident| $run:block
        $(,)?
    ) => {
        struct $name {
            $( $field: $type, )*
        }

        impl $crate::KosApp for $name {
            fn on_init(&mut $si, $ci: &mut $crate::AppContext) -> $crate::Result<()>
                $init

            fn on_run(&mut $sr, $cr: &mut $crate::AppContext) -> $crate::Result<()>
                $run
        }

        impl $name {
            #[allow(dead_code)]
            fn new() -> Self {
                Self {
                    $( $field: $default, )*
                }
            }
        }
    };
}

#[cfg(test)]
mod tests {
    use crate::context::AppContext;
    use crate::error::KosError;
    use crate::traits::{ErrorAction, KosApp};

    kos_app! {
        name: MinimalApp,
        data: {
            initialized: bool = false,
            run_count: u32 = 0,
        },
        on_init: |self, ctx| {
            self.initialized = true;
            ctx.log_info("init");
            Ok(())
        },
        on_run: |self, ctx| {
            self.run_count += 1;
            ctx.log_info("run");
            Ok(())
        },
    }

    #[test]
    fn macro_minimal_compiles_and_runs() {
        let mut app = MinimalApp::new();
        let mut ctx = AppContext::new("test", "default", Default::default());

        assert!(!app.initialized);
        app.on_init(&mut ctx).unwrap();
        assert!(app.initialized);

        app.on_run(&mut ctx).unwrap();
        app.on_run(&mut ctx).unwrap();
        assert_eq!(app.run_count, 2);
    }

    #[test]
    fn macro_minimal_has_defaults_for_optional_callbacks() {
        let mut app = MinimalApp::new();
        let mut ctx = AppContext::new("test", "default", Default::default());

        assert!(app.on_suspend(&mut ctx).is_ok());
        assert!(app.on_resume(&mut ctx).is_ok());
        assert!(app.on_terminate(&mut ctx).is_ok());
        assert_eq!(
            app.on_error(&mut ctx, &KosError::NotFound("x".into())),
            ErrorAction::Restart,
        );
    }

    kos_app! {
        name: FullApp,
        data: {
            state: String = String::new(),
            error_count: u32 = 0,
        },
        on_init: |self, ctx| {
            self.state = "init".into();
            let _ = ctx;
            Ok(())
        },
        on_run: |self, ctx| {
            self.state = "running".into();
            let _ = ctx;
            Ok(())
        },
        on_suspend: |self, ctx| {
            self.state = "suspended".into();
            let _ = ctx;
            Ok(())
        },
        on_resume: |self, ctx| {
            self.state = "resumed".into();
            let _ = ctx;
            Ok(())
        },
        on_terminate: |self, ctx| {
            self.state = "terminated".into();
            let _ = ctx;
            Ok(())
        },
        on_error: |self, ctx, _err| {
            self.error_count += 1;
            let _ = ctx;
            if self.error_count > 3 {
                ErrorAction::Terminate
            } else {
                ErrorAction::Restart
            }
        },
    }

    #[test]
    fn macro_full_callbacks() {
        let mut app = FullApp::new();
        let mut ctx = AppContext::new("test", "default", Default::default());

        app.on_init(&mut ctx).unwrap();
        assert_eq!(app.state, "init");

        app.on_run(&mut ctx).unwrap();
        assert_eq!(app.state, "running");

        app.on_suspend(&mut ctx).unwrap();
        assert_eq!(app.state, "suspended");

        app.on_resume(&mut ctx).unwrap();
        assert_eq!(app.state, "resumed");

        app.on_terminate(&mut ctx).unwrap();
        assert_eq!(app.state, "terminated");
    }

    #[test]
    fn macro_full_on_error_escalation() {
        let mut app = FullApp::new();
        let mut ctx = AppContext::new("test", "default", Default::default());
        let err = KosError::NotFound("x".into());

        for _ in 0..3 {
            assert_eq!(app.on_error(&mut ctx, &err), ErrorAction::Restart);
        }
        assert_eq!(app.on_error(&mut ctx, &err), ErrorAction::Terminate);
    }

    #[test]
    fn macro_data_field_access_across_callbacks() {
        let mut app = MinimalApp::new();
        let mut ctx = AppContext::new("test", "default", Default::default());

        app.on_init(&mut ctx).unwrap();
        assert!(app.initialized);

        app.on_run(&mut ctx).unwrap();
        assert_eq!(app.run_count, 1);
    }
}
