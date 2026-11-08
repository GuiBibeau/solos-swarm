//! The model gate both modes share: pricing and receipts, the upstream
//! profiles and their transport, the request contract, the pause gate, and
//! the Anthropic Messages wire, over the chat one or forwarded to a
//! provider's own.
//! Swarm mode's broker and agent mode's broker each keep their own ledger and
//! principals, and call these for everything else.

pub mod anthropic;
pub mod forward;
pub mod pause;
pub mod pricing;
pub mod upstream;

pub use pause::{effective_control, paused_reply, peak_retry_after, unix_secs};
pub use pricing::{PriceTable, Receipt, Usage, completion_usage};
pub use upstream::{
    AnthropicMessages, BothWires, Dialect, Dispatch, OpenAiCompatible, Provider, Temperature,
    Upstream, Wire, admit_request, endpoint, messages_for, provider_for, reasoning_model, sampled,
    thinking_body, upstream_body, upstream_profile, venice_model,
};
