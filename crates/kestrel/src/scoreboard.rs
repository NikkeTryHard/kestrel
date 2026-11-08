//! A/B testing in production: the league itself settles which strategy wins.
//!
//! Once `config/ab.json` exists, casual matches split two ways: the
//! champion (`params.json`, arm `A`) or a challenger (arm `B:<file>`).
//! Each match triggers a fresh scoreboard over `runs/summary.jsonl`, scored
//! in Swiss points (`n − place`, the same measure tournaments reward). Once
//! both arms hold enough matches, a decisive result gets applied:
//!
//! * challenger clearly ahead → it replaces `params.json` (the outgoing
//!   champion stays behind as `params.prev.json`) and the test closes;
//! * challenger clearly behind → the test closes and the champion stays.
//!
//! Every verdict is appended to `runs/ab-history.jsonl`.

use crate::settings::AbConfig;
use serde::Deserialize;
use std::io::Write;
use std::path::Path;

/// Fewest matches per arm required before a decision.
pub const MIN_MATCHES: usize = 40;
/// |t| magnitude that permits action.
pub const T_DECIDE: f64 = 2.0;
/// Early sequential stops: after `EARLY_N` matches per arm, a challenger
/// that trails at t ≤ −`T_EARLY_REJECT` is retired; one that dominates at
/// `EARLY_PROMOTE_N` with t ≥ `T_EARLY_PROMOTE` is promoted. The stricter
/// thresholds hold the false-decision rate down despite repeated looks.
pub const EARLY_N: usize = 15;
pub const T_EARLY_REJECT: f64 = 2.5;
pub const EARLY_PROMOTE_N: usize = 25;
pub const T_EARLY_PROMOTE: f64 = 3.0;

