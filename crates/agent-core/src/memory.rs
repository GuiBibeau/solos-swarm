use crate::model::{
    ChangeSet, Decision, DecisionId, Effect, EvalId, Event, Thread, ThreadKey, Ticket, TicketId,
    Trigger, TriggerId,
};
use crate::ports::{Error, Repositories};
use std::collections::BTreeMap;

#[derive(Clone, Debug, Default)]
pub struct MemoryRepositories {
    pub triggers: BTreeMap<TriggerId, Trigger>,
    pub threads: BTreeMap<ThreadKey, Thread>,
    pub tickets: BTreeMap<TicketId, Ticket>,
    pub decisions: BTreeMap<DecisionId, Decision>,
    pub effects: Vec<Effect>,
    pub events: Vec<Event>,
}

impl MemoryRepositories {
    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }
}

impl Repositories for MemoryRepositories {
    fn trigger(&self, id: &TriggerId) -> Option<Trigger> {
        self.triggers.get(id).cloned()
    }
    fn thread(&self, key: &ThreadKey) -> Option<Thread> {
        self.threads.get(key).cloned()
    }
    fn ticket(&self, id: &TicketId) -> Option<Ticket> {
        self.tickets.get(id).cloned()
    }
    fn decision(&self, id: &DecisionId) -> Option<Decision> {
        self.decisions.get(id).cloned()
    }
    fn active_threads(&self) -> Vec<ThreadKey> {
        self.threads
            .values()
            .filter(|thread| thread.is_active())
            .map(|thread| thread.key.clone())
            .collect()
    }
    fn thread_of_eval(&self, eval: &EvalId) -> Option<ThreadKey> {
        self.threads
            .values()
            .find(|thread| thread.eval.as_ref().is_some_and(|e| e.id == *eval))
            .map(|thread| thread.key.clone())
    }
    fn commit(&mut self, changes: ChangeSet) -> Result<(), Error> {
        for trigger in changes.triggers {
            self.triggers.insert(trigger.id.clone(), trigger);
        }
        self.threads.extend(changes.threads);
        self.tickets.extend(changes.tickets);
        for decision in changes.decisions {
            self.decisions.insert(decision.id.clone(), decision);
        }
        self.effects.extend(changes.effects);
        self.events.extend(changes.events);
        Ok(())
    }
}
