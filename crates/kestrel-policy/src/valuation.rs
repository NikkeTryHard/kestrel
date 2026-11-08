//! What drives this module: pricing a letter, choosing a bid, selecting a play.
//!
//! **Valuation.** A rack earns its final-dump score — the highest-scoring
//! collection of non-overlapping words, found by `kestrel_words::partition`.
//! Because more tiles should arrive during play, we simulate upcoming
//! purchases: incomplete-tile futures are drawn from the pool of letters
//! nobody has seen yet. Each letter's marginal contribution is then
//! `E[T(R + L + F)] − E[T(R + F)]`, evaluated on the same sampled futures
//! (using common random numbers keeps the variance of that difference down).
//!
//! **Bidding.** Points never fall because coins were spent, and unspent coins
//! expire worthless. Pacing is therefore the whole problem: choose a rate
//! `λ` (points per coin) such that predicted outlay across the auctions still
//! ahead equals the budget, then submit `V / λ`. With second-price pricing,
//! bidding value is optimal; with first-price pricing we instead pick the
//! amount that maximises expected surplus given the distribution of rival
//! bids.
//!
//! **Playing.** A word scores only once it hits the board, and a completed
//! play replenishes the coins used to acquire more tiles. Mid-match, a play
//! is made when the reward it yields, together with the spending power it
//! unlocks, exceeds what keeping those tiles is expected to be worth. At
//! the final auction we empty the rack using the exact best partition.

use crate::finish::{plan, PlanConfig};
use crate::table::{AuctionKind, Tracker};
use kestrel_market::Params;
use kestrel_market::{BidDist, FieldModel, Market};
use kestrel_words::tiles::VALUE;
use kestrel_words::{dictionary, Dictionary, Letter, Rack, Solver, ALPHABET};
use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

/// Preliminary bids go out ahead of the full valuation; details live in `Brain::anchor_bid`.
const ANCHOR_SCALE: f64 = 0.85;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct BidDecision {
    pub amount: i64,
    /// Worth of the letter, measured in points.
    pub value: f64,
    /// Conversion rate: how many points one coin buys.
    pub lambda: f64,
    /// Estimated probability that `amount` takes the auction.
    pub p_win: f64,
}

/// A block of later auctions, represented as `(count, letter mix)`.
type Segment = (f64, Vec<(f64, f64, BidDist)>);

pub struct Brain {
    /// Parameters in force right now; table size selects them via `sync_profile`.
    pub params: Params,
    /// Settings used when at least three players are seated.
    pub multi: Params,
    /// Optional settings for two-player tables, i.e. the tournament final.
    pub duel: Option<Params>,
    pub market: Market,
    dict: &'static Dictionary,
    rng: SmallRng,
    /// Current-rack per-letter marginal values, consumed when pacing the budget.
    table: [f64; ALPHABET],
    table_rack: Option<Rack>,
    table_at: u32,
    pub last_lambda: f64,
    /// The amount we submitted on the live auction, needed to interpret censored prices.
    last_bid: i64,
    /// Optional real-time limit on sampling for the decision in progress.
    deadline: Option<std::time::Instant>,
    /// Scales how many future tiles get sampled (see `premium_choice_mult`).
    future_mult: f64,
}

impl Brain {
    pub fn new(params: Params, field: FieldModel, seed: u64) -> Self {
        // Warm the dictionary index and letter prior eagerly here, so no auction pays that cost.
        let _ = letter_prior();
        Brain {
            params: params.clone(),
            multi: params,
            duel: None,
            market: Market::new(field),
            dict: dictionary(),
            rng: SmallRng::seed_from_u64(seed),
            table: [0.0; ALPHABET],
            table_rack: None,
            table_at: 0,
            last_lambda: 0.3,
            last_bid: 0,
            deadline: None,
            future_mult: 1.0,
        }
    }

    /// Install a separate parameter set for two-player tables.
    pub fn with_duel(mut self, duel: Option<Params>) -> Self {
        self.duel = duel;
        self
    }

    /// Choose which parameter profile this table should use.
    pub fn sync_profile(&mut self, t: &Tracker) {
        let want = match (&self.duel, t.n_players() <= 2) {
            (Some(d), true) => d,
            _ => &self.multi,
        };
        if &self.params != want {
            self.params = want.clone();
        }
    }

    /// Per-letter marginals most recently computed, used by the pacing logic.
    pub fn value_table(&self) -> [f64; ALPHABET] {
        self.table
    }

    /// Reset everything that is specific to a single match.
    pub fn start_match(&mut self) {
        let field = self.market.field.clone();
        self.market = Market::new(field);
        self.table_rack = None;
        self.last_lambda = 0.3;
    }

    // ---------------------------------------------------- pricing the tiles

    fn terminal(&self, rack: &Rack) -> f64 {
        if self.params.solve_nodes == 0 {
            let greedy = kestrel_words::partition::greedy(self.dict, rack);
            let alternatives = self.params.terminal_rollout_candidates.min(4);
            if alternatives == 0 || greedy.words.is_empty() {
                return greedy.score as f64;
            }
            // Picking the best-looking first word may strand high-value tiles.
            // Probe a capped set of alternative first words, each finished
            // greedily. Every probe is a legal partition, so the greedy dump
            // remains a floor; never replace it with a weaker rollout.
            let mut candidates = self.dict.candidates(rack, 2);
            candidates.retain(|&i| i != greedy.words[0]);
            let rank = |&i: &u32| {
                let s = self.dict.sig(i);
                (std::cmp::Reverse(s.reward), s.len, i)
            };
            if candidates.len() > alternatives {
                candidates.select_nth_unstable_by_key(alternatives, rank);
                candidates.truncate(alternatives);
            }
            candidates.sort_unstable_by_key(rank);
            let mut best = greedy.score;
            for &i in candidates.iter().take(alternatives) {
                let s = self.dict.sig(i);
                let mut rest = *rack;
                rest.sub(&s.rack);
                best = best.max(s.reward as u32 + kestrel_words::partition::greedy(self.dict, &rest).score);
            }
            best as f64
        } else {
            Solver::with_budget(self.dict, self.params.solve_nodes).solve(rack).score as f64
        }
    }

    /// Endgame-only exact score for a small rack: with no future tiles being
    /// sampled, greedy evaluation can misprice the addition of a single letter.
    fn terminal_exact(&self, rack: &Rack) -> f64 {
        Solver::with_budget(self.dict, 20_000).solve(rack).score as f64
    }

    /// Later auctions that still yield playable letters; the final one cannot.
    fn usable_future(t: &Tracker) -> u32 {
        t.bag_left.saturating_sub(1)
    }

    /// Predicted number of additional letters acquired beyond the current auction.
    fn expected_wins(&self, t: &Tracker) -> f64 {
        let n = Self::usable_future(t) as f64;
        if n <= 0.0 {
            return 0.0;
        }
        let balance = t.my_balance().max(0) as f64;
        let budget = balance * (1.0 + self.params.income_factor * (1.0 - t.progress()));
        let opp = t.n_players().saturating_sub(1).max(1);
        let unseen = t.unseen();
        let total = unseen.len().max(1) as f64;
        let mut avg_price = 0.0;
        for l in 0..ALPHABET {
            let w = unseen.0[l] as f64 / total;
            if w > 0.0 {
                avg_price += w * self.market.dist(l, t.progress(), opp, t.max_opponent_balance()).median;
            }
        }
        let fair_share = n / t.n_players().max(1) as f64;
        let affordable = budget / avg_price.max(1.0);
        (affordable.min(n).max(fair_share * 0.25) * self.params.win_share_scale).min(n)
    }

    fn sample_futures(&mut self, unseen: &Rack, k: usize, n: usize) -> Vec<Rack> {
        let pool: Vec<Letter> = unseen.letters().collect();
        let k = k.min(pool.len());
        let mut out = Vec::with_capacity(n);
        let mut buf = pool.clone();
        for _ in 0..n {
            let mut f = Rack::EMPTY;
            for i in 0..k {
                let j = self.rng.gen_range(i..buf.len());
                buf.swap(i, j);
                f.add(buf[i]);
            }
            out.push(f);
        }
        out
    }