#[derive(Deserialize)]
struct Line {
    #[serde(default)]
    ts: i64,
    #[serde(default)]
    arm: Option<String>,
    place: usize,
    n_players: usize,
    our_score: i64,
    #[serde(default)]
    tournament_id: Option<u64>,
    #[serde(default)]
    resumed: bool,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArmStats {
    pub n: usize,
    pub mean_points: f64,
    pub var_points: f64,
    pub mean_place: f64,
    pub win_rate: f64,
    pub mean_score: f64,
}

fn stats(xs: &[(f64, f64, f64, f64)]) -> ArmStats {
    let n = xs.len();
    if n == 0 {
        return ArmStats::default();
    }
    let nf = n as f64;
    let mean = xs.iter().map(|x| x.0).sum::<f64>() / nf;
    let var = xs.iter().map(|x| (x.0 - mean).powi(2)).sum::<f64>() / (nf - 1.0).max(1.0);
    ArmStats {
        n,
        mean_points: mean,
        var_points: var,
        mean_place: xs.iter().map(|x| x.1).sum::<f64>() / nf,
        win_rate: xs.iter().map(|x| x.2).sum::<f64>() / nf,
        mean_score: xs.iter().map(|x| x.3).sum::<f64>() / nf,
    }
}

/// Welch t value of B minus A.
pub fn welch_t(a: &ArmStats, b: &ArmStats) -> f64 {
    let se = (a.var_points / a.n.max(1) as f64 + b.var_points / b.n.max(1) as f64).sqrt();
    if se <= 0.0 {
        0.0
    } else {
        (b.mean_points - a.mean_points) / se
    }
}

/// Champion and challenger scoreboards read from the summary log, counting
/// only casual, full-table matches that were not resumed.
pub fn scoreboard(summary: &Path, ab: &AbConfig) -> (ArmStats, ArmStats) {
    let text = std::fs::read_to_string(summary).unwrap_or_default();
    let label_b = format!("B:{}", ab.challenger);
    let mut a = Vec::new();
    let mut b = Vec::new();
    let mut seen_b = false;
    for line in text.lines() {
        let Ok(l) = serde_json::from_str::<Line>(line) else {
            continue;
        };
        if l.tournament_id.is_some() || l.resumed || l.n_players < 3 {
            continue;
        }
        // Restrict to the current test's window.
        if ab.started_ms.is_some_and(|start| l.ts < start) {
            continue;
        }
        let arm = l.arm.unwrap_or_default();
        let row = ((l.n_players - l.place) as f64, l.place as f64, (l.place == 1) as u8 as f64, l.our_score as f64);
        // A test's window begins at the challenger's first match; champion
        // matches earlier than that were played under different conditions.
        if arm == label_b {
            seen_b = true;
            b.push(row);
        } else if arm == "A" && seen_b {
            a.push(row);
        }
    }
    (stats(&a), stats(&b))
}

fn log(runs: &Path, v: serde_json::Value) {
    let _ = std::fs::create_dir_all(runs);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(runs.join("ab-history.jsonl")) {
        let _ = writeln!(f, "{v}");
    }
}

/// Evaluate the active test and apply any clear outcome. Yields a message
/// when a decision was made.
pub fn maybe_decide(config_dir: &Path, runs_dir: &Path, ab: &AbConfig) -> Option<String> {
    let (a, b) = scoreboard(&runs_dir.join("summary.jsonl"), ab);
    let n = a.n.min(b.n);
    if n < EARLY_N {
        return None;
    }
    let t = welch_t(&a, &b);
    let full = n >= MIN_MATCHES;
    let promote = (full && t >= T_DECIDE) || (n >= EARLY_PROMOTE_N && t >= T_EARLY_PROMOTE);
    let reject = (full && t <= -T_DECIDE) || t <= -T_EARLY_REJECT;
    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let record = |verdict: &str| {
        serde_json::json!({
            "ts_ms": ts as u64, "verdict": verdict, "challenger": ab.challenger, "name": ab.name, "t": t,
            "a": {"n": a.n, "points": a.mean_points, "place": a.mean_place, "win": a.win_rate, "score": a.mean_score},
            "b": {"n": b.n, "points": b.mean_points, "place": b.mean_place, "win": b.win_rate, "score": b.mean_score},
        })
    };
    if promote {
        let champ = config_dir.join("params.json");
        let _ = std::fs::copy(&champ, config_dir.join("params.prev.json"));
        if std::fs::copy(config_dir.join(&ab.challenger), &champ).is_ok() {
            let _ = std::fs::remove_file(config_dir.join("ab.json"));
            log(runs_dir, record("promoted"));
            return Some(format!(
                "A/B: challenger {} promoted (t {t:+.2}; {:.2} vs {:.2} Swiss pts over {}/{} matches)",
                ab.challenger, b.mean_points, a.mean_points, b.n, a.n
            ));
        }
    } else if reject {
        let _ = std::fs::remove_file(config_dir.join("ab.json"));
        log(runs_dir, record("rejected"));
        return Some(format!("A/B: challenger {} rejected (t {t:+.2})", ab.challenger));
    }
    None
}

/// Launch the next queued experiment when none is active. `ab-queue.json`
/// holds a list of `{"name": "...", "patch": {param: value, ...}}`; each
/// patch is layered onto the reigning champion, so every challenger differs
/// from the current best by a single change.
pub fn advance_queue(config_dir: &Path) -> Option<String> {
    if config_dir.join("ab.json").exists() {
        return None;
    }
    let qpath = config_dir.join("ab-queue.json");
    let mut queue: Vec<serde_json::Value> = serde_json::from_str(&std::fs::read_to_string(&qpath).ok()?).ok()?;
    if queue.is_empty() {
        return None;
    }
    let next = queue.remove(0);
    let name = next.get("name").and_then(|n| n.as_str()).unwrap_or("queued").to_string();
    let champ: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(config_dir.join("params.json")).ok()?).ok()?;
    let mut challenger = champ;
    if let (Some(obj), Some(patch)) = (challenger.as_object_mut(), next.get("patch").and_then(|p| p.as_object())) {
        for (k, v) in patch {
            obj.insert(k.clone(), v.clone());
        }
    }
    // Make sure it parses before writing anything.
    let text = serde_json::to_string_pretty(&challenger).ok()? + "\n";
    serde_json::from_str::<kestrel_market::Params>(&text).ok()?;
    std::fs::write(config_dir.join("params_b.json"), text).ok()?;
    let now_ms =
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0);
    let ab = serde_json::json!({"challenger": "params_b.json", "share": next.get("share").and_then(|v| v.as_f64()).unwrap_or(0.5), "name": name, "started_ms": now_ms});
    std::fs::write(config_dir.join("ab.json"), serde_json::to_string_pretty(&ab).ok()? + "\n").ok()?;
    std::fs::write(&qpath, serde_json::to_string_pretty(&queue).ok()? + "\n").ok()?;
    Some(format!("A/B: started '{name}' ({} more queued)", queue.len()))
}

