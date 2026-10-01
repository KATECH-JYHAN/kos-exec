// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::process;

use clap::{Parser, Subcommand};

mod commands;
mod format;

#[derive(Parser)]
#[command(name = "kos", about = "KOS application execution manager")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    #[command(about = "Launch apps from a TOML file and manage them until SIGINT/SIGTERM or `kos shutdown`")]
    Launch {
        #[arg(help = "Path to the launch TOML file")]
        toml: String,
        #[arg(long, help = "Only launch apps in this domain (plus dependencies)")]
        domain: Option<String>,
        #[arg(long, help = "Only launch this app (plus dependencies)")]
        app: Option<String>,
        #[arg(long, help = "Restart crashed or hung apps (up to restart.max_retries)")]
        supervise: bool,
    },
    #[command(about = "Show status of all apps or one app")]
    Status {
        #[arg(help = "App identifier for detailed status")]
        app_id: Option<String>,
    },
    #[command(about = "Stop an app")]
    Stop {
        #[arg(help = "App identifier")]
        app_id: String,
    },
    #[command(about = "Start a stopped app")]
    Start {
        #[arg(help = "App identifier")]
        app_id: String,
    },
    #[command(about = "Restart an app")]
    Restart {
        #[arg(help = "App identifier")]
        app_id: String,
    },
    #[command(about = "Pause an app (SIGTSTP, then SIGSTOP; runtime apps get on_suspend)")]
    Suspend {
        #[arg(help = "App identifier")]
        app_id: String,
    },
    #[command(about = "Resume a paused app (SIGCONT; runtime apps get on_resume)")]
    Resume {
        #[arg(help = "App identifier")]
        app_id: String,
    },
    #[command(about = "Raise an event trigger `signal:<NAME>`")]
    Signal {
        #[arg(help = "Signal name")]
        name: String,
    },
    #[command(about = "Set the vehicle state; apps with `state:<STATE>` triggers start on change")]
    State {
        #[arg(help = "PARKED | DRIVING | CHARGING | EMERGENCY")]
        state: String,
    },
    #[command(about = "Stop all apps and the launcher")]
    Shutdown,
    #[command(about = "Show recorded incidents (crashes without restart, hangs, give-ups)")]
    Incidents {
        #[arg(short = 'n', long, default_value_t = 20, help = "Number of most recent incidents")]
        last: usize,
    },
    #[command(about = "SHM topic operations")]
    Shm {
        #[command(subcommand)]
        action: ShmAction,
    },
}

#[derive(Subcommand)]
enum ShmAction {
    #[command(about = "List KOS-comm SHM topics in /dev/shm with queue/publisher/reader stats")]
    Status {
        #[arg(long, help = "Also resolve topic names found in this launch TOML")]
        toml: Option<String>,
        #[arg(long = "topic", help = "Additional topic name to resolve (repeatable)")]
        topics: Vec<String>,
    },
    #[command(about = "Remove SHM topics whose publisher process is gone")]
    Cleanup {
        #[arg(long, help = "Only list what would be removed")]
        dry_run: bool,
        #[arg(long, help = "Also resolve topic names found in this launch TOML")]
        toml: Option<String>,
    },
    #[command(about = "Show header and per-slot details for a topic")]
    Info {
        #[arg(help = "Topic name (e.g. adas/camera/frame)")]
        topic: String,
    },
}

fn main() {
    let cli = Cli::parse();

    if matches!(cli.command, Commands::Launch { .. }) {
        if let Err(e) = kos_exec::capability::ensure_delegated() {
            eprintln!("[kos-exec] WARNING: auto-delegation failed: {e}");
        }
    }

    let result = match cli.command {
        Commands::Launch { toml, domain, app, supervise } => {
            commands::launch(&toml, domain, app, supervise)
        }
        Commands::Status { app_id } => commands::status(app_id),
        Commands::Stop { app_id } => commands::stop(&app_id),
        Commands::Start { app_id } => commands::start(&app_id),
        Commands::Restart { app_id } => commands::restart(&app_id),
        Commands::Suspend { app_id } => commands::suspend(&app_id),
        Commands::Resume { app_id } => commands::resume(&app_id),
        Commands::Signal { name } => commands::signal(&name),
        Commands::State { state } => commands::state(&state),
        Commands::Shutdown => commands::shutdown(),
        Commands::Incidents { last } => commands::incidents(last),
        Commands::Shm { action } => match action {
            ShmAction::Status { toml, topics } => commands::shm_status(toml, topics),
            ShmAction::Info { topic } => commands::shm_info(&topic),
            ShmAction::Cleanup { dry_run, toml } => commands::shm_cleanup(dry_run, toml),
        },
    };

    if let Err(e) = result {
        eprintln!("error: {e}");
        process::exit(1);
    }
}