    fn future_size(&self, t: &Tracker) -> usize {
        let k = self.expected_wins(t) * self.params.choice_mult * self.future_mult;
        (k.round() as usize).min(self.params.max_future).min(Self::usable_future(t) as usize)
    }

    /// For rack `rack`, the per-letter marginal values of `letters` plus the
    /// baseline expectation, averaged across `samples` common futures.
    pub fn marginal_values(
        &mut self,
        t: &Tracker,
        rack: &Rack,
        letters: &[Letter],
        samples: usize,
    ) -> (f64, Vec<f64>) {
        self.marginal_values_capped(t, rack, letters, samples, u64::MAX)
    }

    /// Variant of `marginal_values` that halts sampling after `work` units of
    /// effort. One unit equals one letter² of terminal scoring (greedy work
    /// scales with roughly the rack size squared) and costs about 1.7 µs, so
    /// this budget is deterministic while still mirroring elapsed time.
    pub fn marginal_values_capped(
        &mut self,
        t: &Tracker,
        rack: &Rack,
        letters: &[Letter],
        samples: usize,
        work: u64,
    ) -> (f64, Vec<f64>) {
        // The tile currently on offer is already public, so `unseen` excludes it.
        let unseen = t.unseen();
        let k = self.future_size(t);
        let futures = if k == 0 { vec![Rack::EMPTY] } else { self.sample_futures(&unseen, k, samples.max(1)) };
        // Nothing remains to be sampled late in the match; price the rack exactly instead.
        let exact = k == 0 && self.params.exact_terminal_max > 0 && rack.len() < self.params.exact_terminal_max;
        let eval = |me: &Brain, r: &Rack| {
            if exact {
                me.terminal_exact(r)
            } else {
                me.terminal(r)
            }
        };
        let mut base = 0.0;
        let mut with = vec![0.0; letters.len()];
        let mut used = 0u64;
        let mut n = 0usize;
        for f in &futures {
            let mut r = *rack;
            r.plus(f);
            let baseline = eval(self, &r);
            base += baseline;
            for (j, &l) in letters.iter().enumerate() {
                let added = eval(self, &r.with(l));
                with[j] += if self.params.monotone_marginals { added.max(baseline) } else { added };
            }
            n += 1;
            let len = r.len() as u64 + 1;
            let rollout_work = if self.params.solve_nodes == 0 {
                1 + self.params.terminal_rollout_candidates.min(4) as u64
            } else {
                1
            };
            used += len * len * (letters.len() as u64 + 1) * rollout_work;
            if n >= 3 && (used >= work || self.deadline.is_some_and(|d| std::time::Instant::now() >= d)) {
                break;
            }
        }
        let n = n.max(1) as f64;
        (base / n, with.into_iter().map(|w| ((w - base) / n).max(0.0)).collect())
    }

    /// What `l` would be worth to the most threatening modellable opponent (used for denial).
    fn rival_value(&mut self, t: &Tracker, l: Letter) -> f64 {
        let mut best = 0.0f64;
        let rivals: Vec<Rack> = t.opponents().map(|p| p.known_rack()).collect();
        for r in rivals {
            let (_, v) = self.marginal_values_capped(
                t,
                &r,
                &[l],
                self.params.table_samples.max(4),
                self.params.work_budget / 4,
            );
            best = best.max(v[0]);
        }
        best
    }

    fn refresh_table(&mut self, t: &Tracker) {
        let stale = self.table_rack != Some(t.my_rack) || t.auctions_seen >= self.table_at + 4;
        if !stale {
            return;
        }
        let all: Vec<Letter> = (0..ALPHABET).collect();
        let (_, v) = self.marginal_values_capped(
            t,
            &t.my_rack.clone(),
            &all,
            self.params.table_samples,
            self.params.work_budget,
        );
        let prior = letter_prior();
        let w = self.params.table_shrink.clamp(0.0, 1.0);
        for (l, x) in v.into_iter().enumerate() {
            self.table[l] = (1.0 - w) * x + w * prior[l];
        }
        self.table_rack = Some(t.my_rack);
        self.table_at = t.auctions_seen;
    }

    // -------------------------------------------------------- choosing amounts

    fn dist(&self, t: &Tracker, l: Letter) -> BidDist {
        self.dist_at(t, l, t.progress())
    }

    fn dist_at(&self, t: &Tracker, l: Letter, progress: f64) -> BidDist {
        self.market.dist(l, progress, t.n_players().saturating_sub(1), t.max_opponent_balance())
    }

    /// One `(weight, value, rival-bid distribution)` entry per letter not yet
    /// exhausted, evaluated at the given match position `progress`.
    fn letter_mix_at(&self, t: &Tracker, progress: f64) -> Vec<(f64, f64, BidDist)> {
        let unseen = t.unseen();
        let total = unseen.len().max(1) as f64;
        (0..ALPHABET)
            .filter(|&l| unseen.0[l] > 0)
            .map(|l| (unseen.0[l] as f64 / total, self.table[l], self.dist_at(t, l, progress)))
            .collect()
    }

    /// Remaining usable auctions grouped into `(count, letter mix)` segments.
    /// Phase-aware pacing prices each group at its own position in the match,
    /// letting the budget account for both an expensive start and a discounted
    /// finish; without that flag every group uses the current prices.
    fn segments(&self, t: &Tracker) -> Vec<Segment> {
        let n = Self::usable_now(t);
        if !self.params.phase_aware_pacing || n <= 1.0 {
            return vec![(n, self.letter_mix_at(t, t.progress()))];
        }
        let total = t.total_auctions.max(1) as f64;
        let seen = t.auctions_seen as f64;
        let k = (n as usize).clamp(1, 6);
        let per = n / k as f64;
        (0..k)
            .map(|b| {
                let mid = seen + per * (b as f64 + 0.5);
                (per, self.letter_mix_at(t, (mid / total).clamp(0.0, 1.0)))
            })
            .collect()
    }

    /// Projected outlay from this point on, assuming a starting balance of `balance`.
    fn budget_for(&self, t: &Tracker, balance: f64) -> f64 {
        let p = &self.params;
        balance * (1.0 + p.income_factor * (1.0 - t.progress())) * p.spend_target
    }

    /// Find λ such that predicted outlay across `n` auctions equals `budget`,
    /// while no single bid may exceed `cap` coins.
    fn solve_lambda(&self, segs: &[Segment], cap: f64, budget: f64, kind: AuctionKind) -> f64 {
        let p = &self.params;
        // Precompute the CDF values: shading in first-price mode scans integer
        // bids inside every bisection iteration, yet win chances never change.
        let grids: Vec<Vec<Vec<(i64, f64)>>> = if matches!(kind, AuctionKind::FirstPrice) && p.first_price_shade {
            segs.iter()
                .map(|(_, mix)| {
                    mix.iter()
                        .map(|(_, _, d)| (1..=cap.floor() as i64).map(|b| (b, d.p_win(b as f64))).collect())
                        .collect()
                })
                .collect()
        } else {
            Vec::new()
        };
        let spend = |lam: f64| -> f64 {
            segs.iter()
                .enumerate()
                .map(|(segment, (n, mix))| {
                    mix.iter()
                        .enumerate()
                        .map(|(letter, (w, v, d))| {
                            let truthful = (v / lam).min(cap);
                            let payment = match kind {
                                AuctionKind::SecondPrice => d.expected_pay_second_price(truthful),
                                AuctionKind::FirstPrice => {
                                    let bid = if p.first_price_shade {
                                        Self::first_price_bid_from(truthful, grids[segment][letter].iter().copied())
                                            as f64
                                    } else {
                                        truthful.floor()
                                    };
                                    bid * d.p_win(bid)
                                }
                            };
                            w * payment
                        })
                        .sum::<f64>()
                        * n
                })
                .sum::<f64>()
        };
        let (lo, hi) = (p.lambda_min, p.lambda_max);
        if spend(lo) <= budget {
            return lo;
        }
        if spend(hi) >= budget {
            return hi;
        }
        // Raising λ lowers spend(); bisection therefore runs on logarithms.
        let (mut a, mut b) = (lo.ln(), hi.ln());
        for _ in 0..30 {
            let m = 0.5 * (a + b);
            if spend(m.exp()) > budget {
                a = m;
            } else {
                b = m;
            }
        }
        match kind {
            AuctionKind::SecondPrice => (0.5 * (a + b)).exp(),
            // First-price bids are integers, which turns spend into a staircase.
            // Return the bracket's affordable edge, never a point past the jump.
            AuctionKind::FirstPrice => b.exp(),
        }
    }

