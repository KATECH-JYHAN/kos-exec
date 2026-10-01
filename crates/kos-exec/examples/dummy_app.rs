// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::{env, thread, time::Duration};

fn main() {
    let args: Vec<String> = env::args().collect();
    let mut duration = 60u64;
    let mut crash_after: Option<u64> = None;
    let mut exit_code = 0i32;

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--duration" => {
                i += 1;
                duration = args[i].parse().unwrap_or(60);
            }
            "--crash-after" => {
                i += 1;
                crash_after = Some(args[i].parse().unwrap_or(3));
            }
            "--exit-code" => {
                i += 1;
                exit_code = args[i].parse().unwrap_or(0);
            }
            _ => {}
        }
        i += 1;
    }

    let app_name = env::var("KOS_APP_ID").unwrap_or_else(|_| "unknown".into());
    eprintln!("[{app_name}] started (pid={})", std::process::id());

    if let Some(secs) = crash_after {
        thread::sleep(Duration::from_secs(secs));
        eprintln!("[{app_name}] crashing with code {exit_code}");
        std::process::exit(exit_code);
    }

    thread::sleep(Duration::from_secs(duration));
    eprintln!("[{app_name}] exiting normally");
    std::process::exit(exit_code);
}
