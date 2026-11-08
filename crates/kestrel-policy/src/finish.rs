//! Time-boxed endgame bid comparisons run in pairs over a fixed policy ensemble.
//!
//! What comes out is a comparison between models, not a tournament win rate
//! that has been calibrated. Every candidate faces the same concealed deal,
//! tile sequence, rival behaviour and coin-flip tie-breaks. Snapshots of the
//! public state that disagree, and sample pairs left unfinished, are dropped.
//! A shared deadline is polled cooperatively to cap effort around dictionary
//! lookups.

use crate::{AuctionKind, Market, Tracker};
use kestrel_words::{
    dictionary,
    partition::{greedy, Partition},
    Letter, Rack,
};
use rand::{rngs::SmallRng, seq::SliceRandom, Rng, SeedableRng};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

#[derive(Clone, Debug)]
pub struct PlanConfig {
    pub max_bag_left: u32,
    pub max_samples: usize,
    pub min_samples: usize,
    pub budget_ms: u64,
    pub confidence_z: f64,
    pub min_gain: f64,
}

impl Default for PlanConfig {
    fn default() -> Self {
        Self { max_bag_left: 3, max_samples: 16, min_samples: 8, budget_ms: 30, confidence_z: 3.0, min_gain: 0.02 }
    }
}

#[derive(Clone, Debug)]
pub struct PlanResult {
    pub bid: i64,
    pub baseline_bid: i64,
    pub samples: usize,
    pub candidates: usize,
    pub mean_gain: f64,
    pub standard_error: f64,
    pub elapsed_ms: f64,
}

#[derive(Clone)]
struct State {
    racks: Vec<Rack>,
    balances: Vec<i64>,
    scores: Vec<i64>,
    // Keep the last workable split in case a new tile trips up the greedy search.
    held: Vec<Partition>,
}

struct Sample {
    state: State,
    future_scale: f64,
    bag: Vec<Letter>,
    styles: Vec<u8>,
    // A single combined ceiling draw for each auction rather than a separate
    // ceiling per rival; relative rack-dependent weights then divide it out.
    normal: Vec<f64>,
    none: Vec<f64>,
    weights: Vec<Vec<f64>>,
    ties: Vec<Vec<u64>>,
}

struct Work {
    deadline: Instant,
    cache: HashMap<Rack, Partition>,
}

impl Work {
    fn check(&self) -> Option<()> {
        (Instant::now() < self.deadline).then_some(())
    }

    fn partition(&mut self, rack: &Rack) -> Option<Partition> {
        self.check()?;
        if let Some(p) = self.cache.get(rack) {
            return Some(p.clone());
        }
        let p = greedy(dictionary(), rack);
        self.check()?;
        self.cache.insert(*rack, p.clone());
        Some(p)
    }

    fn refresh(&mut self, state: &mut State, seat: usize) -> Option<()> {
        let next = self.partition(&state.racks[seat])?;
        if next.score > state.held[seat].score {
            state.held[seat] = next;
        }
        Some(())
    }
}

fn draw(pool: &mut Rack, rng: &mut SmallRng) -> Option<Letter> {
    let total = pool.len();
    if total == 0 {
        return None;
    }
    let mut at = rng.gen_range(0..total);
    for l in 0..26 {
        if at < pool.count(l) as usize {
            pool.remove(l);
            return Some(l);
        }
        at -= pool.count(l) as usize;
    }
    None
}

