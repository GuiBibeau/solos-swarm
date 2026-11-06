//! The `health` section: what `agent health`, the check systemd runs beside
//! the agent, treats as trouble. Every field has a default, so the section is
//! optional; it exists to move the disk floor.

use super::AgentConfig;
use anyhow::{Result, ensure};
use serde::Deserialize;

const GIB: u64 = 1 << 30;

#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthConfig {
    /// Free space, in GB (GiB, as `df -BG` counts), on the state directory's
    /// filesystem below which `agent health` pauses new work and alerts.
    /// `0` turns the floor off.
    #[serde(default = "default_min_free_gb")]
    pub min_free_gb: u64,
    /// Free space at which a pause it made is lifted again, so a disk that
    /// hovers at the floor does not flap. Default: the floor plus a fifth of
    /// it, at least 1 GB more.
    #[serde(default)]
    pub resume_free_gb: Option<u64>,
    /// Automatic restarts of the unit since the last check that make it a
    /// crash loop.
    #[serde(default = "default_restart_burst")]
    pub restart_burst: u64,
}

fn default_min_free_gb() -> u64 {
    5
}

fn default_restart_burst() -> u64 {
    3
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            min_free_gb: default_min_free_gb(),
            resume_free_gb: None,
            restart_burst: default_restart_burst(),
        }
    }
}

impl HealthConfig {
    pub fn min_free_bytes(&self) -> u64 {
        self.min_free_gb.saturating_mul(GIB)
    }

    pub fn resume_free_bytes(&self) -> u64 {
        match self.resume_free_gb {
            Some(gb) => gb.saturating_mul(GIB),
            None => self
                .min_free_bytes()
                .saturating_add((self.min_free_bytes() / 5).max(GIB)),
        }
    }
}

impl AgentConfig {
    pub(super) fn validate_health(&self) -> Result<()> {
        let Some(health) = &self.health else {
            return Ok(());
        };
        ensure!(
            health.restart_burst >= 1,
            "health.restart_burst must be at least 1"
        );
        if let Some(resume) = health.resume_free_gb {
            ensure!(
                resume > health.min_free_gb,
                "health.resume_free_gb ({resume}) must be above health.min_free_gb ({})",
                health.min_free_gb
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{GIB, HealthConfig};
    use crate::agent::AgentConfig;

    fn parse(health: &str) -> anyhow::Result<AgentConfig> {
        let config: AgentConfig = serde_yaml::from_str(&format!(
            "policy:\n  people: [slack:alice]\nsources:\n  - kind: jsonl\n    path: t.jsonl\n\
             channels:\n  - kind: jsonl\n    source: slack\n    path: o.jsonl\n\
             router: {{kind: scripted}}\nexecutor: {{kind: scripted}}\n{health}"
        ))?;
        config.validate()?;
        Ok(config)
    }

    #[test]
    fn defaults_floor_five_gb_and_resume_a_gb_above_it() {
        let config = HealthConfig::default();
        assert_eq!(
            (config.min_free_bytes(), config.resume_free_bytes()),
            (5 * GIB, 6 * GIB)
        );
        let big = HealthConfig {
            min_free_gb: 50,
            ..HealthConfig::default()
        };
        assert_eq!(
            big.resume_free_bytes(),
            60 * GIB,
            "a fifth, when that is more"
        );
        assert_eq!(parse("").unwrap().health, None);
    }

    #[test]
    fn a_section_overrides_and_nonsense_is_refused() {
        let tuned = parse("health: {min_free_gb: 10, resume_free_gb: 15, restart_burst: 2}\n")
            .unwrap()
            .health
            .unwrap();
        assert_eq!(
            (
                tuned.min_free_bytes(),
                tuned.resume_free_bytes(),
                tuned.restart_burst
            ),
            (10 * GIB, 15 * GIB, 2)
        );
        assert_eq!(
            parse("health: {min_free_gb: 0}\n")
                .unwrap()
                .health
                .unwrap()
                .min_free_bytes(),
            0,
            "zero turns the floor off"
        );
        for (case, yaml) in [
            (
                "a resume at the floor",
                "health: {min_free_gb: 10, resume_free_gb: 10}\n",
            ),
            ("no restarts that count", "health: {restart_burst: 0}\n"),
            ("an unknown field", "health: {floor: 5}\n"),
        ] {
            assert!(parse(yaml).is_err(), "{case}");
        }
    }
}