    /// Points expected to be bought across `n` auctions given shadow price `lam`, bidding at most `cap`.
    fn expected_points(&self, segs: &[Segment], lam: f64, cap: f64, kind: AuctionKind) -> f64 {
        segs.iter()
            .map(|(n, mix)| {
                mix.iter()
                    .map(|(w, v, d)| {
                        let truthful = (v / lam).min(cap);
                        let bid = match kind {
                            AuctionKind::SecondPrice => truthful,
                            AuctionKind::FirstPrice if self.params.first_price_shade => {
                                Self::first_price_bid(truthful, d) as f64
                            }
                            AuctionKind::FirstPrice => truthful.floor(),
                        };
                        w * v * d.p_win(bid)
                    })
                    .sum::<f64>()
                    * n
            })
            .sum()
    }

    /// Count of auctions left that can deliver usable letters, the open one included.
    fn usable_now(t: &Tracker) -> f64 {
        (Self::usable_future(t) + 1) as f64
    }

    /// Choose λ so the predicted outlay on the remaining usable auctions fits the budget.
    fn pace(&mut self, t: &Tracker) -> f64 {
        let balance = t.my_balance().max(0) as f64;
        let segs = self.segments(t);
        self.solve_lambda(&segs, balance, self.budget_for(t, balance), t.kind)
    }

    /// Extra points obtainable from one more coin at this instant (μ). The
    /// pacing λ bottoms out when funds are low, since capped bids barely
    /// spend, whereas μ peaks exactly while the balance limit is binding.
    /// Computed as the finite difference between `B` and `B + Δ` coins.
    pub fn money_value(&mut self, t: &Tracker) -> f64 {
        if t.is_last_auction() || Self::usable_future(t) == 0 {
            return 0.0;
        }
        self.refresh_table(t);
        let b0 = t.my_balance().max(0) as f64;
        let delta = (0.25 * b0).max(10.0);
        let b1 = b0 + delta;
        let segs = self.segments(t);
        let l0 = self.solve_lambda(&segs, b0, self.budget_for(t, b0), t.kind);
        let l1 = self.solve_lambda(&segs, b1, self.budget_for(t, b1), t.kind);
        let gain = self.expected_points(&segs, l1, b1, t.kind) - self.expected_points(&segs, l0, b0, t.kind);
        (gain / delta).clamp(0.0, 3.0)
    }

    /// Choose the amount to submit for `letter` on the auction now open.
    pub fn bid(&mut self, t: &Tracker, letter: Letter) -> BidDecision {
        self.bid_with_floor(t, letter, 0)
    }

    /// Adjust a live bid in the case where a sealed bid already sent can be
    /// lowered only to a positive token figure instead of being withdrawn.
    pub fn bid_with_floor(&mut self, t: &Tracker, letter: Letter, min_bid: i64) -> BidDecision {
        let mut d = self.bid_inner(t, letter);
        let floor = if t.is_last_auction() { 0 } else { min_bid.clamp(0, t.my_balance().max(0)) };
        d.amount = d.amount.max(floor);
        let baseline = d.amount;
        self.refine_endgame_bid(t, letter, &mut d);
        if d.amount < floor {
            // If the planner's answer falls under the minimum executable bid,
            // it cannot stand in for the baseline used in comparison; keep that.
            d.amount = baseline;
        }
        d.p_win = self.dist(t, letter).p_win(d.amount as f64);
        self.last_bid = d.amount;
        d
    }

    fn refine_endgame_bid(&mut self, t: &Tracker, letter: Letter, decision: &mut BidDecision) {
        let max_bag_left = self.params.endgame_rollout_auctions.min(3);
        if max_bag_left == 0
            || t.is_last_auction()
            || t.bag_left > max_bag_left
            || t.my_balance() <= 0
            || (t.bag_left == 1
                && self.params.endgame_allin_auctions > 0
                && self.params.endgame_allin_min_value <= 0.0)
        {
            return;
        }
        let mut budget_ms = self.params.endgame_rollout_budget_ms.min(30);
        if let Some(deadline) = self.deadline {
            budget_ms =
                budget_ms.min(deadline.saturating_duration_since(std::time::Instant::now()).as_millis() as u64);
        }
        if budget_ms == 0 || self.params.endgame_rollout_samples == 0 {
            return;
        }
        let cfg = PlanConfig {
            max_bag_left,
            max_samples: self.params.endgame_rollout_samples.min(64),
            budget_ms,
            ..PlanConfig::default()
        };
        let Some(result) = plan(t, letter, decision.amount, &self.market, self.rng.clone().gen(), &cfg) else {
            return;
        };
        let mut amount = result.bid.clamp(0, t.my_balance());
        if matches!(t.kind, AuctionKind::SecondPrice) {
            amount = amount.min(t.max_opponent_balance().max(0).saturating_add(1));
        }
        if self.params.premium_cap_mult > 0.0 && VALUE[letter] >= 5 && t.progress() < self.params.premium_cap_until {
            let cap = (self.params.premium_cap_mult * self.dist(t, letter).median).ceil() as i64;
            if amount > cap.max(1) {
                // Applying the cap here would substitute a plan we never evaluated.
                return;
            }
        }
        decision.amount = amount;
        decision.p_win = self.dist(t, letter).p_win(amount as f64);
    }

    fn first_price_bid_from(truthful: f64, bids: impl Iterator<Item = (i64, f64)>) -> i64 {
        let mut best = (0.0, 0i64);
        for (bid, probability) in bids.take_while(|(b, _)| *b <= truthful.floor() as i64) {
            let surplus = (truthful - bid as f64) * probability;
            if surplus > best.0 {
                best = (surplus, bid);
            }
        }
        best.1
    }

    fn first_price_bid(truthful: f64, dist: &BidDist) -> i64 {
        Self::first_price_bid_from(truthful, (1..=truthful.floor() as i64).map(|b| (b, dist.p_win(b as f64))))
    }

    /// Immediate placeholder bid for the instant an auction opens, built from
    /// the cached per-letter table (falling back to the letter prior until a
    /// table exists) divided by the last pacing multiplier, then passed through
    /// the same premium and rival limits as a full decision. In live play the
    /// server's reducer queue can lag by up to a second at peak and treats
    /// anything arriving 150 ms after the open as "Auction closed"; this bid
    /// goes out first, then the fuller valuation replaces it when timing allows.
    pub fn anchor_bid(&mut self, t: &Tracker, letter: Letter) -> i64 {
        self.sync_profile(t);
        let balance = t.my_balance().max(0);
        if t.is_last_auction() || balance <= 0 {
            return 0;
        }
        let value = if self.table_rack.is_some() { self.table[letter] } else { letter_prior()[letter] };
        if value < self.params.min_value {
            return 0;
        }
        let lambda = self.last_lambda.max(self.params.lambda_min).max(1e-3);
        // Shrinkage toward the letter prior leaves premium tiles priced high:
        // measured live, the unadjusted anchor exceeded the full bid in 52% of
        // cases and beat it by 30%+ in 28%. So shade slightly below that anchor;
        // when the full decision arrives in time, it takes over anyway.
        let mut truthful = ANCHOR_SCALE * value / lambda;
        if self.params.opening_shade > 0.0 && t.progress() < 0.25 {
            truthful *= 1.0 - self.params.opening_shade.min(0.9);
        }
        // First-price mode leaves no time to optimise shading, so stay far below value.
        if matches!(t.kind, AuctionKind::FirstPrice) && self.params.first_price_shade {
            truthful *= 0.7;
        }
        let mut amount = truthful.floor() as i64;
        if self.params.premium_cap_mult > 0.0 && VALUE[letter] >= 5 && t.progress() < self.params.premium_cap_until {
            let cap = (self.params.premium_cap_mult * self.dist(t, letter).median).ceil() as i64;
            amount = amount.min(cap.max(1));
        }
        if matches!(t.kind, AuctionKind::SecondPrice) {
            amount = amount.min(t.max_opponent_balance().max(0) + 1);
        }
        amount.clamp(0, balance)
    }