fn sample(t: &Tracker, me: usize, rng: &mut SmallRng, work: &mut Work) -> Option<Sample> {
    work.check()?;
    let mut pool = t.unseen();
    let hidden: usize = t.players.iter().enumerate().filter(|(i, _)| *i != me).map(|(_, p)| p.hidden_left()).sum();
    if pool.len() != hidden + t.bag_left as usize {
        return None;
    }
    let mut racks = Vec::with_capacity(t.players.len());
    let mut held = Vec::with_capacity(t.players.len());
    for (i, player) in t.players.iter().enumerate() {
        let mut rack = if i == me { t.my_rack } else { player.known_rack() };
        if i != me {
            for _ in 0..player.hidden_left() {
                rack.add(draw(&mut pool, rng)?);
            }
        }
        // Searching very big racks is costly, so this optional planner gives up.
        if rack.len() > 32 {
            return None;
        }
        held.push(work.partition(&rack)?);
        racks.push(rack);
    }
    for l in 0..26 {
        let present = pool.count(l) as usize + racks.iter().map(|r| r.count(l) as usize).sum::<usize>();
        if present > kestrel_words::BAG[l] as usize {
            return None;
        }
    }
    let mut bag: Vec<_> = pool.letters().collect();
    bag.shuffle(rng);
    let n = t.players.len();
    let rounds = t.bag_left as usize;
    let normal = (0..rounds)
        .map(|_| {
            let u = rng.gen_range(f64::EPSILON..1.0);
            (-2.0 * u.ln()).sqrt() * (std::f64::consts::TAU * rng.gen::<f64>()).cos()
        })
        .collect();
    Some(Sample {
        state: State {
            racks,
            held,
            balances: t.players.iter().map(|p| p.balance.max(0)).collect(),
            scores: t.players.iter().map(|p| p.score).collect(),
        },
        future_scale: 1.0,
        bag,
        styles: (0..n).map(|_| rng.gen_range(0..3)).collect(),
        normal,
        none: (0..rounds).map(|_| rng.gen()).collect(),
        weights: (0..rounds).map(|_| (0..n).map(|_| rng.gen_range(0.65..1.35)).collect()).collect(),
        ties: (0..rounds).map(|_| (0..n).map(|_| rng.gen()).collect()).collect(),
    })
}

fn cash_one(state: &mut State, seat: usize) {
    if let Some(&word) = state.held[seat].words.first() {
        let sig = dictionary().sig(word);
        state.racks[seat].sub(&sig.rack);
        state.balances[seat] += sig.reward as i64;
        state.scores[seat] += sig.reward as i64;
        state.held[seat].score -= sig.reward as u32;
        state.held[seat].words.remove(0);
    }
}

fn settle(
    state: &mut State,
    bids: &[i64],
    letter: Letter,
    kind: AuctionKind,
    ties: &[u64],
    work: &mut Work,
) -> Option<()> {
    let mut order: Vec<_> = (0..bids.len()).filter(|&i| bids[i] > 0).collect();
    order.sort_unstable_by_key(|&i| (std::cmp::Reverse(bids[i]), ties[i], i));
    if let Some(&winner) = order.first() {
        let paid = match kind {
            AuctionKind::FirstPrice => bids[winner],
            AuctionKind::SecondPrice => order.get(1).map(|&i| bids[i]).unwrap_or(1),
        };
        if paid > state.balances[winner] {
            return None;
        }
        state.balances[winner] -= paid;
        state.racks[winner].add(letter);
        work.refresh(state, winner)?;
    }
    Some(())
}

fn utility(scores: &[i64], me: usize) -> f64 {
    if scores.len() == 2 {
        (scores[me] - scores[1 - me]) as f64
    } else {
        let above = scores.iter().filter(|&&s| s > scores[me]).count();
        (scores.len() - 1 - above) as f64 + 0.001 * scores[me] as f64
    }
}

