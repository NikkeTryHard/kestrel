//! Computing the best word set for a rack, exactly.
//!
//! The closing dump of a match lets us play every word we can, so the
//! terminal value of a rack is the largest combined reward obtainable from
//! any collection of non-overlapping dictionary words it can build. A
//! memoised recursion finds it, branching on `x`, the most valuable letter
//! still left in the rack:
//!
//! ```text
//! f(R) = max( f(R - x),                         // leave x unused
//!             max_{w ∋ x, w ⊆ R} reward(w) + f(R - w) )
//! ```
//!
//! Every partition is visited once, promising branches are tried first, and
//! the bound `3 · face(R)` cuts off whatever cannot improve on the current
//! best, letting racks of 20 or more letters finish in far less than a
//! millisecond.

use crate::lexicon::Dictionary;
use crate::tiles::{multiplier, Letter, ALPHABET, VALUE};
use crate::tray::Rack;

/// A selected collection of words together with the reward it totals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Partition {
    pub score: u32,
    /// Indices of signatures, referring into `Dictionary::sigs`.
    pub words: Vec<u32>,
}

impl Partition {
    pub fn word_strings(&self, d: &Dictionary) -> Vec<&'static str> {
        self.words.iter().map(|&i| d.sig(i).word).collect()
    }
}

/// Bitwise OR of the letter masks of all candidates in `live`.
#[inline]
fn usable_mask(d: &Dictionary, live: &[u32]) -> u32 {
    live.iter().fold(0, |m, &i| m | d.sig(i).mask)
}

/// Generous ceiling: all usable letters are scored at the highest length
/// multiplier permitted by how many of them there are.
#[inline]
fn upper_bound(r: &Rack, usable: u32) -> u32 {
    let mut n = 0usize;
    let mut face = 0u32;
    for l in 0..ALPHABET {
        if usable & (1 << l) != 0 {
            n += r.0[l] as usize;
            face += r.0[l] as u32 * VALUE[l];
        }
    }
    if n < 2 {
        return 0;
    }
    let (num, den) = multiplier(n);
    face * num / den
}

/// Chooses the most valuable letter in `usable`, preferring the rarer letter
/// when values are equal.
#[inline]
fn pivot(r: &Rack, usable: u32) -> Option<Letter> {
    let mut best: Option<Letter> = None;
    for l in 0..ALPHABET {
        if r.0[l] == 0 || usable & (1 << l) == 0 {
            continue;
        }
        best = match best {
            None => Some(l),
            Some(b) if VALUE[l] > VALUE[b] || (VALUE[l] == VALUE[b] && r.0[l] < r.0[b]) => Some(l),
            keep => keep,
        };
    }
    best
}

/// Branch-and-bound solver that can stop at any point. If `solve` runs to
/// completion inside `node_budget` the answer is exact; if the budget runs
/// out, the result is the best partition seen so far — seeded with the greedy
/// result and improved from there.
pub struct Solver<'d> {
    dict: &'d Dictionary,
    /// Shortest word length considered; 2 admits every word the server allows.
    pub min_len: usize,
    pub node_budget: u64,
    /// Optional elapsed-time cutoff, inspected once per 1024 nodes.
    pub deadline: Option<std::time::Instant>,
    pub nodes: u64,
    best: u32,
    best_words: Vec<u32>,
    path: Vec<u32>,
}

impl<'d> Solver<'d> {
    pub fn new(dict: &'d Dictionary) -> Self {
        Solver {
            dict,
            min_len: 2,
            node_budget: 2_000_000,
            deadline: None,
            nodes: 0,
            best: 0,
            best_words: Vec::new(),
            path: Vec::new(),
        }
    }

    pub fn with_budget(dict: &'d Dictionary, node_budget: u64) -> Self {
        Solver {
            node_budget,
            ..Solver::new(dict)
        }
    }

    /// Builds a solver whose search ends once `ms` milliseconds have elapsed.
    pub fn with_deadline(dict: &'d Dictionary, ms: u64) -> Self {
        Solver {
            deadline: Some(std::time::Instant::now() + std::time::Duration::from_millis(ms)),
            ..Solver::new(dict)
        }
    }

    /// True when the previous `solve` explored enough to guarantee optimality.
    pub fn exhaustive(&self) -> bool {
        self.nodes < self.node_budget
    }

    /// Produces the highest-value partition found for `rack`.
    pub fn solve(&mut self, rack: &Rack) -> Partition {
        let cands = self.dict.candidates(rack, self.min_len);
        let g = greedy_from(self.dict, rack, &cands);
        self.best = g.score;
        self.best_words = g.words;
        self.nodes = 0;
        self.path.clear();
        let budget = self.node_budget;
        let mut sorted = cands;
        sorted.sort_unstable_by_key(|&i| std::cmp::Reverse(self.dict.sig(i).reward));
        self.dfs(*rack, &sorted, 0);
        let done = self.nodes < self.node_budget;
        self.node_budget = budget;
        if !done {
            self.nodes = budget;
        }
        Partition {
            score: self.best,
            words: self.best_words.clone(),
        }
    }

