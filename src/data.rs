//! Data layer.
//!
//! Two sources, by cadence:
//!   - Team strength (Elo + xG) is slow-moving: bundled from `data/teams.json`,
//!     produced daily by the Python prep step (soccerdata -> FBref + Elo).
//!   - Match state (fixtures, injuries, lineups) is live: API-FOOTBALL at runtime,
//!     gated on the API_FOOTBALL_KEY secret (host vault when deployed, env var
//!     locally). Primed once per tool call via `prime_secret`.
//!   - Market prices: Polymarket Gamma API, public, no key.

use crate::sim::TeamStrength;
use aomi_sdk::{resolve_secret_value, DynToolCallCtx};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::LazyLock;

const TEAMS_JSON: &str = include_str!("../data/teams.json");
const GAMMA_BASE: &str = "https://gamma-api.polymarket.com";
const APIFOOTBALL_BASE: &str = "https://v3.football.api-sports.io";
/// World Cup league id and season in API-FOOTBALL.
const WC_LEAGUE_ID: u32 = 1;
const WC_SEASON: u32 = 2026;

#[derive(Clone, Debug, Deserialize)]
struct TeamRow {
    name: String,
    elo: f64,
    xg_for: f64,
    xg_against: f64,
    #[serde(default)]
    aliases: Vec<String>,
}

static TEAMS: LazyLock<HashMap<String, TeamRow>> = LazyLock::new(|| {
    let rows: Vec<TeamRow> = serde_json::from_str(TEAMS_JSON).unwrap_or_default();
    let mut map = HashMap::new();
    for row in rows {
        map.insert(norm(&row.name), row.clone());
        for a in &row.aliases {
            map.insert(norm(a), row.clone());
        }
    }
    map
});

fn norm(s: &str) -> String {
    s.trim().to_lowercase()
}

/// Look up a team's bundled strength by name or alias.
pub fn team_strength(name: &str) -> Result<TeamStrength, String> {
    TEAMS
        .get(&norm(name))
        .map(|r| TeamStrength {
            name: r.name.clone(),
            elo: r.elo,
            xg_for: r.xg_for,
            xg_against: r.xg_against,
        })
        .ok_or_else(|| format!("[goal-digger] unknown team '{name}'. Check data/teams.json coverage."))
}

/// Every team we have strength data for (for tournament sims and listing).
#[allow(dead_code)]
pub fn all_team_names() -> Vec<String> {
    let mut names: Vec<String> = TEAMS.values().map(|r| r.name.clone()).collect();
    names.sort();
    names.dedup();
    names
}

// ─── API-FOOTBALL (live, key-gated) ──────────────────────────────────────────

/// API-FOOTBALL key, resolved from the host secret vault (deployed) or the env
/// var (local dev) and cached for the lifetime of the process.
static API_KEY: LazyLock<std::sync::Mutex<Option<String>>> =
    LazyLock::new(|| std::sync::Mutex::new(None));

/// Prime the API-FOOTBALL key from the per-app secret vault at tool-call time.
/// Call at the top of any tool that reads live data. A no-op when the secret is
/// absent (sims still run on bundled strength). The SDK resolver falls back to
/// the `API_FOOTBALL_KEY` env var for local runs where no vault is in scope.
pub fn prime_secret(ctx: &DynToolCallCtx) {
    if let Ok(k) = resolve_secret_value(ctx, None, "API_FOOTBALL_KEY", "missing") {
        *API_KEY.lock().unwrap() = Some(k);
    }
}

fn api_football_key() -> Result<String, String> {
    if let Some(k) = API_KEY.lock().unwrap().clone() {
        return Ok(k);
    }
    std::env::var("API_FOOTBALL_KEY").map_err(|_| {
        "[goal-digger] API_FOOTBALL_KEY not set. Live fixtures/injuries unavailable; \
         strength-only simulation still works."
            .to_string()
    })
}

fn af_get(path: &str, query: &[(&str, String)]) -> Result<Value, String> {
    let key = api_football_key()?;
    let client = reqwest::blocking::Client::new();
    let resp = client
        .get(format!("{APIFOOTBALL_BASE}{path}"))
        .header("x-apisports-key", key)
        .query(query)
        .timeout(std::time::Duration::from_secs(12))
        .send()
        .map_err(|e| format!("[goal-digger] api-football request failed: {e}"))?
        .json::<Value>()
        .map_err(|e| format!("[goal-digger] api-football parse failed: {e}"))?;
    Ok(resp)
}

