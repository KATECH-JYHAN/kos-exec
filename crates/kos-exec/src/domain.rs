// SPDX-License-Identifier: Apache-2.0
// Copyright (c) 2026 Jun-young Han <jyhan@katech.re.kr>
// KATECH SDV Platform Research Center

use std::collections::HashMap;

use crate::error::{KosError, Result};

pub use kos_safety::AsilLevel;

#[derive(Debug, Clone)]
pub struct DomainConfig {
    pub id: String,
    pub asil: AsilLevel,
    pub cores: Vec<u32>,
    pub rt_priority: Option<u8>,
    pub memory_limit_mb: Option<u64>,
    pub max_pids: Option<u64>,
    pub cpu_quota: Option<u8>,
}

pub struct DomainController {
    domains: HashMap<String, DomainConfig>,
    app_map: HashMap<String, String>,
}

impl DomainController {
    pub fn from_config(domains: &[DomainConfig]) -> Result<Self> {
        let mut map = HashMap::new();
        for d in domains {
            if d.id.is_empty() {
                return Err(KosError::InvalidConfig("empty domain id".into()));
            }
            if let Some(p) = d.rt_priority {
                if !(1..=99).contains(&p) {
                    return Err(KosError::InvalidConfig(format!(
                        "domain '{}': rt_priority must be 1–99, got {p}",
                        d.id
                    )));
                }
            }
            if let Some(q) = d.cpu_quota {
                if !(1..=100).contains(&q) {
                    return Err(KosError::InvalidConfig(format!(
                        "domain '{}': cpu_quota must be 1–100, got {q}",
                        d.id
                    )));
                }
            }
            if map.contains_key(&d.id) {
                return Err(KosError::AlreadyExists(d.id.clone()));
            }
            map.insert(d.id.clone(), d.clone());
        }
        Ok(Self {
            domains: map,
            app_map: HashMap::new(),
        })
    }

    pub fn assign_app(&mut self, app_id: &str, domain_id: &str) -> Result<()> {
        if !self.domains.contains_key(domain_id) {
            return Err(KosError::NotFound(format!("domain {domain_id}")));
        }
        if self.app_map.contains_key(app_id) {
            return Err(KosError::AlreadyExists(format!("app {app_id}")));
        }
        self.app_map.insert(app_id.to_string(), domain_id.to_string());
        Ok(())
    }

