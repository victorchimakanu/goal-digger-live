//! Goal Digger tool surface.
//!
//! Brain tools only. Trade execution is borrowed from the official `polymarket`
//! app at runtime (ResolvePolymarketTradeIntent -> BuildPolymarketOrder ->
//! evm_commit_message -> simulate_batch -> SubmitPolymarketOrder).

use aomi_sdk::schemars::JsonSchema;
use aomi_sdk::*;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::data;
use crate::sim::{self, Adjustments, MatchSetup};

#[derive(Clone, Default)]
pub(crate) struct GoalDiggerApp;

// ─── Tool: simulate_match ────────────────────────────────────────────────────

pub(crate) struct SimulateMatch;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SimulateMatchArgs {
    /// Home team name (or alias, e.g. "ESP"). Neutral venue by default.
    pub(crate) home: String,
    /// Away team name (or alias).
    pub(crate) away: String,
    /// True for a neutral venue. World Cup group games are neutral; set false
    /// only when `home` is a host nation actually playing at home.
    #[serde(default = "tru")]
    pub(crate) neutral: bool,
    /// Knockout match: no draw, resolve via extra time + penalties.
    #[serde(default)]
    pub(crate) knockout: bool,
    /// Extra Elo for a host nation's crowd (e.g. 40 for USA/Mexico/Canada at home).
    #[serde(default)]
    pub(crate) host_elo_bonus: f64,
    /// Attack multiplier for home (1.0 = none). Carries injury/fitness/news reads. Clamped 0.6..1.4.
    #[serde(default = "one")]
    pub(crate) home_attack_adj: f64,
    /// Defense leakiness multiplier for home (1.0 = none; >1 = more goals conceded).
    #[serde(default = "one")]
    pub(crate) home_defense_adj: f64,
    #[serde(default = "one")]
    pub(crate) away_attack_adj: f64,
    #[serde(default = "one")]
    pub(crate) away_defense_adj: f64,
    /// Optional fixed seed for a reproducible on-stage run.
    #[serde(default)]
    pub(crate) seed: Option<u64>,
    /// Number of Monte-Carlo draws (default 50000, clamped 1k..500k).
    #[serde(default)]
    pub(crate) sims: Option<usize>,
}

fn tru() -> bool {
    true
}
fn one() -> f64 {
    1.0
}

impl DynAomiTool for SimulateMatch {
    type App = GoalDiggerApp;
    type Args = SimulateMatchArgs;

    const NAME: &'static str = "simulate_match";
    const DESCRIPTION: &'static str =
        "Simulate a World Cup match 50,000 times (Dixon-Coles + Elo + xG with bounded \
         adjustments for injuries, fitness, host crowd) and return the full outcome \
         distribution: win/draw/loss, advance probability for knockouts, over/under 2.5, \
         both-teams-to-score, and the most likely scorelines. Pass *_adj multipliers to \
         encode injuries, suspensions, fatigue, or news (clamped 0.6..1.4).";

    fn run(_app: &GoalDiggerApp, args: Self::Args, _ctx: DynToolCallCtx) -> Result<Value, String> {
        let home = data::team_strength(&args.home)?;
        let away = data::team_strength(&args.away)?;
        let setup = MatchSetup {
            home,
            away,
            neutral: args.neutral,
            host_elo_bonus: args.host_elo_bonus,
            knockout: args.knockout,
            home_adj: Adjustments { attack: args.home_attack_adj, defense: args.home_defense_adj },
            away_adj: Adjustments { attack: args.away_attack_adj, defense: args.away_defense_adj },
            seed: args.seed,
            sims: args.sims,
        };
        let outcome = sim::simulate(&setup);
        Ok(json!({ "source": "goal-digger", "model": "dixon-coles+elo+xg/monte-carlo", "outcome": outcome }))
    }
}

// ─── Tool: simulate_tournament ───────────────────────────────────────────────

