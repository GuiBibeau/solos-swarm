//! Lives at: crates/agent-store/src/ledger.rs (additions), and schema.rs
//! (the `kind` column). PR 1.
//!
//! An external hold is a `spend` row like a model call's, on the same
//! account, so `committed` and every cap see it. It differs in two ways:
//! a restart does not close it, since the job it holds for may still run on
//! Modal, and it settles once, at the metered total, with no tokens.
//!
//! ```sql
//! ALTER TABLE spend ADD COLUMN kind TEXT NOT NULL DEFAULT 'model'
//!     CHECK (kind IN ('model', 'external'));
//! ```

use crate::{SqliteStore, StoreError};
use solos_agent_core::model::{MicroUsd, Ms};

use crate::ledger::HoldId;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpendKind {
    /// A model call: closed at start if the last run left it open.
    Model,
    /// Spend outside the broker, such as a Modal job: open until its meter
    /// settles it, across restarts.
    External,
}

/// One cap the hold must fit, checked in the hold's own transaction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Cap<'a> {
    /// Everything the account committed since `since_ms`, both kinds: the
    /// Mandate budget.
    Account { since_ms: Ms, cap_microusd: MicroUsd },
    /// The account's `external` rows of `provider`: the meter's per-Ticket
    /// cap.
    AccountExternal {
        provider: &'a str,
        cap_microusd: MicroUsd,
    },
    /// Every account's `external` rows of `provider` since `since_ms`: the
    /// meter's daily cap.
    AllExternal {
        provider: &'a str,
        since_ms: Ms,
        cap_microusd: MicroUsd,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalHold<'a> {
    /// `ticket:<id>`.
    pub account: &'a str,
    /// The meter's name, such as `modal`.
    pub provider: &'a str,
    /// `job:<id>`, so a repeat after a crash finds the hold it made.
    pub request_id: &'a str,
    pub hold_microusd: MicroUsd,
    /// Each cap that applies. None: the hold is recorded and never refused.
    pub caps: Vec<Cap<'a>>,
    pub at_ms: Ms,
}

/// Which cap refused, and what it had committed, so the refusal can say how
/// long the job could run instead.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    pub cap: usize,
    pub committed: MicroUsd,
    pub cap_microusd: MicroUsd,
}

impl SqliteStore {
    /// Hold for an external job if it fits every cap, in one IMMEDIATE
    /// transaction, so two launches cannot both take the last of a cap. A
    /// hold already made for `request_id` is returned as it is.
    pub fn reserve_external(
        &mut self,
        hold: &ExternalHold<'_>,
    ) -> Result<Result<HoldId, Refusal>, StoreError> {
        todo!()
    }

    /// Close an external hold at what its meter reported. A cost above the
    /// hold is kept as reported, as `settle` does; the caller raises the
    /// overrun. Tokens stay NULL.
    pub fn settle_external(
        &mut self,
        id: HoldId,
        cost_microusd: MicroUsd,
        at_ms: Ms,
    ) -> Result<(), StoreError> {
        todo!()
    }

    // `close_open_holds` gains `AND kind = 'model'`: an external hold is
    // closed by its meter, or by `agent jobs settle --at-hold` through
    // `charge_hold`, never by a restart.
    //
    // `charge` (unchanged) takes a row that arrives after its job settled,
    // once per `meter:<hour>:<app>`, and the unattributed rows on
    // `external:<meter>`.
}

#[cfg(test)]
mod tests {
    // reserve_external_refuses_past_the_mandate_budget_counting_model_calls
    // reserve_external_refuses_past_the_meter_cap_counting_external_rows_only
    // reserve_external_refuses_past_the_daily_cap_across_accounts
    // two_reservations_racing_for_the_last_of_a_cap_take_it_once
    // a_repeat_with_the_same_request_id_returns_the_same_hold
    // close_open_holds_leaves_external_holds_open
    // settle_external_once_then_not_held
    // a_model_call_on_a_capped_ticket_sees_an_open_external_hold
}
