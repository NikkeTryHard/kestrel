//! ENABLE lexicon shipped by the server, keyed by the multiset of letters
//! each entry consumes.
//!
//! Since a word's score comes solely from the letters it contains, any
//! rearrangement of those letters is equivalent. For each distinct multiset
//! we therefore keep a single record (a "signature") plus one example
//! spelling that can be submitted.

use crate::tiles::{reward, Letter, ALPHABET};
use crate::tray::Rack;
use rustc_hash::FxHashMap;
use std::sync::OnceLock;

/// Verbatim lexicon used by the module's validator (`spacetimedb/wordlist.txt`).
pub const WORDLIST: &str = include_str!("../../../data/wordlist.txt");

#[derive(Clone, Debug)]
pub struct Sig {
    pub rack: Rack,
    pub mask: u32,
    pub len: u8,
    pub base: u16,
    pub reward: u16,
    pub word: &'static str,
}

pub struct Dictionary {
    /// Uppercase entries in lexicographic order, located by bisection from
    /// `is_valid`.
    words: Vec<&'static str>,
    pub sigs: Vec<Sig>,
    /// Character trie built from all signature letters, kept in ascending
    /// order. Node 0 is the root; the children of node `n` occupy
    /// `child_start[n]..child_start[n+1]` in `child_letter` and `child_node`.
    child_start: Vec<u32>,
    child_letter: Vec<u8>,
    child_node: Vec<u32>,
    /// Id of the signature terminating at a node, or `u32::MAX` if none does.
    terminal: Vec<u32>,
}

/// Growable trie node, alive only during construction.
struct BuildNode {
    children: Vec<(u8, u32)>,
    terminal: u32,
}

static DICT: OnceLock<Dictionary> = OnceLock::new();

/// Process-wide dictionary, initialised lazily at a cost of about 100 ms.
pub fn dictionary() -> &'static Dictionary {
    DICT.get_or_init(|| Dictionary::from_wordlist(WORDLIST))
}

impl Dictionary {
    pub fn from_wordlist(text: &'static str) -> Dictionary {
        let mut words: Vec<&'static str> = text
            .lines()
            .map(str::trim)
            .filter(|w| w.len() >= 2 && w.bytes().all(|b| b.is_ascii_uppercase()))
            .collect();
        words.sort_unstable();
        words.dedup();

        let mut by_rack: FxHashMap<Rack, usize> = FxHashMap::default();
        let mut sigs = Vec::with_capacity(160_000);
        for &w in &words {
            let rack = Rack::from_word(w);
            if by_rack.contains_key(&rack) {
                continue;
            }
            let base = rack.face_value();
            by_rack.insert(rack, sigs.len());
            sigs.push(Sig {
                rack,
                mask: rack.mask(),
                len: w.len() as u8,
                base: base as u16,
                reward: reward(base, w.len()) as u16,
                word: w,
            });
        }
        // Construct the trie first, then store it as flat CSR arrays.
        let mut nodes = vec![BuildNode {
            children: Vec::new(),
            terminal: u32::MAX,
        }];
        for (i, sig) in sigs.iter().enumerate() {
            let mut n = 0usize;
            for l in sig.rack.letters() {
                let l = l as u8;
                let next = match nodes[n].children.iter().find(|c| c.0 == l) {
                    Some(&(_, c)) => c as usize,
                    None => {
                        nodes.push(BuildNode {
                            children: Vec::new(),
                            terminal: u32::MAX,
                        });
                        let c = nodes.len() - 1;
                        nodes[n].children.push((l, c as u32));
                        c
                    }
                };
                n = next;
            }
            nodes[n].terminal = i as u32;
        }
        let mut child_start = Vec::with_capacity(nodes.len() + 1);
        let mut child_letter = Vec::new();
        let mut child_node = Vec::new();
        let mut terminal = Vec::with_capacity(nodes.len());
        for node in &mut nodes {
            node.children.sort_unstable();
            child_start.push(child_letter.len() as u32);
            for &(l, c) in &node.children {
                child_letter.push(l);
                child_node.push(c);
            }
            terminal.push(node.terminal);
        }
        child_start.push(child_letter.len() as u32);
        Dictionary {
            words,
            sigs,
            child_start,
            child_letter,
            child_node,
            terminal,
        }
    }

