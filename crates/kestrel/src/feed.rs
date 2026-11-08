//! Side-effect-free helpers used by the daemon, replay and setup: building
//! racks from server rows, sorting history, finish places, token parsing and
//! latency statistics. None of it touches I/O, so all of it is unit-testable.

use kestrel_policy::Tracker;
use kestrel_words::tiles::BAG_SIZE;
use kestrel_words::{index, Letter, Rack, ALPHABET};

/// Index of the first letter in a server letter string (`"Q"`).
pub fn letter_of(s: &str) -> Option<Letter> {
    s.chars().next().and_then(index)
}

/// Turns `(letter, count)` rows, like those of the `my_rack` view, into a rack.
/// Letters outside `A..=Z` are skipped; counts of a repeated letter are summed.
pub fn rack_from_rows<'a>(rows: impl IntoIterator<Item = (&'a str, u32)>) -> Rack {
    let mut r = Rack::EMPTY;
    for (letter, count) in rows {
        if let Some(l) = letter_of(letter) {
            r.0[l] = r.0[l].saturating_add(count.min(u8::MAX as u32) as u8);
        }
    }
    r
}

/// `a - b` with each letter count floored at zero.
pub fn rack_minus(a: Rack, b: &Rack) -> Rack {
    let mut out = a;
    for l in 0..ALPHABET {
        out.0[l] = out.0[l].saturating_sub(b.0[l]);
    }
    out
}

/// The tiles we were dealt, worked out mid-match: our current rack, plus the
/// ones spent on words, minus the ones won at auction.
pub fn derive_deal<'a>(rack_now: Rack, played: impl IntoIterator<Item = &'a str>, won: &Rack) -> Rack {
    let mut r = rack_now;
    for w in played {
        r.plus(&Rack::from_word(w));
    }
    rack_minus(r, won)
}

/// `match_state.bag_total` just after the `seq`-th auction (counting from 1)
/// draws its letter: 98 to start, minus `n * starting_rack` dealt.
pub fn bag_left_at(seq: u32, n_players: usize, starting_rack: usize) -> u32 {
    BAG_SIZE.saturating_sub((n_players * starting_rack) as u32 + seq)
}

/// 1 means first. Equal scores share the better place.
pub fn place_of(my_score: i64, others: impl IntoIterator<Item = i64>) -> usize {
    1 + others.into_iter().filter(|&s| s > my_score).count()
}

/// A single public-history entry from a match, replayed into a tracker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistItem {
    Open { auction_id: u64, letter: String, ts: i64 },
    Result { auction_id: u64, letter: String, winner: Option<u64>, paid: i64, ts: i64 },
    Word { id: u64, bot_id: u64, word: String, reward: i64, ts: i64 },
}

impl HistItem {
    fn key(&self) -> (i64, u8, u64) {
        // Closing an auction, letting sim winners play and opening the next
        // one happen in one server transaction, so all three carry the same
        // timestamp; sort them as result, then word, then open.
        match self {
            HistItem::Result { ts, auction_id, .. } => (*ts, 0, *auction_id),
            HistItem::Word { ts, id, .. } => (*ts, 1, *id),
            HistItem::Open { ts, auction_id, .. } => (*ts, 2, *auction_id),
        }
    }
}

/// Sorts a match's history into the order the server emitted it.
pub fn order_history(items: &mut [HistItem]) {
    items.sort_by_key(|i| i.key());
}

/// Feeds one history entry to a tracker the way the live event would.
/// Opens are counted in arrival order, which lets the bag count be computed.
pub fn feed_tracker(t: &mut Tracker, item: &HistItem, n_players: usize, starting_rack: usize) {
    match item {
        HistItem::Open { letter, .. } => {
            if let Some(l) = letter_of(letter) {
                let seq = t.auctions_seen + 1;
                t.on_auction_open(l, bag_left_at(seq, n_players, starting_rack));
            }
        }
        HistItem::Result { letter, winner, paid, .. } => {
            if let Some(l) = letter_of(letter) {
                t.on_auction_result(l, *winner, *paid);
            }
        }
        HistItem::Word { bot_id, word, .. } => t.on_word(*bot_id, word),
    }
}

/// Pulls the JWT out of `spacetime login show --token` output.
pub fn extract_jwt(text: &str) -> Option<String> {
    text.split(|c: char| c.is_whitespace() || c == '"' || c == '\'')
        .map(|t| t.trim_matches(|c: char| c == '.' || c == ',' || c == ':'))
        .find(|t| {
            let parts: Vec<&str> = t.split('.').collect();
            parts.len() == 3
                && t.starts_with("eyJ")
                && parts.iter().all(|p| {
                    !p.is_empty() && p.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '=')
                })
        })
        .map(str::to_string)
}

/// Nearest-rank percentile over `xs` (`p` between 0 and 100). `None` if empty.
pub fn percentile(xs: &[f64], p: f64) -> Option<f64> {
    if xs.is_empty() {
        return None;
    }
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.total_cmp(b));
    let rank = ((p / 100.0) * v.len() as f64).ceil() as usize;
    Some(v[rank.clamp(1, v.len()) - 1])
}

