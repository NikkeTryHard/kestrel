//! A letter multiset stored as one counter per alphabet position.

use crate::tiles::{char_of, index, Letter, ALPHABET, VALUE};
use std::fmt;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Default, PartialOrd, Ord)]
pub struct Rack(pub [u8; ALPHABET]);

impl Rack {
    pub const EMPTY: Rack = Rack([0; ALPHABET]);

    pub fn from_word(s: &str) -> Rack {
        let mut r = Rack::EMPTY;
        for l in s.chars().filter_map(index) {
            r.0[l] += 1;
        }
        r
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.0.iter().map(|&n| n as usize).sum()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&n| n == 0)
    }

    #[inline]
    pub fn count(&self, l: Letter) -> u8 {
        self.0[l]
    }

    #[inline]
    pub fn add(&mut self, l: Letter) {
        self.0[l] += 1;
    }

    #[inline]
    pub fn with(mut self, l: Letter) -> Rack {
        self.0[l] += 1;
        self
    }

    /// Lowers the count of `l` by one and reports success; if the letter is
    /// absent, nothing changes and the result is false.
    #[inline]
    pub fn remove(&mut self, l: Letter) -> bool {
        if self.0[l] == 0 {
            return false;
        }
        self.0[l] -= 1;
        true
    }

    /// Produces a bitfield in which bit `l` is set exactly when the rack
    /// still contains at least one `l`.
    #[inline]
    pub fn mask(&self) -> u32 {
        let mut m = 0;
        for (l, &n) in self.0.iter().enumerate() {
            if n > 0 {
                m |= 1 << l;
            }
        }
        m
    }

    #[inline]
    pub fn contains(&self, other: &Rack) -> bool {
        self.0.iter().zip(other.0.iter()).all(|(a, b)| a >= b)
    }

    #[inline]
    pub fn sub(&mut self, other: &Rack) {
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            *a -= b;
        }
    }

    #[inline]
    pub fn plus(&mut self, other: &Rack) {
        for (a, b) in self.0.iter_mut().zip(other.0.iter()) {
            *a += b;
        }
    }

    /// Adds up the point values of every tile, duplicates included.
    pub fn face_value(&self) -> u32 {
        self.0
            .iter()
            .zip(VALUE.iter())
            .map(|(&n, &v)| n as u32 * v)
            .sum()
    }

    pub fn letters(&self) -> impl Iterator<Item = Letter> + '_ {
        self.0
            .iter()
            .enumerate()
            .flat_map(|(l, &n)| std::iter::repeat_n(l, n as usize))
    }
}

impl fmt::Debug for Rack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: String = self.letters().map(char_of).collect();
        write!(f, "Rack({s})")
    }
}

impl fmt::Display for Rack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s: String = self.letters().map(char_of).collect();
        f.write_str(&s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_ops() {
        let mut r = Rack::from_word("QUIZZES");
        assert_eq!(r.len(), 7);
        assert_eq!(r.to_string(), "EIQSUZZ");
        assert!(r.contains(&Rack::from_word("QUIZ")));
        r.sub(&Rack::from_word("QUIZ"));
        assert_eq!(r.to_string(), "ESZ");
        assert!(!r.remove(0));
        assert_eq!(Rack::from_word("AQ").face_value(), 11);
    }
}
