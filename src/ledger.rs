//! One serializable ledger for placement allocations, action liabilities and all caps.
use crate::audit::{Hash, Signed, content_hash};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub total: u64,
    pub per_call: u64,
    pub per_tick: u64,
    pub tick_seconds: u64,
    pub per_day: u64,
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.total > 0
                && self.per_call > 0
                && self.per_tick > 0
                && self.tick_seconds > 0
                && self.per_day > 0,
            "missing cap means zero authority"
        );
        Ok(())
    }
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Rate {
    pub numerator: u64,
    pub denominator: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PriceTable {
    pub version: String,
    pub provider: String,
    pub model: String,
    pub currency: String,
    pub not_before: u64,
    pub expires: u64,
    pub rates: BTreeMap<String, Rate>,
}
impl PriceTable {
    pub fn validate(&self, now: u64) -> Result<()> {
        ensure!(
            !self.version.is_empty()
                && !self.provider.is_empty()
                && !self.model.is_empty()
                && !self.currency.is_empty()
                && self.not_before <= now
                && now < self.expires
                && !self.rates.is_empty(),
            "unknown/stale price configuration"
        );
        ensure!(
            self.rates.values().all(|r| r.denominator > 0),
            "invalid price denominator"
        );
        Ok(())
    }
    pub fn cost(&self, usage: &BTreeMap<String, u64>) -> Result<u64> {
        ensure!(!usage.is_empty(), "empty/unbounded charge categories");
        let mut total = 0u128;
        for (category, units) in usage {
            let rate = self
                .rates
                .get(category)
                .context("unpriced charge category")?;
            ensure!(rate.denominator > 0, "invalid price denominator");
            let amount =
                (*units as u128 * rate.numerator as u128).div_ceil(rate.denominator as u128);
            total = total.checked_add(amount).context("price amount overflow")?;
        }
        total.try_into().context("charge exceeds accounting range")
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Counter {
    pub spent: u64,
    pub held: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub limits: Limits,
    pub total: Counter,
    pub ticks: BTreeMap<u64, Counter>,
    pub days: BTreeMap<u64, Counter>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Envelope {
    pub id: String,
    pub placement_intent_id: String,
    pub placement_hash: Hash,
    pub parent: Option<String>,
    pub goal_id: String,
    pub holder: String,
    pub accounts: BTreeSet<String>,
    pub allocation: u64,
    pub spent: u64,
    pub held: u64,
    pub active: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum IntentState {
    Reserved,
    Dispatched,
    Unknown,
    Settled,
    Released,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReservationRequest {
    pub intent_id: String,
    pub envelope_id: String,
    pub goal_id: String,
    pub holder: String,
    pub capability_accounts: BTreeSet<String>,
    pub request_hash: Hash,
    pub usage_bound: BTreeMap<String, u64>,
    pub price_version: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reservation {
    pub request: ReservationRequest,
    pub state: IntentState,
    pub hold: u64,
    pub settled: u64,
    pub price: PriceTable,
    pub ancestors: Vec<String>,
    pub accounts: BTreeSet<String>,
    pub ticks: BTreeMap<String, u64>,
    pub day: u64,
    pub dispatch_hash: Option<Hash>,
    pub receipt: Option<Receipt>,
    pub overrun: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Receipt {
    pub id: String,
    pub provider: String,
    pub intent_id: String,
    pub request_hash: Hash,
    pub usage: BTreeMap<String, u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Settlement {
    pub amount: u64,
    pub released: u64,
    pub overrun: bool,
    pub duplicate: bool,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ledger {
    pub currency: String,
    pub accounts: BTreeMap<String, Account>,
    pub envelopes: BTreeMap<String, Envelope>,
    pub intents: BTreeMap<String, Reservation>,
    pub stopped_goals: BTreeSet<String>,
    receipts: BTreeMap<String, Hash>,
}
fn add(a: u64, b: u64) -> Result<u64> {
    a.checked_add(b).context("accounting overflow")
}
fn liability(counter: &Counter) -> Result<u64> {
    add(counter.spent, counter.held)
}
impl Ledger {
    pub fn new(currency: String) -> Result<Self> {
        ensure!(!currency.is_empty(), "accounting currency required");
        Ok(Self {
            currency,
            ..Self::default()
        })
    }
    pub fn add_account(&mut self, id: String, limits: Limits) -> Result<()> {
        limits.validate()?;
        ensure!(
            !id.is_empty() && !self.accounts.contains_key(&id),
            "account already exists/invalid ID"
        );
        self.accounts.insert(
            id,
            Account {
                limits,
                total: Counter::default(),
                ticks: BTreeMap::new(),
                days: BTreeMap::new(),
            },
        );
        Ok(())
    }
    /// Changing a ceiling retains every receipt and unresolved liability. Tick duration is
    /// immutable because prior reservations are indexed in that account's original windows.
    pub fn amend_account(&mut self, id: &str, limits: Limits) -> Result<()> {
        limits.validate()?;
        let account = self
            .accounts
            .get(id)
            .context("unknown accounting ceiling")?;
        ensure!(
            limits.tick_seconds == account.limits.tick_seconds,
            "window duration cannot change on a live ledger account"
        );
        ensure!(
            add(liability(&account.total)?, self.allocated_unused(id)?)? <= limits.total
                && account
                    .ticks
                    .values()
                    .all(|c| liability(c).is_ok_and(|n| n <= limits.per_tick))
                && account
                    .days
                    .values()
                    .all(|c| liability(c).is_ok_and(|n| n <= limits.per_day)),
            "new ceiling would erase committed expenditure or unresolved liability"
        );
        self.accounts.get_mut(id).unwrap().limits = limits;
        Ok(())
    }
    /// An authority amendment can cancel never-dispatched reservations. Dispatched or
    /// unknown effects retain their holds until a trusted receipt/reconciliation arrives.
    pub fn retire_goal_allocations(&mut self, goal: &str, proof_hash: Hash) -> Result<()> {
        ensure!(proof_hash != [0; 32], "amendment identity required");
        let undispatched: Vec<_> = self
            .intents
            .iter()
            .filter(|(_, r)| r.request.goal_id == goal && r.state == IntentState::Reserved)
            .map(|(id, _)| id.clone())
            .collect();
        for id in undispatched {
            self.release_proven_unbilled(&id, proof_hash)?;
        }
        let roots: Vec<_> = self
            .envelopes
            .values()
            .filter(|e| e.goal_id == goal && e.parent.is_none())
            .map(|e| e.id.clone())
            .collect();
        for id in roots {
            self.release_allocation(&id)?;
        }
        Ok(())
    }
    fn ancestors(&self, id: &str) -> Result<Vec<String>> {
        let mut chain = vec![];
        let mut next = Some(id.to_owned());
        let mut seen = BTreeSet::new();
        while let Some(id) = next {
            ensure!(
                seen.insert(id.clone()) && chain.len() < 5,
                "cyclic or too-deep envelope ancestry"
            );
            let env = self.envelopes.get(&id).context("unknown envelope")?;
            next = env.parent.clone();
            chain.push(id);
        }
        Ok(chain)
    }
    fn unused(env: &Envelope) -> u64 {
        env.allocation
            .saturating_sub(env.spent.saturating_add(env.held))
    }
    fn free(&self, id: &str) -> Result<u64> {
        let env = self.envelopes.get(id).context("unknown envelope")?;
        let children = self
            .envelopes
            .values()
            .filter(|c| c.active && c.parent.as_deref() == Some(id))
            .try_fold(0u64, |sum, c| add(sum, Self::unused(c)))?;
        Ok(Self::unused(env).saturating_sub(children))
    }
    fn allocated_unused(&self, account: &str) -> Result<u64> {
        self.envelopes
            .values()
            .filter(|e| e.active && e.parent.is_none() && e.accounts.contains(account))
            .try_fold(0, |sum, e| add(sum, Self::unused(e)))
    }
    /// The root state machine commits this and its fenced placement record together.
    pub fn allocate(&mut self, envelope: Envelope) -> Result<()> {
        ensure!(
            envelope.accounts.contains("global")
                && envelope
                    .accounts
                    .contains(&format!("goal/{}", envelope.goal_id))
                && envelope
                    .accounts
                    .iter()
                    .filter(|a| a.starts_with("tenant/") && a.len() > 7)
                    .count()
                    == 1,
            "allocation must charge its Goal, tenant and global ledger ceilings"
        );
        ensure!(
            !envelope.id.is_empty()
                && !envelope.placement_intent_id.is_empty()
                && envelope.allocation > 0
                && envelope.spent == 0
                && envelope.held == 0
                && envelope.active
                && !envelope.accounts.is_empty(),
            "invalid allocation"
        );
        if let Some(existing) = self.envelopes.get(&envelope.id) {
            ensure!(
                existing == &envelope,
                "placement ID reused with different allocation"
            );
            return Ok(());
        }
        ensure!(
            !self
                .envelopes
                .values()
                .any(|e| e.placement_intent_id == envelope.placement_intent_id),
            "placement intent reused"
        );
        ensure!(
            !self.stopped_goals.contains(&envelope.goal_id),
            "Goal stopped"
        );
        if let Some(parent) = &envelope.parent {
            let ancestor = self
                .envelopes
                .get(parent)
                .context("missing allocation parent")?;
            ensure!(
                ancestor.active
                    && ancestor.goal_id == envelope.goal_id
                    && ancestor.accounts == envelope.accounts,
                "allocation widens parent scope"
            );
            ensure!(
                self.ancestors(parent)?.len() < 5 && self.free(parent)? >= envelope.allocation,
                "parent allocation exhausted/depth cap"
            );
        } else {
            for id in &envelope.accounts {
                let account = self
                    .accounts
                    .get(id)
                    .context("missing allocation account/cap")?;
                ensure!(
                    add(
                        add(liability(&account.total)?, self.allocated_unused(id)?)?,
                        envelope.allocation
                    )? <= account.limits.total,
                    "allocation exceeds ancestor/global cap"
                );
            }
        }
        self.envelopes.insert(envelope.id.clone(), envelope);
        Ok(())
    }
    pub fn reserve(
        &mut self,
        request: ReservationRequest,
        prices: &Signed<PriceTable>,
        trusted_key: &Hash,
        now: u64,
    ) -> Result<Reservation> {
        if let Some(existing) = self.intents.get(&request.intent_id) {
            ensure!(
                existing.request == request,
                "action intent reused with different request"
            );
            return Ok(existing.clone());
        }
        ensure!(
            !request.intent_id.is_empty() && !self.stopped_goals.contains(&request.goal_id),
            "missing intent or stopped Goal"
        );
        prices.verify("price-table", trusted_key)?;
        prices.payload.validate(now)?;
        ensure!(
            prices.payload.version == request.price_version
                && prices.payload.currency == self.currency,
            "price/currency mismatch"
        );
        let env = self
            .envelopes
            .get(&request.envelope_id)
            .context("no placement envelope")?;
        ensure!(
            env.active && env.goal_id == request.goal_id && env.holder == request.holder,
            "allocation identity mismatch or released"
        );
        let hold = prices.payload.cost(&request.usage_bound)?;
        ensure!(self.free(&env.id)? >= hold, "Task envelope exhausted");
        let ancestors = self.ancestors(&env.id)?;
        for id in &ancestors {
            let env = &self.envelopes[id];
            ensure!(
                env.active && add(add(env.spent, env.held)?, hold)? <= env.allocation,
                "ancestor envelope exhausted/released"
            );
        }
        let accounts: BTreeSet<_> = env
            .accounts
            .union(&request.capability_accounts)
            .cloned()
            .collect();
        let day = now / 86400;
        let mut ticks = BTreeMap::new();
        for id in &accounts {
            let account = self
                .accounts
                .get(id)
                .context("missing Capability/ancestor cap")?;
            account.limits.validate()?;
            let tick = now / account.limits.tick_seconds;
            ticks.insert(id.clone(), tick);
            ensure!(
                hold <= account.limits.per_call
                    && add(liability(&account.total)?, hold)? <= account.limits.total,
                "total/per-call cap exhausted"
            );
            ensure!(
                add(
                    liability(account.ticks.get(&tick).unwrap_or(&Counter::default()))?,
                    hold
                )? <= account.limits.per_tick,
                "tick cap exhausted"
            );
            ensure!(
                add(
                    liability(account.days.get(&day).unwrap_or(&Counter::default()))?,
                    hold
                )? <= account.limits.per_day,
                "daily cap exhausted"
            );
        }
        let reservation = Reservation {
            request: request.clone(),
            state: IntentState::Reserved,
            hold,
            settled: 0,
            price: prices.payload.clone(),
            ancestors,
            accounts,
            ticks,
            day,
            dispatch_hash: None,
            receipt: None,
            overrun: false,
        };
        for id in &reservation.ancestors {
            self.envelopes.get_mut(id).unwrap().held += hold;
        }
        for id in &reservation.accounts {
            let account = self.accounts.get_mut(id).unwrap();
            account.total.held += hold;
            account.ticks.entry(reservation.ticks[id]).or_default().held += hold;
            account.days.entry(day).or_default().held += hold;
        }
        self.intents.insert(request.intent_id, reservation.clone());
        Ok(reservation)
    }
    pub fn mark_dispatched(&mut self, intent: &str, permit_hash: Hash) -> Result<()> {
        let reservation = self
            .intents
            .get_mut(intent)
            .context("unknown action intent")?;
        ensure!(
            reservation.state == IntentState::Reserved
                && !self.stopped_goals.contains(&reservation.request.goal_id),
            "duplicate or stopped dispatch"
        );
        reservation.state = IntentState::Dispatched;
        reservation.dispatch_hash = Some(permit_hash);
        Ok(())
    }
    pub fn mark_unknown(&mut self, intent: &str) -> Result<()> {
        let reservation = self.intents.get_mut(intent).context("unknown intent")?;
        ensure!(
            matches!(
                reservation.state,
                IntentState::Dispatched | IntentState::Unknown
            ),
            "only dispatched effects can become unknown"
        );
        reservation.state = IntentState::Unknown;
        Ok(())
    }
    pub fn settle(&mut self, receipt: Receipt) -> Result<Settlement> {
        let mut reservation = self
            .intents
            .get(&receipt.intent_id)
            .context("unknown billed action")?
            .clone();
        ensure!(
            receipt.provider == reservation.price.provider
                && receipt.request_hash == reservation.request.request_hash
                && !receipt.id.is_empty(),
            "receipt identity/request mismatch"
        );
        if let Some(existing) = &reservation.receipt {
            ensure!(existing == &receipt, "conflicting settlement receipt");
            return Ok(Settlement {
                amount: reservation.settled,
                released: reservation.hold.saturating_sub(reservation.settled),
                overrun: reservation.overrun,
                duplicate: true,
            });
        }
        ensure!(
            matches!(
                reservation.state,
                IntentState::Dispatched | IntentState::Unknown
            ),
            "unbilled/released action cannot settle"
        );
        let receipt_key = format!("{}:{}", receipt.provider, receipt.id);
        let receipt_hash = content_hash(&receipt)?;
        ensure!(
            !self.receipts.contains_key(&receipt_key),
            "receipt reused across action intents"
        );
        let amount = match reservation.price.cost(&receipt.usage) {
            Ok(amount) => amount,
            Err(error) => {
                self.stopped_goals
                    .insert(reservation.request.goal_id.clone());
                self.mark_unknown(&receipt.intent_id)?;
                return Err(error);
            }
        };
        // Preflight arithmetic so an error cannot partially update a durable transaction.
        for id in &reservation.accounts {
            let account = &self.accounts[id];
            add(account.total.spent, amount)?;
            add(account.ticks[&reservation.ticks[id]].spent, amount)?;
            add(account.days[&reservation.day].spent, amount)?;
        }
        for id in &reservation.ancestors {
            add(self.envelopes[id].spent, amount)?;
        }
        let overrun = amount > reservation.hold
            || receipt.usage.iter().any(|(category, actual)| {
                reservation
                    .request
                    .usage_bound
                    .get(category)
                    .is_none_or(|limit| actual > limit)
            });
        for id in &reservation.accounts {
            let account = self.accounts.get_mut(id).unwrap();
            account.total.held -= reservation.hold;
            account.total.spent += amount;
            let tick = account.ticks.get_mut(&reservation.ticks[id]).unwrap();
            tick.held -= reservation.hold;
            tick.spent += amount;
            let day = account.days.get_mut(&reservation.day).unwrap();
            day.held -= reservation.hold;
            day.spent += amount;
        }
        for id in &reservation.ancestors {
            let env = self.envelopes.get_mut(id).unwrap();
            env.held -= reservation.hold;
            env.spent += amount;
        }
        reservation.state = IntentState::Settled;
        reservation.settled = amount;
        reservation.receipt = Some(receipt);
        reservation.overrun = overrun;
        if overrun {
            self.stopped_goals
                .insert(reservation.request.goal_id.clone());
        }
        self.receipts.insert(receipt_key, receipt_hash);
        self.intents
            .insert(reservation.request.intent_id.clone(), reservation.clone());
        Ok(Settlement {
            amount,
            released: reservation.hold.saturating_sub(amount),
            overrun,
            duplicate: false,
        })
    }
    /// Caller must authenticate a trusted reconciliation record, not an agent assertion.
    pub fn release_proven_unbilled(&mut self, intent: &str, proof_hash: Hash) -> Result<u64> {
        ensure!(proof_hash != [0; 32], "reconciliation evidence required");
        let reservation = self
            .intents
            .get(intent)
            .context("unknown action intent")?
            .clone();
        if reservation.state == IntentState::Released {
            return Ok(0);
        }
        ensure!(
            !matches!(reservation.state, IntentState::Settled),
            "cannot undo billed effect"
        );
        for id in &reservation.ancestors {
            self.envelopes.get_mut(id).unwrap().held -= reservation.hold;
        }
        for id in &reservation.accounts {
            let account = self.accounts.get_mut(id).unwrap();
            account.total.held -= reservation.hold;
            account.ticks.get_mut(&reservation.ticks[id]).unwrap().held -= reservation.hold;
            account.days.get_mut(&reservation.day).unwrap().held -= reservation.hold;
        }
        self.intents.get_mut(intent).unwrap().state = IntentState::Released;
        Ok(reservation.hold)
    }
    pub fn release_allocation(&mut self, id: &str) -> Result<u64> {
        ensure!(self.envelopes.contains_key(id), "unknown allocation");
        if !self.envelopes[id].active {
            return Ok(0);
        }
        let descendants: Vec<_> = self
            .envelopes
            .keys()
            .filter_map(|child| {
                self.ancestors(child)
                    .ok()
                    .filter(|chain| chain.iter().any(|p| p == id))
                    .map(|_| child.clone())
            })
            .collect();
        let unused = Self::unused(&self.envelopes[id]);
        for child in descendants {
            self.envelopes.get_mut(&child).unwrap().active = false;
        }
        Ok(unused)
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(!self.currency.is_empty(), "missing ledger currency");
        for (id, account) in &self.accounts {
            account.limits.validate()?;
            let mut total = Counter::default();
            let mut ticks = BTreeMap::<u64, Counter>::new();
            let mut days = BTreeMap::<u64, Counter>::new();
            for reservation in self.intents.values().filter(|r| r.accounts.contains(id)) {
                let held = if matches!(
                    reservation.state,
                    IntentState::Reserved | IntentState::Dispatched | IntentState::Unknown
                ) {
                    reservation.hold
                } else {
                    0
                };
                let spent = if reservation.state == IntentState::Settled {
                    reservation.settled
                } else {
                    0
                };
                total.held = add(total.held, held)?;
                total.spent = add(total.spent, spent)?;
                let tick = ticks
                    .entry(*reservation.ticks.get(id).context("missing intent tick")?)
                    .or_default();
                tick.held = add(tick.held, held)?;
                tick.spent = add(tick.spent, spent)?;
                let day = days.entry(reservation.day).or_default();
                day.held = add(day.held, held)?;
                day.spent = add(day.spent, spent)?;
            }
            ensure!(
                total == account.total && ticks == account.ticks && days == account.days,
                "ledger counters do not match durable intents"
            );
        }
        for (id, env) in &self.envelopes {
            self.ancestors(id)?;
            let mut held = 0;
            let mut spent = 0;
            for reservation in self.intents.values().filter(|r| r.ancestors.contains(id)) {
                if matches!(
                    reservation.state,
                    IntentState::Reserved | IntentState::Dispatched | IntentState::Unknown
                ) {
                    held = add(held, reservation.hold)?;
                }
                if reservation.state == IntentState::Settled {
                    spent = add(spent, reservation.settled)?;
                }
            }
            ensure!(
                env.held == held && env.spent == spent,
                "envelope lost/double-counted liability"
            );
        }
        for (id, reservation) in &self.intents {
            ensure!(
                id == &reservation.request.intent_id
                    && reservation
                        .accounts
                        .iter()
                        .all(|a| self.accounts.contains_key(a))
                    && reservation.ancestors == self.ancestors(&reservation.request.envelope_id)?,
                "invalid durable reservation identity"
            );
            if let Some(receipt) = &reservation.receipt {
                ensure!(
                    self.receipts
                        .get(&format!("{}:{}", receipt.provider, receipt.id))
                        == Some(&content_hash(receipt)?),
                    "missing settled receipt deduplication"
                );
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::CryptoSigner;
    fn setup() -> (Ledger, Signed<PriceTable>, Hash) {
        let signer = CryptoSigner::from_seed([8; 32]);
        let prices = signer
            .sign(
                "price-table",
                PriceTable {
                    version: "v1".into(),
                    provider: "local".into(),
                    model: "m1".into(),
                    currency: "USD-microunits".into(),
                    not_before: 0,
                    expires: 10000,
                    rates: [(
                        "tokens".into(),
                        Rate {
                            numerator: 1,
                            denominator: 1,
                        },
                    )]
                    .into(),
                },
            )
            .unwrap();
        let mut ledger = Ledger::new("USD-microunits".into()).unwrap();
        for id in ["global", "tenant/t", "goal/g", "cap/c"] {
            ledger
                .add_account(
                    id.into(),
                    Limits {
                        total: 1000,
                        per_call: 100,
                        per_tick: 300,
                        tick_seconds: 10,
                        per_day: 500,
                    },
                )
                .unwrap();
        }
        ledger
            .allocate(Envelope {
                id: "root".into(),
                placement_intent_id: "p-root".into(),
                placement_hash: [1; 32],
                parent: None,
                goal_id: "g".into(),
                holder: "a".into(),
                accounts: ["global".into(), "tenant/t".into(), "goal/g".into()].into(),
                allocation: 900,
                spent: 0,
                held: 0,
                active: true,
            })
            .unwrap();
        (ledger, prices, signer.public_key())
    }
    fn request(id: &str, env: &str, holder: &str) -> ReservationRequest {
        ReservationRequest {
            intent_id: id.into(),
            envelope_id: env.into(),
            goal_id: "g".into(),
            holder: holder.into(),
            capability_accounts: ["cap/c".into()].into(),
            request_hash: [2; 32],
            usage_bound: [("tokens".into(), 50)].into(),
            price_version: "v1".into(),
        }
    }
    #[test]
    fn unknown_liabilities_survive_serde_release_and_once_only_settlement() {
        let (mut ledger, prices, key) = setup();
        ledger
            .reserve(request("i", "root", "a"), &prices, &key, 1)
            .unwrap();
        ledger.mark_dispatched("i", [3; 32]).unwrap();
        ledger.mark_unknown("i").unwrap();
        ledger.release_allocation("root").unwrap();
        let mut restored: Ledger =
            serde_json::from_slice(&serde_json::to_vec(&ledger).unwrap()).unwrap();
        restored.validate().unwrap();
        assert_eq!(restored.accounts["global"].total.held, 50);
        let receipt = Receipt {
            id: "r".into(),
            provider: "local".into(),
            intent_id: "i".into(),
            request_hash: [2; 32],
            usage: [("tokens".into(), 40)].into(),
        };
        assert_eq!(restored.settle(receipt.clone()).unwrap().amount, 40);
        assert!(restored.settle(receipt).unwrap().duplicate);
        restored.validate().unwrap();
        assert_eq!(
            restored.accounts["global"].total,
            Counter { spent: 40, held: 0 }
        );
    }
    #[test]
    fn child_allocations_prevent_sibling_reuse_and_account_once() {
        let (mut ledger, prices, key) = setup();
        let mut child = ledger.envelopes["root"].clone();
        child.id = "child".into();
        child.placement_intent_id = "pc".into();
        child.parent = Some("root".into());
        child.holder = "b".into();
        child.allocation = 600;
        ledger.allocate(child.clone()).unwrap();
        child.id = "sibling".into();
        child.placement_intent_id = "ps".into();
        assert!(ledger.allocate(child).is_err());
        ledger
            .reserve(request("i", "child", "b"), &prices, &key, 1)
            .unwrap();
        assert_eq!(ledger.envelopes["root"].held, 50);
        assert_eq!(ledger.envelopes["child"].held, 50);
        assert_eq!(ledger.accounts["global"].total.held, 50);
        ledger.validate().unwrap();
        assert!(
            ledger
                .reserve(request("i", "root", "a"), &prices, &key, 1)
                .is_err()
        );
        assert!(
            ledger
                .reserve(request("missing", "child", "wrong"), &prices, &key, 1)
                .is_err()
        );
    }
    #[test]
    fn overruns_are_recorded_and_stop_admission() {
        let (mut ledger, prices, key) = setup();
        ledger
            .reserve(request("i", "root", "a"), &prices, &key, 1)
            .unwrap();
        ledger.mark_dispatched("i", [1; 32]).unwrap();
        let result = ledger
            .settle(Receipt {
                id: "r".into(),
                provider: "local".into(),
                intent_id: "i".into(),
                request_hash: [2; 32],
                usage: [("tokens".into(), 120)].into(),
            })
            .unwrap();
        assert!(result.overrun);
        assert_eq!(ledger.accounts["global"].total.spent, 120);
        assert!(
            ledger
                .reserve(request("next", "root", "a"), &prices, &key, 1)
                .is_err()
        );
        ledger.validate().unwrap();
    }
    #[test]
    fn signed_prices_and_all_window_caps_fail_closed() {
        let (mut ledger, mut prices, key) = setup();
        prices.payload.rates.clear();
        assert!(
            ledger
                .reserve(request("i", "root", "a"), &prices, &key, 1)
                .is_err()
        );
        let (mut ledger, prices, key) = setup();
        ledger.accounts.get_mut("global").unwrap().limits.per_tick = 49;
        assert!(
            ledger
                .reserve(request("i", "root", "a"), &prices, &key, 1)
                .is_err()
        );
        assert!(
            ledger
                .reserve(request("i", "root", "a"), &prices, &key, 10001)
                .is_err()
        );
    }
    #[test]
    fn an_envelope_cannot_omit_global_or_goal_accounting() {
        let (mut ledger, _, _) = setup();
        let mut envelope = ledger.envelopes["root"].clone();
        envelope.id = "bypass".into();
        envelope.placement_intent_id = "bypass".into();
        envelope.allocation = 1;
        envelope.accounts.remove("global");
        assert!(ledger.allocate(envelope.clone()).is_err());
        envelope.accounts.insert("global".into());
        envelope.accounts.remove("goal/g");
        assert!(ledger.allocate(envelope).is_err());
    }
}
