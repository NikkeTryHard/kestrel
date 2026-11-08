//! Connection helpers that block, used by the one-shot commands (`status`,
//! `replay`, `setup`). The daemon owns its connection lifecycle inside
//! `engine.rs`; only the reducer-outcome plumbing here is shared with it.
//!
//! SDK 2.13 threading model, checked against the `spacetimedb-sdk` source:
//! `DbConnection` holds `Arc`-shared state, and `run_threaded()` starts a
//! thread that `tokio::select!`s over arriving WebSocket messages and a
//! queue of "pending mutations". A reducer call (`conn.reducers.x_then(..)`)
//! from any thread only queues a mutation; that wakes the thread, which
//! sends it at once. Row callbacks run on the SDK thread after the whole
//! transaction reaches the client cache, so they — and any later cache
//! read — see the state left behind by that transaction.

use crate::wire::{DbConnection, ReducerEventContext, SubscriptionHandle};
use anyhow::{anyhow, Context, Result};
use spacetimedb_sdk::__codegen::InternalError;
use spacetimedb_sdk::{DbContext, Identity};
use std::sync::mpsc;
use std::time::{Duration, Instant};

pub type ReducerOutcome = std::result::Result<std::result::Result<(), String>, InternalError>;

/// Collapses the SDK's nested reducer outcome to `Ok` or the server's error text.
pub fn flatten_outcome(o: ReducerOutcome) -> std::result::Result<(), String> {
    match o {
        Ok(Ok(())) => Ok(()),
        Ok(Err(msg)) => Err(msg),
        Err(e) => Err(format!("internal error: {e}")),
    }
}

pub struct Session {
    pub conn: DbConnection,
    pub identity: Identity,
    pub token: String,
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.conn.disconnect();
    }
}

/// Opens a connection (anonymous if `token` is `None`) and awaits the identity.
pub fn connect(host: &str, db: &str, token: Option<String>, timeout: Duration) -> Result<Session> {
    let (tx, rx) = mpsc::channel::<std::result::Result<(Identity, String), String>>();
    let tx_err = tx.clone();
    let conn = DbConnection::builder()
        .with_uri(host)
        .with_database_name(db)
        .with_token(token)
        .on_connect(move |_, identity, token| {
            let _ = tx.send(Ok((identity, token.to_string())));
        })
        .on_connect_error(move |_, e| {
            let _ = tx_err.send(Err(e.to_string()));
        })
        .build()
        .with_context(|| format!("connecting to {db} at {host}"))?;
    conn.run_threaded();
    let (identity, token) = rx
        .recv_timeout(timeout)
        .map_err(|_| anyhow!("timed out connecting to {db} at {host}"))?
        .map_err(|e| anyhow!("connection refused: {e}"))?;
    Ok(Session { conn, identity, token })
}

/// Subscribes to `queries`, blocking until the first rows reach the cache.
pub fn subscribe_wait(conn: &DbConnection, queries: &[String], timeout: Duration) -> Result<SubscriptionHandle> {
    let (tx, rx) = mpsc::channel::<std::result::Result<(), String>>();
    let tx_err = tx.clone();
    let handle = conn
        .subscription_builder()
        .on_applied(move |_| {
            let _ = tx.send(Ok(()));
        })
        .on_error(move |_, e| {
            let _ = tx_err.send(Err(e.to_string()));
        })
        .subscribe(queries.to_vec());
    rx.recv_timeout(timeout)
        .map_err(|_| anyhow!("timed out waiting for subscription"))?
        .map_err(|e| anyhow!("subscription failed: {e}"))?;
    Ok(handle)
}

/// Invokes a reducer via its `_then` form and blocks for the server's answer.
pub fn call_wait<F>(invoke: F, timeout: Duration) -> Result<std::result::Result<(), String>>
where
    F: FnOnce(Box<dyn FnOnce(&ReducerEventContext, ReducerOutcome) + Send>) -> spacetimedb_sdk::Result<()>,
{
    let (tx, rx) = mpsc::channel();
    invoke(Box::new(move |_, outcome| {
        let _ = tx.send(flatten_outcome(outcome));
    }))
    .map_err(|e| anyhow!("could not send reducer call: {e}"))?;
    rx.recv_timeout(timeout).map_err(|_| anyhow!("timed out waiting for reducer result"))
}

/// Re-runs `cond` every 25 ms until it yields `Some` or `timeout` elapses.
pub fn wait_for<T>(timeout: Duration, mut cond: impl FnMut() -> Option<T>) -> Option<T> {
    let start = Instant::now();
    loop {
        if let Some(v) = cond() {
            return Some(v);
        }
        if start.elapsed() >= timeout {
            return None;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_flatten() {
        assert_eq!(flatten_outcome(Ok(Ok(()))), Ok(()));
        assert_eq!(flatten_outcome(Ok(Err("Auction closed".into()))), Err("Auction closed".into()));
    }

    #[test]
    fn wait_for_times_out_and_succeeds() {
        assert_eq!(wait_for(Duration::from_millis(30), || None::<u8>), None);
        let mut n = 0;
        assert_eq!(
            wait_for(Duration::from_secs(1), || {
                n += 1;
                (n > 2).then_some(n)
            }),
            Some(3)
        );
    }
}
