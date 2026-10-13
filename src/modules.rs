use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleSpec {
    pub id: String,
    pub dependencies: BTreeSet<String>,
    pub provides: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cleanup {
    pub id: String,
    pub kind: String,
    pub resource: String,
    pub done: bool,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ModuleRecord {
    pub spec: ModuleSpec,
    pub active: bool,
    pub unloaded: bool,
    pub cleanup: Vec<Cleanup>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CleanupReport {
    pub deactivated: Vec<String>,
    pub errors: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ModuleRegistry {
    pub modules: BTreeMap<String, ModuleRecord>,
}

impl ModuleRegistry {
    pub fn register(&mut self, spec: ModuleSpec) -> Result<()> {
        ensure!(
            !spec.id.is_empty()
                && !spec.provides.is_empty()
                && spec.dependencies.is_disjoint(&spec.provides),
            "invalid module declaration"
        );
        ensure!(
            !self.modules.contains_key(&spec.id),
            "module already registered"
        );
        ensure!(
            self.modules
                .values()
                .all(|m| (m.unloaded && m.cleanup.iter().all(|c| c.done))
                    || m.spec.provides.is_disjoint(&spec.provides)),
            "service already provided or awaiting cleanup"
        );
        self.modules.insert(
            spec.id.clone(),
            ModuleRecord {
                spec,
                active: false,
                unloaded: false,
                cleanup: vec![],
            },
        );
        self.activate();
        Ok(())
    }
    pub fn services(&self) -> BTreeSet<String> {
        self.modules
            .values()
            .filter(|m| m.active)
            .flat_map(|m| m.spec.provides.iter().cloned())
            .collect()
    }
    pub fn activate(&mut self) -> Vec<String> {
        let mut activated = vec![];
        loop {
            let services = self.services();
            let ready: Vec<_> = self
                .modules
                .iter()
                .filter(|(_, m)| {
                    !m.active
                        && !m.unloaded
                        && m.cleanup.iter().all(|c| c.done)
                        && m.spec.dependencies.is_subset(&services)
                })
                .map(|(id, _)| id.clone())
                .collect();
            if ready.is_empty() {
                break;
            }
            for id in ready {
                self.modules.get_mut(&id).unwrap().active = true;
                activated.push(id);
            }
        }
        activated
    }
    pub fn register_cleanup(&mut self, module_id: &str, mut cleanup: Cleanup) -> Result<()> {
        let module = self.modules.get_mut(module_id).context("unknown module")?;
        ensure!(
            module.active
                && !module.unloaded
                && !cleanup.id.is_empty()
                && !cleanup.kind.is_empty()
                && !cleanup.resource.is_empty(),
            "inactive module or invalid cleanup"
        );
        cleanup.done = false;
        cleanup.last_error = None;
        if let Some(existing) = module.cleanup.iter_mut().find(|c| c.id == cleanup.id) {
            ensure!(
                existing.done
                    && existing.kind == cleanup.kind
                    && existing.resource == cleanup.resource,
                "cleanup already registered or resource changed"
            );
            *existing = cleanup;
        } else {
            module.cleanup.push(cleanup);
        }
        Ok(())
    }
    /// Callbacks perform only idempotent, reversible, runtime-owned cleanup effects.
    pub fn unload(
        &mut self,
        module_id: &str,
        mut cleanup: impl FnMut(&Cleanup) -> Result<()>,
    ) -> Result<CleanupReport> {
        self.modules
            .get_mut(module_id)
            .context("unknown module")?
            .unloaded = true;
        let mut affected = BTreeSet::from([module_id.to_string()]);
        loop {
            let lost: BTreeSet<_> = affected
                .iter()
                .flat_map(|id| self.modules[id].spec.provides.iter().cloned())
                .collect();
            let dependents: Vec<_> = self
                .modules
                .iter()
                .filter(|(id, m)| {
                    !affected.contains(*id) && !m.spec.dependencies.is_disjoint(&lost)
                })
                .map(|(id, _)| id.clone())
                .collect();
            if dependents.is_empty() {
                break;
            }
            affected.extend(dependents);
        }
        // Withdraw every affected service before any cleanup callback can fail.
        for id in &affected {
            self.modules.get_mut(id).unwrap().active = false;
        }
        let mut order: Vec<_> = affected.iter().cloned().collect();
        order.sort_by_key(|id| std::cmp::Reverse(self.dependency_depth(id, &mut BTreeSet::new())));
        for id in &order {
            for effect in self
                .modules
                .get_mut(id)
                .unwrap()
                .cleanup
                .iter_mut()
                .rev()
                .filter(|c| !c.done)
            {
                match cleanup(effect) {
                    Ok(()) => {
                        effect.done = true;
                        effect.last_error = None;
                    }
                    Err(error) => effect.last_error = Some(error.to_string()),
                }
            }
        }
        let errors = self
            .modules
            .iter()
            .flat_map(|(id, m)| {
                m.cleanup.iter().filter_map(move |effect| {
                    effect
                        .last_error
                        .as_ref()
                        .map(|error| (format!("{id}/{}", effect.id), error.clone()))
                })
            })
            .collect();
        Ok(CleanupReport {
            deactivated: order,
            errors,
        })
    }
    pub fn retry_cleanup(
        &mut self,
        module_id: &str,
        mut cleanup: impl FnMut(&Cleanup) -> Result<()>,
    ) -> Result<()> {
        let module = self.modules.get_mut(module_id).context("unknown module")?;
        ensure!(
            !module.active,
            "active module cannot retry deactivation cleanup"
        );
        for effect in module.cleanup.iter_mut().rev().filter(|c| !c.done) {
            match cleanup(effect) {
                Ok(()) => {
                    effect.done = true;
                    effect.last_error = None;
                }
                Err(error) => effect.last_error = Some(error.to_string()),
            }
        }
        Ok(())
    }
    pub fn reload(&mut self, module_id: &str) -> Result<Vec<String>> {
        let provides = &self
            .modules
            .get(module_id)
            .context("unknown module")?
            .spec
            .provides;
        ensure!(
            self.modules.iter().all(|(id, m)| id == module_id
                || m.unloaded
                || m.spec.provides.is_disjoint(provides)),
            "another module owns the provided service"
        );
        let module = self.modules.get_mut(module_id).context("unknown module")?;
        ensure!(
            module.cleanup.iter().all(|c| c.done),
            "failed cleanup blocks reactivation"
        );
        module.unloaded = false;
        Ok(self.activate())
    }
    fn dependency_depth(&self, id: &str, seen: &mut BTreeSet<String>) -> usize {
        if !seen.insert(id.to_string()) {
            return 0;
        }
        let depth = self.modules[id]
            .spec
            .dependencies
            .iter()
            .filter_map(|service| {
                self.modules
                    .iter()
                    .find(|(_, m)| m.spec.provides.contains(service))
                    .map(|(parent, _)| self.dependency_depth(parent, seen) + 1)
            })
            .max()
            .unwrap_or(0);
        seen.remove(id);
        depth
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec(id: &str, dependencies: &[&str], provides: &[&str]) -> ModuleSpec {
        ModuleSpec {
            id: id.into(),
            dependencies: dependencies.iter().map(|s| s.to_string()).collect(),
            provides: provides.iter().map(|s| s.to_string()).collect(),
        }
    }
    #[test]
    fn dependencies_cleanup_failure_retry_and_repeated_unload() {
        let mut registry = ModuleRegistry::default();
        registry
            .register(spec("consumer", &["provider"], &["result"]))
            .unwrap();
        assert!(!registry.modules["consumer"].active);
        registry
            .register(spec("source", &[], &["provider"]))
            .unwrap();
        assert!(registry.modules["consumer"].active);
        for id in ["source", "consumer"] {
            registry
                .register_cleanup(
                    id,
                    Cleanup {
                        id: format!("cleanup-{id}"),
                        kind: "subscription".into(),
                        resource: id.into(),
                        done: false,
                        last_error: None,
                    },
                )
                .unwrap();
        }
        let order = registry
            .unload("source", |effect| {
                if effect.resource == "source" {
                    anyhow::bail!("injected failure")
                }
                Ok(())
            })
            .unwrap();
        assert_eq!(order.deactivated, vec!["consumer", "source"]);
        assert_eq!(order.errors.len(), 1);
        assert!(registry.services().is_empty());
        assert!(registry.modules["source"].cleanup[0].last_error.is_some());
        assert!(registry.reload("source").is_err());
        registry.retry_cleanup("source", |_| Ok(())).unwrap();
        let mut calls = 0;
        registry
            .unload("source", |_| {
                calls += 1;
                Ok(())
            })
            .unwrap();
        assert_eq!(calls, 0);
        registry.reload("source").unwrap();
        assert!(registry.modules["consumer"].active);
        registry
            .register_cleanup(
                "source",
                Cleanup {
                    id: "cleanup-source".into(),
                    kind: "subscription".into(),
                    resource: "source".into(),
                    done: false,
                    last_error: None,
                },
            )
            .unwrap();
        assert!(!registry.modules["source"].cleanup[0].done);
    }
    #[test]
    fn cycle_stays_inactive_and_provider_conflict_is_rejected() {
        let mut registry = ModuleRegistry::default();
        registry.register(spec("a", &["b"], &["a"])).unwrap();
        registry.register(spec("b", &["a"], &["b"])).unwrap();
        assert!(registry.services().is_empty());
        assert!(registry.register(spec("duplicate", &[], &["a"])).is_err());
    }
}
