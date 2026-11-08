//! Word machinery: the letter alphabet, racks, the dictionary, and the
//! exact solver for the best set of disjoint words in a rack.

pub mod lexicon;
pub mod tiles;
pub mod tray;
pub mod partition;

pub use lexicon::{dictionary, Dictionary, Sig};
pub use tiles::{char_of, index, reward, word_reward, Letter, ALPHABET, BAG, VALUE};
pub use tray::Rack;
pub use partition::{best_partition, Partition, Solver};