pub(crate) struct SimulateTournament;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct SimulateTournamentArgs {
    /// Teams in seeded bracket order. Must be a power of two (8, 16, 32).
    pub(crate) teams: Vec<String>,
    /// Bracket rollouts (default 20000, clamped 1k..200k).
    #[serde(default)]
    pub(crate) rollouts: Option<usize>,
    #[serde(default)]
    pub(crate) seed: Option<u64>,
}

impl DynAomiTool for SimulateTournament {
    type App = GoalDiggerApp;
    type Args = SimulateTournamentArgs;

    const NAME: &'static str = "simulate_tournament";
    const DESCRIPTION: &'static str =
        "Simulate a single-elimination bracket many times and return each team's title \
         probability. Teams must be given in seeded bracket order and number a power of two.";

    fn run(_app: &GoalDiggerApp, args: Self::Args, _ctx: DynToolCallCtx) -> Result<Value, String> {
        let mut strengths = Vec::with_capacity(args.teams.len());
        for name in &args.teams {
            strengths.push(data::team_strength(name)?);
        }
        let champions = sim::simulate_tournament(
            &strengths,
            args.rollouts.unwrap_or(20_000),
            args.seed.unwrap_or(0x60A1),
        )?;
        let table: Vec<Value> = champions
            .into_iter()
            .map(|(team, p)| json!({ "team": team, "title_probability": p }))
            .collect();
        Ok(json!({ "source": "goal-digger", "championship": table }))
    }
}

// ─── Tool: find_edge ─────────────────────────────────────────────────────────

pub(crate) struct FindEdge;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct FindEdgeArgs {
    /// Polymarket market slug, e.g. "will-spain-win-the-2026-world-cup".
    pub(crate) market_slug: String,
    /// The outcome to price-check, e.g. "Yes" or a team name. Case-insensitive.
    pub(crate) outcome: String,
    /// Your model probability for that outcome (0..1), from simulate_match/tournament.
    pub(crate) model_prob: f64,
    /// Minimum edge to call it a value bet (default 0.04 = 4 points).
    #[serde(default)]
    pub(crate) threshold: Option<f64>,
}

impl DynAomiTool for FindEdge {
    type App = GoalDiggerApp;
    type Args = FindEdgeArgs;

    const NAME: &'static str = "find_edge";
    const DESCRIPTION: &'static str =
        "Compare a model probability to the live Polymarket price for one market outcome \
         and report the edge plus a quarter-Kelly stake suggestion. Fetches the price from \
         the public Polymarket Gamma API (no key).";

    fn run(_app: &GoalDiggerApp, args: Self::Args, _ctx: DynToolCallCtx) -> Result<Value, String> {
        let market = data::gamma_market(&args.market_slug)?;
        let want = args.outcome.trim().to_lowercase();
        let price = market
            .get("outcomes")
            .and_then(|o| o.as_array())
            .and_then(|arr| {
                arr.iter().find(|row| {
                    row.get("outcome")
                        .and_then(|v| v.as_str())
                        .map(|s| s.trim().to_lowercase() == want)
                        .unwrap_or(false)
                })
            })
            .and_then(|row| row.get("price").and_then(|p| p.as_f64()))
            .ok_or_else(|| format!("[goal-digger] outcome '{}' not found in market", args.outcome))?;

        let q = args.model_prob.clamp(0.0, 1.0);
        let edge = q - price;
        let threshold = args.threshold.unwrap_or(0.04);
        // Quarter-Kelly on a binary contract paying 1 at price p with true prob q.
        let kelly = if price < 1.0 { (q - price) / (1.0 - price) } else { 0.0 };
        let quarter_kelly = (kelly * 0.25).clamp(0.0, 0.25);

        let verdict = if edge >= threshold {
            "VALUE_BUY"
        } else if edge <= -threshold {
            "OVERPRICED"
        } else {
            "FAIR"
        };

        Ok(json!({
            "source": "goal-digger",
            "market": args.market_slug,
            "outcome": args.outcome,
            "market_price": price,
            "model_prob": (q * 10_000.0).round() / 10_000.0,
            "edge": (edge * 10_000.0).round() / 10_000.0,
            "verdict": verdict,
            "suggested_stake_fraction": (quarter_kelly * 10_000.0).round() / 10_000.0
        }))
    }
}

