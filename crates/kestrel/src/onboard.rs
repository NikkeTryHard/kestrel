//! `kestrel setup --team T --bot B`: onboarding without a browser.
//!
//! 1. Connect with the human's identity (the spacetime.com one from `spacetime login`).
//! 2. Create the team and its bot, unless the human already owns a pair.
//! 3. Mint a single-use credential nonce, then read its code from `my_nonces`.
//! 4. Open a second, anonymous connection, redeem the nonce through
//!    `claim_credential`, verify the `bot_credential` row exists, and store
//!    that connection's token (mode 0600) as the bot token.

use crate::feed::extract_jwt;
use crate::settings::{save_token, Common};
use anyhow::{anyhow, bail, Context, Result};
use kestrel_net::transport::{call_wait, connect, subscribe_wait, wait_for};
use kestrel_net::wire::*;
use spacetimedb_sdk::Table;
use std::collections::HashSet;
use std::time::Duration;

const T: Duration = Duration::from_secs(20);

/// The human's spacetime.com token, from `SPACETIME_HUMAN_TOKEN` when set,
/// or otherwise from the output of `spacetime login show --token`.
pub fn human_token() -> Result<String> {
    if let Ok(t) = std::env::var("SPACETIME_HUMAN_TOKEN") {
        let t = t.trim().to_string();
        if !t.is_empty() {
            return Ok(t);
        }
    }
    let out = std::process::Command::new("spacetime")
        .args(["login", "show", "--token"])
        .output()
        .context("running `spacetime login show --token` (is the spacetime CLI installed?)")?;
    let text = format!("{}\n{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    extract_jwt(&text).ok_or_else(|| {
        anyhow!("no token in `spacetime login show --token` output; run `spacetime login` first or set SPACETIME_HUMAN_TOKEN")
    })
}

pub fn setup(common: &Common, team: &str, bot: &str, force: bool) -> Result<()> {
    let token_path = common.token_path();
    if token_path.exists() && !force {
        bail!("{} already exists; pass --force to mint a new credential and overwrite it", token_path.display());
    }

    // ---- phase one: acting as the human
    let human = connect(&common.host, &common.db, Some(human_token()?), T).context("connecting as the human")?;
    println!("human identity {}", human.identity.to_hex());
    subscribe_wait(
        &human.conn,
        &[
            "SELECT * FROM my_team".to_string(),
            "SELECT * FROM my_nonces".to_string(),
            "SELECT * FROM bot".to_string(),
        ],
        T,
    )?;
    let db = &human.conn.db;
    let summary = match db.my_team().iter().next() {
        Some(s) => {
            println!("already on team '{}' with bot '{}' (id {})", s.team_name, s.bot_name, s.bot_id);
            if s.bot_name != bot || s.team_name != team {
                println!("note: using the existing team/bot, not '{team}'/'{bot}'");
            }
            s
        }
        None => {
            println!("creating team '{team}' with bot '{bot}'");
            let (t, b) = (team.to_string(), bot.to_string());
            call_wait(|cb| human.conn.reducers.create_team_then(t, b, cb), T)?
                .map_err(|e| anyhow!("create_team refused: {e}"))?;
            wait_for(T, || db.my_team().iter().next())
                .ok_or_else(|| anyhow!("team created but my_team stayed empty"))?
        }
    };
    let before: HashSet<u64> = db.my_nonces().iter().map(|n| n.id).collect();
    call_wait(|cb| human.conn.reducers.mint_credential_nonce_then(cb), T)?
        .map_err(|e| anyhow!("mint_credential_nonce refused: {e}"))?;
    let nonce = wait_for(T, || {
        db.my_nonces().iter().filter(|n| !before.contains(&n.id) && n.bot_id == summary.bot_id).max_by_key(|n| n.id)
    })
    .ok_or_else(|| anyhow!("nonce minted but never appeared in my_nonces"))?;
    println!("minted credential nonce for bot {}", nonce.bot_id);
    drop(human);

    // ---- phase two: the fresh bot identity
    let fresh = connect(&common.host, &common.db, None, T).context("opening a fresh anonymous connection")?;
    subscribe_wait(&fresh.conn, &["SELECT * FROM bot_credential".to_string(), "SELECT * FROM bot".to_string()], T)?;
    let code = nonce.code.clone();
    call_wait(|cb| fresh.conn.reducers.claim_credential_then(code, cb), T)?
        .map_err(|e| anyhow!("claim_credential refused: {e}"))?;
    let id = fresh.identity;
    let cred = wait_for(T, || fresh.conn.db.bot_credential().identity().find(&id))
        .ok_or_else(|| anyhow!("credential claimed but no bot_credential row for {}", id.to_hex()))?;
    save_token(&token_path, &fresh.token)?;
    let name =
        fresh.conn.db.bot().id().find(&cred.bot_id).map(|b| b.name).unwrap_or_else(|| summary.bot_name.clone());
    println!("bot '{name}' (id {}) credential {} saved to {}", cred.bot_id, id.to_hex(), token_path.display());
    println!("next: `kestrel status`, then `kestrel run` (or ops/install-launchd.sh)");
    Ok(())
}
