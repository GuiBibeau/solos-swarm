//! The model gate both modes share: pricing and receipts, the upstream
//! profiles and their transport, the request contract, the pause gate, and
//! the Anthropic Messages wire over the chat one.
//! Swarm mode's broker and agent mode's broker each keep their own ledger and
//! principals, and call these for everything else.

pub mod anthropic;
pub mod pause;
pub mod pricing;
pub mod upstream;

pub use pause::{effective_control, paused_reply, peak_retry_after, unix_secs};
pub use pricing::{PriceTable, Receipt, Usage, completion_usage};
pub use upstream::{
    Dialect, Dispatch, OpenAiCompatible, Provider, Upstream, admit_request, endpoint, provider_for,
    reasoning_model, upstream_body, upstream_profile, venice_model,
};
