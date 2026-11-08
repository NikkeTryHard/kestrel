//! `kestrel status`: our identity, our record, and what is currently open.
//! Read-only; falls back to anonymous access when no bot token is present.

use crate::settings::Common;
use anyhow::Result;
use kestrel_net::transport::{connect, subscribe_wait};
use kestrel_net::wire::*;
use spacetimedb_sdk::{Table, Timestamp};
use std::time::Duration;

fn secs_until(ts: Timestamp) -> i64 {
    (ts.to_micros_since_unix_epoch() - Timestamp::now().to_micros_since_unix_epoch()) / 1_000_000
}

fn status_name(s: &TournamentStatus) -> &'static str {
    match s {
        TournamentStatus::Lobby => "Lobby",
        TournamentStatus::Swiss => "Swiss",
        TournamentStatus::Bracket => "Bracket",
        TournamentStatus::Final => "Final",
        TournamentStatus::Ended => "Ended",
        TournamentStatus::Registration => "Registration",
        TournamentStatus::ForceEnded => "ForceEnded",
    }
}

pub fn status(common: &Common) -> Result<()> {
    let token = common.bot_token()?;
    let anonymous = token.is_none();
    let s = connect(&common.host, &common.db, token, Duration::from_secs(20))?;
    let queries: Vec<String> = [
        "bot",
        "bot_credential",
        "bot_stats",
        "lobby",
        "lobby_member",
        "tournament",
        "tournament_entry",
        "match_state",
        "match_participant",
    ]
    .iter()
    .map(|t| format!("SELECT * FROM {t}"))
    .collect();
    subscribe_wait(&s.conn, &queries, Duration::from_secs(30))?;
    let db = &s.conn.db;
    let name = |id: u64| db.bot().id().find(&id).map(|b| b.name).unwrap_or_else(|| format!("#{id}"));

    println!("server     {} / {}", common.host, common.db);
    println!(
        "identity   {}{}",
        s.identity.to_hex(),
        if anonymous { "  (anonymous: no bot token found)" } else { "" }
    );

    let me = db.bot_credential().identity().find(&s.identity).map(|c| c.bot_id);
    match me {
        Some(id) => {
            println!("bot        {} (id {id})", name(id));
            match db.bot_stats().bot_id().find(&id) {
                Some(st) => println!(
                    "stats      rating {} | mu {:.2} sigma {:.2} | {} matches, {} wins | total score {}",
                    st.rating, st.openskill_mu, st.openskill_sigma, st.matches_played, st.wins, st.total_score
                ),
                None => println!("stats      no matches played yet"),
            }
            let running: Vec<u64> = db
                .match_participant()
                .iter()
                .filter(|p| p.bot_id == id)
                .filter(|p| {
                    db.match_state().id().find(&p.match_id).is_some_and(|m| matches!(m.status, MatchStatus::Running))
                })
                .map(|p| p.match_id)
                .collect();
            if running.is_empty() {
                println!("playing    not in a running match");
            } else {
                println!("playing    match {running:?}");
            }
        }
        None if !anonymous => println!("bot        this token is not a credential for any bot"),
        None => {}
    }

    let real_bots = db.bot().iter().filter(|b| !b.is_simulated).count();
    let running = db.match_state().iter().filter(|m| matches!(m.status, MatchStatus::Running)).count();
    println!(
        "server     {} real bots registered, {running} matches running, {} played",
        real_bots,
        db.match_state().count()
    );

    match db.lobby().iter().filter(|l| matches!(l.status, LobbyStatus::Open)).max_by_key(|l| l.id) {
        Some(l) => {
            let mut members: Vec<String> = db
                .lobby_member()
                .iter()
                .filter(|m| m.lobby_id == l.id)
                .map(|m| {
                    let tag = if Some(m.bot_id) == me { " (us)" } else { "" };
                    format!("{}{tag}", name(m.bot_id))
                })
                .collect();
            members.sort();
            println!(
                "lobby      #{} {}/{} closes in {}s: {}",
                l.id,
                members.len(),
                l.max_size,
                secs_until(l.closes_at).max(0),
                if members.is_empty() { "(empty)".to_string() } else { members.join(", ") }
            );
        }
        None => println!("lobby      none open"),
    }

    let mut tours: Vec<Tournament> = db.tournament().iter().collect();
    tours.sort_by_key(|t| t.id);
    if tours.is_empty() {
        println!("tournament none");
    }
    for t in tours {
        let entries: Vec<TournamentEntry> =
            db.tournament_entry().iter().filter(|e| e.tournament_id == t.id).collect();
        let registered = me.is_some_and(|id| entries.iter().any(|e| e.bot_id == id));
        println!(
            "tournament #{} {} | {} entrants | {} {}-bot matches, {} swiss rounds (round {}), top {} | {}{}",
            t.id,
            status_name(&t.status),
            entries.len(),
            match t.auction_type {
                AuctionType::SecondPrice => "SecondPrice",
                AuctionType::FirstPrice => "FirstPrice",
            },
            t.match_size,
            t.swiss_rounds_total,
            t.current_round,
            t.top_cut,
            if me.is_none() {
                "registration n/a (anonymous)"
            } else if registered {
                "WE ARE REGISTERED"
            } else {
                "we are NOT registered"
            },
            t.winner_bot_id.map(|w| format!(" | winner {}", name(w))).unwrap_or_default(),
        );
    }
    Ok(())
}
