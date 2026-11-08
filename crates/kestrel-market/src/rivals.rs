//! Predictions of how the other players at the table will bid.
//!
//! For each letter, the largest bid expected from rivals follows a log-normal
//! distribution centred on that letter's median; the medians were fitted from
//! recorded server matches. Two further adjustments apply: the current stage
//! of the match, and an online "heat" term estimated from the prices this
//! table has actually paid. Because every opponent's balance is visible, the
//! distribution is cut off at the wealth of the richest rival.

use kestrel_words::{Letter, ALPHABET};
use serde::{Deserialize, Serialize};

/// Field-level priors, re-fitted against recorded match data.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FieldModel {
    /// Per-letter median of the top rival bid, assuming a full table of
    /// `ref_players` opponents of ordinary wealth.
    pub median: [f64; ALPHABET],
    /// Spread of the distribution on the log scale.
    pub sigma: f64,
    /// Scale factors spanning the match, uniformly spaced across its auctions.
    pub phase: Vec<f64>,
    /// Chance that every opponent declines to bid.
    pub p_none: f64,
    pub ref_players: usize,
}

impl Default for FieldModel {
    fn default() -> Self {
        // Estimated from 287 matches (Oct 24 2026) in which all six seats were
        // real players. With five rivals, the median of their maximum lies
        // between the top and second-highest observed bids.
        let top: [f64; ALPHABET] = [
            13.0, 19.0, 20.0, 16.0, 14.0, 21.0, 15.0, 24.0, 13.0, 32.0, 27.0, 13.0, 20.0, 13.0, 13.0, 20.0, 33.0,
            14.0, 16.0, 13.0, 13.0, 20.0, 20.0, 35.0, 23.0, 43.0,
        ];
        let mut median = [0.0; ALPHABET];
        for l in 0..ALPHABET {
            median[l] = top[l] * 0.9;
        }
        FieldModel {
            median,
            sigma: 0.42,
            phase: vec![
                1.346, 1.247, 1.126, 1.054, 1.003, 0.962, 0.948, 0.933, 0.913, 0.93, 0.939, 0.962, 0.951, 0.966,
                1.188, 1.114, 0.855,
            ],
            p_none: 0.01,
            ref_players: 6,
        }
    }
}

impl FieldModel {
    pub fn phase_factor(&self, progress: f64) -> f64 {
        if self.phase.is_empty() {
            return 1.0;
        }
        let x = progress.clamp(0.0, 1.0) * (self.phase.len() - 1) as f64;
        let i = x.floor() as usize;
        let j = (i + 1).min(self.phase.len() - 1);
        let t = x - i as f64;
        self.phase[i] * (1.0 - t) + self.phase[j] * t
    }
}

/// Cumulative distribution function of the standard normal, computed with
/// the erf approximation 7.1.26 from Abramowitz–Stegun.
pub fn norm_cdf(z: f64) -> f64 {
    let t = 1.0 / (1.0 + 0.3275911 * z.abs() / std::f64::consts::SQRT_2);
    let y = 1.0
        - (((((1.061405429 * t - 1.453152027) * t) + 1.421413741) * t - 0.284496736) * t + 0.254829592)
            * t
            * (-(z * z) / 2.0).exp();
    if z >= 0.0 {
        0.5 * (1.0 + y)
    } else {
        0.5 * (1.0 - y)
    }
}

/// Distribution of the strongest rival bid in a single auction.
#[derive(Clone, Copy, Debug)]
pub struct BidDist {
    pub median: f64,
    pub sigma: f64,
    /// Upper limit on any bid: the balance held by the wealthiest opponent.
    pub cap: f64,
    pub p_none: f64,
}

impl BidDist {
    /// Probability that the highest rival bid is below `b`, so bidding `b`
    /// wins; ties count as half because they are decided by chance.
    pub fn p_win(&self, b: f64) -> f64 {
        if b <= 0.0 {
            return 0.0;
        }
        if self.cap <= 0.0 {
            return 1.0;
        }
        let below = |x: f64| -> f64 {
            if x >= self.cap {
                return 1.0;
            }
            if x <= 0.0 {
                return 0.0;
            }
            norm_cdf((x / self.median).ln() / self.sigma)
        };
        // Bids are whole numbers, so use P(M < b) + 0.5·P(M == b) for integer M.
        let lt = below(b - 0.5);
        let le = below(b + 0.5);
        let p = lt + 0.5 * (le - lt);
        self.p_none + (1.0 - self.p_none) * p
    }

    /// Expected payout under second-price rules, where the winner hands over
    /// the runner-up's bid: E[M · 1{M < b}].
    pub fn expected_pay_second_price(&self, b: f64) -> f64 {
        if b <= 0.0 {
            return 0.0;
        }
        let top = b.min(self.cap + 1.0).floor() as i64;
        let mut e = 0.0;
        let mut prev = self.p_win_strict(1.0);
        for m in 1..top.max(1) {
            let next = self.p_win_strict(m as f64 + 1.0);
            e += m as f64 * (next - prev);
            prev = next;
        }
        // With no rival bidding, the reserve price of 1 is charged.
        e + self.p_none
    }

    fn p_win_strict(&self, b: f64) -> f64 {
        if b <= 0.0 {
            return 0.0;
        }
        let x = b - 0.5;
        if x >= self.cap {
            return 1.0;
        }
        norm_cdf((x / self.median).ln() / self.sigma)
    }
}

/// Tracks price levels online for the duration of one match.
#[derive(Clone, Debug)]
pub struct Market {
    pub field: FieldModel,
    /// Scaling correction estimated from prices observed at this table.
    pub heat: f64,
    obs: u32,
}

impl Market {
    pub fn new(field: FieldModel) -> Self {
        Market { field, heat: 1.0, obs: 0 }
    }

