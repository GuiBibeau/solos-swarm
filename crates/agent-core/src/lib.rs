//! This crate does no I/O and has no async runtime: it is a state machine over
//! [`ports::Repositories`] that returns external work as
//! [`model::Effect`]s. Adapters in the `solos-swarm` crate perform them and
//! report back through [`Engine`] commands.

#![forbid(unsafe_code)]

pub mod engine;
pub mod ids;
pub mod memory;
pub mod model;
pub mod policy;
pub mod ports;

pub use engine::{Engine, Intake, MAX_EXPIRED_EVALS, MAX_SWARM_AGENTS};
pub use memory::MemoryRepositories;
pub use policy::{AcceptMode, AllowlistPolicy};
pub use ports::{Clock, Error, Gate, Policy, Repositories};