    fn bid_inner(&mut self, t: &Tracker, letter: Letter) -> BidDecision {
        self.sync_profile(t);
        self.deadline = (self.params.bid_deadline_ms > 0)
            .then(|| std::time::Instant::now() + std::time::Duration::from_millis(self.params.bid_deadline_ms));
        let balance = t.my_balance().max(0);
        if t.is_last_auction() || balance <= 0 {
            return BidDecision::default();
        }
        self.refresh_table(t);
        let (_, v) = self.marginal_values_capped(
            t,
            &t.my_rack.clone(),
            &[letter],
            self.params.samples,
            self.params.work_budget,
        );
        let mut value = v[0];
        self.table[letter] = value;
        // High-value tiles depend on particular companions, so price them against a longer horizon.
        if self.params.premium_choice_mult > 1.0 && VALUE[letter] >= 5 {
            self.future_mult = self.params.premium_choice_mult;
            let (_, v) = self.marginal_values_capped(
                t,
                &t.my_rack.clone(),
                &[letter],
                self.params.samples,
                self.params.work_budget,
            );
            self.future_mult = 1.0;
            value = v[0];
            self.table[letter] = value;
        }
        if self.params.deny_weight > 0.0 && t.n_players() <= self.params.deny_max_players {
            value += self.params.deny_weight * self.rival_value(t, letter);
        }
        let lambda = self.pace(t);
        self.last_lambda = lambda;
        // Only enabled explicitly: once the last usable auction arrives, money
        // has no further purpose. A zero threshold additionally permits a pure
        // denial purchase, even where the tile adds nothing to our own rack.
        if t.bag_left == 1 && self.params.endgame_allin_auctions > 0 && self.params.endgame_allin_min_value <= 0.0 {
            let amount = if matches!(t.kind, AuctionKind::SecondPrice) {
                balance.min(t.max_opponent_balance().max(0).saturating_add(1))
            } else {
                balance
            };
            return BidDecision { amount, value, lambda, p_win: self.dist(t, letter).p_win(amount as f64) };
        }
        if value < self.params.min_value {
            let amount =
                if value >= self.params.cheap_grab_value.min(self.params.min_value) && value > 0.0 { 1 } else { 0 };
            let d = self.dist(t, letter);
            return BidDecision { amount, value, lambda, p_win: d.p_win(amount as f64) };
        }
        let d = self.dist(t, letter);
        let mut truthful = (value / lambda).min(balance as f64);
        // Completion bonus: in live data 64% of valuable auctions we lost went
        // to a rival's 7+ word, median gap 5 coins. If this tile immediately
        // enables a 7+ word from our rack, bid above raw value; with second-price
        // pricing that premium is charged only on a winning bid.
        if self.params.completion_premium > 0.0 && value >= self.params.completion_min_value {
            let dict = self.dict;
            let has7 = |r: &Rack| !dict.candidates(r, 7).is_empty();
            if !has7(&t.my_rack) && has7(&t.my_rack.with(letter)) {
                truthful *= 1.0 + self.params.completion_premium;
            }
        }
        // Endgame: once only a handful of usable auctions remain, spare coins
        // have no value, so a tile worth genuine points justifies the whole
        // balance. Under second-price rules the runner-up sets the price, so
        // this adds no cost in cases we were winning regardless.
        let left = Self::usable_future(t) + 1;
        if self.params.endgame_allin_auctions > 0
            && left <= self.params.endgame_allin_auctions
            && value >= self.params.endgame_allin_min_value
        {
            truthful = balance as f64;
        }
        // Opening-quarter prices sit about 35% above fair; an optional underbid applies there.
        if self.params.opening_shade > 0.0 && t.progress() < 0.25 {
            truthful *= 1.0 - self.params.opening_shade.min(0.9);
        }
        let mut amount = match t.kind {
            AuctionKind::SecondPrice => truthful.floor() as i64,
            AuctionKind::FirstPrice if self.params.first_price_shade => {
                // Choose b maximising (v/λ − b) · P(win | b)
                Self::first_price_bid(truthful, &d)
            }
            AuctionKind::FirstPrice => truthful.floor() as i64,
        };
        if amount < 1 && value >= self.params.cheap_grab_value {
            amount = 1;
        }
        // Volume floor: for ordinary tiles that slot into the rack, never bid
        // below a fraction of the going rate. That way uncontested letters still
        // land with us (second-price: the rivals' bid sets the price, not ours).
        // The live leader secured 4.5 opening letters at 11.7 this way; we paid 15.6.
        if self.params.volume_floor > 0.0
            && VALUE[letter] < 5
            && value >= self.params.volume_min_value
            && !t.is_last_auction()
        {
            let floor = (self.params.volume_floor * d.median).floor() as i64;
            amount = amount.max(floor);
        }
        // The high-value tiles (J, K, Q, X, Z) only pay off inside particular
        // words; the strongest live competitor would not bid far above market for them.
        if self.params.premium_cap_mult > 0.0 && VALUE[letter] >= 5 && t.progress() < self.params.premium_cap_until {
            let cap = (self.params.premium_cap_mult * d.median).ceil() as i64;
            amount = amount.min(cap.max(1));
        }
        // Strong live players reliably bid roughly 2.3-3.0x face for premium
        // tiles, plus a term tied to the round (fit on 20k winning bids:
        // LUCENT 9.8 + 3.0*face; Bron 5.6 + 2.3*face + 0.16*round). When the
        // sampled future happens to omit a premium's partners, our Monte Carlo
        // estimate drops under that level, so this floor holds us at market.
        if self.params.premium_floor_mult > 0.0 && VALUE[letter] >= 5 {
            let floor = self.params.premium_floor_base
                + self.params.premium_floor_mult * VALUE[letter] as f64
                + self.params.premium_floor_round * t.auctions_seen as f64;
            amount = amount.max((floor.floor() as i64).clamp(1, balance));
        }
        // Completion floor (from Lala): when a tile substantially improves our
        // best long word, its value is at least some multiple of the going price.
        if self.params.completion_floor_mult > 0.0 {
            let len = self.params.completion_floor_len;
            let dict = self.dict;
            let best = |r: &Rack| dict.best_word(r, len).map(|w| w.reward as f64).unwrap_or(0.0);
            let gain = best(&t.my_rack.with(letter)) - best(&t.my_rack);
            if gain >= self.params.completion_floor_gain {
                let floor = (self.params.completion_floor_mult * d.median).ceil() as i64;
                amount = amount.max(floor);
            }
        }
        // Drain floor (Lambi Lambiii), two-player tables only: any letter with
        // nonzero value is bid at a fraction of the going price to cost the rival more.
        if self.params.drain_floor > 0.0 && t.n_players() == 2 && value > 0.0 {
            let floor = (self.params.drain_floor * d.median).floor() as i64;
            amount = amount.max(floor);
        }
        // A rival cannot exceed their balance, so with second-price pricing any
        // amount above richest-rival balance + 1 wins and pays identically.
        // Capping at that point is strictly dominant, and an all-in bid then
        // costs nothing extra when every opponent is nearly broke.
        let rival_cap = t.max_opponent_balance().max(0) + 1;
        let amount = if matches!(t.kind, AuctionKind::SecondPrice) { amount.min(rival_cap) } else { amount };
        let amount = amount.clamp(0, balance);
        BidDecision { amount, value, lambda, p_win: d.p_win(amount as f64) }
    }