    /// Adjusts for table size: the maximum shrinks as rivals become fewer.
    fn table_factor(n_opponents: usize, ref_players: usize) -> f64 {
        let n = n_opponents.max(1) as f64;
        let r = (ref_players.max(2) - 1) as f64;
        // The maximum of n log-normal draws scales about as sqrt(2 ln n).
        let g = |k: f64| 1.0 + 0.25 * (2.0 * k.ln()).max(0.0).sqrt();
        g(n) / g(r)
    }

    pub fn dist(&self, letter: Letter, progress: f64, n_opponents: usize, cap: i64) -> BidDist {
        let m = self.field.median[letter]
            * self.field.phase_factor(progress)
            * self.heat
            * Self::table_factor(n_opponents, self.field.ref_players);
        BidDist { median: m.max(0.5), sigma: self.field.sigma, cap: cap.max(0) as f64, p_none: self.field.p_none }
    }

    /// Records an observed price. If another player won, `paid` equals the
    /// second-highest bid in the auction; if we won, it is the largest rival
    /// bid instead.
    pub fn observe(
        &mut self,
        letter: Letter,
        progress: f64,
        paid: i64,
        we_won: bool,
        n_opponents: usize,
        our_bid: i64,
    ) {
        if paid <= 1 {
            return;
        }
        // A loss at exactly our own bid means the winner's bid was at least
        // that high, yet the excess is hidden from us (censored data). Use the
        // observation only when it suggests more heat than currently assumed.
        let censored = !we_won && our_bid > 0 && paid >= our_bid;
        let pred = self.field.median[letter]
            * self.field.phase_factor(progress)
            * Self::table_factor(n_opponents, self.field.ref_players);
        // The second price underestimates the top rival bid, so adjust upward.
        let adj = if we_won { 1.0 } else { 1.0 / 0.8 };
        let mut ratio = (paid as f64 * adj / pred).clamp(0.2, 5.0);
        if censored {
            if ratio <= self.heat {
                return;
            }
            // The real maximum exceeds `paid`; shift slightly beyond it.
            ratio *= 1.15;
        }
        self.obs += 1;
        let w = (1.0 / (self.obs as f64 + 3.0)).max(0.12);
        self.heat = (self.heat.ln() * (1.0 - w) + ratio.ln() * w).exp();
    }

    /// In a first-price auction the winner pays what they bid. Losing tells us
    /// the top rival bid exactly, whereas winning only proves it did not exceed
    /// `paid`. So a win may lower an inflated estimate toward that ceiling, but
    /// it must not raise the estimate in the direction of our own valuation.
    pub fn observe_first_price(
        &mut self,
        letter: Letter,
        progress: f64,
        paid: i64,
        we_won: bool,
        n_opponents: usize,
    ) {
        if paid <= 0 {
            return;
        }
        let pred = self.field.median[letter]
            * self.field.phase_factor(progress)
            * Self::table_factor(n_opponents, self.field.ref_players);
        let ratio = (paid as f64 / pred).clamp(0.2, 5.0);
        if we_won && ratio >= self.heat {
            return;
        }
        self.obs += 1;
        let w = (1.0 / (self.obs as f64 + 3.0)).max(0.12);
        self.heat = (self.heat.ln() * (1.0 - w) + ratio.ln() * w).exp();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win_probability_is_monotone_and_capped() {
        let d = BidDist { median: 12.0, sigma: 0.4, cap: 30.0, p_none: 0.0 };
        let mut prev = 0.0;
        for b in 1..40 {
            let p = d.p_win(b as f64);
            assert!(p >= prev - 1e-12);
            prev = p;
        }
        assert!((d.p_win(31.0) - 1.0).abs() < 1e-9);
        assert!((d.p_win(12.0) - 0.5).abs() < 0.08);
    }

    #[test]
    fn expected_pay_grows_with_bid() {
        let d = BidDist { median: 12.0, sigma: 0.4, cap: 100.0, p_none: 0.0 };
        assert!(d.expected_pay_second_price(20.0) > d.expected_pay_second_price(10.0));
        assert!(d.expected_pay_second_price(20.0) < 20.0);
    }

    #[test]
    fn heat_tracks_expensive_tables() {
        let mut m = Market::new(FieldModel::default());
        for _ in 0..20 {
            m.observe(0, 0.5, 30, true, 5, 0);
        }
        assert!(m.heat > 1.8);
    }

    fn flat_field() -> FieldModel {
        FieldModel { median: [10.0; ALPHABET], phase: vec![1.0], ref_players: 6, ..FieldModel::default() }
    }

    #[test]
    fn first_price_loss_reveals_rival_bid_without_second_price_inflation() {
        let mut first = Market::new(flat_field());
        let mut second = Market::new(flat_field());
        first.observe_first_price(0, 0.5, 10, false, 5);
        second.observe(0, 0.5, 10, false, 5, 8);
        assert_eq!(first.heat, 1.0);
        assert!(second.heat > 1.0);
        // Losing a first-price auction at the reserve price is exact evidence too.
        first.observe_first_price(0, 0.5, 1, false, 5);
        assert!(first.heat < 1.0);
    }

    #[test]
    fn first_price_win_is_only_an_upper_bound_on_rival_bid() {
        let mut market = Market::new(flat_field());
        for _ in 0..20 {
            market.observe_first_price(0, 0.5, 100, true, 5);
        }
        assert_eq!(market.heat, 1.0);
        assert_eq!(market.obs, 0);
        // A win at five proves the rival maximum is no larger than five.
        // Move toward that ceiling without assuming it was reached exactly.
        market.observe_first_price(0, 0.5, 5, true, 5);
        assert!(market.heat < 1.0 && market.heat > 0.5);
        assert_eq!(market.obs, 1);
    }
}
