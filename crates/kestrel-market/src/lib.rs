//! Market side of the bot: how much rivals are likely to bid, and the
//! parameters that shape our own bidding.

pub mod rivals;
pub mod tuning;

pub use rivals::{BidDist, FieldModel, Market};
pub use tuning::Params;