// ─── Tool: get_team_dossier ──────────────────────────────────────────────────

pub(crate) struct GetTeamDossier;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetTeamDossierArgs {
    /// Team name or alias.
    pub(crate) team: String,
    /// API-FOOTBALL team id, if you want live injuries (needs API_FOOTBALL_KEY).
    #[serde(default)]
    pub(crate) team_id: Option<u32>,
}

impl DynAomiTool for GetTeamDossier {
    type App = GoalDiggerApp;
    type Args = GetTeamDossierArgs;

    const NAME: &'static str = "get_team_dossier";
    const DESCRIPTION: &'static str =
        "Get a team's modelling inputs: Elo rating and expected goals for/against (bundled). \
         If a team_id and API_FOOTBALL_KEY are present, also pulls the live injury list.";

    fn run(_app: &GoalDiggerApp, args: Self::Args, ctx: DynToolCallCtx) -> Result<Value, String> {
        data::prime_secret(&ctx);
        let s = data::team_strength(&args.team)?;
        let mut out = json!({
            "source": "goal-digger",
            "team": s.name,
            "elo": s.elo,
            "xg_for": s.xg_for,
            "xg_against": s.xg_against
        });
        if let Some(id) = args.team_id {
            match data::injuries(id) {
                Ok(inj) => out["injuries"] = inj,
                Err(e) => out["injuries_note"] = json!(e),
            }
        }
        Ok(out)
    }
}

// ─── Tool: get_wc_fixtures ───────────────────────────────────────────────────

pub(crate) struct GetWcFixtures;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct GetWcFixturesArgs {
    /// Date filter YYYY-MM-DD (optional). Omit for the whole tournament.
    #[serde(default)]
    pub(crate) date: Option<String>,
}

impl DynAomiTool for GetWcFixtures {
    type App = GoalDiggerApp;
    type Args = GetWcFixturesArgs;

    const NAME: &'static str = "get_wc_fixtures";
    const DESCRIPTION: &'static str =
        "Get 2026 World Cup fixtures and live scores from API-FOOTBALL (needs API_FOOTBALL_KEY). \
         Optionally filter by date.";

    fn run(_app: &GoalDiggerApp, args: Self::Args, ctx: DynToolCallCtx) -> Result<Value, String> {
        data::prime_secret(&ctx);
        let data = data::fixtures(args.date)?;
        Ok(json!({ "source": "goal-digger", "fixtures": data }))
    }
}

// ─── Tool: watch_match (live momentum, stub) ─────────────────────────────────

pub(crate) struct WatchMatch;

#[derive(Debug, Deserialize, JsonSchema)]
pub(crate) struct WatchMatchArgs {
    /// API-FOOTBALL fixture id to watch.
    pub(crate) fixture_id: u64,
}

impl DynAomiTool for WatchMatch {
    type App = GoalDiggerApp;
    type Args = WatchMatchArgs;

    const NAME: &'static str = "watch_match";
    const DESCRIPTION: &'static str =
        "Watch a live match for momentum mispricing (early-goal overreactions) and surface \
         a trap-buy alert. Streaming mode is wired in a later pass; this returns the current \
         snapshot for now.";

    fn run(_app: &GoalDiggerApp, args: Self::Args, _ctx: DynToolCallCtx) -> Result<Value, String> {
        Ok(json!({
            "source": "goal-digger",
            "fixture_id": args.fixture_id,
            "status": "snapshot-only",
            "note": "Live streaming (DynAsyncSink) lands in the next pass. Use get_wc_fixtures + simulate_match for now."
        }))
    }
}