fn rollout(
    t: &Tracker,
    current: Letter,
    me: usize,
    first_bid: i64,
    market: &Market,
    s: &Sample,
    work: &mut Work,
) -> Option<f64> {
    let mut state = s.state.clone();
    let mut bag = s.bag.clone();
    let mut letter = current;
    let rounds = t.bag_left as usize;
    let scale = s.future_scale;
    for step in 0..rounds {
        work.check()?;
        let remaining = rounds - step;
        for i in 0..state.racks.len() {
            // Word choices we make now are already folded into the input.
            // Rivals blend hoarding with cash-raising styles, while our own
            // later turns realise a single playable word when funds run thin.
            let cashable =
                remaining > 1 || state.held[i].words.first().is_some_and(|&w| dictionary().sig(w).reward >= 30);
            if cashable && !(step == 0 && i == me) && state.balances[i] < 15 && (i == me || s.styles[i] != 1) {
                cash_one(&mut state, i);
                work.refresh(&mut state, i)?;
            }
        }
        let mut gain = vec![0.0; state.racks.len()];
        for (i, g) in gain.iter_mut().enumerate() {
            let after = work.partition(&state.racks[i].with(letter))?;
            *g = after.score.saturating_sub(state.held[i].score) as f64;
        }
        let progress = ((t.auctions_seen as usize + step) as f64 / t.total_auctions.max(1) as f64).clamp(0.0, 1.0);
        let richest = state.balances.iter().enumerate().filter(|(i, _)| *i != me).map(|(_, b)| *b).max().unwrap_or(0);
        let dist = market.dist(letter, progress, state.racks.len() - 1, richest);
        let maximum = if s.none[step] < dist.p_none {
            0.0
        } else {
            (dist.median * (dist.sigma * s.normal[step]).exp()).min(dist.cap)
        };
        let weights: Vec<_> = gain
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let fit = match s.styles[i] {
                    0 => 1.0,
                    1 => 0.4 + g.sqrt(),
                    _ => 0.7 + 0.3 * g.sqrt(),
                };
                fit * s.weights[step][i]
            })
            .collect();
        let max_weight =
            weights.iter().enumerate().filter(|(i, _)| *i != me).map(|(_, w)| *w).fold(0.0, f64::max).max(0.01);
        let mut bids: Vec<_> = (0..state.racks.len())
            .map(|i| {
                let b = if i == me {
                    if step == 0 {
                        first_bid as f64
                    } else if remaining == 1 {
                        if gain[i] > 0.0 {
                            state.balances[i] as f64
                        } else {
                            0.0
                        }
                    } else {
                        (gain[i] * 2.0).max(dist.median * scale * 0.65)
                    }
                } else if remaining == 1 && s.styles[i] == 2 && gain[i] > 0.0 {
                    state.balances[i] as f64
                } else {
                    maximum * weights[i] / max_weight
                };
                (b.floor() as i64).clamp(0, state.balances[i])
            })
            .collect();
        if matches!(t.kind, AuctionKind::SecondPrice) {
            bids[me] = bids[me].min(richest.saturating_add(1));
        }
        settle(&mut state, &bids, letter, t.kind, &s.ties[step], work)?;
        letter = bag.pop()?;
        // The bag's final tile drives the closing auction and is never played.
    }
    for i in 0..state.scores.len() {
        state.scores[i] += state.held[i].score as i64;
    }
    Some(utility(&state.scores, me))
}

fn paired_stats(xs: &[f64]) -> (f64, f64) {
    let n = xs.len() as f64;
    let mean = xs.iter().sum::<f64>() / n;
    let variance = xs.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / (n - 1.0);
    (mean, (variance / n).sqrt())
}

fn choose_candidate(diffs: &[Vec<f64>], cfg: &PlanConfig) -> (usize, f64, f64) {
    let mut best = (0usize, 0.0, 0.0);
    for (i, xs) in diffs.iter().enumerate().skip(1) {
        let (mean, se) = paired_stats(xs);
        if mean > best.1 && mean > cfg.min_gain.max(0.0) + cfg.confidence_z.max(3.0) * se {
            best = (i, mean, se);
        }
    }
    best
}

