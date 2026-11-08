//! All tunable settings in one serialisable structure. The tuner can evolve
//! them, and a running bot can reload them between matches.

use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Params {
    // ---- value estimation ----
    /// Simulated futures drawn for every letter valuation.
    pub samples: usize,
    /// Simulated futures used for the low-cost all-letters table that drives
    /// pacing.
    pub table_samples: usize,
    /// How much the fixed letter prior is mixed into the pacing table; 0
    /// leaves it entirely Monte Carlo.
    pub table_shrink: f64,
    /// Ceiling on work per valuation, measured in letter² units of roughly
    /// 1.7 µs, which limits latency for very large racks. The main valuation
    /// and the pacing table are each allowed this amount.
    pub work_budget: u64,
    /// Racks no larger than this are evaluated exactly rather than greedily;
    /// 0 disables the exact path.
    pub exact_terminal_max: usize,
    /// Search-node allowance for each terminal solve in the simulation; 0
    /// means use greedy.
    pub solve_nodes: u64,
    /// How many alternative opening words are attempted ahead of a greedy
    /// terminal dump; 0 disables. At most four, keeping each sample bounded,
    /// and it applies only when `solve_nodes` is 0.
    pub terminal_rollout_candidates: usize,
    /// When an added tile is dropped, keep the baseline partition in every
    /// sampled future, so greedy regressions cannot cancel out the gains.
    pub monotone_marginals: bool,
    /// Multiplier on the number of future letters we anticipate winning.
    pub win_share_scale: f64,
    /// Additional letters drawn per anticipated win, approximating the freedom
    /// we have in choosing which letters to acquire.
    pub choice_mult: f64,
    /// Premium tiles (J, K, Q, X, Z) are scored against a future pool enlarged
    /// by this factor. Such a tile needs particular partners, and a short
    /// sampled future seldom holds them, so its marginal value drops sharply
    /// after the midpoint: the server's leading bots won 42% of late premium
    /// auctions against our 3%. Set to 1.0 to disable.
    pub premium_choice_mult: f64,
    /// Upper limit on the number of future letters considered.
    pub max_future: usize,

    // ---- pacing and bidding ----
    /// Share of future word income treated as spendable budget, computed as
    /// the current balance times the fraction of the match still to play.
    pub income_factor: f64,
    /// Lower bound on λ (points per coin), our most aggressive pricing.
    pub lambda_min: f64,
    pub lambda_max: f64,
    /// Share of the budget for the remaining auctions that we intend to spend;
    /// values below 1 preserve a reserve, values above 1 spend early.
    pub spend_target: f64,
    /// While pacing, value each later stretch of the match using its own phase
    /// (pricey opening, cheaper close) rather than applying current prices
    /// throughout.
    pub phase_aware_pacing: bool,
    /// In the first quarter, reduce the bid below value by this fraction; 0
    /// disables.
    pub opening_shade: f64,
    /// Letters whose marginal value is under this many points are ignored.
    pub min_value: f64,
    /// Guarantee a 1-coin bid on letters worth at least this many points,
    /// matching the reserve price.
    pub cheap_grab_value: f64,
    /// Under first-price rules, search a grid of bids for maximum surplus.
    pub first_price_shade: bool,
    /// Never bid below this fraction of the market median for ordinary letters
    /// valued at `volume_min_value` points or more; 0 disables.
    pub volume_floor: f64,
    pub volume_min_value: f64,
    /// During the final N usable auctions, commit the entire balance to any
    /// letter worth at least `endgame_allin_min_value` points; 0 disables.
    pub endgame_allin_auctions: u32,
    pub endgame_allin_min_value: f64,
    /// Improve the bid with paired rank rollouts once at most this many
    /// playable auctions remain. 0 disables; the planner never exceeds three.
    pub endgame_rollout_auctions: u32,
    /// Cap on paired samples of hidden states for the endgame planner.
    pub endgame_rollout_samples: usize,
    /// Time reserved inside the bid deadline for endgame refinement, in
    /// milliseconds. The planner also has its own 30 ms ceiling.
    pub endgame_rollout_budget_ms: u64,
    /// Limits J/K/Q/X/Z bids to this multiple of the market median; 0 disables.
    pub premium_cap_mult: f64,
    /// Restrict the premium cap to match progress below this value; 1.0
    /// applies it throughout.
    pub premium_cap_until: f64,
    /// Lower bound on premium bids, given by
    /// `base + mult * face + round * auctions_seen`. Fitted to the winning bids
    /// of the server's top players, who paid roughly 2.3-3.0x face value plus a
    /// per-round term; a Monte Carlo estimate can otherwise collapse when the
    /// premium's partners are missing from the sampled future. 0 disables.
    pub premium_floor_mult: f64,
    pub premium_floor_base: f64,
    pub premium_floor_round: f64,
    /// Raise the bid above value by this fraction when the letter immediately
    /// completes a word of 7+ letters and is worth at least
    /// `completion_min_value` points; 0 disables.
    pub completion_premium: f64,
    pub completion_min_value: f64,
    /// Time limit for a bid's Monte Carlo sampling, in milliseconds (0 = no
    /// limit). It is off by default so tests and the simulator stay
    /// deterministic; the deployed configuration enables it because a bid that
    /// arrives late has no value.
    pub bid_deadline_ms: u64,
    /// Importance assigned to denying the leading rival in relative-score
    /// games.
    pub deny_weight: f64,
    /// Denial is active only when the table has no more than this many players.
    pub deny_max_players: usize,

    // ---- word play ----
    /// Minimum word length for mid-match plays; the final dump is unrestricted.
    pub min_play_len: usize,
    /// Mid-match words that pay less than this are saved for a longer, more
    /// valuable one, unless we are starved or inside the late window. The
    /// server's best players (james, Waupus, Lambi Lambiii) make 84-86% of
    /// their mid-match plays at 45+ points or while broke; their first play
    /// comes around auction 36-40, and they score 32-36 points in the final
    /// quarter, whereas we cash in near 40 points around auction 20 for 12.7.
    /// 0 disables.
    pub cash_min_reward: u32,
    /// "Lala" completion floor: if a letter would raise the value of our best
    /// word of at least `completion_floor_len` letters by `completion_floor_gain`
    /// points or more, bid no less than `completion_floor_mult` times the market
    /// median for that letter. 0 disables.
    pub completion_floor_mult: f64,
    pub completion_floor_gain: f64,
    pub completion_floor_len: usize,
    /// "Lambi Lambiii" drain floor, used only heads-up: every letter with any
    /// value is bid at no less than `drain_floor` times the market median, which
    /// pushes the rival's winning prices upward. 0 disables.
    pub drain_floor: f64,
    /// Worth of playing immediately: per-point liquidity credit multiplied by
    /// λ.
    pub liquidity: f64,
    /// Use μ — the marginal points per coin at the balance limit — to price
    /// liquidity instead of the pacing λ. Disabled by default because it is
    /// neutral to slightly worse in simulation, though the tuner may enable it.
    pub mu_liquidity: bool,
    /// Once the bag holds at most this many tiles, play words as short as
    /// `late_min_play_len` and price coins with μ. 0 disables.
    pub late_window: u32,
    pub late_min_play_len: usize,
    /// With a balance under this amount and at least three usable auctions
    /// remaining, we are "starved": play words as short as
    /// `starve_min_play_len` and price coins with μ.
    pub starve_coins: i64,
    pub starve_min_play_len: usize,
    /// A mid-match word must exceed the value of holding by this many points.
    pub play_margin: f64,
    /// Number of leading words examined for every play decision.
    pub play_candidates: usize,
    /// Time allowed for the final exact partition, in milliseconds.
    pub final_solve_ms: u64,
}

