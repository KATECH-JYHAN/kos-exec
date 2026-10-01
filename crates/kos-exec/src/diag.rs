// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use crate::error::KosError;

pub trait DiagNotifier: Send {
    fn notify_incident(&self, app_id: &str, error: &KosError);
}

pub struct LogOnlyDiag {
    log_buf: std::sync::Mutex<Vec<String>>,
}

impl LogOnlyDiag {
    pub fn new() -> Self {
        Self {
            log_buf: std::sync::Mutex::new(Vec::new()),
        }
    }

    pub fn drain_logs(&self) -> Vec<String> {
        std::mem::take(&mut self.log_buf.lock().unwrap())
    }
}

impl Default for LogOnlyDiag {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagNotifier for LogOnlyDiag {
    fn notify_incident(&self, app_id: &str, error: &KosError) {
        let msg = format!("[DIAG] {app_id}: {error}");
        eprintln!("[kos-exec] {msg}");
        let mut buf = self.log_buf.lock().unwrap();
        if buf.len() >= 256 {
            buf.remove(0);
        }
        buf.push(msg);
    }
}

pub struct FileDiag {
    path: std::path::PathBuf,
}

const INCIDENT_LOG_MAX_BYTES: u64 = 1 << 20;

impl FileDiag {
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn default_path() -> std::path::PathBuf {
        use std::path::PathBuf;
        if let Ok(p) = std::env::var("KOS_INCIDENT_LOG") {
            return PathBuf::from(p);
        }
        if let Ok(d) = std::env::var("KOS_DATA_DIR") {
            return PathBuf::from(d).join("incidents.log");
        }
        let uid = unsafe { libc::getuid() };
        if uid == 0 {
            return PathBuf::from("/var/log/kos/incidents.log");
        }
        if let Ok(d) = std::env::var("XDG_STATE_HOME") {
            return PathBuf::from(d).join("kos/incidents.log");
        }
        if let Ok(h) = std::env::var("HOME") {
            return PathBuf::from(h).join(".local/state/kos/incidents.log");
        }
        PathBuf::from(format!("/tmp/kos-{uid}/incidents.log"))
    }

    fn append(&self, line: &str) -> std::io::Result<()> {
        use std::io::Write;
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if std::fs::metadata(&self.path).map(|m| m.len() >= INCIDENT_LOG_MAX_BYTES).unwrap_or(false) {
            let _ = std::fs::rename(&self.path, self.path.with_extension("log.1"));
        }
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&self.path)?;
        writeln!(f, "{line}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Incident {
    pub unix_secs: u64,
    pub app_id: String,
    pub message: String,
}

impl Incident {
    fn to_line(&self) -> String {
        let clean = |s: &str| s.replace(['\t', '\n'], " ");
        format!("{}\t{}\t{}", self.unix_secs, clean(&self.app_id), clean(&self.message))
    }

    fn from_line(line: &str) -> Option<Self> {
        let mut it = line.splitn(3, '\t');
        Some(Self {
            unix_secs: it.next()?.parse().ok()?,
            app_id: it.next()?.to_string(),
            message: it.next()?.to_string(),
        })
    }
}

pub fn read_incidents(path: &std::path::Path, last: usize) -> std::io::Result<Vec<Incident>> {
    let text = std::fs::read_to_string(path)?;
    let all: Vec<Incident> = text.lines().filter_map(Incident::from_line).collect();
    Ok(all[all.len().saturating_sub(last)..].to_vec())
}

impl DiagNotifier for FileDiag {
    fn notify_incident(&self, app_id: &str, error: &KosError) {
        let unix_secs = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let incident = Incident { unix_secs, app_id: app_id.to_string(), message: error.to_string() };
        eprintln!("[kos-exec] [DIAG] {app_id}: {error}");
        if let Err(e) = self.append(&incident.to_line()) {
            eprintln!("[kos-exec] WARNING: cannot write incident log {}: {e}", self.path.display());
        }
    }
}

pub struct CountingDiag {
    count: std::sync::atomic::AtomicU32,
}

impl CountingDiag {
    pub fn new() -> Self {
        Self {
            count: std::sync::atomic::AtomicU32::new(0),
        }
    }

    pub fn incident_count(&self) -> u32 {
        self.count.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for CountingDiag {
    fn default() -> Self {
        Self::new()
    }
}

impl DiagNotifier for CountingDiag {
    fn notify_incident(&self, _app_id: &str, _error: &KosError) {
        self.count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_only_diag_records_incidents() {
        let diag = LogOnlyDiag::new();
        let error = KosError::PermissionDenied("max_retries exceeded".into());

        diag.notify_incident("adas.camera", &error);
        diag.notify_incident("ivi.media", &error);

        let logs = diag.drain_logs();
        assert_eq!(logs.len(), 2);
        assert!(logs[0].contains("[DIAG] adas.camera"));
        assert!(logs[0].contains("max_retries exceeded"));
        assert!(logs[1].contains("[DIAG] ivi.media"));

        assert!(diag.drain_logs().is_empty());
    }

    #[test]
    fn counting_diag_increments() {
        let diag = CountingDiag::new();
        assert_eq!(diag.incident_count(), 0);

        diag.notify_incident("app-1", &KosError::NotFound("x".into()));
        diag.notify_incident("app-2", &KosError::NotFound("y".into()));
        diag.notify_incident("app-3", &KosError::NotFound("z".into()));

        assert_eq!(diag.incident_count(), 3);
    }

    #[test]
    fn diag_notifier_trait_object_works() {
        let diag: Box<dyn DiagNotifier> = Box::new(LogOnlyDiag::new());
        diag.notify_incident("test", &KosError::InvalidConfig("bad".into()));
    }

    #[test]
    fn max_retries_triggers_diag() {
        let diag = LogOnlyDiag::new();
        let error = KosError::PermissionDenied("app crash exceeded max_retries (3)".into());

        diag.notify_incident("adas.lka", &error);

        let logs = diag.drain_logs();
        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("max_retries"));
        assert!(logs[0].contains("adas.lka"));
    }

    #[test]
    fn file_diag_appends_and_reads_back() {
        let dir = std::env::temp_dir().join(format!("kos_diag_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let diag = FileDiag::new(dir.join("sub/incidents.log"));
        diag.notify_incident("adas.lka", &KosError::PermissionDenied("gave up\tnow".into()));
        diag.notify_incident("ivi.media", &KosError::InvalidConfig("hung".into()));
        let all = read_incidents(diag.path(), 10).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].app_id, "adas.lka");
        assert!(all[0].message.contains("gave up now"));
        assert!(all[0].unix_secs > 0);
        let last = read_incidents(diag.path(), 1).unwrap();
        assert_eq!(last[0].app_id, "ivi.media");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