/// Weigh playable bid values over paired samples that ran to completion. The
/// chosen bid stands unless its modelled improvement clears a wide, cautious
/// margin of uncertainty. Set the dictionary up beforehand the way `Brain::new`
/// does.
pub fn plan(
    t: &Tracker,
    current: Letter,
    baseline_bid: i64,
    market: &Market,
    seed: u64,
    cfg: &PlanConfig,
) -> Option<PlanResult> {
    let start = Instant::now();
    if current >= 26
        || t.bag_left == 0
        || t.bag_left > cfg.max_bag_left.min(3)
        || t.players.len() < 2
        || t.players.len() > 8
        || cfg.budget_ms == 0
    {
        return None;
    }
    let me = t.players.iter().position(|p| p.bot_id == t.my_id)?;
    let balance = t.my_balance().max(0);
    if balance == 0 {
        return None;
    }
    let max = if matches!(t.kind, AuctionKind::SecondPrice) {
        balance.min(t.max_opponent_balance().max(0).saturating_add(1))
    } else {
        balance
    };
    let baseline = baseline_bid.clamp(0, max);
    let near = market.dist(current, t.progress(), t.players.len() - 1, t.max_opponent_balance()).median.ceil() as i64;
    let mut candidates = vec![baseline];
    for b in [
        if baseline > 0 { 1 } else { 0 },
        baseline / 2,
        near.saturating_add(1),
        baseline.saturating_add(baseline / 2),
        max,
    ] {
        let b = b.clamp(if baseline > 0 { 1 } else { 0 }, max);
        if !candidates.contains(&b) {
            candidates.push(b);
        }
    }
    let mut work = Work { deadline: start + Duration::from_millis(cfg.budget_ms.min(30)), cache: HashMap::new() };
    let mut rng = SmallRng::seed_from_u64(seed);
    let mut diffs = vec![Vec::new(); candidates.len()];
    let required = cfg.min_samples.max(8);
    let mut completed = 0;
    for _ in 0..cfg.max_samples.min(64) {
        let Some(mut s) = sample(t, me, &mut rng, &mut work) else {
            break;
        };
        s.future_scale = (baseline.max(1) as f64 / market.field.median[current].max(1.0)).clamp(0.4, 2.0);
        let values: Option<Vec<_>> =
            candidates.iter().map(|&b| rollout(t, current, me, b, market, &s, &mut work)).collect();
        let Some(values) = values else {
            break;
        };
        for i in 1..values.len() {
            diffs[i].push(values[i] - values[0]);
        }
        completed += 1;
    }
    if completed < required {
        return None;
    }
    let best = choose_candidate(&diffs, cfg);
    Some(PlanResult {
        bid: candidates[best.0],
        baseline_bid: baseline,
        samples: completed,
        candidates: candidates.len(),
        mean_gain: best.1,
        standard_error: best.2,
        elapsed_ms: start.elapsed().as_secs_f64() * 1000.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::FieldModel;
    use kestrel_words::{index, BAG};

    fn position() -> Tracker {
        let mut t = Tracker::new(1, 1, AuctionKind::SecondPrice, &[1, 2], 0);
        t.my_rack = Rack::from_word("AT");
        t.my_start = t.my_rack;
        t.players[0].score = 10;
        t.players[0].balance = 20;
        t.players[1].won = Rack::from_word("DO");
        t.players[1].score = 9;
        t.players[1].balance = 5;
        t.bag_left = 1;
        t.auctions_seen = 97;
        t.revealed = Rack(BAG);
        t.revealed.sub(&t.my_start);
        t.revealed.remove(index('E').unwrap());
        t
    }

    fn work() -> Work {
        Work { deadline: Instant::now() + Duration::from_secs(2), cache: HashMap::new() }
    }

    #[test]
    fn hidden_deals_and_future_bag_conserve_the_unseen_pool() {
        let mut t = position();
        t.players[1].starting_rack = 2;
        t.revealed.remove(index('A').unwrap());
        t.revealed.remove(index('I').unwrap());
        let before = t.unseen();
        let mut rng = SmallRng::seed_from_u64(7);
        let s = sample(&t, 0, &mut rng, &mut work()).unwrap();
        let mut hidden = s.state.racks[1];
        hidden.sub(&t.players[1].known_rack());
        for l in &s.bag {
            hidden.add(*l);
        }
        assert_eq!(hidden, before);
        assert_eq!(s.bag.len(), 1);
        t.bag_left = 2;
        assert!(sample(&t, 0, &mut rng, &mut work()).is_none());
    }

    #[test]
    fn last_playable_bid_changes_the_exact_hand_checked_outcome() {
        let mut t = position();
        t.players[0].balance = 40;
        t.players[1].balance = 20;
        let market = Market::new(FieldModel::default());
        let mut rng = SmallRng::seed_from_u64(2);
        let mut w = work();
        let mut s = sample(&t, 0, &mut rng, &mut w).unwrap();
        s.styles[1] = 2; // the rival would rather spend its 20 coins on DOC than DO.
        let c = index('C').unwrap();
        let lost = rollout(&t, c, 0, 1, &market, &s, &mut w).unwrap();
        let won = rollout(&t, c, 0, 40, &market, &s, &mut w).unwrap();
        // Defeat leaves AT=2 and DOC=6; victory gives CAT=5 and DO=3.
        assert_eq!(lost, (10 + 2 - 9 - 6) as f64);
        assert_eq!(won, (10 + 5 - 9 - 3) as f64);
        assert!(won > lost);
        assert_eq!(rollout(&t, c, 0, 40, &market, &s, &mut w), Some(won));
    }

    #[test]
    fn prices_and_balance_differ_between_auction_formats() {
        let t = position();
        let mut w = work();
        let mut rng = SmallRng::seed_from_u64(2);
        let mut s = sample(&t, 0, &mut rng, &mut w).unwrap().state;
        let mut fp = s.clone();
        let c = index('C').unwrap();
        settle(&mut s, &[10, 5], c, AuctionKind::SecondPrice, &[1, 2], &mut w).unwrap();
        settle(&mut fp, &[10, 5], c, AuctionKind::FirstPrice, &[1, 2], &mut w).unwrap();
        assert_eq!(s.balances[0], 15);
        assert_eq!(fp.balances[0], 10);
        assert!(fp.balances.iter().all(|&b| b >= 0));
    }

    #[test]
    fn useless_own_tile_can_block_the_rivals_winning_word() {
        let mut t = position();
        t.my_rack = Rack::EMPTY;
        t.my_start = Rack::EMPTY;
        t.players[0].score = 12;
        t.players[1].won = Rack::from_word("AT");
        t.revealed = Rack(BAG);
        t.revealed.remove(index('E').unwrap());
        let market = Market::new(FieldModel::default());
        let mut rng = SmallRng::seed_from_u64(1);
        let mut w = work();
        let mut s = sample(&t, 0, &mut rng, &mut w).unwrap();
        s.styles[1] = 2;
        let c = index('C').unwrap();
        assert_eq!(rollout(&t, c, 0, 1, &market, &s, &mut w), Some(-2.0));
        assert_eq!(rollout(&t, c, 0, 20, &market, &s, &mut w), Some(1.0));
    }

    #[test]
    fn paired_statistics_and_deadline_abstention() {
        assert_eq!(paired_stats(&[2.0; 8]), (2.0, 0.0));
        let cfg = PlanConfig::default();
        assert_eq!(choose_candidate(&[vec![], vec![2.0; 8]], &cfg), (1, 2.0, 0.0));
        assert_eq!(choose_candidate(&[vec![], vec![0.01; 8]], &cfg).0, 0);
        assert_eq!(choose_candidate(&[vec![], vec![4.0, -3.0, 4.0, -3.0, 4.0, -3.0, 4.0, -3.0]], &cfg).0, 0);
        let t = position();
        let cfg = PlanConfig { budget_ms: 0, ..PlanConfig::default() };
        assert!(plan(&t, index('C').unwrap(), 1, &Market::new(FieldModel::default()), 2, &cfg).is_none());
        let mut last = t;
        last.bag_left = 0;
        assert!(plan(&last, index('C').unwrap(), 1, &Market::new(FieldModel::default()), 2, &PlanConfig::default())
            .is_none());
    }

    #[test]
    #[ignore = "manual timing check on the shared live host"]
    fn six_seat_late_position_coverage() {
        // In production the Brain is built up front, absorbing this warm-up cost.
        let _ = dictionary();
        let mut t = Tracker::new(9, 1, AuctionKind::SecondPrice, &[1, 2, 3, 4, 5, 6], 5);
        t.my_rack = Rack::from_word("AEINRST");
        t.my_start = Rack::from_word("AEINR");
        t.bag_left = 3;
        t.auctions_seen = 65;
        let mut remaining = Rack(BAG);
        remaining.sub(&t.my_rack);
        remaining.remove(index('E').unwrap());
        let mut rng = SmallRng::seed_from_u64(81);
        for (i, p) in t.players.iter_mut().enumerate() {
            p.balance = 20 + i as i64 * 3;
            p.score = 60 + i as i64 * 2;
            if i > 0 {
                for _ in 0..(i + 2) {
                    p.won.add(draw(&mut remaining, &mut rng).unwrap());
                }
            }
        }
        let mut unseen = Rack::EMPTY;
        for _ in 0..28 {
            unseen.add(draw(&mut remaining, &mut rng).unwrap());
        }
        t.revealed = Rack(BAG);
        t.revealed.sub(&t.my_start);
        t.revealed.sub(&unseen);
        assert_eq!(t.unseen().len(), 28);
        let market = Market::new(FieldModel::default());
        let mut completed = Vec::new();
        for seed in 0..10 {
            let result = plan(&t, index('E').unwrap(), 12, &market, seed, &PlanConfig::default());
            completed.push(result.as_ref().map(|r| r.samples).unwrap_or(0));
            if let Some(r) = result {
                assert!(r.elapsed_ms < 50.0);
            }
        }
        println!("six-seat paired samples per 30ms call: {completed:?}");
    }
}
