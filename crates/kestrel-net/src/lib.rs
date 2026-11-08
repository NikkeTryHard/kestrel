//! SpacetimeDB transport: the generated module bindings plus the blocking
//! connection helpers used by the one-shot commands.

pub mod transport;
pub mod wire;

pub use transport::{call_wait, connect, flatten_outcome, subscribe_wait, wait_for, Session};