pub fn fixtures(date: Option<String>) -> Result<Value, String> {
    let mut q = vec![
        ("league", WC_LEAGUE_ID.to_string()),
        ("season", WC_SEASON.to_string()),
    ];
    if let Some(d) = date {
        q.push(("date", d));
    }
    af_get("/fixtures", &q)
}

pub fn injuries(team_id: u32) -> Result<Value, String> {
    af_get(
        "/injuries",
        &[
            ("league", WC_LEAGUE_ID.to_string()),
            ("season", WC_SEASON.to_string()),
            ("team", team_id.to_string()),
        ],
    )
}

// ─── Live lineup-driven adjustments (API-FOOTBALL) ───────────────────────────
// WC 2026 has no injury feed, but lineups and player ratings ARE live. We derive
// each team's expected-goals adjustment from who is actually in the starting XI
// vs the team's highest-rated players. Stronger signal than injury rumors.

static AF_CACHE: LazyLock<std::sync::Mutex<HashMap<String, Value>>> =
    LazyLock::new(|| std::sync::Mutex::new(HashMap::new()));

/// Process-lifetime cache over API-FOOTBALL GETs (also freezes data for a demo run).
fn af_cached(path: &str, query: &[(&str, String)]) -> Result<Value, String> {
    let key = format!("{path}?{query:?}");
    if let Some(v) = AF_CACHE.lock().unwrap().get(&key) {
        return Ok(v.clone());
    }
    let v = af_get(path, query)?;
    AF_CACHE.lock().unwrap().insert(key, v.clone());
    Ok(v)
}

fn norm_team(s: &str) -> String {
    match s.trim().to_lowercase().as_str() {
        "usa" | "united states" | "usmnt" => "usa".into(),
        other => other.to_string(),
    }
}

const PLAYERS_JSON: &str = include_str!("../data/players.json");

/// Curated key players per team: normalized team name -> [(name, position-letter)].
static KEY_PLAYERS: LazyLock<HashMap<String, Vec<(String, char)>>> = LazyLock::new(|| {
    let raw: serde_json::Map<String, Value> = serde_json::from_str(PLAYERS_JSON).unwrap_or_default();
    let mut m = HashMap::new();
    for (team, arr) in raw {
        if team.starts_with('_') {
            continue;
        }
        let players: Vec<(String, char)> = arr
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|p| {
                        let pr = p.as_array()?;
                        let name = pr.first()?.as_str()?.to_string();
                        let pos = pr.get(1)?.as_str()?.chars().next()?.to_ascii_uppercase();
                        Some((name, pos))
                    })
                    .collect()
            })
            .unwrap_or_default();
        m.insert(norm_team(&team), players);
    }
    m
});

/// ASCII-lowercase letters only, so accented names match the lineup feed.
fn simplify(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphabetic()).flat_map(|c| c.to_lowercase()).collect()
}

/// Resolve an API-FOOTBALL team id by name (cached WC teams list).
pub fn wc_team_id(name: &str) -> Option<u32> {
    let v = af_cached(
        "/teams",
        &[("league", WC_LEAGUE_ID.to_string()), ("season", WC_SEASON.to_string())],
    )
    .ok()?;
    let want = norm_team(name);
    v.get("response")?.as_array()?.iter().find_map(|t| {
        let team = t.get("team")?;
        if norm_team(team.get("name")?.as_str()?) == want {
            team.get("id")?.as_u64().map(|n| n as u32)
        } else {
            None
        }
    })
}

/// Top key players by current WC rating: (name, position-letter G/D/M/A, rating).
fn wc_key_players(team_id: u32) -> Vec<(String, char, f64)> {
    let v = match af_cached(
        "/players",
        &[("team", team_id.to_string()), ("season", WC_SEASON.to_string()), ("page", "1".into())],
    ) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let mut out: Vec<(String, char, f64)> = v
        .get("response")
        .and_then(|r| r.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|p| {
                    let name = p.get("player")?.get("name")?.as_str()?.to_string();
                    let st = p.get("statistics")?.as_array()?.first()?;
                    let pos = st.get("games")?.get("position")?.as_str().unwrap_or("M");
                    let rating: f64 = st
                        .get("games")?
                        .get("rating")
                        .and_then(|r| r.as_str())
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0.0);
                    let pc = pos.chars().next().unwrap_or('M').to_ascii_uppercase();
                    Some((name, pc, rating))
                })
                .collect()
        })
        .unwrap_or_default();
    out.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(6);
    out
}