    /// Feed a finished auction into the price model.
    pub fn observe_result(&mut self, t: &Tracker, letter: Letter, winner: Option<u64>, paid: i64) {
        let we_won = winner == Some(t.my_id);
        if winner.is_some() {
            match t.kind {
                AuctionKind::SecondPrice => self.market.observe(
                    letter,
                    t.progress(),
                    paid,
                    we_won,
                    t.n_players().saturating_sub(1),
                    self.last_bid,
                ),
                AuctionKind::FirstPrice => self.market.observe_first_price(
                    letter,
                    t.progress(),
                    paid,
                    we_won,
                    t.n_players().saturating_sub(1),
                ),
            }
        }
    }

    // -------------------------------------------------------- laying tiles

    /// Mid-match plays for the current turn; an empty list means holding is preferable.
    pub fn plays(&mut self, t: &Tracker) -> Vec<&'static str> {
        self.sync_profile(t);
        if t.is_last_auction() {
            return self.final_plays(&t.my_rack);
        }
        // Over the closing auctions letters cost little, and coins earned from
        // the final dump cannot be spent afterwards, so short words become worth cashing.
        let late = self.params.late_window > 0 && t.bag_left <= self.params.late_window;
        // Low on coins while auctions remain: each one we skip hands a cheap
        // letter to an opponent. Live games exhibited 20–40-auction stretches
        // with no play, despite a usable 5–6 letter word sitting in the rack.
        let starved = t.my_balance() < self.params.starve_coins && Self::usable_future(t) >= 3;
        let mut min_len = self.params.min_play_len;
        if late {
            min_len = min_len.min(self.params.late_min_play_len);
        }
        if starved {
            min_len = min_len.min(self.params.starve_min_play_len);
        }
        if self.dict.candidates(&t.my_rack, min_len).is_empty() {
            return Vec::new();
        }
        let mu = if self.params.mu_liquidity || late || starved { self.money_value(t) } else { self.last_lambda };
        let p = self.params.clone();
        let mut rack = t.my_rack;
        let mut out = Vec::new();
        let k = self.future_size(t);
        let futures =
            if k == 0 { vec![Rack::EMPTY] } else { self.sample_futures(&t.unseen(), k, p.samples.max(8) / 2) };
        let eval = |me: &Brain, r: &Rack| -> f64 {
            futures
                .iter()
                .map(|f| {
                    let mut x = *r;
                    x.plus(f);
                    me.terminal(&x)
                })
                .sum::<f64>()
                / futures.len() as f64
        };
        for _ in 0..3 {
            let mut cands: Vec<u32> = self.dict.candidates(&rack, min_len);
            if cands.is_empty() {
                break;
            }
            cands.sort_unstable_by_key(|&i| std::cmp::Reverse(self.dict.sig(i).reward));
            cands.truncate(p.play_candidates);
            let hold = eval(self, &rack);
            let mut best: Option<(f64, u32)> = None;
            for &i in &cands {
                let s = self.dict.sig(i);
                // Save low-scoring words in hope of a better one, except when short of funds or near the end.
                if p.cash_min_reward > 0 && !late && !starved && (s.reward as u32) < p.cash_min_reward {
                    continue;
                }
                let mut rest = rack;
                rest.sub(&s.rack);
                let gain = s.reward as f64 * (1.0 + p.liquidity * mu) + eval(self, &rest) - hold;
                if gain > p.play_margin && best.is_none_or(|(g, _)| gain > g) {
                    best = Some((gain, i));
                }
            }
            let Some((_, i)) = best else { break };
            let s = self.dict.sig(i);
            rack.sub(&s.rack);
            out.push(s.word);
        }
        out
    }

    /// Full set of plays available for the last auction.
    pub fn final_plays(&self, rack: &Rack) -> Vec<&'static str> {
        let mut s = Solver::with_deadline(self.dict, self.params.final_solve_ms);
        s.solve(rack).word_strings(self.dict)
    }

    /// Letter's printed face value, exposed for logging.
    pub fn face(l: Letter) -> u32 {
        VALUE[l]
    }
}