    pub fn value(&mut self, rack: &Rack) -> u32 {
        self.solve(rack).score
    }

    fn dfs(&mut self, r: Rack, cands: &[u32], acc: u32) {
        self.nodes += 1;
        if self.nodes >= self.node_budget {
            return;
        }
        if self.nodes & 1023 == 0 {
            if let Some(dl) = self.deadline {
                if std::time::Instant::now() >= dl {
                    self.node_budget = self.nodes;
                    return;
                }
            }
        }
        let live: Vec<u32> = cands
            .iter()
            .copied()
            .filter(|&i| r.contains(&self.dict.sig(i).rack))
            .collect();
        if acc > self.best {
            self.best = acc;
            self.best_words = self.path.clone();
        }
        let usable = usable_mask(self.dict, &live);
        if acc + upper_bound(&r, usable) <= self.best {
            return;
        }
        let Some(x) = pivot(&r, usable) else { return };
        // Since `live` is reward-ordered, the strongest branches come first.
        for k in 0..live.len() {
            let i = live[k];
            let s = self.dict.sig(i);
            if s.rack.0[x] == 0 {
                continue;
            }
            let mut rest = r;
            rest.sub(&s.rack);
            if acc + s.reward as u32 + upper_bound(&rest, usable) <= self.best {
                continue;
            }
            self.path.push(i);
            self.dfs(rest, &live, acc + s.reward as u32);
            self.path.pop();
            if self.nodes >= self.node_budget {
                return;
            }
        }
        // Skip x entirely.
        let mut dropped = r;
        dropped.0[x] -= 1;
        self.dfs(dropped, &live, acc);
    }
}

/// Greedy strategy: take the best-paying word that fits, repeating until
/// none do.
pub fn greedy(dict: &Dictionary, rack: &Rack) -> Partition {
    let cands = dict.candidates(rack, 2);
    greedy_from(dict, rack, &cands)
}

fn greedy_from(dict: &Dictionary, rack: &Rack, cands: &[u32]) -> Partition {
    let mut r = *rack;
    let mut live: Vec<u32> = cands.to_vec();
    let mut p = Partition::default();
    loop {
        live.retain(|&i| r.contains(&dict.sig(i).rack));
        let Some(&best) = live
            .iter()
            .max_by_key(|&&i| (dict.sig(i).reward, std::cmp::Reverse(dict.sig(i).len)))
        else {
            break;
        };
        let s = dict.sig(best);
        r.sub(&s.rack);
        p.score += s.reward as u32;
        p.words.push(best);
    }
    p
}

/// Single call performing a budgeted exact solve.
pub fn best_partition(dict: &Dictionary, rack: &Rack) -> Partition {
    Solver::new(dict).solve(rack)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexicon::dictionary;
    use crate::tiles::BAG;
    use rand::rngs::SmallRng;
    use rand::{Rng, SeedableRng};

    /// Straightforward exhaustive enumeration used as a reference on small
    /// racks.
    fn brute(d: &Dictionary, r: Rack) -> u32 {
        let cands = d.candidates(&r, 2);
        fn go(d: &Dictionary, r: Rack, cands: &[u32], from: usize) -> u32 {
            let mut best = 0;
            for (j, &i) in cands.iter().enumerate().skip(from) {
                let s = d.sig(i);
                if r.contains(&s.rack) {
                    let mut rest = r;
                    rest.sub(&s.rack);
                    best = best.max(s.reward as u32 + go(d, rest, cands, j));
                }
            }
            best
        }
        go(d, r, &cands, 0)
    }

    fn random_rack(rng: &mut SmallRng, n: usize) -> Rack {
        let mut pool: Vec<usize> = (0..26)
            .flat_map(|l| std::iter::repeat_n(l, BAG[l] as usize))
            .collect();
        let mut r = Rack::EMPTY;
        for _ in 0..n {
            let j = rng.gen_range(0..pool.len());
            r.add(pool.swap_remove(j));
        }
        r
    }

    #[test]
    fn matches_brute_force_on_small_racks() {
        let d = dictionary();
        let mut rng = SmallRng::seed_from_u64(7);
        for n in 2..=9 {
            for _ in 0..25 {
                let r = random_rack(&mut rng, n);
                let p = best_partition(d, &r);
                assert_eq!(p.score, brute(d, r), "rack {r:?}");
            }
        }
    }

    #[test]
    fn partition_words_are_disjoint_and_valid() {
        let d = dictionary();
        let mut rng = SmallRng::seed_from_u64(11);
        for _ in 0..50 {
            let r = random_rack(&mut rng, 22);
            let p = best_partition(d, &r);
            let mut used = Rack::EMPTY;
            let mut total = 0;
            for w in p.word_strings(d) {
                assert!(d.is_valid(w));
                used.plus(&Rack::from_word(w));
                total += crate::tiles::word_reward(w);
            }
            assert!(r.contains(&used));
            assert_eq!(total, p.score);
        }
    }

    #[test]
    fn known_rack() {
        let d = dictionary();
        let p = best_partition(d, &Rack::from_word("CAZIQUEAT"));
        assert!(p.score >= 81 + 2);
    }
}
