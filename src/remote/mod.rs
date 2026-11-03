//! Separate execution (P9): an agent's worker runs its commands on a
//! user's machine. `solos-swarm node` ([`daemon`]) dials the agent, which
//! admits it by its pinned certificate ([`listener`]); the agent keeps the
//! session, the node only executes and answers ([`wire`]). Secrets stay on
//! the node ([`redact`]).

pub mod daemon;
pub mod listener;
pub mod redact;
pub mod tls;
pub mod wire;

pub use daemon::{Node, NodeConfig};
pub use listener::Listener;
pub use tls::Fingerprint;
