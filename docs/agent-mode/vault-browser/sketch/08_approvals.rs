//! Lives at: src/agent/ports.rs (`SendOptions`, `Order`), src/agent/runtime.rs
//! (`job` for Sends, `command`), src/agent/slack/{mod.rs, channel.rs,
//! source.rs}. The core half (`OutboundMessage.ask`, `Ticket.secrets`,
//! `ask_secret`, `answer_secret`) is in 03_core.rs.
//!
//! A first use of a vault item on a Ticket asks a person in the Ticket's
//! thread, with Allow and Deny buttons. The only way to answer is a click:
//! Slack names the user on the socket the agent opened with its app token
//! (the budget approval's existing trust path), the core checks who may
//! answer, and the answer reaches the worker as new input. Message text,
//! page text and tool arguments have no path to a grant.
//!
//! Budget approvals move onto the same field (`OutboundMessage.ask`), so the
//! runtime stops recognizing them by comparing a message's text with
//! `ticket.attention`: one way to say "this message asks a person".

use solos_agent_core::model::TicketId;

// ---------------------------------------------------------------------------
// src/agent/ports.rs
// ---------------------------------------------------------------------------

/// `SendOptions.approval: bool` becomes `ask`: what buttons a Channel with
/// buttons offers. Channels without buttons (jsonl, mcp, github) ignore it,
/// as they ignore `approval` today. (Loses `Copy`: `Ask::Secret` holds a
/// String.)
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SendOptions {
    pub repeat: bool,
    /// `solos_agent_core::model::Ask` (03_core.rs), filtered by the runtime
    /// to asks that still stand.
    pub ask: Option<solos_agent_core::model::Ask>,
}

/// `Order` gains the answer to a secret ask (and loses `Copy`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Order {
    Approve,
    Stop,
    Secret { secret: String, allow: bool },
}

// ---------------------------------------------------------------------------
// src/agent/runtime.rs
// ---------------------------------------------------------------------------
//
// Runtime::job, Effect::Send:
//     // Buttons only while the ask still stands, so a Send retried after the answer
//     // posts a plain message.
//     let ask = message.ask.clone().filter(|ask| match ask {
//         Ask::Budget => ticket(&message).is_some_and(|t| t.approval == Approval::Waiting),
//         Ask::Secret { secret } => ticket(&message).is_some_and(|t|
//             matches!(t.secrets.get(secret), Some(SecretGrant::Waiting { .. }))),
//     });
//     Job::Send(message, SendOptions { repeat, ask })
//
// Runtime::command:
//     Order::Secret { secret, allow } => told(
//         self.engine.answer_secret(&ticket, &secret, &source, by.clone(), allow),
//         format!("{} by {who}: {secret} for this ticket.", if allow { "Allowed" } else { "Denied" }),
//     )?,
//     // The Engine decides who may answer (answerable_by, else Policy::may_approve);
//     // its refusal is the reply, as for Approve.

pub struct RuntimeChanges;

// ---------------------------------------------------------------------------
// src/agent/slack
// ---------------------------------------------------------------------------

/// mod.rs: two more action ids beside `solos_approve` and `solos_stop`.
pub const ALLOW_SECRET_ACTION: &str = "solos_secret_allow";
pub const DENY_SECRET_ACTION: &str = "solos_secret_deny";

/// channel.rs `body`: `ask: Some(Ask::Budget)` renders Approve and Stop as
/// today; `Some(Ask::Secret { secret })` renders "Allow for this ticket"
/// (primary) and "Deny" (danger), each with the value
/// `{"ticket":"tk_…","secret":"local/github-gui"}` (well under Slack's
/// 2,000-character button value). The message text is the Engine's, built
/// from vault metadata and the Ticket's title, escaped by `shape` as every
/// post is:
///
/// ```text
/// Triage my GitHub notifications wants to sign in to https://github.com as gui
/// (login github-gui) for the first time.            [Allow for this ticket] [Deny]
/// ```
pub fn secret_buttons(text: &str, ticket: &TicketId, secret: &str) -> serde_json::Value {
    todo!()
}

/// source.rs `commands`: the two new action ids parse their JSON value into
/// `Command { order: Order::Secret { secret, allow }, ticket, .. }`, with the
/// same channel filter, user and reply thread as Approve. A value that does
/// not parse is dropped, as an unknown action id is.
pub struct SourceChanges;

#[cfg(test)]
mod tests {
    // Against the fake Slack (tests/agent_slack.rs):
    // - a secret ask posts one message with the two buttons; a repeated Send after a crash
    //   finds it by its MessageId metadata and posts nothing.
    // - Allow by a listed user grants it; by an approver who is not in the item's --for list
    //   is refused with a reply; Deny records Denied; a second click replies "does not wait".
    // - a Send of an ask already answered goes out without buttons.
    // - the budget ask renders exactly as before (the golden body test passes unedited), now
    //   from `ask: Budget` rather than the attention text.
    // - a person typing "allow" in the thread grants nothing: it is an Input the worker reads.
}
