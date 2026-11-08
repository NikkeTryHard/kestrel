//! `kestrel`: client that plays ScrabbleBot matches live.
//!
//! - `kestrel run`      daemon on autopilot (tournaments, lobby, bidding, words, telemetry)
//! - `kestrel run --spectate <bot_id|all>`  observe live matches while never calling reducers
//! - `kestrel setup`    onboarding with no browser: team, bot, credential, token file
//! - `kestrel status`   our identity, rating, lobby, tournaments
//! - `kestrel replay`   the bids our brain would have placed in a past match
//! - `kestrel ab-report` the A/B scoreboard as it stands (champion vs challenger)

mod daemon;
mod feed;
mod onboard;
mod probe;
mod records;
mod replay;
mod scoreboard;
mod settings;

use anyhow::Result;
use clap::{Parser, Subcommand};
use daemon::{Engine, Mode, RunOpts, Shadow};
use settings::Common;
use std::time::Duration;

#[derive(Parser, Debug)]
#[command(name = "kestrel", version, about = "Kestrel: an auction-savvy ScrabbleBot for SpacetimeDB")]
struct Cli {
    #[command(flatten)]
    common: Common,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Start the autopilot: join lobbies and tournaments, bid, play words, write telemetry.
    Run {
        /// Skip joining the casual lobby.
        #[arg(long)]
        no_lobby: bool,
        /// Skip automatic registration for tournaments.
        #[arg(long)]
        no_tournaments: bool,
        /// Watch one bot (or all running matches) anonymously: compute our
        /// decisions yet never call a reducer. Telemetry lands in runs/spectate/.
        #[arg(long, value_name = "BOT_ID|all")]
        spectate: Option<Shadow>,
        /// Halt after this many seconds (by default it runs indefinitely).
        #[arg(long)]
        duration: Option<u64>,
        /// Fail with a non-zero exit once this many reconnects in a row have failed.
        #[arg(long, default_value_t = 20)]
        max_reconnect_failures: u32,
        /// Warn if one decision takes longer than this many milliseconds.
        #[arg(long, default_value_t = 400.0)]
        slow_ms: f64,
        /// Evaluate with a quarter of the Monte Carlo samples whenever, at
        /// decision time, less than this many ms remain until the auction closes.
        #[arg(long, default_value_t = 550)]
        fast_below_ms: i64,
    },
    /// Ensure a team and bot exist, mint and claim a credential, then store the bot token.
    Setup {
        #[arg(long)]
        team: String,
        #[arg(long)]
        bot: String,
        /// Replace a token file that already exists.
        #[arg(long)]
        force: bool,
    },
    /// Print our bot, rating, the open lobby and tournaments (anonymous access works).
    Status,
    /// The A/B scoreboard as it stands: champion (params.json) against ab.json's challenger.
    AbReport,
    /// Reconstruct a match from public history, printing the bids our brain would submit.
    Replay {
        match_id: u64,
        /// Which bot to act as (default: the match winner).
        #[arg(long = "as")]
        as_bot: Option<u64>,
    },
}

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with_target(false)
        .init();
    let cli = Cli::parse();
    if let Err(e) = dispatch(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn dispatch(cli: Cli) -> Result<()> {
    match cli.cmd {
        Cmd::Run { no_lobby, no_tournaments, spectate, duration, max_reconnect_failures, slow_ms, fast_below_ms } => {
            let mode = spectate.map(Mode::Spectate).unwrap_or(Mode::Live);
            let stats = Engine::new(RunOpts {
                common: cli.common,
                mode,
                no_lobby,
                no_tournaments,
                duration: duration.map(Duration::from_secs),
                max_reconnect_failures,
                slow_ms,
                fast_below_ms,
            })
            .run()?;
            println!("{}", stats.report());
            Ok(())
        }
        Cmd::Setup { team, bot, force } => onboard::setup(&cli.common, &team, &bot, force),
        Cmd::Status => probe::status(&cli.common),
        Cmd::AbReport => {
            scoreboard::report(&cli.common.config_dir, &cli.common.runs_dir);
            Ok(())
        }
        Cmd::Replay { match_id, as_bot } => replay::replay(&cli.common, match_id, as_bot),
    }
}
