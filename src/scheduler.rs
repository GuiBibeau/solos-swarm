//! Placement is a pure proposal; capacity, allocations and ledger holds commit together.

use crate::cluster::{Cluster, Mutation, Transaction, TxnResult};
use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Resources {
    pub cpu_millicores: u64,
    pub memory_bytes: u64,
    pub provider_slots: u64,
    pub rate_tokens: u64,
}
impl Resources {
    pub fn checked_add(self, other: Self) -> Option<Self> {
        Some(Self {
            cpu_millicores: self.cpu_millicores.checked_add(other.cpu_millicores)?,
            memory_bytes: self.memory_bytes.checked_add(other.memory_bytes)?,
            provider_slots: self.provider_slots.checked_add(other.provider_slots)?,
            rate_tokens: self.rate_tokens.checked_add(other.rate_tokens)?,
        })
    }
    pub fn fits(self, capacity: Self) -> bool {
        self.cpu_millicores <= capacity.cpu_millicores
            && self.memory_bytes <= capacity.memory_bytes
            && self.provider_slots <= capacity.provider_slots
            && self.rate_tokens <= capacity.rate_tokens
    }
    pub fn subtract(self, other: Self) -> Option<Self> {
        Some(Self {
            cpu_millicores: self.cpu_millicores.checked_sub(other.cpu_millicores)?,
            memory_bytes: self.memory_bytes.checked_sub(other.memory_bytes)?,
            provider_slots: self.provider_slots.checked_sub(other.provider_slots)?,
            rate_tokens: self.rate_tokens.checked_sub(other.rate_tokens)?,
        })
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub capacity: Resources,
    pub used: Resources,
    pub labels: BTreeMap<String, String>,
    pub capabilities: BTreeSet<String>,
    pub healthy: bool,
    pub provider_rate_remaining: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Bundle {
    pub agent_id: String,
    pub placement_intent_id: String,
    pub incarnation: u64,
    pub resources: Resources,
    pub labels: BTreeMap<String, String>,
    pub required_capabilities: BTreeSet<String>,
    pub budget_microusd: u64,
}
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Strategy {
    Pack,
    StrictPack,
    Spread,
    StrictSpread,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Gang {
    pub id: String,
    pub goal_id: String,
    pub strategy: Strategy,
    pub bundles: Vec<Bundle>,
    pub priority: i64,
    pub created_revision: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Allocation {
    pub agent_id: String,
    pub gang_id: String,
    pub goal_id: String,
    pub node_id: String,
    pub placement_intent_id: String,
    pub budget_envelope_ref: String,
    pub incarnation: u64,
    pub resources: Resources,
    pub fence: u64,
}
#[derive(Clone, Debug)]
pub struct PlacementSnapshot {
    pub revision: u64,
    pub nodes: BTreeMap<String, Node>,
    pub allocations: BTreeMap<String, Allocation>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Plan {
    pub expected_revision: u64,
    pub gang_id: String,
    pub allocations: Vec<Allocation>,
    pub updated_nodes: BTreeMap<String, Node>,
    pub budget_requests: BTreeMap<String, u64>,
    pub stop_allocations: Vec<String>,
}

/// Membership is authoritative at commit, even before the health reconciler updates a node row.
pub(crate) fn node_lease_checks<'a>(
    view: &crate::cluster::StateView,
    nodes: impl IntoIterator<Item = &'a String>,
) -> Result<Vec<crate::cluster::LeaseCheck>> {
    let now = crate::cluster::now_ms();
    nodes
        .into_iter()
        .map(|node| {
            let id = format!("node/{node}");
            let lease = view
                .leases
                .get(&id)
                .ok_or_else(|| anyhow::anyhow!("placement node membership lease absent"))?;
            ensure!(
                !lease.revoked && lease.holder == *node && lease.expires_ms > now,
                "placement node membership expired or fenced"
            );
            Ok(crate::cluster::LeaseCheck {
                id,
                holder: node.clone(),
                fence: lease.fence,
            })
        })
        .collect()
}

pub trait QueueSort: Send + Sync {
    fn key(&self, gang: &Gang) -> (i64, u64, String);
}
pub trait Filter: Send + Sync {
    fn feasible(&self, bundle: &Bundle, node: &Node) -> bool;
}
pub trait Score: Send + Sync {
    fn score(&self, bundle: &Bundle, node: &Node, same_gang: usize) -> i64;
}
pub trait Normalize: Send + Sync {
    fn normalize(&self, scores: &mut BTreeMap<String, i64>);
}
/// Return pure ledger mutations; the scheduler never owns a second spend counter.
pub trait Reserve: Send + Sync {
    fn compares(&self) -> Result<Vec<crate::cluster::Compare>> {
        Ok(vec![])
    }
    fn mutations(&self, plan: &Plan) -> Result<Vec<Mutation>>;
}
/// Release unused allocation only; dispatched and unknown holds remain authoritative.
pub trait Unreserve: Send + Sync {
    fn compares(&self) -> Result<Vec<crate::cluster::Compare>> {
        Ok(vec![])
    }
    fn mutations(&self, allocations: &[Allocation]) -> Result<Vec<Mutation>>;
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Permission {
    Allow,
    Deny(String),
    Wait(String),
}
pub trait Permit: Send + Sync {
    fn permit(&self, plan: &Plan) -> Permission;
}

pub struct PriorityQueue;
impl QueueSort for PriorityQueue {
    fn key(&self, gang: &Gang) -> (i64, u64, String) {
        (-gang.priority, gang.created_revision, gang.id.clone())
    }
}
pub struct Capacity;
impl Filter for Capacity {
    fn feasible(&self, bundle: &Bundle, node: &Node) -> bool {
        node.used
            .checked_add(bundle.resources)
            .is_some_and(|used| used.fits(node.capacity))
            && bundle.resources.rate_tokens <= node.provider_rate_remaining
    }
}
pub struct LabelsAndCapabilities;
impl Filter for LabelsAndCapabilities {
    fn feasible(&self, bundle: &Bundle, node: &Node) -> bool {
        bundle
            .labels
            .iter()
            .all(|(key, value)| node.labels.get(key) == Some(value))
            && bundle.required_capabilities.is_subset(&node.capabilities)
    }
}
pub struct Health;
impl Filter for Health {
    fn feasible(&self, _bundle: &Bundle, node: &Node) -> bool {
        node.healthy
    }
}
pub struct BinPack;
impl Score for BinPack {
    fn score(&self, _bundle: &Bundle, node: &Node, same_gang: usize) -> i64 {
        let utilization = if node.capacity.memory_bytes == 0 {
            0
        } else {
            (u128::from(node.used.memory_bytes) * 100 / u128::from(node.capacity.memory_bytes))
                as i64
        };
        utilization - same_gang as i64 * 5 + node.provider_rate_remaining.min(100) as i64
    }
}
pub struct Spread;
impl Score for Spread {
    fn score(&self, _bundle: &Bundle, node: &Node, same_gang: usize) -> i64 {
        -(same_gang as i64) * 1000 - node.used.provider_slots as i64
    }
}
pub struct NormalizeRange;
impl Normalize for NormalizeRange {
    fn normalize(&self, scores: &mut BTreeMap<String, i64>) {
        let min = scores.values().copied().min().unwrap_or(0);
        let max = scores.values().copied().max().unwrap_or(0);
        for value in scores.values_mut() {
            *value = if max == min {
                100
            } else {
                (((i128::from(*value) - i128::from(min)) * 100)
                    / (i128::from(max) - i128::from(min))) as i64
            };
        }
    }
}
pub struct Admit;
impl Permit for Admit {
    fn permit(&self, _plan: &Plan) -> Permission {
        Permission::Allow
    }
}

pub struct Scheduler {
    pub queue_sort: Box<dyn QueueSort>,
    pub filters: Vec<Box<dyn Filter>>,
    pub pack_score: Box<dyn Score>,
    pub spread_score: Box<dyn Score>,
    pub normalize: Box<dyn Normalize>,
    pub permit: Box<dyn Permit>,
    queue: BTreeMap<String, Gang>,
    queue_limit: usize,
}
impl Scheduler {
    pub fn new(queue_limit: usize) -> Result<Self> {
        ensure!(
            queue_limit > 0,
            "scheduler queue limit must be finite and positive"
        );
        Ok(Self {
            queue_sort: Box::new(PriorityQueue),
            filters: vec![
                Box::new(Capacity),
                Box::new(LabelsAndCapabilities),
                Box::new(Health),
            ],
            pack_score: Box::new(BinPack),
            spread_score: Box::new(Spread),
            normalize: Box::new(NormalizeRange),
            permit: Box::new(Admit),
            queue: BTreeMap::new(),
            queue_limit,
        })
    }
    pub fn enqueue(&mut self, gang: Gang) -> Result<()> {
        ensure!(
            !gang.id.is_empty() && !gang.bundles.is_empty() && gang.bundles.len() <= 500,
            "invalid gang"
        );
        ensure!(
            self.queue.contains_key(&gang.id) || self.queue.len() < self.queue_limit,
            "scheduler busy"
        );
        self.queue.insert(gang.id.clone(), gang);
        Ok(())
    }
    pub fn pop(&mut self) -> Option<Gang> {
        let id = self
            .queue
            .values()
            .min_by_key(|gang| self.queue_sort.key(gang))?
            .id
            .clone();
        self.queue.remove(&id)
    }
    pub fn depth(&self) -> usize {
        self.queue.len()
    }
    pub fn plan(&self, gang: &Gang, snapshot: &PlacementSnapshot) -> Result<Plan> {
        ensure!(
            !gang.bundles.is_empty() && gang.bundles.len() <= 500,
            "gang size outside admission bounds"
        );
        let ids = gang
            .bundles
            .iter()
            .map(|bundle| &bundle.agent_id)
            .collect::<BTreeSet<_>>();
        ensure!(ids.len() == gang.bundles.len(), "duplicate gang agent");
        ensure!(
            gang.bundles
                .iter()
                .all(|bundle| !bundle.placement_intent_id.is_empty() && bundle.budget_microusd > 0),
            "missing placement intent or budget envelope"
        );
        let mut nodes = snapshot.nodes.clone();
        let mut selected = BTreeMap::<String, usize>::new();
        for allocation in snapshot
            .allocations
            .values()
            .filter(|allocation| allocation.gang_id == gang.id)
        {
            *selected.entry(allocation.node_id.clone()).or_default() += 1;
        }
        let strict_node = if gang.strategy == Strategy::StrictPack {
            let mut candidates = vec![];
            for (id, node) in &nodes {
                let mut staged = node.clone();
                let mut feasible = true;
                for bundle in &gang.bundles {
                    if !self
                        .filters
                        .iter()
                        .all(|filter| filter.feasible(bundle, &staged))
                    {
                        feasible = false;
                        break;
                    }
                    staged.used = staged
                        .used
                        .checked_add(bundle.resources)
                        .expect("capacity filter checked");
                    staged.provider_rate_remaining -= bundle.resources.rate_tokens;
                }
                if feasible {
                    candidates.push((
                        self.pack_score.score(
                            &gang.bundles[0],
                            node,
                            *selected.get(id).unwrap_or(&0),
                        ),
                        id.clone(),
                    ));
                }
            }
            Some(
                candidates
                    .into_iter()
                    .max()
                    .ok_or_else(|| {
                        anyhow::anyhow!("infeasible strict-pack gang; nothing reserved")
                    })?
                    .1,
            )
        } else {
            None
        };
        let mut allocations = vec![];
        let mut budgets = BTreeMap::new();
        for bundle in &gang.bundles {
            let mut scores = BTreeMap::new();
            for (id, node) in &nodes {
                if strict_node.as_ref().is_some_and(|required| required != id)
                    || gang.strategy == Strategy::StrictSpread
                        && selected.get(id).copied().unwrap_or(0) > 0
                    || !self
                        .filters
                        .iter()
                        .all(|filter| filter.feasible(bundle, node))
                {
                    continue;
                }
                let same = selected.get(id).copied().unwrap_or(0);
                let score = if matches!(gang.strategy, Strategy::Spread | Strategy::StrictSpread) {
                    self.spread_score.score(bundle, node, same)
                } else {
                    self.pack_score.score(bundle, node, same)
                };
                scores.insert(id.clone(), score);
            }
            self.normalize.normalize(&mut scores);
            let node_id = scores
                .into_iter()
                .max_by(|left, right| left.1.cmp(&right.1).then_with(|| right.0.cmp(&left.0)))
                .ok_or_else(|| anyhow::anyhow!("infeasible gang; no partial reservation"))?
                .0;
            let node = nodes.get_mut(&node_id).expect("ranked existing node");
            node.used = node
                .used
                .checked_add(bundle.resources)
                .expect("capacity checked");
            node.provider_rate_remaining -= bundle.resources.rate_tokens;
            *selected.entry(node_id.clone()).or_default() += 1;
            let envelope = format!("envelope/{}", bundle.placement_intent_id);
            ensure!(
                budgets
                    .insert(envelope.clone(), bundle.budget_microusd)
                    .is_none(),
                "duplicate placement envelope"
            );
            allocations.push(Allocation {
                agent_id: bundle.agent_id.clone(),
                gang_id: gang.id.clone(),
                goal_id: gang.goal_id.clone(),
                node_id,
                placement_intent_id: bundle.placement_intent_id.clone(),
                budget_envelope_ref: envelope,
                incarnation: bundle.incarnation,
                resources: bundle.resources,
                fence: snapshot.revision + 1,
            });
        }
        nodes.retain(|id, _| {
            allocations
                .iter()
                .any(|allocation| &allocation.node_id == id)
        });
        let plan = Plan {
            expected_revision: snapshot.revision,
            gang_id: gang.id.clone(),
            allocations,
            updated_nodes: nodes,
            budget_requests: budgets,
            stop_allocations: vec![],
        };
        match self.permit.permit(&plan) {
            Permission::Allow => Ok(plan),
            Permission::Deny(reason) => anyhow::bail!("placement denied: {reason}"),
            Permission::Wait(reason) => anyhow::bail!("placement waiting: {reason}"),
        }
    }
    pub async fn bind(
        &self,
        cluster: &Cluster,
        plan: &Plan,
        reserve: &dyn Reserve,
        lease_ms: u64,
    ) -> Result<TxnResult> {
        let _write = cluster.kernel_write(false).await;
        self.bind_while_locked(cluster, plan, reserve, lease_ms)
            .await
    }
    /// Runtime holds the shared gate before the fresh state snapshot and placement plan.
    /// Standalone callers use `bind`, which acquires the gate and rejects an older plan.
    pub(crate) async fn bind_while_locked(
        &self,
        cluster: &Cluster,
        plan: &Plan,
        reserve: &dyn Reserve,
        lease_ms: u64,
    ) -> Result<TxnResult> {
        ensure!(
            (100..=3_600_000).contains(&lease_ms),
            "invalid allocation lease TTL"
        );
        let view = cluster.view().await?;
        ensure!(
            view.revision == plan.expected_revision,
            "placement plan is stale"
        );
        let mut recomputed = BTreeMap::<String, Node>::new();
        for allocation in &plan.allocations {
            if !recomputed.contains_key(&allocation.node_id) {
                let node: Node = serde_json::from_value(
                    view.kv
                        .get(&format!("nodes/{}", allocation.node_id))
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("authoritative placement node missing"))?,
                )?;
                recomputed.insert(allocation.node_id.clone(), node);
            }
            let node = recomputed
                .get_mut(&allocation.node_id)
                .expect("loaded node");
            ensure!(
                node.healthy
                    && node
                        .used
                        .checked_add(allocation.resources)
                        .is_some_and(|used| used.fits(node.capacity))
                    && node.provider_rate_remaining >= allocation.resources.rate_tokens,
                "leader rejects oversubscribed or unhealthy placement"
            );
            node.used = node
                .used
                .checked_add(allocation.resources)
                .expect("validated capacity");
            node.provider_rate_remaining -= allocation.resources.rate_tokens;
        }
        ensure!(
            serde_json::to_value(&recomputed)? == serde_json::to_value(&plan.updated_nodes)?,
            "placement capacity proposal does not match authoritative state"
        );
        let mut ops = reserve.mutations(plan)?;
        ensure!(
            !ops.is_empty(),
            "ledger reserve plugin returned no authoritative envelope mutations"
        );
        for (id, node) in &plan.updated_nodes {
            ops.push(Mutation::Put {
                key: format!("nodes/{id}"),
                value: serde_json::to_value(node)?,
            });
        }
        for allocation in &plan.allocations {
            ops.push(Mutation::Put {
                key: format!("allocations/{}", allocation.agent_id),
                value: serde_json::to_value(allocation)?,
            });
            ops.push(Mutation::GrantLease {
                id: format!("allocation/{}", allocation.agent_id),
                holder: allocation.node_id.clone(),
                expires_ms: crate::cluster::now_ms() + lease_ms,
            });
        }
        let mut transaction = Transaction::new(
            format!("placement/{}/{}", plan.gang_id, plan.expected_revision),
            ops,
        );
        transaction.expected_revision = Some(plan.expected_revision);
        transaction.leases = node_lease_checks(&view, plan.updated_nodes.keys())?;
        transaction.compares.extend(reserve.compares()?);
        cluster.transact(transaction).await
    }
    pub async fn unreserve(
        &self,
        cluster: &Cluster,
        allocations: &[Allocation],
        plugin: &dyn Unreserve,
    ) -> Result<TxnResult> {
        let _write = cluster.kernel_write(true).await;
        ensure!(!allocations.is_empty(), "empty unreserve selection");
        let transaction_id = format!(
            "unreserve/{}",
            allocations
                .iter()
                .map(|allocation| allocation.placement_intent_id.as_str())
                .collect::<Vec<_>>()
                .join("+")
        );
        if let Some(previous) = cluster.transaction_result(&transaction_id).await? {
            return Ok(previous);
        }
        let view = cluster.view().await?;
        let mut ops = plugin.mutations(allocations)?;
        let mut nodes = BTreeMap::<String, Node>::new();
        for allocation in allocations {
            let current = view.kv.get(&format!("allocations/{}", allocation.agent_id));
            if current != Some(&serde_json::to_value(allocation)?) {
                continue;
            }
            let node = if let Some(node) = nodes.get_mut(&allocation.node_id) {
                node
            } else {
                let node: Node = serde_json::from_value(
                    view.kv
                        .get(&format!("nodes/{}", allocation.node_id))
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("allocation node missing"))?,
                )?;
                nodes.entry(allocation.node_id.clone()).or_insert(node)
            };
            node.used = node
                .used
                .subtract(allocation.resources)
                .ok_or_else(|| anyhow::anyhow!("resource unreserve underflow"))?;
            ops.extend([
                Mutation::Delete {
                    key: format!("allocations/{}", allocation.agent_id),
                },
                Mutation::RevokeLease {
                    id: format!("allocation/{}", allocation.agent_id),
                },
            ]);
        }
        for (id, node) in nodes {
            ops.push(Mutation::Put {
                key: format!("nodes/{id}"),
                value: serde_json::to_value(node)?,
            });
        }
        ensure!(
            !ops.is_empty(),
            "already unreserved with no remaining effects"
        );
        let mut transaction = Transaction::new(transaction_id, ops);
        transaction.expected_revision = Some(view.revision);
        transaction.compares.extend(plugin.compares()?);
        cluster.transact(transaction).await
    }
    /// Reschedule only expired/lost bundles, keeping survivors active and priority elevated.
    pub fn reconcile(
        &mut self,
        desired: &[Gang],
        snapshot: &PlacementSnapshot,
    ) -> Result<Vec<String>> {
        let desired_agents = desired
            .iter()
            .flat_map(|gang| gang.bundles.iter().map(|bundle| bundle.agent_id.clone()))
            .collect::<BTreeSet<_>>();
        let stops = snapshot
            .allocations
            .keys()
            .filter(|id| !desired_agents.contains(*id))
            .cloned()
            .collect();
        for gang in desired {
            let mut missing = gang.clone();
            missing.bundles.retain(|bundle| {
                snapshot
                    .allocations
                    .get(&bundle.agent_id)
                    .is_none_or(|allocation| {
                        !snapshot
                            .nodes
                            .get(&allocation.node_id)
                            .is_some_and(|node| node.healthy)
                    })
            });
            if !missing.bundles.is_empty() {
                if missing.bundles.len() < gang.bundles.len() {
                    missing.priority = missing.priority.saturating_add(1_000_000);
                }
                self.enqueue(missing)?;
            }
        }
        Ok(stops)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn snapshot() -> PlacementSnapshot {
        PlacementSnapshot {
            revision: 4,
            nodes: (0..3)
                .map(|id| {
                    let id = id.to_string();
                    (
                        id.clone(),
                        Node {
                            id,
                            capacity: Resources {
                                cpu_millicores: 3000,
                                memory_bytes: 3000,
                                provider_slots: 3,
                                rate_tokens: 300,
                            },
                            used: Resources::default(),
                            labels: BTreeMap::new(),
                            capabilities: BTreeSet::new(),
                            healthy: true,
                            provider_rate_remaining: 300,
                        },
                    )
                })
                .collect(),
            allocations: BTreeMap::new(),
        }
    }
    fn gang(strategy: Strategy, count: usize) -> Gang {
        Gang {
            id: "gang".into(),
            goal_id: "goal".into(),
            strategy,
            bundles: (0..count)
                .map(|index| Bundle {
                    agent_id: format!("a{index}"),
                    placement_intent_id: format!("p{index}"),
                    incarnation: 1,
                    resources: Resources {
                        cpu_millicores: 1000,
                        memory_bytes: 1000,
                        provider_slots: 1,
                        rate_tokens: 100,
                    },
                    labels: BTreeMap::new(),
                    required_capabilities: BTreeSet::new(),
                    budget_microusd: 1000,
                })
                .collect(),
            priority: 0,
            created_revision: 0,
        }
    }
    #[test]
    fn strict_strategies_and_infeasible_gangs_never_partially_reserve() {
        let scheduler = Scheduler::new(10).unwrap();
        let before = snapshot();
        let pack = scheduler
            .plan(&gang(Strategy::StrictPack, 3), &before)
            .unwrap();
        assert_eq!(
            pack.allocations
                .iter()
                .map(|a| &a.node_id)
                .collect::<BTreeSet<_>>()
                .len(),
            1
        );
        let spread = scheduler
            .plan(&gang(Strategy::StrictSpread, 3), &before)
            .unwrap();
        assert_eq!(spread.updated_nodes.len(), 3);
        assert!(
            scheduler
                .plan(&gang(Strategy::StrictPack, 4), &before)
                .is_err()
        );
        assert!(
            scheduler
                .plan(&gang(Strategy::StrictSpread, 4), &before)
                .is_err()
        );
        assert!(
            before
                .nodes
                .values()
                .all(|n| n.used == Resources::default())
        );
    }
    #[test]
    fn healthy_row_cannot_override_expired_membership() {
        let mut view = crate::cluster::StateView::default();
        let node = "node-a".to_string();
        assert!(node_lease_checks(&view, [&node]).is_err());
        view.leases.insert(
            "node/node-a".into(),
            crate::cluster::Lease {
                holder: node.clone(),
                fence: 7,
                expires_ms: crate::cluster::now_ms() + 10000,
                revoked: false,
            },
        );
        assert_eq!(node_lease_checks(&view, [&node]).unwrap()[0].fence, 7);
        view.leases.get_mut("node/node-a").unwrap().expires_ms = 0;
        assert!(node_lease_checks(&view, [&node]).is_err());
        let lease = view.leases.get_mut("node/node-a").unwrap();
        lease.expires_ms = crate::cluster::now_ms() + 10000;
        lease.revoked = true;
        assert!(node_lease_checks(&view, [&node]).is_err());
    }
    #[test]
    fn queue_is_bounded_and_lost_bundles_take_priority() {
        let mut scheduler = Scheduler::new(1).unwrap();
        scheduler.enqueue(gang(Strategy::Pack, 1)).unwrap();
        let mut second = gang(Strategy::Spread, 1);
        second.id = "other".into();
        assert!(scheduler.enqueue(second).is_err());
        assert!(scheduler.pop().is_some());
        assert_eq!(scheduler.depth(), 0);
    }
}