/// Mean marginal worth of every letter across random mid-match racks
/// (12 in hand plus 8 incoming), evaluated once per process. Serves as the
/// shrinkage target for the noisy per-auction value table.
pub fn letter_prior() -> &'static [f64; ALPHABET] {
    static PRIOR: std::sync::OnceLock<[f64; ALPHABET]> = std::sync::OnceLock::new();
    PRIOR.get_or_init(|| {
        let d = dictionary();
        let mut rng = SmallRng::seed_from_u64(0x5eed);
        let pool: Vec<Letter> =
            (0..ALPHABET).flat_map(|l| std::iter::repeat_n(l, kestrel_words::BAG[l] as usize)).collect();
        let mut sum = [0.0; ALPHABET];
        let n = 160;
        for _ in 0..n {
            let mut buf = pool.clone();
            let mut r = Rack::EMPTY;
            for i in 0..20 {
                let j = rng.gen_range(i..buf.len());
                buf.swap(i, j);
                r.add(buf[i]);
            }
            let base = kestrel_words::partition::greedy(d, &r).score as f64;
            for (l, slot) in sum.iter_mut().enumerate() {
                *slot += (kestrel_words::partition::greedy(d, &r.with(l)).score as f64 - base).max(0.0);
            }
        }
        sum.map(|x| x / n as f64)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kestrel_words::{index, word_reward};

    fn tracker_with(rack: &str, bag_left: u32) -> Tracker {
        let mut t = Tracker::new(1, 1, AuctionKind::SecondPrice, &[1, 2, 3, 4, 5, 6], 5);
        t.set_my_rack(Rack::from_word(rack));
        t.on_auction_open(index('E').unwrap(), bag_left);
        t
    }

    #[test]
    fn rollout_recovers_the_partition_hidden_by_a_greedy_switch() {
        // Greedy is lured by H into a weaker opening word even though H may be
        // discarded; trying two alternate openings recovers the exact score.
        let rack = Rack::from_word("DDEGILLORTUX");
        let next = rack.with(index('H').unwrap());
        let greedy = Brain::new(Params::default(), FieldModel::default(), 1);
        let mut rollout =
            Brain::new(Params { terminal_rollout_candidates: 2, ..Params::default() }, FieldModel::default(), 1);
        assert_eq!((greedy.terminal(&rack), greedy.terminal(&next)), (47.0, 42.0));
        assert_eq!((rollout.terminal(&rack), rollout.terminal(&next)), (49.0, 55.0));
        assert_eq!(rollout.terminal(&rack), rollout.terminal_exact(&rack));
        assert_eq!(rollout.terminal(&next), rollout.terminal_exact(&next));
        // Deadline checks happen only between whole samples. Letting it expire
        // inside a baseline/addition pair must leave terminal effort unchanged.
        rollout.deadline = Some(std::time::Instant::now());
        assert_eq!((rollout.terminal(&rack), rollout.terminal(&next)), (49.0, 55.0));
    }

    #[test]
    fn monotone_samples_do_not_let_greedy_losses_cancel_real_gains() {
        let rack = Rack::from_word("DDEILLORTUX");
        let mut t = tracker_with("DDEILLORTUX", 2);
        // A and G are the only unseen tiles. Adding G makes greedy score 47
        // before H and 42 once H is present; discarding H means that drop cannot
        // occur. Adding A lets H raise the greedy score by ten.
        let unseen = Rack::from_word("AG");
        t.revealed = Rack(kestrel_words::BAG);
        t.revealed.sub(&rack);
        t.revealed.sub(&unseen);
        assert_eq!(t.unseen(), unseen);
        let mut plain = Brain::new(Params::default(), FieldModel::default(), 1);
        let mut monotone =
            Brain::new(Params { monotone_marginals: true, ..Params::default() }, FieldModel::default(), 1);
        assert_eq!(plain.future_size(&t), 1);
        let h = index('H').unwrap();
        let (base, raw) = plain.marginal_values(&t, &rack, &[h], 16);
        let (same_base, corrected) = monotone.marginal_values(&t, &rack, &[h], 16);
        assert_eq!(base, same_base);
        assert_eq!(base, 48.5);
        assert_eq!(raw, vec![0.625]);
        assert_eq!(corrected, vec![3.75]);
    }

    #[test]
    fn first_price_pacing_budgets_own_bid_instead_of_the_runner_up() {
        let brain = Brain::new(Params { first_price_shade: false, ..Params::default() }, FieldModel::default(), 1);
        let dist = BidDist { median: 8.0, sigma: 0.3, cap: 20.0, p_none: 0.0 };
        let segs = vec![(40.0, vec![(1.0, 20.0, dist)])];
        let second_price = brain.solve_lambda(&segs, 50.0, 200.0, AuctionKind::SecondPrice);
        let first = brain.solve_lambda(&segs, 50.0, 200.0, AuctionKind::FirstPrice);
        let richer = brain.solve_lambda(&segs, 50.0, 400.0, AuctionKind::FirstPrice);
        let bid = (20.0 / first).floor();
        let actual_spend = 40.0 * bid * dist.p_win(bid);
        let upper_spend = 40.0 * (bid + 1.0) * dist.p_win(bid + 1.0);
        // Reusing the second-price multiplier in a first-price game overspends,
        // because the winner pays its own amount, not the rival's smaller one.
        let old_bid = (20.0 / second_price).floor();
        assert!(40.0 * old_bid * dist.p_win(old_bid) > 200.0);
        assert!(first > second_price);
        assert!(richer < first);
        assert!(actual_spend <= 200.0 + 1e-6);
        assert!(upper_spend >= 200.0 - 1e-6);
    }

    #[test]
    fn first_price_pacing_and_point_estimate_use_the_shaded_bid() {
        let brain = Brain::new(Params::default(), FieldModel::default(), 1);
        let dist = BidDist { median: 8.0, sigma: 0.3, cap: 20.0, p_none: 0.0 };
        let segs = vec![(40.0, vec![(1.0, 20.0, dist)])];
        let lam = brain.solve_lambda(&segs, 50.0, 200.0, AuctionKind::FirstPrice);
        let truth = (20.0 / lam).min(50.0);
        let bid = Brain::first_price_bid(truth, &dist);
        let spend = 40.0 * bid as f64 * dist.p_win(bid as f64);
        assert!((bid as f64) < truth);
        assert!(spend <= 200.0 + 1e-6);
        assert!(40.0 * (bid + 1) as f64 * dist.p_win((bid + 1) as f64) >= 200.0 - 1e-6);
        assert_eq!(
            brain.expected_points(&segs, lam, 50.0, AuctionKind::FirstPrice),
            40.0 * 20.0 * dist.p_win(bid as f64)
        );
    }

    #[test]
    fn market_observations_use_the_auction_payment_rule() {
        let mut tracker = tracker_with("RETAINS", 40);
        let mut first = Brain::new(Params::default(), FieldModel::default(), 1);
        let mut second = Brain::new(Params::default(), FieldModel::default(), 1);
        let letter = index('A').unwrap();
        // A 100 payment after winning a first-price auction reveals nothing
        // about rival heat: that was our own bid. Under second-price it is their maximum.
        tracker.kind = AuctionKind::FirstPrice;
        first.observe_result(&tracker, letter, Some(tracker.my_id), 100);
        tracker.kind = AuctionKind::SecondPrice;
        second.observe_result(&tracker, letter, Some(tracker.my_id), 100);
        assert_eq!(first.market.heat, 1.0);
        assert!(second.market.heat > 1.0);
    }

    #[test]
    fn endgame_abstention_preserves_the_bid_and_valuation_rng() {
        let mut brain =
            Brain::new(Params { endgame_rollout_auctions: 3, ..Params::default() }, FieldModel::default(), 1);
        let tracker = tracker_with("RETAINS", 2);
        let mut decision = BidDecision { amount: 17, value: 10.0, lambda: 0.3, p_win: 0.5 };
        brain.deadline = Some(std::time::Instant::now());
        let mut untouched_rng = brain.rng.clone();
        brain.refine_endgame_bid(&tracker, index('S').unwrap(), &mut decision);
        assert_eq!(decision.amount, 17);
        assert_eq!(decision.p_win, 0.5);
        // The bidding cut-off prevented any hidden-state rollout from starting.
        assert_eq!(brain.rng.gen::<u64>(), untouched_rng.gen::<u64>());

        // A plan attempt that finds too few complete pairs must likewise leave
        // the random stream untouched for subsequent valuations.
        brain.deadline = None;
        brain.params.endgame_rollout_samples = 1;
        let mut untouched_rng = brain.rng.clone();
        brain.refine_endgame_bid(&tracker, index('S').unwrap(), &mut decision);
        assert_eq!(decision.amount, 17);
        assert_eq!(decision.p_win, 0.5);
        assert_eq!(brain.rng.gen::<u64>(), untouched_rng.gen::<u64>());
    }

    #[test]
    fn endgame_refinement_does_not_buy_an_unplayable_terminal_tile() {
        let mut brain = Brain::new(
            Params { endgame_rollout_auctions: 3, deny_weight: 0.0, ..Params::default() },
            FieldModel::default(),
            1,
        );
        let letter = index('Z').unwrap();
        let mut tracker = Tracker::new(1, 1, AuctionKind::SecondPrice, &[1, 2, 3, 4, 5, 6], 5);
        tracker.on_auction_open(letter, 1);
        // This tile could be played in time, yet a lone Z cannot make a word
        // from an empty rack, and no helpful draws remain in the future.
        let decision = brain.bid(&tracker, letter);
        assert_eq!(decision.value, 0.0);
        assert_eq!(decision.amount, 0);
        tracker.bag_left = 0;
        assert_eq!(brain.bid(&tracker, letter).amount, 0);
    }

    #[test]
    #[ignore = "manual coverage probe for the cooperative 30 ms planner budget"]
    fn endgame_refinement_can_block_a_rival_without_own_tile_value() {
        let mut brain = Brain::new(
            Params {
                endgame_rollout_auctions: 1,
                endgame_rollout_samples: 64,
                deny_weight: 0.0,
                ..Params::default()
            },
            FieldModel { median: [100.0; ALPHABET], sigma: 0.01, phase: vec![1.0], p_none: 0.0, ref_players: 2 },
            1,
        );
        let mut tracker = Tracker::new(1, 1, AuctionKind::SecondPrice, &[1, 2], 0);
        tracker.players[0].score = 12;
        tracker.players[0].balance = 20;
        tracker.players[1].score = 9;
        tracker.players[1].balance = 5;
        tracker.players[1].won = Rack::from_word("AT");
        tracker.revealed = Rack(kestrel_words::BAG);
        tracker.revealed.remove(index('E').unwrap());
        tracker.bag_left = 1;
        tracker.auctions_seen = 97;
        // C adds nothing to our empty rack. If the rival receives it, AT=2
        // becomes CAT=5, lifting their final total from 11 to 14 and beating 12.
        let decision = brain.bid(&tracker, index('C').unwrap());
        assert_eq!(decision.value, 0.0);
        assert!(decision.amount > 5 && decision.amount <= 20);
    }

    #[test]
    fn zero_threshold_last_playable_bid_denies_even_without_own_points() {
        let params =
            Params { endgame_allin_auctions: 1, endgame_allin_min_value: 0.0, deny_weight: 0.0, ..Params::default() };
        let mut brain = Brain::new(params, FieldModel::default(), 1);
        let mut tracker = Tracker::new(1, 1, AuctionKind::SecondPrice, &[1, 2], 0);
        tracker.players[0].balance = 20;
        tracker.players[1].balance = 5;
        tracker.players[1].won = Rack::from_word("AT");
        tracker.on_auction_open(index('C').unwrap(), 1);
        // Alone the tile scores nothing, but denial keeps the rival at AT=2
        // rather than CAT=5. Bidding past their five coins secures the denial.
        let decision = brain.bid(&tracker, index('C').unwrap());
        assert_eq!(decision.value, 0.0);
        assert_eq!(decision.amount, 6);
        let score =
            |rack: &str| brain.final_plays(&Rack::from_word(rack)).iter().map(|w| word_reward(w)).sum::<u32>();
        assert_eq!((score("AT"), score("ACT"), score("C")), (2, 5, 0));
        tracker.kind = AuctionKind::FirstPrice;
        // Shading would be pointless: leftover coins cannot buy another useful tile.
        assert_eq!(brain.bid(&tracker, index('C').unwrap()).amount, 20);
        tracker.bag_left = 0;
        assert_eq!(brain.bid(&tracker, index('C').unwrap()).amount, 0);
    }

    #[test]
    fn executable_bid_floor_survives_zero_value_and_planner_abstention() {
        for window in [0, 3] {
            let mut brain = Brain::new(
                Params { endgame_rollout_auctions: window, ..Params::default() },
                FieldModel::default(),
                1,
            );
            let letter = index('Z').unwrap();
            let mut tracker = Tracker::new(1, 1, AuctionKind::SecondPrice, &[1, 2, 3, 4, 5, 6], 0);
            tracker.players[0].balance = 3;
            tracker.on_auction_open(letter, 1);
            // With history incomplete, hidden-state rollouts would be
            // inconsistent. Declining to plan must keep the executable token bid.
            let decision = brain.bid_with_floor(&tracker, letter, 1);
            assert_eq!((decision.value, decision.amount), (0.0, 1));
            assert_eq!(brain.last_bid, 1);
            assert_eq!(decision.p_win, brain.dist(&tracker, letter).p_win(1.0));
            // The floor is bounded by the funds actually available.
            assert_eq!(brain.bid_with_floor(&tracker, letter, 100).amount, 3);
            // Without a prior submission, a zero-value tile stays at a zero bid.
            assert_eq!(brain.bid_with_floor(&tracker, letter, 0).amount, 0);
            tracker.bag_left = 0;
            let final_bid = brain.bid_with_floor(&tracker, letter, 1);
            assert_eq!((final_bid.amount, final_bid.p_win), (0, 0.0));
            assert_eq!(brain.last_bid, 0);
        }
    }

    #[test]
    fn drain_floor_applies_heads_up_only() {
        // A letter carrying value: at heads-up the drain floor raises the bid
        // toward a fraction of the market median; at a six-player table it has no effect.
        let e = index('E').unwrap();
        let p = Params { drain_floor: 0.8, ..Params::default() };
        let mut duel = Tracker::new(1, 1, AuctionKind::SecondPrice, &[1, 2], 5);
        duel.set_my_rack(Rack::from_word("RTNSL"));
        duel.on_participant(1, 90, 0);
        duel.on_participant(2, 90, 0);
        duel.on_auction_open(e, 40);
        let mut b = Brain::new(p.clone(), FieldModel::default(), 7);
        let d = b.bid(&duel, e);
        if d.value > 0.0 {
            let floor = (0.8 * b.dist(&duel, e).median).floor() as i64;
            assert!(d.amount >= floor.min(91), "{d:?} floor {floor}");
        }
        let mut six = tracker_with("RTNSL", 40);
        six.on_participant(1, 90, 0);
        let mut a = Brain::new(p, FieldModel::default(), 7);
        let mut base = Brain::new(Params::default(), FieldModel::default(), 7);
        assert_eq!(a.bid(&six, e).amount, base.bid(&six, e).amount);
    }

    #[test]
    fn cash_min_reward_holds_cheap_words_but_not_when_starved() {
        // Mid-match with ample coins, a 7-letter word scoring below 45 points.
        let mut t = Tracker::new(1, 1, AuctionKind::SecondPrice, &[1, 2, 3, 4, 5, 6], 5);
        t.set_my_rack(Rack::from_word("RETAINS"));
        t.on_auction_open(index('E').unwrap(), 40);
        t.on_participant(1, 80, 0);
        let rich = Params { cash_min_reward: 45, ..Params::default() };
        let mut b = Brain::new(rich.clone(), FieldModel::default(), 2);
        assert!(b.plays(&t).iter().all(|w| word_reward(w) >= 45), "{:?}", b.plays(&t));
        // Once broke, the threshold stops applying.
        t.on_participant(1, 3, 0);
        let mut b = Brain::new(rich, FieldModel::default(), 2);
        let base = Brain::new(Params::default(), FieldModel::default(), 2).plays(&t).len();
        assert_eq!(b.plays(&t).len(), base);
    }

    #[test]
    fn longer_premium_horizon_raises_a_premium_value() {
        // Mid-match with a U already held, a Q seldom finds its partners in a
        // short sampled future, yet does so over a longer horizon.
        let mut t = Tracker::new(1, 1, AuctionKind::SecondPrice, &[1, 2, 3, 4, 5, 6], 5);
        t.set_my_rack(Rack::from_word("UIRTE"));
        let q = index('Q').unwrap();
        t.on_auction_open(q, 34);
        let mut short = Brain::new(Params::default(), FieldModel::default(), 4);
        let p = Params { premium_choice_mult: 3.0, ..Params::default() };
        let mut long = Brain::new(p, FieldModel::default(), 4);
        let (a, b) = (short.bid(&t, q).value, long.bid(&t, q).value);
        assert!(b >= a, "short {a} long {b}");
        // Ordinary letters see no change.
        let e = index('E').unwrap();
        let mut s2 = Brain::new(Params::default(), FieldModel::default(), 4);
        let mut l2 = Brain::new(Params { premium_choice_mult: 3.0, ..Params::default() }, FieldModel::default(), 4);
        t.on_auction_open(e, 33);
        assert_eq!(s2.bid(&t, e).value, l2.bid(&t, e).value);
    }

    #[test]
    fn anchor_bid_is_bounded_and_near_the_full_bid() {
        let mut b = Brain::new(Params::default(), FieldModel::default(), 3);
        let t = tracker_with("CAZIQU", 30);
        let e = index('E').unwrap();
        // Until a value table has been built, the anchor is taken from the letter prior.
        let a0 = b.anchor_bid(&t, e);
        assert!((0..=100).contains(&a0), "{a0}");
        let full = b.bid(&t, e);
        // A full decision has warmed the table and λ; the anchor should stay within the same scale.
        let a1 = b.anchor_bid(&t, e);
        assert!(a1 > 0, "anchor {a1} full {full:?}");
        assert!((a1 as f64) <= 2.0 * full.amount.max(1) as f64 + 5.0, "anchor {a1} full {full:?}");
        assert_eq!(b.anchor_bid(&tracker_with("CAZIQU", 0), e), 0);
    }

    #[test]
    fn last_auction_is_worthless() {
        let mut b = Brain::new(Params::default(), FieldModel::default(), 1);
        let t = tracker_with("QUIZAS", 0);
        assert_eq!(b.bid(&t, index('E').unwrap()).amount, 0);
    }

    #[test]
    fn completing_letter_is_valued() {
        let mut b = Brain::new(Params::default(), FieldModel::default(), 1);
        // Just before the end, CAZIQU plus E yields CAZIQUE for 81 points.
        let t = tracker_with("CAZIQU", 1);
        let d = b.bid(&t, index('E').unwrap());
        assert!(d.value > 40.0, "{d:?}");
        assert!(d.amount >= 50, "{d:?}");
    }

    #[test]
    fn final_dump_plays_everything_useful() {
        let b = Brain::new(Params::default(), FieldModel::default(), 1);
        let words = b.final_plays(&Rack::from_word("CAZIQUEXI"));
        assert!(words.iter().any(|w| w.len() == 7));
    }

    #[test]
    fn money_is_worth_more_when_broke() {
        let mut b = Brain::new(Params::default(), FieldModel::default(), 5);
        let mut t = tracker_with("ACDESUX", 20);
        t.on_participant(1, 8, 50);
        let broke = b.money_value(&t);
        t.on_participant(1, 200, 50);
        let rich = b.money_value(&t);
        assert!(broke > rich, "broke {broke} rich {rich}");
        assert!(broke > 0.05);
    }

    #[test]
    fn cashes_short_word_when_starved() {
        let mut b = Brain::new(Params::default(), FieldModel::default(), 9);
        // JIVER (five letters, 30 points) while 38 auctions remain and only 7 coins are held.
        let mut t = tracker_with("EIJRV", 38);
        t.on_participant(1, 7, 48);
        assert_eq!(b.plays(&t), vec!["JIVER"]);
        // With funds available, keep building toward a seven-letter word.
        t.on_participant(1, 90, 48);
        assert!(b.plays(&t).is_empty());
    }

    #[test]
    fn premium_cap_limits_premium_bids_only() {
        let capped = Params { premium_cap_mult: 0.5, ..Params::default() };
        let mut b = Brain::new(capped, FieldModel::default(), 4);
        // Z finishes a seven-letter word, so it is valuable, yet the cap holds it to half the market.
        let t = tracker_with("ABLEIT", 20);
        let z = b.bid(&t, index('Z').unwrap());
        let median = b.market.dist(index('Z').unwrap(), t.progress(), 5, 100).median;
        assert!((z.amount as f64) <= (0.5 * median).ceil(), "{z:?} median {median}");
        // Frequent letters are unaffected.
        let mut free = Brain::new(Params::default(), FieldModel::default(), 4);
        let mut capped2 = Brain::new(Params { premium_cap_mult: 0.5, ..Params::default() }, FieldModel::default(), 4);
        let e = index('E').unwrap();
        assert_eq!(free.bid(&t, e).amount, capped2.bid(&t, e).amount);
    }

    #[test]
    fn opening_shade_only_applies_early() {
        let shaded = Params { opening_shade: 0.3, ..Params::default() };
        let e = index('E').unwrap();
        // Early on, the shaded bid comes out lower.
        let early = tracker_with("ABLEIT", 66);
        let (mut a, mut b) = (
            Brain::new(Params::default(), FieldModel::default(), 9),
            Brain::new(shaded.clone(), FieldModel::default(), 9),
        );
        let (plain, shade) = (a.bid(&early, e).amount, b.bid(&early, e).amount);
        assert!(shade < plain || plain == 0, "plain {plain} shaded {shade}");
        // Past 25% progress, the two bids match.
        let mut late = tracker_with("ABLEIT", 20);
        for _ in 0..40 {
            late.on_auction_open(e, 20);
        }
        let (mut a, mut b) =
            (Brain::new(Params::default(), FieldModel::default(), 9), Brain::new(shaded, FieldModel::default(), 9));
        assert_eq!(a.bid(&late, e).amount, b.bid(&late, e).amount);
    }

    #[test]
    fn phase_aware_pacing_gives_a_sane_lambda() {
        let p = Params { phase_aware_pacing: true, ..Params::default() };
        let mut b = Brain::new(p.clone(), FieldModel::default(), 2);
        let t = tracker_with("ABLEIT", 60);
        let d = b.bid(&t, index('S').unwrap());
        assert!(d.lambda >= p.lambda_min && d.lambda <= p.lambda_max, "{d:?}");
        assert!(d.amount >= 0 && d.amount <= 100);
    }

    #[test]
    fn exact_terminal_sees_what_greedy_misses() {
        // Greedy takes the single strongest word up front, while the exact
        // solver locates a superior two-word split. Exact value may never be lower.
        let exact = Brain::new(Params { exact_terminal_max: 16, ..Params::default() }, FieldModel::default(), 1);
        let greedy = Brain::new(Params::default(), FieldModel::default(), 1);
        let mut rng = SmallRng::seed_from_u64(3);
        let pool: Vec<Letter> =
            (0..ALPHABET).flat_map(|l| std::iter::repeat_n(l, kestrel_words::BAG[l] as usize)).collect();
        let mut strictly_better = 0;
        for _ in 0..60 {
            let mut r = Rack::EMPTY;
            for _ in 0..12 {
                r.add(pool[rng.gen_range(0..pool.len())]);
            }
            let (e, g) = (exact.terminal_exact(&r), greedy.terminal(&r));
            assert!(e >= g, "exact {e} < greedy {g} for {r}");
            strictly_better += (e > g) as u32;
        }
        assert!(strictly_better > 0);
    }

    #[test]
    fn volume_floor_lifts_low_bids_on_fitting_common_letters() {
        let t = tracker_with("ABLEIT", 40);
        let s = index('S').unwrap();
        let mut plain = Brain::new(Params::default(), FieldModel::default(), 6);
        let mut floored = Brain::new(
            Params { volume_floor: 0.9, volume_min_value: 0.0, ..Params::default() },
            FieldModel::default(),
            6,
        );
        let (p, f) = (plain.bid(&t, s), floored.bid(&t, s));
        let median = floored.market.dist(s, t.progress(), 5, 100).median;
        assert!(f.amount >= (0.9 * median).floor() as i64, "{f:?} median {median}");
        assert!(f.amount >= p.amount);
    }

    #[test]
    fn endgame_allin_bids_the_balance_on_valuable_letters() {
        let p = Params { endgame_allin_auctions: 6, endgame_allin_min_value: 3.0, ..Params::default() };
        let e = index('E').unwrap();
        // Four usable auctions remain and E completes CAZIQUE: bid the full balance.
        let mut t = tracker_with("CAZIQU", 4);
        t.on_participant(1, 37, 0);
        let mut b = Brain::new(p.clone(), FieldModel::default(), 8);
        assert_eq!(b.bid(&t, e).amount, 37);
        // Identical rack but 40 auctions left: ordinary pacing keeps the bid far below the balance.
        let mut t2 = tracker_with("CAZIQU", 40);
        t2.on_participant(1, 100, 0);
        let mut b2 = Brain::new(p, FieldModel::default(), 8);
        assert!(b2.bid(&t2, e).amount < 100);
    }

    #[test]
    fn second_price_bids_never_exceed_the_richest_rival_plus_one() {
        let e = index('E').unwrap();
        let mut t = tracker_with("CAZIQU", 4);
        t.on_participant(1, 80, 0);
        for id in 2..=6 {
            t.on_participant(id, 9, 0);
        }
        let mut b = Brain::new(Params { endgame_allin_auctions: 6, ..Params::default() }, FieldModel::default(), 8);
        // An all-in rule would choose 80, yet the richest rival holds 9, so 10 is an equally winning bid.
        assert_eq!(b.bid(&t, e).amount, 10);
    }

    #[test]
    fn completion_premium_raises_the_bid_only_when_a_seven_completes() {
        // Pin λ at 1 so each bid equals its value in coins, keeping the
        // balance cap slack; the match is late, so sampled futures are scarce.
        let base = Params { lambda_min: 1.0, lambda_max: 1.0, ..Params::default() };
        let prem_p = Params { completion_premium: 0.25, completion_min_value: 8.0, ..base.clone() };
        let e = index('E').unwrap();
        // Adding E to CAZIQU yields CAZIQUE for 81 points, so the premium kicks in.
        let t = tracker_with("CAZIQU", 6);
        let mut plain = Brain::new(base.clone(), FieldModel::default(), 5);
        let mut prem = Brain::new(prem_p.clone(), FieldModel::default(), 5);
        let (a, b) = (plain.bid(&t, e).amount, prem.bid(&t, e).amount);
        assert!(a > 0 && b > a, "plain {a} premium {b}");
        // When the rack already contains a seven-letter word, no premium is added.
        let t2 = tracker_with("CAZIQUE", 6);
        let mut plain2 = Brain::new(base, FieldModel::default(), 5);
        let mut prem2 = Brain::new(prem_p, FieldModel::default(), 5);
        assert_eq!(plain2.bid(&t2, e).amount, prem2.bid(&t2, e).amount);
    }

    #[test]
    fn premium_cap_stops_applying_after_its_window() {
        let z = index('Z').unwrap();
        let p = Params { premium_cap_mult: 0.5, premium_cap_until: 0.5, ..Params::default() };
        let mut late = tracker_with("ABLEIT", 20);
        for _ in 0..40 {
            late.on_auction_open(index('E').unwrap(), 20);
        }
        let mut free = Brain::new(Params::default(), FieldModel::default(), 4);
        let mut capped = Brain::new(p, FieldModel::default(), 4);
        assert_eq!(free.bid(&late, z).amount, capped.bid(&late, z).amount);
    }

    #[test]
    fn bids_never_exceed_balance() {
        let mut b = Brain::new(Params::default(), FieldModel::default(), 3);
        let mut t = tracker_with("CAZIQU", 1);
        t.on_participant(1, 7, 0);
        assert!(b.bid(&t, index('E').unwrap()).amount <= 7);
    }
}
