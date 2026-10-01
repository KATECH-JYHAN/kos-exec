// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

pub mod app_manager;
pub mod app_scheduler;
pub mod capability;
pub mod cgroup;
pub mod comm;
#[cfg(feature = "iceoryx2")]
pub mod comm_iox2;
pub mod config;
pub mod control;
pub mod context;
pub mod dependency;
pub mod diag;
pub mod domain;
pub mod error;
pub mod health_monitor;
pub mod heartbeat;
pub mod launch;
pub mod launch_env;
pub mod lifecycle;
#[macro_use]
pub mod macros;
pub mod main_task;
pub mod node;
pub mod notify;
pub mod reaper;
pub mod resource_manager;
pub mod runtime;
pub mod service;
pub mod shm_inspect;
pub mod task_scheduler;
pub mod thread_context;
pub mod thread_manager;
pub mod traits;

pub use app_manager::{AppCrashEvent, AppManager, ShmLifecycle, SpawnResources, ThreadStatusReport};
pub use app_scheduler::{AppSchedule, AppScheduler, AppTrigger, SystemContext, VehicleState};
pub use config::{AppConfig, RestartPolicy, RestartStrategy, ScheduleConfig, ThreadConfigToml};
pub use context::AppContext;
pub use dependency::DependencyResolver;
pub use domain::{AsilLevel, DomainConfig, DomainController};
pub use error::{KosError, Result};
pub use health_monitor::{HealthConfig, HealthEvent, HealthMonitor, HealthStatus};
pub use launch::{AppInfo, Launcher, ThreadInfo};
pub use launch_env::LaunchEnv;
pub use lifecycle::{AppState, Lifecycle};
pub use runtime::RuntimeConfig;
pub use task_scheduler::{OverrunPolicy, TaskPriority, TaskScheduler};
pub use comm::{
    CommHandle, MockTransport, RecvGuard, ShmMessage, ShmPublisher, ShmSubscriber, ShmTransport,
    TopicPublisher, TopicSubscriber, SHM_MSG_SIZE,
};
pub use service::{ServiceClient, ServiceServer};
pub use diag::{read_incidents, CountingDiag, DiagNotifier, FileDiag, Incident, LogOnlyDiag};
pub use main_task::{MainTask, MainTaskConfig};
pub use node::{Node, NodeConfig};
pub use notify::{NotifyClient, NotifyCommand, NotifyMessage, NotifyServer, NotifyStatus};
pub use thread_context::ThreadContext;
pub use thread_manager::{
    Priority, ThreadCallbacks, ThreadConfig, ThreadHandle, ThreadManager, ThreadState, ThreadStats,
    Trigger,
};
pub use resource_manager::{ResourceConfig, ResourceEvent, ResourceManager};
pub use traits::{ErrorAction, KosApp};
