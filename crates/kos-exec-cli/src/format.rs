// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use kos_exec::launch::{AppInfo, ThreadInfo};
use kos_exec::shm_inspect::{ShmTopicDetail, ShmTopicSummary};

fn header() -> String {
    format!(
        "{:<8} {:<18} {:<12} {:<8} {:<6} {:<16} {}",
        "DOMAIN", "APP", "STATE", "PID", "CORE", "SCHEDULE", "RESTARTS"
    )
}

fn row(info: &AppInfo) -> String {
    let pid = info
        .pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".into());

    let core = info
        .cores
        .as_ref()
        .map(|c| {
            c.iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|| "-".into());

    let state = format!("{}", info.state).to_uppercase();

    format!(
        "{:<8} {:<18} {:<12} {:<8} {:<6} {:<16} {}",
        info.domain, info.app_id, state, pid, core, info.schedule, info.restarts
    )
}

pub fn status_table(infos: &[AppInfo]) -> String {
    let mut lines = vec![header()];
    for info in infos {
        lines.push(row(info));
    }
    lines.join("\n")
}

pub fn status_detail(info: &AppInfo) -> String {
    let pid = info
        .pid
        .map(|p| p.to_string())
        .unwrap_or_else(|| "-".into());

    let core = info
        .cores
        .as_ref()
        .map(|c| {
            c.iter()
                .map(|n| n.to_string())
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_else(|| "-".into());

    let state = format!("{}", info.state).to_uppercase();

    let mut out = format!(
        "App:       {}\n\
         Domain:    {}\n\
         State:     {}\n\
         PID:       {}\n\
         Cores:     {}\n\
         Schedule:  {}\n\
         Restarts:  {}",
        info.app_id, info.domain, state, pid, core, info.schedule, info.restarts
    );

    if !info.threads.is_empty() {
        out.push_str("\n  Threads:");
        for t in &info.threads {
            out.push_str(&format_thread(t));
        }
    }

    out
}

fn format_thread(t: &ThreadInfo) -> String {
    let cpu = t
        .cpu_affinity
        .map(|c| format!(", CPU {c}"))
        .unwrap_or_default();

    let subs_str = t.subs.join(", ");
    let pubs_str = t.pubs.join(", ");
    let io = match (subs_str.is_empty(), pubs_str.is_empty()) {
        (true, true) => String::new(),
        (false, true) => format!("  ({subs_str} →)"),
        (true, false) => format!("  (→ {pubs_str})"),
        (false, false) => format!("  ({subs_str} → {pubs_str})"),
    };

    format!(
        "\n    {:<8} [{}, {}{}] {}{}",
        t.name,
        t.trigger,
        t.priority.chars().next().unwrap_or('N').to_uppercase().collect::<String>()
            + &t.priority[1..],
        cpu,
        t.state,
        io,
    )
}

fn topic_label(s: &ShmTopicSummary) -> String {
    s.topic.clone().unwrap_or_else(|| format!("? ({})", s.shm_name))
}

fn publisher_label(s: &ShmTopicSummary) -> String {
    match (s.publisher_pid, s.publisher_alive) {
        (0, _) => "-".to_string(),
        (pid, true) => pid.to_string(),
        (pid, false) => format!("{pid}(dead)"),
    }
}

pub fn shm_table(list: &[ShmTopicSummary]) -> String {
    let w = list.iter().map(|s| topic_label(s).len()).max().unwrap_or(5).max(5);
    let mut lines = vec![format!(
        "{:<w$}  {:>6}  {:>7}  {:>9}  {:>12}  {:>7}  {:>8}  {:>6}",
        "TOPIC", "MSG", "QUEUE", "PUBLISHED", "PUBLISHER", "READERS", "OVERFLOW", "MISS"
    )];
    for s in list {
        if let Some(err) = &s.error {
            lines.push(format!("{:<w$}  (unreadable: {err})", topic_label(s)));
            continue;
        }
        lines.push(format!(
            "{:<w$}  {:>5}B  {:>3}/{:<3}  {:>9}  {:>12}  {:>7}  {:>8}  {:>6}",
            topic_label(s),
            s.data_size,
            s.queue_len,
            s.history,
            s.publish_count,
            publisher_label(s),
            s.readers,
            s.overflow,
            s.sub_miss
        ));
    }
    lines.join("\n")
}

pub fn shm_detail(d: &ShmTopicDetail) -> String {
    let s = &d.summary;
    let mut out = vec![
        format!("Topic:       {}", topic_label(s)),
        format!("SHM:         {} ({} bytes)", s.shm_name, s.size_bytes),
        format!("Message:     max {} bytes", s.data_size),
        format!("Queue:       {} / history {} (+margin {}, {} slots)", s.queue_len, s.history, s.margin, s.slots),
        format!("Published:   {}", s.publish_count),
        format!("Publisher:   {}", publisher_label(s)),
        format!("Readers:     {} attached", s.readers),
        format!("Overflow:    {}   Starvation: {}   Subscriber miss: {}", s.overflow, s.starvation, s.sub_miss),
        format!(
            "Deadline:    {}",
            if s.deadline_us == 0 { "none".to_string() } else { format!("{}us", s.deadline_us) }
        ),
        String::new(),
        format!("{:>4}  {:>5}  {:>8}  {:>6}  {:>7}  {:>7}  {:>9}", "SLOT", "QUEUE", "SEQ", "SIZE", "READERS", "PENDING", "AGE"),
    ];
    let now_ns = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for slot in &d.slots {
        let age = if slot.timestamp_ns == 0 {
            "-".to_string()
        } else {
            let ms = now_ns.saturating_sub(slot.timestamp_ns) as f64 / 1e6;
            if ms < 1000.0 { format!("{ms:.1}ms") } else { format!("{:.1}s", ms / 1000.0) }
        };
        out.push(format!(
            "{:>4}  {:>5}  {:>8}  {:>6}  {:>7}  {:>7}  {:>9}",
            slot.index,
            slot.queue_pos.map_or("-".to_string(), |p| p.to_string()),
            if slot.queue_pos.is_some() || slot.pending { slot.sequence.to_string() } else { "-".to_string() },
            slot.size,
            slot.readers,
            if slot.pending { "yes" } else { "" },
            age
        ));
    }
    out.push(String::new());
    out.push("QUEUE: 0 = oldest. PENDING: evicted but still being read.".to_string());
    out.join("\n")
}

pub fn local_datetime(unix_secs: u64) -> String {
    let t = unix_secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&t, &mut tm) }.is_null() {
        return unix_secs.to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use kos_exec::AppState;

    fn sample_info() -> AppInfo {
        AppInfo {
            app_id: "adas.camera".into(),
            domain: "adas".into(),
            state: AppState::Running,
            pid: Some(1234),
            cores: Some(vec![0]),
            schedule: "periodic/10ms".into(),
            restarts: 0,
            threads: vec![],
        }
    }

    #[test]
    fn status_table_format() {
        let infos = vec![
            sample_info(),
            AppInfo {
                app_id: "ivi.media".into(),
                domain: "ivi".into(),
                state: AppState::Suspended,
                pid: None,
                cores: None,
                schedule: "event/media_play".into(),
                restarts: 0,
                threads: vec![],
            },
        ];

        let table = status_table(&infos);
        let lines: Vec<&str> = table.lines().collect();

        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("DOMAIN"));
        assert!(lines[0].contains("APP"));
        assert!(lines[0].contains("STATE"));
        assert!(lines[1].contains("adas.camera"));
        assert!(lines[1].contains("RUNNING"));
        assert!(lines[1].contains("1234"));
        assert!(lines[2].contains("ivi.media"));
        assert!(lines[2].contains("SUSPENDED"));
    }

    #[test]
    fn status_detail_format() {
        let info = sample_info();
        let detail = status_detail(&info);

        assert!(detail.contains("App:       adas.camera"));
        assert!(detail.contains("Domain:    adas"));
        assert!(detail.contains("State:     RUNNING"));
        assert!(detail.contains("PID:       1234"));
        assert!(detail.contains("Cores:     0"));
        assert!(detail.contains("Schedule:  periodic/10ms"));
        assert!(detail.contains("Restarts:  0"));
    }

    #[test]
    fn status_detail_with_threads() {
        use kos_exec::launch::ThreadInfo;
        let info = AppInfo {
            app_id: "adas.fusion".into(),
            domain: "adas".into(),
            state: AppState::Running,
            pid: Some(5678),
            cores: Some(vec![0, 1]),
            schedule: "periodic/10ms".into(),
            restarts: 0,
            threads: vec![
                ThreadInfo {
                    name: "fusion".into(),
                    trigger: "Periodic 10ms".into(),
                    priority: "critical".into(),
                    cpu_affinity: Some(0),
                    subs: vec!["lidar".into(), "radar".into()],
                    pubs: vec!["control_cmd".into()],
                    state: "Running".into(),
                },
                ThreadInfo {
                    name: "diag".into(),
                    trigger: "Event diag_req".into(),
                    priority: "low".into(),
                    cpu_affinity: None,
                    subs: vec!["diag_req".into()],
                    pubs: vec!["diag_resp".into()],
                    state: "Running".into(),
                },
            ],
        };

        let detail = status_detail(&info);
        assert!(detail.contains("Threads:"));
        assert!(detail.contains("fusion"));
        assert!(detail.contains("Periodic 10ms"));
        assert!(detail.contains("CPU 0"));
        assert!(detail.contains("lidar, radar → control_cmd"));
        assert!(detail.contains("diag"));
        assert!(detail.contains("Event diag_req"));
    }

    #[test]
    fn status_table_no_pid_shows_dash() {
        let info = AppInfo {
            app_id: "test.app".into(),
            domain: "test".into(),
            state: AppState::Installed,
            pid: None,
            cores: None,
            schedule: "periodic/100ms".into(),
            restarts: 2,
            threads: vec![],
        };

        let table = status_table(&[info]);
        let data_line = table.lines().nth(1).unwrap();
        assert!(data_line.contains("test.app"));
        let parts: Vec<&str> = data_line.split_whitespace().collect();
        assert!(parts.contains(&"-"));
    }
}