    pub fn len(&self) -> usize {
        self.words.len()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// True when `word` occurs in the server's lexicon, ignoring case.
    pub fn is_valid(&self, word: &str) -> bool {
        let up = word.to_ascii_uppercase();
        self.words.binary_search(&up.as_str()).is_ok()
    }

    /// Collects signature ids for all words `rack` can spell using at least
    /// `min_len` letters.
    pub fn candidates(&self, rack: &Rack, min_len: usize) -> Vec<u32> {
        let mut out = Vec::new();
        let mut r = *rack;
        self.walk(0, 0, 0, &mut r, min_len, &mut out);
        out
    }

    fn walk(
        &self,
        node: usize,
        from: u8,
        depth: usize,
        r: &mut Rack,
        min_len: usize,
        out: &mut Vec<u32>,
    ) {
        let t = self.terminal[node];
        if t != u32::MAX && depth >= min_len {
            out.push(t);
        }
        let (a, b) = (
            self.child_start[node] as usize,
            self.child_start[node + 1] as usize,
        );
        for k in a..b {
            let l = self.child_letter[k];
            if l < from || r.0[l as usize] == 0 {
                continue;
            }
            r.0[l as usize] -= 1;
            self.walk(self.child_node[k] as usize, l, depth + 1, r, min_len, out);
            r.0[l as usize] += 1;
        }
    }

    /// Picks the single word fitting `rack` whose reward is highest.
    pub fn best_word(&self, rack: &Rack, min_len: usize) -> Option<&Sig> {
        self.candidates(rack, min_len)
            .into_iter()
            .map(|i| &self.sigs[i as usize])
            .max_by_key(|s| (s.reward, s.len))
    }

    /// Picks the longest word `rack` can spell, mirroring the server's
    /// simulation bots (`findBestPlayable`): length ranks first, and ties
    /// fall to lexicographic order.
    pub fn longest_word(&self, rack: &Rack, min_len: usize) -> Option<&'static str> {
        let mut best: Option<&'static str> = None;
        let mask = rack.mask();
        let total = rack.len();
        for &w in &self.words {
            if w.len() < min_len || w.len() > total {
                continue;
            }
            if let Some(b) = best {
                if w.len() <= b.len() {
                    continue;
                }
            }
            let r = Rack::from_word(w);
            if r.mask() & !mask == 0 && rack.contains(&r) {
                best = Some(w);
            }
        }
        best
    }

    pub fn sig(&self, id: u32) -> &Sig {
        &self.sigs[id as usize]
    }

    /// Rather than a first-letter word histogram, this counts how many
    /// signatures contain each letter — a low-cost "flexibility" measure.
    pub fn letter_frequency(&self) -> [u32; ALPHABET] {
        let mut f = [0u32; ALPHABET];
        for s in &self.sigs {
            for (l, slot) in f.iter_mut().enumerate() {
                if s.mask & (1 << (l as Letter)) != 0 {
                    *slot += 1;
                }
            }
        }
        f
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loads_and_validates() {
        let d = dictionary();
        assert_eq!(d.len(), 172_820);
        assert_eq!(d.sigs.len(), 156_473);
        assert!(d.is_valid("cazique"));
        assert!(d.is_valid("XI"));
        assert!(!d.is_valid("QI")); // QI and ZA postdate ENABLE and are absent
        assert!(!d.is_valid("QWZX"));
        assert!(!d.is_valid("A"));
    }

    #[test]
    fn candidates_and_best() {
        let d = dictionary();
        let r = Rack::from_word("CAZIQUE");
        let best = d.best_word(&r, 2).unwrap();
        assert_eq!(best.reward, 81);
        assert_eq!(d.longest_word(&r, 3), Some("CAZIQUE"));
        assert!(d.candidates(&Rack::from_word("QQ"), 2).is_empty());
    }
}
