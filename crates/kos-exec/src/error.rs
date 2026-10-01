// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KosError {
    NotFound(String),
    InvalidTransition(String),
    PermissionDenied(String),
    AlreadyExists(String),
    InvalidConfig(String),
    Timeout(String),
    Remote(String),
    Io(String),
    Failed(String),
}

impl core::fmt::Display for KosError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            KosError::NotFound(msg) => write!(f, "not found: {msg}"),
            KosError::InvalidTransition(msg) => write!(f, "invalid transition: {msg}"),
            KosError::PermissionDenied(msg) => write!(f, "permission denied: {msg}"),
            KosError::AlreadyExists(msg) => write!(f, "already exists: {msg}"),
            KosError::InvalidConfig(msg) => write!(f, "invalid config: {msg}"),
            KosError::Timeout(msg) => write!(f, "timeout: {msg}"),
            KosError::Remote(msg) => write!(f, "remote error: {msg}"),
            KosError::Io(msg) => write!(f, "io error: {msg}"),
            KosError::Failed(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for KosError {}

pub type Result<T> = core::result::Result<T, KosError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_not_found() {
        let e = KosError::NotFound("app-1".into());
        assert_eq!(format!("{e}"), "not found: app-1");
    }

    #[test]
    fn display_invalid_transition() {
        let e = KosError::InvalidTransition("Installed -> Running".into());
        assert_eq!(format!("{e}"), "invalid transition: Installed -> Running");
    }

    #[test]
    fn display_permission_denied() {
        let e = KosError::PermissionDenied("write from QM to AsilD".into());
        assert_eq!(format!("{e}"), "permission denied: write from QM to AsilD");
    }

    #[test]
    fn display_already_exists() {
        let e = KosError::AlreadyExists("domain-0".into());
        assert_eq!(format!("{e}"), "already exists: domain-0");
    }

    #[test]
    fn display_invalid_config() {
        let e = KosError::InvalidConfig("empty cores".into());
        assert_eq!(format!("{e}"), "invalid config: empty cores");
    }
}