    pub fn cores_for(&self, app_id: &str) -> Result<&[u32]> {
        let domain_id = self
            .app_map
            .get(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;
        Ok(&self.domains[domain_id].cores)
    }

    pub fn asil_for(&self, app_id: &str) -> Result<AsilLevel> {
        let domain_id = self
            .app_map
            .get(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;
        Ok(self.domains[domain_id].asil)
    }

    pub fn apps_in(&self, domain_id: &str) -> Vec<&str> {
        self.app_map
            .iter()
            .filter(|(_, did)| did.as_str() == domain_id)
            .map(|(aid, _)| aid.as_str())
            .collect()
    }

    pub fn domain_asil(&self, domain_id: &str) -> Result<AsilLevel> {
        self.domains
            .get(domain_id)
            .map(|d| d.asil)
            .ok_or_else(|| KosError::NotFound(format!("domain {domain_id}")))
    }

    pub fn domain_config_for(&self, app_id: &str) -> Result<&DomainConfig> {
        let domain_id = self
            .app_map
            .get(app_id)
            .ok_or_else(|| KosError::NotFound(format!("app {app_id}")))?;
        Ok(&self.domains[domain_id])
    }

    pub fn domain_ids(&self) -> Vec<&str> {
        self.domains.keys().map(|s| s.as_str()).collect()
    }

    pub fn domain_config(&self, domain_id: &str) -> Result<&DomainConfig> {
        self.domains
            .get(domain_id)
            .ok_or_else(|| KosError::NotFound(format!("domain {domain_id}")))
    }

    pub fn can_access(&self, from_app: &str, to_app: &str, write: bool) -> bool {
        let from_asil = match self.asil_for(from_app) {
            Ok(a) => a,
            Err(_) => return false,
        };
        let to_asil = match self.asil_for(to_app) {
            Ok(a) => a,
            Err(_) => return false,
        };

        kos_safety::can_access(from_asil, to_asil, write)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> DomainController {
        let configs = vec![
            DomainConfig {
                id: "safety".into(),
                asil: AsilLevel::AsilD,
                cores: vec![0, 1],
                rt_priority: None,
                memory_limit_mb: None,
                max_pids: None,
                cpu_quota: None,
            },
            DomainConfig {
                id: "infotainment".into(),
                asil: AsilLevel::QM,
                cores: vec![2, 3],
                rt_priority: None,
                memory_limit_mb: None,
                max_pids: None,
                cpu_quota: None,
            },
        ];
        let mut ctrl = DomainController::from_config(&configs).unwrap();
        ctrl.assign_app("brake-app", "safety").unwrap();
        ctrl.assign_app("media-app", "infotainment").unwrap();
        ctrl
    }

    #[test]
    fn domain_creation_and_app_assignment() {
        let ctrl = setup();
        assert_eq!(ctrl.asil_for("brake-app").unwrap(), AsilLevel::AsilD);
        assert_eq!(ctrl.asil_for("media-app").unwrap(), AsilLevel::QM);
        assert!(ctrl.apps_in("safety").contains(&"brake-app"));
    }

    #[test]
    fn cores_for_returns_correct_cores() {
        let ctrl = setup();
        assert_eq!(ctrl.cores_for("brake-app").unwrap(), &[0, 1]);
        assert_eq!(ctrl.cores_for("media-app").unwrap(), &[2, 3]);
    }

    #[test]
    fn can_access_qm_to_d_write_denied() {
        let ctrl = setup();
        assert!(!ctrl.can_access("media-app", "brake-app", true));
        assert!(ctrl.can_access("media-app", "brake-app", false));
    }

    #[test]
    fn can_access_d_to_qm_write_allowed() {
        let ctrl = setup();
        assert!(ctrl.can_access("brake-app", "media-app", false));
        assert!(ctrl.can_access("brake-app", "media-app", true));
    }

    #[test]
    fn empty_domain_id_rejected() {
        let configs = vec![DomainConfig {
            id: "".into(),
            asil: AsilLevel::QM,
            cores: vec![0],
            rt_priority: None,
            memory_limit_mb: None,
            max_pids: None,
            cpu_quota: None,
        }];
        assert!(matches!(
            DomainController::from_config(&configs),
            Err(KosError::InvalidConfig(_))
        ));
    }

    #[test]
    fn duplicate_domain_id_rejected() {
        let configs = vec![
            DomainConfig { id: "d1".into(), asil: AsilLevel::QM, cores: vec![0], rt_priority: None, memory_limit_mb: None, max_pids: None, cpu_quota: None },
            DomainConfig { id: "d1".into(), asil: AsilLevel::AsilA, cores: vec![1], rt_priority: None, memory_limit_mb: None, max_pids: None, cpu_quota: None },
        ];
        assert!(matches!(
            DomainController::from_config(&configs),
            Err(KosError::AlreadyExists(_))
        ));
    }

    #[test]
    fn duplicate_app_assignment_rejected() {
        let mut ctrl = setup();
        assert!(matches!(
            ctrl.assign_app("brake-app", "infotainment"),
            Err(KosError::AlreadyExists(_))
        ));
    }

    #[test]
    fn invalid_rt_priority_rejected() {
        let configs = vec![DomainConfig {
            id: "d".into(),
            asil: AsilLevel::QM,
            cores: vec![0],
            rt_priority: Some(0),
            memory_limit_mb: None,
            max_pids: None,
            cpu_quota: None,
        }];
        assert!(matches!(
            DomainController::from_config(&configs),
            Err(KosError::InvalidConfig(_))
        ));

        let configs = vec![DomainConfig {
            id: "d".into(),
            asil: AsilLevel::QM,
            cores: vec![0],
            rt_priority: Some(100),
            memory_limit_mb: None,
            max_pids: None,
            cpu_quota: None,
        }];
        assert!(matches!(
            DomainController::from_config(&configs),
            Err(KosError::InvalidConfig(_))
        ));
    }

    #[test]
    fn not_found_errors() {
        let ctrl = setup();
        assert!(matches!(
            ctrl.cores_for("ghost"),
            Err(KosError::NotFound(_))
        ));
        let mut ctrl2 = DomainController::from_config(&[]).unwrap();
        assert!(matches!(
            ctrl2.assign_app("a", "nope"),
            Err(KosError::NotFound(_))
        ));
    }
}