/// Find the WC fixture id for a matchup (either home/away orientation).
pub fn wc_fixture_id(home: &str, away: &str) -> Option<u32> {
    let v = af_cached(
        "/fixtures",
        &[("league", WC_LEAGUE_ID.to_string()), ("season", WC_SEASON.to_string())],
    )
    .ok()?;
    let (h, a) = (norm_team(home), norm_team(away));
    v.get("response")?.as_array()?.iter().find_map(|f| {
        let teams = f.get("teams")?;
        let th = norm_team(teams.get("home")?.get("name")?.as_str()?);
        let ta = norm_team(teams.get("away")?.get("name")?.as_str()?);
        if (th == h && ta == a) || (th == a && ta == h) {
            f.get("fixture")?.get("id")?.as_u64().map(|n| n as u32)
        } else {
            None
        }
    })
}

/// Starting-XI player names for a team in a fixture (None if lineup not posted).
fn wc_startxi(fixture_id: u32, team_id: u32) -> Option<Vec<String>> {
    let v = af_cached("/fixtures/lineups", &[("fixture", fixture_id.to_string())]).ok()?;
    let arr = v.get("response")?.as_array()?;
    for t in arr {
        if t.get("team")?.get("id")?.as_u64()? as u32 == team_id {
            let xi = t
                .get("startXI")?
                .as_array()?
                .iter()
                .filter_map(|p| p.get("player")?.get("name")?.as_str().map(|s| s.to_string()))
                .collect();
            return Some(xi);
        }
    }
    None
}

/// Live attack/defense multipliers for a team, from who is missing from the XI.
/// A team's defense multiplier scales the OPPONENT's goals (matches the engine).
/// Returns (attack_mult, defense_mult, reasons). Neutral if lineup not posted.
pub fn live_adjustment(team_name: &str, fixture_id: u32) -> (f64, f64, Vec<String>) {
    let stars = match KEY_PLAYERS.get(&norm_team(team_name)) {
        Some(s) if !s.is_empty() => s,
        _ => return (1.0, 1.0, vec![format!("{team_name}: no key-player list")]),
    };
    let tid = match wc_team_id(team_name) {
        Some(t) => t,
        None => return (1.0, 1.0, vec![format!("{team_name}: not found in WC teams")]),
    };
    let xi = match wc_startxi(fixture_id, tid) {
        Some(x) => x,
        None => return (1.0, 1.0, vec![format!("{team_name}: lineup not posted yet")]),
    };
    let xi_simplified: Vec<String> = xi.iter().map(|n| simplify(n)).collect();
    let (mut atk, mut def) = (1.0_f64, 1.0_f64);
    let mut reasons = vec![];
    for (name, pos) in stars {
        let surname = name.split_whitespace().last().unwrap_or(name);
        let key = simplify(surname);
        if key.len() >= 3 && xi_simplified.iter().any(|x| x.contains(&key)) {
            continue; // key player is starting
        }
        let (a, d, role) = match pos {
            'A' => (0.95, 1.0, "attacker"),
            'M' => (0.97, 1.0, "midfielder"),
            'D' => (1.0, 1.06, "defender"),
            'G' => (1.0, 1.08, "goalkeeper"),
            _ => (0.98, 1.0, "player"),
        };
        atk *= a;
        def *= d;
        reasons.push(format!("{name} ({role}) not in starting XI"));
    }
    if reasons.is_empty() {
        reasons.push(format!("{team_name}: key players all starting"));
    }
    (atk.clamp(0.6, 1.4), def.clamp(0.6, 1.4), reasons)
}

/// The next N upcoming WC fixtures (uncached so live scores stay fresh on poll).
pub fn wc_fixtures_next(n: u32) -> Result<Value, String> {
    af_get(
        "/fixtures",
        &[
            ("league", WC_LEAGUE_ID.to_string()),
            ("season", WC_SEASON.to_string()),
            ("next", n.to_string()),
        ],
    )
}

/// Currently in-play WC fixtures (live scores).
pub fn wc_fixtures_live() -> Result<Value, String> {
    af_get("/fixtures", &[("league", WC_LEAGUE_ID.to_string()), ("live", "all".into())])
}

/// Short team code (ESP, GER, ...) from the bundled alias list.
pub fn team_code(name: &str) -> String {
    TEAMS
        .get(&norm(name))
        .and_then(|r| r.aliases.first().cloned())
        .unwrap_or_else(|| name.chars().take(3).collect::<String>().to_uppercase())
}

