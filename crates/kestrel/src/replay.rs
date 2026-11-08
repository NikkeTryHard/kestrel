//! `kestrel replay <match_id>`: reconstruct a match, finished or still
//! running, from public history and print the bids our brain would place at
//! every auction beside what really happened. Read-only, anonymous access.

use crate::feed::{feed_tracker, letter_of, order_history, percentile, HistItem};
use crate::settings::Common;
use anyhow::{anyhow, Result};
use kestrel_net::transport::{connect, subscribe_wait};
use kestrel_net::wire::*;
use kestrel_policy::{AuctionKind, Brain, Tracker};
use kestrel_words::tiles::STARTING_BALANCE;
use spacetimedb_sdk::Table;
use std::collections::HashMap;
use std::time::{Duration, Instant};

pub fn replay(common: &Common, match_id: u64, as_bot: Option<u64>) -> Result<()> {
    let s = connect(&common.host, &common.db, None, Duration::from_secs(20))?;
    let queries = vec![
        format!("SELECT * FROM match_state WHERE id = {match_id}"),
        format!("SELECT * FROM match_participant WHERE match_id = {match_id}"),
        format!("SELECT * FROM auction WHERE match_id = {match_id}"),
        format!("SELECT * FROM auction_result WHERE match_id = {match_id}"),
        format!("SELECT * FROM word_play WHERE match_id = {match_id}"),
        format!("SELECT * FROM auction_top_bid_archive WHERE match_id = {match_id}"),
        "SELECT * FROM bot".to_string(),
    ];
    subscribe_wait(&s.conn, &queries, Duration::from_secs(30))?;
    let db = &s.conn.db;
    let m = db.match_state().id().find(&match_id).ok_or_else(|| anyhow!("match {match_id} not found"))?;
    let name = |id: u64| db.bot().id().find(&id).map(|b| b.name).unwrap_or_else(|| format!("#{id}"));
    let mut parts: Vec<MatchParticipant> = db.match_participant().iter().collect();
    parts.sort_by_key(|p| p.id);
    let ids: Vec<u64> = parts.iter().map(|p| p.bot_id).collect();
    let shadow = match as_bot {
        Some(b) if ids.contains(&b) => b,
        Some(b) => return Err(anyhow!("bot {b} did not play match {match_id} (players: {ids:?})")),
        None => parts.iter().max_by_key(|p| p.score).map(|p| p.bot_id).ok_or_else(|| anyhow!("no participants"))?,
    };
    let kind = match m.auction_type {
        AuctionType::SecondPrice => AuctionKind::SecondPrice,
        AuctionType::FirstPrice => AuctionKind::FirstPrice,
    };
    let srs = m.starting_rack_size as usize;
    let mut tracker = Tracker::new(match_id, shadow, kind, &ids, srs);
    let mut brain =
        Brain::new(common.params(), common.field(), match_id ^ 0x5eed_cafe).with_duel(common.duel_params());
    brain.start_match();

    let mut hist = Vec::new();
    for a in db.auction().iter() {
        hist.push(HistItem::Open { auction_id: a.id, letter: a.letter, ts: a.opens_at.to_micros_since_unix_epoch() });
    }
    for r in db.auction_result().iter() {
        hist.push(HistItem::Result {
            auction_id: r.auction_id,
            letter: r.letter,
            winner: r.winner_bot_id,
            paid: r.paid,
            ts: r.closed_at.to_micros_since_unix_epoch(),
        });
    }
    for w in db.word_play().iter() {
        hist.push(HistItem::Word {
            id: w.id,
            bot_id: w.bot_id,
            word: w.word,
            reward: w.total_reward,
            ts: w.played_at.to_micros_since_unix_epoch(),
        });
    }
    order_history(&mut hist);
    let top: HashMap<u64, i64> = db.auction_top_bid_archive().iter().map(|t| (t.auction_id, t.top_bid)).collect();

    println!(
        "match {match_id}: {:?}, {} players, shadowing '{}' (id {shadow}); racks are what the public record reveals",
        m.status,
        ids.len(),
        name(shadow)
    );
    println!(
        "{:>3} {:>2} {:>4} {:<16} {:>4} | {:>4} {:>6} {:>5} {:>5} {:>6} | {:<16} {:>4} {:>4}",
        "#", "L", "bag", "rack", "bal", "bid", "value", "lam", "pwin", "ms", "winner", "paid", "top"
    );

    let mut bal: HashMap<u64, i64> = ids.iter().map(|&b| (b, STARTING_BALANCE)).collect();
    let mut score: HashMap<u64, i64> = ids.iter().map(|&b| (b, 0)).collect();
    let mut pending: HashMap<u64, String> = HashMap::new();
    let mut lat = Vec::new();
    let mut would_win = 0;
    let n = ids.len();
    for item in &hist {
        match item {
            HistItem::Open { auction_id, letter, .. } => {
                let Some(l) = letter_of(letter) else { continue };
                feed_tracker(&mut tracker, item, n, srs);
                let (seq, bag) = (tracker.auctions_seen, tracker.bag_left);
                for &b in &ids {
                    tracker.on_participant(b, bal[&b], score[&b]);
                }
                let rack = tracker.me().map(|p| p.known_rack()).unwrap_or_default();
                tracker.set_my_rack(rack);
                let t0 = Instant::now();
                let line = if bag == 0 {
                    let words = brain.final_plays(&rack);
                    let dt = t0.elapsed().as_secs_f64() * 1000.0;
                    lat.push(dt);
                    format!(
                        "{seq:>3} {letter:>2} {bag:>4} {:<16} {:>4} | final dump [{}] {dt:.1} ms",
                        rack.to_string(),
                        bal[&shadow],
                        words.join(" ")
                    )
                } else {
                    let d = brain.bid(&tracker, l);
                    let dt = t0.elapsed().as_secs_f64() * 1000.0;
                    lat.push(dt);
                    if top.get(auction_id).is_some_and(|&t| d.amount > t) {
                        would_win += 1;
                    }
                    format!(
                        "{seq:>3} {letter:>2} {bag:>4} {:<16} {:>4} | {:>4} {:>6.2} {:>5.2} {:>5.2} {dt:>6.1}",
                        rack.to_string(),
                        bal[&shadow],
                        d.amount,
                        d.value,
                        d.lambda,
                        d.p_win
                    )
                };
                pending.insert(*auction_id, line);
            }
            HistItem::Result { auction_id, letter, winner, paid, .. } => {
                let Some(l) = letter_of(letter) else { continue };
                feed_tracker(&mut tracker, item, n, srs);
                brain.observe_result(&tracker, l, *winner, *paid);
                if let Some(w) = winner {
                    *bal.entry(*w).or_default() -= paid;
                }
                let t = top.get(auction_id).map(|t| t.to_string()).unwrap_or_else(|| "-".into());
                let who = winner.map(name).unwrap_or_else(|| "(no bids)".into());
                if let Some(line) = pending.remove(auction_id) {
                    println!("{line} | {who:<16.16} {paid:>4} {t:>4}");
                }
            }
            HistItem::Word { bot_id, word, reward, .. } => {
                feed_tracker(&mut tracker, item, n, srs);
                *bal.entry(*bot_id).or_default() += reward;
                *score.entry(*bot_id).or_default() += reward;
                println!("      {} plays {word} (+{reward})", name(*bot_id));
            }
        }
    }
    for line in pending.into_values() {
        println!("{line} | (open)");
    }
    let mut final_scores: Vec<(String, i64)> = parts.iter().map(|p| (name(p.bot_id), p.score)).collect();
    final_scores.sort_by_key(|s| std::cmp::Reverse(s.1));
    println!("final: {}", final_scores.iter().map(|(n, s)| format!("{n} {s}")).collect::<Vec<_>>().join(", "));
    println!(
        "brain: {} decisions, p50 {:.1} ms, p95 {:.1} ms; our bid beat the recorded top bid on {would_win} auctions",
        lat.len(),
        percentile(&lat, 50.0).unwrap_or(0.0),
        percentile(&lat, 95.0).unwrap_or(0.0)
    );
    Ok(())
}