impl Default for Params {
    fn default() -> Self {
        Params {
            samples: 48,
            table_samples: 10,
            table_shrink: 0.5,
            work_budget: 70_000,
            exact_terminal_max: 0,
            solve_nodes: 0,
            terminal_rollout_candidates: 0,
            monotone_marginals: false,
            win_share_scale: 1.0,
            choice_mult: 1.6,
            premium_choice_mult: 1.0,
            max_future: 24,
            income_factor: 0.8,
            lambda_min: 0.05,
            lambda_max: 4.0,
            spend_target: 1.0,
            phase_aware_pacing: false,
            opening_shade: 0.0,
            min_value: 0.3,
            cheap_grab_value: 0.8,
            first_price_shade: true,
            volume_floor: 0.0,
            volume_min_value: 2.0,
            endgame_allin_auctions: 0,
            endgame_allin_min_value: 3.0,
            endgame_rollout_auctions: 0,
            endgame_rollout_samples: 12,
            endgame_rollout_budget_ms: 30,
            premium_cap_mult: 0.0,
            premium_cap_until: 1.0,
            premium_floor_mult: 0.0,
            premium_floor_base: 0.0,
            premium_floor_round: 0.0,
            completion_premium: 0.0,
            completion_min_value: 8.0,
            bid_deadline_ms: 0,
            deny_weight: 0.5,
            deny_max_players: 2,
            min_play_len: 6,
            cash_min_reward: 0,
            completion_floor_mult: 0.0,
            completion_floor_gain: 12.0,
            completion_floor_len: 6,
            drain_floor: 0.0,
            liquidity: 0.6,
            mu_liquidity: false,
            late_window: 0,
            starve_coins: 15,
            starve_min_play_len: 5,
            late_min_play_len: 4,
            play_margin: 0.5,
            play_candidates: 10,
            final_solve_ms: 180,
        }
    }
}

impl Params {
    pub fn load(path: &Path) -> anyhow_lite::Result<Params> {
        let text = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    pub fn save(&self, path: &Path) -> anyhow_lite::Result<()> {
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(self)? + "\n")?;
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

/// Small boxed-error result type, keeping the brain's dependency list short.
pub mod anyhow_lite {
    pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
}