/// The `kestrel ab-report` command.
pub fn report(config_dir: &Path, runs_dir: &Path) {
    let Some(ab) = crate::settings::load_ab(config_dir) else {
        println!("no A/B test running (config/ab.json absent)");
        if let Ok(h) = std::fs::read_to_string(runs_dir.join("ab-history.jsonl")) {
            println!("history:\n{h}");
        }
        return;
    };
    let (a, b) = scoreboard(&runs_dir.join("summary.jsonl"), &ab);
    println!(
        "A/B test '{}': champion params.json vs challenger {} ({}% of casual matches)",
        ab.name,
        ab.challenger,
        (ab.share * 100.0).round()
    );
    for (name, s) in [("A champion", &a), ("B challenger", &b)] {
        println!(
            "  {name:<13} n {:>4}  swiss pts {:.2}  place {:.2}  win {:>3.0}%  score {:.1}",
            s.n,
            s.mean_points,
            s.mean_place,
            s.win_rate * 100.0,
            s.mean_score
        );
    }
    println!(
        "  t (B − A) = {:+.2}; decides at |t| ≥ {T_DECIDE} with ≥ {MIN_MATCHES} matches per arm",
        welch_t(&a, &b)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, lines: &[(&str, usize)]) {
        let body: String = lines
            .iter()
            .map(|(arm, place)| {
                format!(
                    "{{\"arm\":\"{arm}\",\"place\":{place},\"n_players\":6,\"our_score\":80,\"resumed\":false}}\n"
                )
            })
            .collect();
        std::fs::write(dir.join("summary.jsonl"), body).unwrap();
    }

    #[test]
    fn queue_starts_the_next_experiment_on_the_new_champion() {
        let dir = std::env::temp_dir().join(format!("kestrel-abq-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("params.json"), "{\"min_play_len\": 6}").unwrap();
        std::fs::write(
            dir.join("ab-queue.json"),
            "[{\"name\": \"cap\", \"patch\": {\"premium_cap_mult\": 1.0}}, {\"name\": \"two\", \"patch\": {}}]",
        )
        .unwrap();
        let msg = advance_queue(&dir).unwrap();
        assert!(msg.contains("'cap'") && msg.contains("1 more"), "{msg}");
        let b: kestrel_market::Params =
            serde_json::from_str(&std::fs::read_to_string(dir.join("params_b.json")).unwrap()).unwrap();
        assert_eq!((b.min_play_len, b.premium_cap_mult), (6, 1.0));
        // With a test already running, nothing new gets started.
        assert!(advance_queue(&dir).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn window_ignores_matches_from_earlier_tests() {
        let dir = std::env::temp_dir().join(format!("kestrel-abw-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mut body = String::new();
        for (ts, arm) in [(100, "B:params_b.json"), (200, "A"), (1_000, "B:params_b.json"), (1_100, "A")] {
            body.push_str(&format!(
                "{{\"ts\":{ts},\"arm\":\"{arm}\",\"place\":2,\"n_players\":6,\"our_score\":80,\"resumed\":false}}\n"
            ));
        }
        std::fs::write(dir.join("summary.jsonl"), body).unwrap();
        let ab = AbConfig { challenger: "params_b.json".into(), share: 0.5, name: "t".into(), started_ms: Some(900) };
        let (a, b) = scoreboard(&dir.join("summary.jsonl"), &ab);
        assert_eq!((a.n, b.n), (1, 1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn retires_a_clearly_losing_challenger_early() {
        let dir = std::env::temp_dir().join(format!("kestrel-abe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("params.json"), "{}").unwrap();
        std::fs::write(dir.join("b.json"), "{}").unwrap();
        std::fs::write(dir.join("ab.json"), "{\"challenger\": \"b.json\", \"started_ms\": 0}").unwrap();
        let ab = crate::settings::load_ab(&dir).unwrap();
        let mut rows = Vec::new();
        for i in 0..16 {
            rows.push(("A", 1 + i % 2));
            rows.push(("B:b.json", 5 + i % 2));
        }
        write(&dir, &rows);
        let msg = maybe_decide(&dir, &dir, &ab).unwrap();
        assert!(msg.contains("rejected"), "{msg}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn promotes_a_clearly_better_challenger() {
        let dir = std::env::temp_dir().join(format!("kestrel-ab-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("params.json"), "{\"min_play_len\": 7}").unwrap();
        std::fs::write(dir.join("b.json"), "{\"min_play_len\": 5}").unwrap();
        std::fs::write(dir.join("ab.json"), "{\"challenger\": \"b.json\", \"started_ms\": 0}").unwrap();
        let ab = crate::settings::load_ab(&dir).unwrap();
        // B finishes 1st or 2nd; A finishes 4th or 5th.
        let mut rows = Vec::new();
        for i in 0..50 {
            rows.push(("A", 4 + i % 2));
            rows.push(("B:b.json", 1 + i % 2));
        }
        write(&dir, &rows[..20]);
        assert!(maybe_decide(&dir, &dir, &ab).is_none(), "too few matches to decide");
        write(&dir, &rows);
        let msg = maybe_decide(&dir, &dir, &ab).unwrap();
        assert!(msg.contains("promoted"), "{msg}");
        assert!(std::fs::read_to_string(dir.join("params.json")).unwrap().contains('5'));
        assert!(!dir.join("ab.json").exists());
        assert!(dir.join("params.prev.json").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