// ─── Polymarket per-match prices (the real WC match markets) ─────────────────
// Each 2026 WC match is a 3-way market on Polymarket under a `fifwc-...` slug:
// "Will {home} win?", "Will {away} win?", "Will it end in a draw?". We find it by
// search and return the three Yes prices to compare against the model.

fn gamma_get(path: &str, query: &[(&str, String)]) -> Result<Value, String> {
    let ck = format!("gamma{path}?{query:?}");
    if let Some(v) = AF_CACHE.lock().unwrap().get(&ck) {
        return Ok(v.clone());
    }
    let client = reqwest::blocking::Client::new();
    let v = client
        .get(format!("{GAMMA_BASE}{path}"))
        .query(query)
        .timeout(std::time::Duration::from_secs(12))
        .send()
        .map_err(|e| format!("[goal-digger] gamma req failed: {e}"))?
        .json::<Value>()
        .map_err(|e| format!("[goal-digger] gamma parse failed: {e}"))?;
    AF_CACHE.lock().unwrap().insert(ck, v.clone());
    Ok(v)
}

fn first_price(v: Option<&Value>) -> Option<f64> {
    parse_str_array(v).first().and_then(|s| s.parse::<f64>().ok())
}

/// Live Polymarket 3-way prices for a real WC match: (home_win, draw, away_win).
/// None if the market is not found, is settled, or the team names don't match.
pub fn match_market(home: &str, away: &str) -> Option<(f64, f64, f64)> {
    let search = gamma_get(
        "/public-search",
        &[("q", format!("{home} vs {away}")), ("limit_per_type", "10".into())],
    )
    .ok()?;
    let slug = search.get("events")?.as_array()?.iter().find_map(|e| {
        e.get("slug")
            .and_then(|s| s.as_str())
            .filter(|s| s.starts_with("fifwc-"))
            .map(|s| s.to_string())
    })?;
    let ev = gamma_get("/events", &[("slug", slug)]).ok()?;
    let ev = ev.as_array()?.first()?;
    if ev.get("closed").and_then(|c| c.as_bool()).unwrap_or(false) {
        return None;
    }
    let (hl, al) = (home.to_lowercase(), away.to_lowercase());
    let (mut ph, mut pd, mut pa) = (None, None, None);
    for m in ev.get("markets")?.as_array()? {
        let q = m.get("question")?.as_str()?.to_lowercase();
        let yes = first_price(m.get("outcomePrices"));
        if q.contains("draw") {
            pd = yes;
        } else if q.contains(&hl) {
            ph = yes;
        } else if q.contains(&al) {
            pa = yes;
        }
    }
    Some((ph?, pd?, pa?))
}

// ─── Polymarket Gamma (public, no key) ───────────────────────────────────────

/// Fetch a market by slug and return its outcomes with current prices.
pub fn gamma_market(slug: &str) -> Result<Value, String> {
    let client = reqwest::blocking::Client::new();
    let arr = client
        .get(format!("{GAMMA_BASE}/markets"))
        .query(&[("slug", slug)])
        .timeout(std::time::Duration::from_secs(12))
        .send()
        .map_err(|e| format!("[goal-digger] gamma request failed: {e}"))?
        .json::<Value>()
        .map_err(|e| format!("[goal-digger] gamma parse failed: {e}"))?;

    let market = arr
        .as_array()
        .and_then(|a| a.first())
        .cloned()
        .ok_or_else(|| format!("[goal-digger] no Polymarket market for slug '{slug}'"))?;

    let outcomes = parse_str_array(market.get("outcomes"));
    let prices = parse_str_array(market.get("outcomePrices"));
    let pairs: Vec<Value> = outcomes
        .iter()
        .zip(prices.iter())
        .map(|(o, p)| json!({ "outcome": o, "price": p.parse::<f64>().unwrap_or(0.0) }))
        .collect();

    Ok(json!({
        "slug": slug,
        "question": market.get("question"),
        "outcomes": pairs,
        "clobTokenIds": parse_str_array(market.get("clobTokenIds")),
    }))
}

/// Gamma returns `outcomes`/`outcomePrices`/`clobTokenIds` as stringified JSON arrays.
fn parse_str_array(v: Option<&Value>) -> Vec<String> {
    match v {
        Some(Value::String(s)) => serde_json::from_str::<Vec<String>>(s).unwrap_or_default(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|x| x.as_str().unwrap_or_default().to_string())
            .collect(),
        _ => Vec::new(),
    }
}
