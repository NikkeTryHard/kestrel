//! # kestrel-policy
//!
//! Strategy for the ScrabbleBot auction game: a per-match [`Tracker`] fed
//! by public events, a [`Market`] model of what rivals bid, and the
//! [`Brain`] that values letters by Monte Carlo over the best final word
//! partition, paces its budget, and decides when to cash words in.

// Fixed 26-slot letter arrays read most clearly as indexed loops.
#![expect(
    clippy::needless_range_loop,
    reason = "26-slot letter arrays are indexed deliberately; iterator rewrites hide the slot"
)]

pub mod finish;
pub mod table;
pub mod valuation;

pub use kestrel_market::{BidDist, FieldModel, Market, Params};
pub use table::{AuctionKind, Player, Tracker};
pub use valuation::{BidDecision, Brain};
