//! Schedules: named slots that fire as System Triggers, for proactive
//! sweeps. A Trigger's id names its slot's start, so a restart in the same
//! slot repeats an id the Engine already holds, and a start after a gap
//! fires only the latest slot. That includes the first start.

use super::ports::{BoxFuture, FactSink, Source, TriggerSink};
use super::utc;
use crate::broker::now_ms;
use anyhow::Result;
use serde_json::json;
use solos_agent_core::model::{Actor, Addressed, ChannelRef, Ms, ThreadKey, Trigger, TriggerId};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

/// The Source name schedule Triggers carry.
pub const SOURCE: &str = "schedule";

/// Wall-clock milliseconds since the Unix epoch; tests set their own.
pub type Clock = Arc<dyn Fn() -> Ms + Send + Sync>;

/// The longest sleep between two looks at the clock, so a clock that jumps
/// or a host that slept is noticed.
const TICK: Duration = Duration::from_secs(30);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Schedule {
    pub name: String,
    /// Slots start at `offset_ms + k * period_ms` after the Unix epoch, UTC.
    pub period_ms: Ms,
    pub offset_ms: Ms,
    pub text: String,
    /// Where its Tickets answer and ask; none means nobody hears from them.
    pub reply_to: Option<ChannelRef>,
    /// Threads besides the schedule's own, such as that of the issue it
    /// replies on, so an answer there reaches the Ticket.
    pub threads: Vec<ThreadKey>,
}

impl Schedule {
    /// The start of the latest slot at or before `now`.
    fn latest(&self, now: Ms) -> Option<Ms> {
        let since = now.checked_sub(self.offset_ms)?;
        let slots = since.checked_div(self.period_ms)?;
        Some(self.offset_ms + slots * self.period_ms)
    }

    fn next(&self, now: Ms) -> Option<Ms> {
        match self.latest(now) {
            Some(latest) => latest.checked_add(self.period_ms),
            None => (self.period_ms > 0).then_some(self.offset_ms),
        }
    }

    fn trigger(&self, slot: Ms) -> Trigger {
        let at = utc::format(slot);
        let mut threads = vec![ThreadKey::new(format!("{SOURCE}:{}", self.name))];
        threads.extend(self.threads.iter().cloned());
        Trigger {
            id: TriggerId::new(format!("{SOURCE}:{}:{at}", self.name)),
            source: SOURCE.into(),
            at_ms: slot,
            actor: Actor::system(self.name.clone()),
            addressed: Addressed::Direct,
            text: self.text.clone(),
            threads,
            reply_to: self.reply_to.iter().cloned().collect(),
            meta: json!({ "schedule": self.name, "slot": at, "slot_ms": slot }),
        }
    }
}

pub struct ScheduleSource {
    schedules: Vec<Schedule>,
    follow: bool,
    clock: Clock,
    tick: Duration,
}

impl ScheduleSource {
    /// With `follow` it fires each slot as it starts until stopped; without,
    /// it fires the latest slots once and ends, for tests and dry runs.
    pub fn new(schedules: Vec<Schedule>, follow: bool) -> Self {
        Self {
            schedules,
            follow,
            clock: Arc::new(now_ms),
            tick: TICK,
        }
    }

    /// Read `clock` instead of the system's, at least every `tick`.
    #[must_use]
    pub fn with_clock(mut self, clock: Clock, tick: Duration) -> Self {
        self.clock = clock;
        self.tick = tick;
        self
    }
}

impl Source for ScheduleSource {
    fn run(self: Box<Self>, sink: TriggerSink, _: FactSink) -> BoxFuture<'static, Result<()>> {
        Box::pin(async move {
            let mut fired: BTreeMap<String, Ms> = BTreeMap::new();
            loop {
                let now = (self.clock)();
                let batch: Vec<Trigger> = self
                    .schedules
                    .iter()
                    .filter_map(|schedule| {
                        let slot = schedule.latest(now)?;
                        let new = fired.insert(schedule.name.clone(), slot) != Some(slot);
                        new.then(|| schedule.trigger(slot))
                    })
                    .collect();
                if !batch.is_empty() && sink.send(batch).await.is_err() {
                    return Ok(());
                }
                if !self.follow {
                    return Ok(());
                }
                let next = self.schedules.iter().filter_map(|s| s.next(now)).min();
                let wait = next.map_or(self.tick, |next| {
                    Duration::from_millis(next.saturating_sub(now)).min(self.tick)
                });
                tokio::time::sleep(wait.max(Duration::from_millis(1))).await;
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Schedule;
    use crate::agent::utc;

    fn schedule(period_ms: u64, offset_ms: u64) -> Schedule {
        Schedule {
            name: "sweep".into(),
            period_ms,
            offset_ms,
            text: "Sweep.".into(),
            reply_to: None,
            threads: Vec::new(),
        }
    }

    fn at(text: &str) -> u64 {
        utc::parse(text).unwrap()
    }

    #[test]
    fn slots_start_at_whole_periods_from_the_epoch_plus_the_offset() {
        let hourly = schedule(3_600_000, 0);
        assert_eq!(
            hourly.latest(at("2026-11-02T12:59:59Z")),
            Some(at("2026-11-02T12:00:00Z"))
        );
        assert_eq!(
            hourly.latest(at("2026-11-02T13:00:00Z")),
            Some(at("2026-11-02T13:00:00Z"))
        );
        assert_eq!(
            hourly.next(at("2026-11-02T12:10:00Z")),
            Some(at("2026-11-02T13:00:00Z"))
        );
        let daily = schedule(86_400_000, 9 * 3_600_000);
        assert_eq!(
            daily.latest(at("2026-11-02T08:59:00Z")),
            Some(at("2026-11-01T09:00:00Z"))
        );
        assert_eq!(
            daily.latest(at("2026-11-02T09:00:00Z")),
            Some(at("2026-11-02T09:00:00Z"))
        );
        assert_eq!(daily.latest(0), None);
        assert_eq!(daily.next(0), Some(9 * 3_600_000));
        assert_eq!(schedule(0, 0).latest(at("2026-11-02T09:00:00Z")), None);
        assert_eq!(schedule(0, 0).next(0), None);
        let trigger = daily.trigger(at("2026-11-02T09:00:00Z"));
        assert_eq!(trigger.id.as_str(), "schedule:sweep:2026-11-02T09:00:00Z");
    }
}