/// Current Unix-epoch time in milliseconds, used by telemetry.
pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_policy::AuctionKind;

    #[test]
    fn rack_rows_add_up_and_skip_junk() {
        let r = rack_from_rows([("Q", 1), ("U", 2), ("q", 1), ("?", 3), ("", 1)]);
        assert_eq!(r.to_string(), "QQUU");
        assert_eq!(rack_from_rows(std::iter::empty()), Rack::EMPTY);
    }

    #[test]
    fn minus_saturates() {
        let r = rack_minus(Rack::from_word("AAB"), &Rack::from_word("ABBC"));
        assert_eq!(r.to_string(), "A");
    }

    #[test]
    fn deal_is_rack_plus_played_minus_won() {
        // Dealt CATSX, won E and R, played CRATE, leaving SX in hand.
        let deal = derive_deal(Rack::from_word("SX"), ["CRATE"], &Rack::from_word("ER"));
        assert_eq!(deal, Rack::from_word("CATSX"));
    }

    #[test]
    fn bag_math_matches_server() {
        // Six bots times five tiles is 30 dealt, so the first auction leaves 67.
        assert_eq!(bag_left_at(1, 6, 5), 67);
        assert_eq!(bag_left_at(68, 6, 5), 0);
        assert_eq!(bag_left_at(200, 6, 5), 0);
    }

    #[test]
    fn places_share_ties() {
        assert_eq!(place_of(50, [10, 20]), 1);
        assert_eq!(place_of(20, [50, 20, 10]), 2);
        assert_eq!(place_of(0, [5, 5]), 3);
    }

    #[test]
    fn history_orders_result_word_open_on_ties() {
        let mut h = vec![
            HistItem::Open { auction_id: 2, letter: "B".into(), ts: 10 },
            HistItem::Word { id: 7, bot_id: 1, word: "AB".into(), reward: 4, ts: 10 },
            HistItem::Result { auction_id: 1, letter: "A".into(), winner: Some(1), paid: 3, ts: 10 },
            HistItem::Open { auction_id: 1, letter: "A".into(), ts: 9 },
        ];
        order_history(&mut h);
        assert!(matches!(h[0], HistItem::Open { auction_id: 1, .. }));
        assert!(matches!(h[1], HistItem::Result { auction_id: 1, .. }));
        assert!(matches!(h[2], HistItem::Word { id: 7, .. }));
        assert!(matches!(h[3], HistItem::Open { auction_id: 2, .. }));
    }

    #[test]
    fn finds_jwt_in_cli_output() {
        let out = "You are logged in as c200aa\nYour auth token (don't share this!) is eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJ4In0.sig-_part\n";
        assert_eq!(extract_jwt(out).as_deref(), Some("eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJ4In0.sig-_part"));
        assert_eq!(extract_jwt("not logged in"), None);
        assert_eq!(extract_jwt("eyJ.only.two"), Some("eyJ.only.two".into()));
        assert_eq!(extract_jwt("eyJabc.def"), None);
    }

    #[test]
    fn percentiles() {
        let xs = [5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(percentile(&xs, 50.0), Some(3.0));
        assert_eq!(percentile(&xs, 95.0), Some(5.0));
        assert_eq!(percentile(&xs, 0.0), Some(1.0));
        assert_eq!(percentile(&[], 50.0), None);
    }

    #[test]
    fn history_feeds_tracker_like_live_events() {
        let mut t = Tracker::new(1, 10, AuctionKind::SecondPrice, &[10, 20], 5);
        t.set_my_rack(Rack::from_word("CATSX"));
        let mut h = vec![
            HistItem::Open { auction_id: 1, letter: "Q".into(), ts: 1 },
            HistItem::Result { auction_id: 1, letter: "Q".into(), winner: Some(20), paid: 9, ts: 2 },
            HistItem::Word { id: 5, bot_id: 20, word: "QI".into(), reward: 11, ts: 2 },
            HistItem::Open { auction_id: 2, letter: "E".into(), ts: 2 },
            HistItem::Result { auction_id: 2, letter: "E".into(), winner: None, paid: 0, ts: 3 },
            HistItem::Open { auction_id: 3, letter: "Z".into(), ts: 3 },
        ];
        order_history(&mut h);
        for item in &h {
            feed_tracker(&mut t, item, 2, 5);
        }
        assert_eq!(t.auctions_seen, 3);
        assert_eq!(t.bag_left, 98 - 10 - 3);
        let rival = t.players.iter().find(|p| p.bot_id == 20).unwrap();
        assert_eq!(rival.won, Rack::from_word("Q"));
        // I was never bought for QI, so it has to come from the rival's deal.
        assert_eq!(rival.start_known, Rack::from_word("I"));
        assert_eq!(t.my_start, Rack::from_word("CATSX"));
        assert_eq!(t.revealed, Rack::from_word("QEZ"));
    }

    #[test]
    fn letter_parsing() {
        assert_eq!(letter_of("E"), Some(4));
        assert_eq!(letter_of("z"), Some(25));
        assert_eq!(letter_of(""), None);
    }
}
