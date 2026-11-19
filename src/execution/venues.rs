//! Swap quotes within each venue's rate limit.
//!
//! A liquidation sells its seized collateral in the same transaction, so it needs a route from a
//! swap venue, and each venue meters its requests: Jupiter 10 a second, Titan 1 a second. The
//! aggregator crate asks every venue it is configured with on every quote, so one client over all
//! of them spends each venue's whole limit on every liquidation, and a burst of liquidations is
//! mostly turned away with 429s.
//!
//! Instead there is one client per venue and a schedule over them:
//!
//! - **Pacing.** Each venue has a lane that hands out request slots at its rate (less some
//!   headroom). A liquidation reserves its slots before it asks, and waits for them (briefly) or is
//!   told there is no budget, rather than asking and being refused.
//! - **Jupiter first.** It takes the bulk of the traffic: a quote and then its swap instructions,
//!   two requests per route.
//! - **Titan where it matters.** Its one request a second goes to a liquidation worth enough for a
//!   better route to be worth it, in parallel with Jupiter, or to a liquidation Jupiter could not
//!   route, as a fallback. Otherwise it is left alone, so its slot is there when it is needed.
//! - **Background work yields.** The rebalancer's swaps use the same limits, but only when the
//!   liquidations have not wanted a venue for a while.

use std::{
    collections::HashMap,
    future::Future,
    sync::Mutex,
    time::{Duration, Instant},
};

use anyhow::{anyhow, Result};
use futures::{future::BoxFuture, stream::FuturesUnordered, StreamExt};
use log::{debug, info};
use solana_dex_superagg::{DexSuperAggClient, RouteRequest, SwapInstructions};

use super::nds::{NdsAnswer, NdsSource};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Venue {
    Jupiter,
    Titan,
    Dflow,
}

impl Venue {
    fn name(self) -> &'static str {
        match self {
            Venue::Jupiter => "jupiter",
            Venue::Titan => "titan",
            Venue::Dflow => "dflow",
        }
    }

    /// Requests one route costs the venue: Jupiter a quote and then its swap instructions; Titan one
    /// request that returns the route with its instructions; DFlow a quote and a build.
    pub fn requests_per_route(self) -> u32 {
        match self {
            Venue::Jupiter => 2,
            Venue::Titan => 1,
            Venue::Dflow => 2,
        }
    }
}

/// How a venue's request slots are handed out.
struct Lane {
    /// Time between request slots: the venue's limit, less headroom.
    interval: Duration,
    /// How far ahead of its slot a request may go, so a route's requests need not be spread out.
    burst: Duration,
    /// The next slot nobody has reserved.
    next_free: Instant,
    /// When a liquidation last reserved on this lane.
    last_foreground: Option<Instant>,
    stats: LaneStats,
}

#[derive(Default)]
struct LaneStats {
    /// Requests reserved, and how long each reservation waited for its slot.
    reserved: usize,
    waits: Vec<Duration>,
    /// Liquidations turned away because the wait would be too long, and ones that left the venue out
    /// (no slot free at once, or not worth its slot).
    over_budget: usize,
    no_slot: usize,
    not_worth_it: usize,
    /// How long each request to the venue took, answered or not: a Jupiter route is its quote and
    /// its swap instructions together, a Titan route one websocket request.
    round_trips: Vec<Duration>,
    /// Routes the venue won, errors it returned, and rate-limit answers among them.
    won: usize,
    errors: usize,
    rate_limited: usize,
}

/// The venues' rate limits, shared by everything that asks them for a route.
pub struct VenueLimits {
    lanes: Mutex<HashMap<Venue, Lane>>,
}

impl VenueLimits {
    /// A lane per `(venue, requests per second)`, run at `headroom` of that rate (0.9 keeps a tenth
    /// back); a venue not listed is not limited.
    pub fn new(limits: &[(Venue, f64)], headroom: f64) -> Self {
        let now = Instant::now();
        let lanes = limits
            .iter()
            .filter(|(_, rps)| *rps > 0.0)
            .map(|(venue, rps)| {
                let interval = Duration::from_secs_f64(1.0 / (rps * headroom.clamp(0.1, 1.0)));
                // A venue limited to one request a second gets no burst; a faster one may start a
                // route's requests together.
                let burst = if *rps > 1.0 {
                    interval * venue.requests_per_route()
                } else {
                    Duration::ZERO
                };
                (
                    *venue,
                    Lane {
                        interval,
                        burst,
                        next_free: now,
                        last_foreground: None,
                        stats: LaneStats::default(),
                    },
                )
            })
            .collect();
        Self {
            lanes: Mutex::new(lanes),
        }
    }

    /// Limits from `JUP_RPS` (default 10), `TITAN_RPS` (default 1) and `DFLOW_RPS` (default
    /// unlimited), at `VENUE_RATE_HEADROOM` (default 0.9) of each.
    pub fn from_env() -> Self {
        let rps = |var: &str, default: f64| {
            std::env::var(var)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
                .filter(|v| v.is_finite() && *v >= 0.0)
                .unwrap_or(default)
        };
        let headroom = rps("VENUE_RATE_HEADROOM", 0.9);
        Self::new(
            &[
                (Venue::Jupiter, rps("JUP_RPS", 10.0)),
                (Venue::Titan, rps("TITAN_RPS", 1.0)),
                (Venue::Dflow, rps("DFLOW_RPS", 0.0)),
            ],
            headroom,
        )
    }

    /// Reserves `routes` routes' worth of requests on `venue` for a liquidation. Returns how long to
    /// wait before asking, or how long it would have to wait when that is more than `max_wait`
    /// (nothing is reserved then).
    pub fn reserve(
        &self,
        venue: Venue,
        routes: u32,
        max_wait: Duration,
        now: Instant,
    ) -> Result<Duration, Duration> {
        self.reserve_counting(venue, routes, max_wait, now, true)
    }

    /// Like [`Self::reserve`] with no waiting: the venue is asked only if a slot is free this instant.
    pub fn try_reserve_now(&self, venue: Venue, routes: u32, now: Instant) -> bool {
        match self.reserve_counting(venue, routes, Duration::ZERO, now, false) {
            Ok(_) => true,
            Err(_) => {
                self.with_lane(venue, |lane| lane.stats.no_slot += 1);
                false
            }
        }
    }

    fn reserve_counting(
        &self,
        venue: Venue,
        routes: u32,
        max_wait: Duration,
        now: Instant,
        count_over_budget: bool,
    ) -> Result<Duration, Duration> {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        let Some(lane) = lanes.get_mut(&venue) else {
            return Ok(Duration::ZERO);
        };
        let requests = routes * venue.requests_per_route();
        let slot = lane.next_free.max(now);
        let wait = slot
            .saturating_duration_since(now)
            .saturating_sub(lane.burst);
        if wait > max_wait {
            if count_over_budget {
                lane.stats.over_budget += 1;
            }
            return Err(wait);
        }
        lane.next_free = slot + lane.interval * requests;
        lane.last_foreground = Some(now);
        lane.stats.reserved += requests as usize;
        lane.stats.waits.push(wait);
        Ok(wait)
    }

    /// Gives back `requests` of a reservation that was not used (a quote that covered nothing is not
    /// built, so its second request is never made).
    pub fn refund(&self, venue: Venue, requests: u32, now: Instant) {
        self.with_lane(venue, |lane| {
            if lane.next_free <= now {
                return;
            }
            let back = lane.interval * requests;
            lane.next_free = lane
                .next_free
                .checked_sub(back)
                .map_or(now, |slot| slot.max(now));
            lane.stats.reserved = lane.stats.reserved.saturating_sub(requests as usize);
        });
    }

    /// Reserves for background work (the rebalancer) on every venue in `costs` (routes each), only
    /// when each is free now and no liquidation has reserved on it for `quiet`. Otherwise says how
    /// long to wait before trying again. Nothing is reserved unless every venue can be.
    pub fn reserve_background(
        &self,
        costs: &[(Venue, u32)],
        quiet: Duration,
        now: Instant,
    ) -> Result<(), Duration> {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        let mut retry = Duration::ZERO;
        for (venue, _) in costs {
            let Some(lane) = lanes.get(venue) else {
                continue;
            };
            retry = retry.max(lane.next_free.saturating_duration_since(now));
            if let Some(last) = lane.last_foreground {
                retry = retry.max(quiet.saturating_sub(now.saturating_duration_since(last)));
            }
        }
        if !retry.is_zero() {
            return Err(retry);
        }
        for (venue, routes) in costs {
            if let Some(lane) = lanes.get_mut(venue) {
                let requests = routes * venue.requests_per_route();
                lane.next_free = now + lane.interval * requests;
                lane.stats.reserved += requests as usize;
            }
        }
        Ok(())
    }

    /// Waits for the background reservation of [`Self::reserve_background`].
    pub async fn acquire_background(&self, costs: &[(Venue, u32)], quiet: Duration) {
        loop {
            match self.reserve_background(costs, quiet, Instant::now()) {
                Ok(()) => return,
                Err(retry) => {
                    tokio::time::sleep(
                        retry.clamp(Duration::from_millis(50), Duration::from_secs(2)),
                    )
                    .await
                }
            }
        }
    }

    fn with_lane(&self, venue: Venue, f: impl FnOnce(&mut Lane)) {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(lane) = lanes.get_mut(&venue) {
            f(lane);
        }
    }

    fn note_round_trip(&self, venue: Venue, took: Duration) {
        self.with_lane(venue, |lane| lane.stats.round_trips.push(took));
    }

    fn note_not_worth_it(&self, venue: Venue) {
        self.with_lane(venue, |lane| lane.stats.not_worth_it += 1);
    }

    fn note_result(&self, venue: Venue, won: bool, error: Option<&str>) {
        self.with_lane(venue, |lane| {
            if won {
                lane.stats.won += 1;
            }
            if let Some(error) = error {
                lane.stats.errors += 1;
                if is_rate_limited(error) {
                    lane.stats.rate_limited += 1;
                }
            }
        });
    }

    /// One line on each limited venue since the last call, for the minute report: how much of its
    /// limit was used, how long liquidations waited for it, how often it was left out and why, and
    /// whether the venue still said 429.
    pub fn take_report(&self) -> Option<String> {
        let mut lanes = self.lanes.lock().unwrap_or_else(|e| e.into_inner());
        let mut venues: Vec<_> = lanes.iter_mut().collect();
        venues.sort_by_key(|(venue, _)| venue.name());
        let parts: Vec<String> = venues
            .into_iter()
            .map(|(venue, lane)| {
                let stats = std::mem::take(&mut lane.stats);
                let mut waits: Vec<u128> = stats.waits.iter().map(Duration::as_millis).collect();
                waits.sort_unstable();
                let wait = |q: f64| {
                    waits
                        .get(((waits.len() as f64 - 1.0).max(0.0) * q).round() as usize)
                        .copied()
                        .unwrap_or(0)
                };
                let mut trips: Vec<u128> = stats.round_trips.iter().map(Duration::as_millis).collect();
                trips.sort_unstable();
                let trip = |q: f64| {
                    trips
                        .get(((trips.len() as f64 - 1.0).max(0.0) * q).round() as usize)
                        .copied()
                        .unwrap_or(0)
                };
                format!(
                    "{}(requests={} waited_ms p50/max={}/{} round_trip_ms p50/p90/max={}/{}/{} over_budget={} no_slot={} not_worth_it={} won={} errors={} rate_limited={})",
                    venue.name(),
                    stats.reserved,
                    wait(0.5),
                    wait(1.0),
                    trip(0.5),
                    trip(0.9),
                    trip(1.0),
                    stats.over_budget,
                    stats.no_slot,
                    stats.not_worth_it,
                    stats.won,
                    stats.errors,
                    stats.rate_limited
                )
            })
            .collect();
        (!parts.is_empty()).then(|| format!("VENUE_BUDGET {}", parts.join(" ")))
    }
}

/// Whether a venue's error says it is rate limiting us.
fn is_rate_limited(error: &str) -> bool {
    let error = error.to_lowercase();
    error.contains("429") || error.contains("too many requests") || error.contains("rate limit")
}

/// Whether a venue answered that it has no route that pays or fits, as opposed to failing to answer.
fn is_no_route(error: &str) -> bool {
    error.starts_with("No route pays") || error.starts_with("No route fits")
}

/// Something that can be asked for a route: a client over one venue.
pub trait QuoteSource: Send + Sync {
    fn routes<'a>(
        &'a self,
        request: &'a RouteRequest,
    ) -> impl Future<Output = Result<Vec<SwapInstructions>>> + Send + 'a;
}

impl QuoteSource for DexSuperAggClient {
    fn routes<'a>(
        &'a self,
        request: &'a RouteRequest,
    ) -> impl Future<Output = Result<Vec<SwapInstructions>>> + Send + 'a {
        self.build_swap_instructions(request)
    }
}

/// How the schedule behaves (`JUP_MAX_WAIT_MS`, `TITAN_MIN_PROFIT_USD`).
#[derive(Clone, Copy, Debug)]
pub struct VenueConfig {
    /// The longest a liquidation waits for its Jupiter slot before it is turned away.
    pub jupiter_wait: Duration,
    /// The longest a liquidation Jupiter could not route waits for Titan's slot.
    pub fallback_wait: Duration,
    /// How long the other venue still gets once one has returned a route.
    pub grace: Duration,
    /// A liquidation earning at least this goes to Titan in parallel with Jupiter, when a slot is
    /// free; a smaller one only when Jupiter fails.
    pub parallel_min_profit_usd: f64,
}

impl VenueConfig {
    pub fn from_env() -> Self {
        let number = |var: &str| {
            std::env::var(var)
                .ok()
                .and_then(|v| v.trim().parse::<f64>().ok())
        };
        Self {
            jupiter_wait: Duration::from_millis(
                number("JUP_MAX_WAIT_MS").map_or(600, |ms| ms.max(0.0) as u64),
            ),
            fallback_wait: Duration::from_millis(800),
            grace: Duration::from_millis(100),
            parallel_min_profit_usd: number("TITAN_MIN_PROFIT_USD").unwrap_or(25.0),
        }
    }
}

/// A route and how it was found.
pub struct Routed {
    pub swap: SwapInstructions,
    /// Whether the local router found it (its route carries no lookup tables, so it may not fit a
    /// v0 transaction; the caller then asks the venues instead).
    pub from_nds: bool,
    /// The venues asked, what each answered and how long it took, for the liquidation's timeline.
    pub detail: String,
}

type Answer<'a> = BoxFuture<'a, (Venue, Duration, Result<Vec<SwapInstructions>>)>;

/// The venues a liquidation can get a route from, each its own client, and the limits between them.
pub struct VenueSet<S: QuoteSource> {
    pub jupiter: Option<S>,
    pub titan: Option<S>,
    pub dflow: Option<S>,
    /// The local router, asked before any venue; `None` when it is off.
    pub nds: Option<NdsSource>,
    pub limits: std::sync::Arc<VenueLimits>,
    pub config: VenueConfig,
}

impl<S: QuoteSource> VenueSet<S> {
    fn source(&self, venue: Venue) -> Option<&S> {
        match venue {
            Venue::Jupiter => self.jupiter.as_ref(),
            Venue::Titan => self.titan.as_ref(),
            Venue::Dflow => self.dflow.as_ref(),
        }
    }

    fn ask<'a>(&'a self, venue: Venue, request: &'a RouteRequest) -> Option<Answer<'a>> {
        let source = self.source(venue)?;
        Some(Box::pin(async move {
            let started = Instant::now();
            let result = source.routes(request).await;
            (venue, started.elapsed(), result)
        }))
    }

    /// A route for `request`, for a liquidation expected to earn `profit_usd`.
    pub async fn route(&self, request: &RouteRequest, profit_usd: f64) -> Result<Routed> {
        self.route_with(request, profit_usd, true).await
    }

    /// Like [`Self::route`] without asking the local router: for a liquidation whose transaction
    /// did not fit with its route.
    pub async fn route_venues_only(&self, request: &RouteRequest, profit_usd: f64) -> Result<Routed> {
        self.route_with(request, profit_usd, false).await
    }

    async fn route_with(
        &self,
        request: &RouteRequest,
        profit_usd: f64,
        use_nds: bool,
    ) -> Result<Routed> {
        let started = Instant::now();
        let mut notes: Vec<String> = Vec::new();
        let mut failures: Vec<(Venue, String)> = Vec::new();
        let mut left_out = false;
        let mut asked: Vec<Venue> = Vec::new();
        let mut pending: FuturesUnordered<Answer<'_>> = FuturesUnordered::new();

        // The local router first: no round trip and no rate limit, and any route it returns
        // already clears the floor. A decline or a failure leaves the venues to answer.
        if let Some(nds) = self.nds.as_ref().filter(|_| use_nds) {
            match nds.route(request) {
                Ok(NdsAnswer::Routed(swap, note)) => {
                    let detail = format!("{note} total_ms={}", started.elapsed().as_millis());
                    debug!("Route from nds: {detail}");
                    return Ok(Routed { swap: *swap, from_nds: true, detail });
                }
                Ok(NdsAnswer::Declined(note)) => notes.push(note),
                Err(e) => notes.push(format!("nds failed: {e}")),
            }
        }

        // Jupiter first, within its pace.
        if self.jupiter.is_some() {
            match self
                .limits
                .reserve(Venue::Jupiter, 1, self.config.jupiter_wait, Instant::now())
            {
                Ok(wait) => {
                    if !wait.is_zero() {
                        notes.push(format!(
                            "jupiter waited {}ms for its slot",
                            wait.as_millis()
                        ));
                        tokio::time::sleep(wait).await;
                    }
                    pending.extend(self.ask(Venue::Jupiter, request));
                    asked.push(Venue::Jupiter);
                }
                Err(wait) => {
                    left_out = true;
                    notes.push(format!("jupiter over budget (wait {}ms)", wait.as_millis()));
                }
            }
        }

        // The second tier in parallel when the liquidation is worth its scarce slot.
        let second_tier = [Venue::Titan, Venue::Dflow];
        let parallel = profit_usd >= self.config.parallel_min_profit_usd || asked.is_empty();
        for venue in second_tier {
            if self.source(venue).is_none() {
                continue;
            }
            if !parallel {
                self.limits.note_not_worth_it(venue);
                left_out = true;
                notes.push(format!(
                    "{} not asked (profit under its threshold)",
                    venue.name()
                ));
                continue;
            }
            if self.limits.try_reserve_now(venue, 1, Instant::now()) {
                pending.extend(self.ask(venue, request));
                asked.push(venue);
            } else {
                left_out = true;
                notes.push(format!("{} has no slot free", venue.name()));
            }
        }

        let mut best: Option<(Venue, SwapInstructions)> = None;
        self.collect(&mut pending, &mut best, &mut failures, &mut notes)
            .await;

        // Nothing yet: the second tier as a fallback, with a short wait for its slot.
        if best.is_none() {
            for venue in second_tier {
                if asked.contains(&venue) || self.source(venue).is_none() {
                    continue;
                }
                match self
                    .limits
                    .reserve(venue, 1, self.config.fallback_wait, Instant::now())
                {
                    Ok(wait) => {
                        if !wait.is_zero() {
                            tokio::time::sleep(wait).await;
                        }
                        notes.push(format!("{} asked as a fallback", venue.name()));
                        pending.extend(self.ask(venue, request));
                        asked.push(venue);
                        self.collect(&mut pending, &mut best, &mut failures, &mut notes)
                            .await;
                        if best.is_some() {
                            break;
                        }
                    }
                    Err(wait) => {
                        left_out = true;
                        notes.push(format!(
                            "{} over budget as a fallback (wait {}ms)",
                            venue.name(),
                            wait.as_millis()
                        ));
                    }
                }
            }
        }

        let detail = format!(
            "{} total_ms={}",
            notes.join("; "),
            started.elapsed().as_millis()
        );
        match best {
            Some((winner, swap)) => {
                self.limits.note_result(winner, true, None);
                debug!("Route from {}: {detail}", winner.name());
                Ok(Routed { swap, from_nds: false, detail })
            }
            None => Err(self.no_route(&failures, left_out, &asked, &notes)),
        }
    }

    /// Takes the answers as they come: once one venue has a route the others get `grace` to beat it,
    /// and the best guaranteed output wins.
    async fn collect<'a>(
        &self,
        pending: &mut FuturesUnordered<Answer<'a>>,
        best: &mut Option<(Venue, SwapInstructions)>,
        failures: &mut Vec<(Venue, String)>,
        notes: &mut Vec<String>,
    ) {
        let mut deadline: Option<tokio::time::Instant> = None;
        loop {
            let next = match deadline {
                Some(deadline) => match tokio::time::timeout_at(deadline, pending.next()).await {
                    Ok(next) => next,
                    Err(_) => return,
                },
                None => pending.next().await,
            };
            let Some((venue, took, result)) = next else {
                return;
            };
            self.limits.note_round_trip(venue, took);
            match result {
                Ok(mut routes) if !routes.is_empty() => {
                    let route = routes.swap_remove(0);
                    notes.push(format!(
                        "{} routed in {}ms (guaranteed out {})",
                        venue.name(),
                        took.as_millis(),
                        route.min_out_amount
                    ));
                    if best
                        .as_ref()
                        .is_none_or(|(_, held)| route.min_out_amount > held.min_out_amount)
                    {
                        *best = Some((venue, route));
                    }
                    deadline.get_or_insert(tokio::time::Instant::now() + self.config.grace);
                }
                Ok(_) => {
                    notes.push(format!(
                        "{} returned no route in {}ms",
                        venue.name(),
                        took.as_millis()
                    ));
                    failures.push((venue, "returned no route".to_string()));
                }
                Err(e) => {
                    let text = e.to_string();
                    self.limits.note_result(venue, false, Some(&text));
                    // A quote that covered nothing is never built: its second request was not made.
                    if is_no_route(&text) && venue.requests_per_route() > 1 {
                        self.limits.refund(venue, 1, Instant::now());
                    }
                    notes.push(format!(
                        "{} failed in {}ms: {}",
                        venue.name(),
                        took.as_millis(),
                        text.lines().next().unwrap_or_default()
                    ));
                    failures.push((venue, text));
                }
            }
        }
    }

    /// The error when no venue routed. "No route pays" and "No route fits" are kept as the venues said
    /// them only when every venue answered that and none was left out: otherwise it is a failure to
    /// get an answer, which the executor treats as worth retrying soon.
    fn no_route(
        &self,
        failures: &[(Venue, String)],
        left_out: bool,
        asked: &[Venue],
        notes: &[String],
    ) -> anyhow::Error {
        if !left_out && !failures.is_empty() && failures.iter().all(|(_, text)| is_no_route(text)) {
            return anyhow!("{}", failures[0].1);
        }
        let mut parts: Vec<String> = failures
            .iter()
            .map(|(venue, text)| {
                format!(
                    "{}: {}",
                    venue.name(),
                    text.lines().next().unwrap_or_default()
                )
            })
            .collect();
        if asked.is_empty() || left_out {
            parts.push(format!("left out: {}", notes.join("; ")));
        }
        anyhow!("No aggregator returned a quote ({})", parts.join("; "))
    }
}

/// Logs how a route was found. The route's venue and the schedule's notes go on the liquidation's
/// timeline as well.
pub fn log_routed(attempt_id: u64, routed: &Routed, elapsed: Duration) {
    info!(
        "LIQUIDATION_QUOTE_ROUTE attempt_id={} venue={:?} quote_ms={} detail=\"{}\"",
        attempt_id,
        routed.swap.aggregator,
        elapsed.as_millis(),
        routed.detail
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use solana_dex_superagg::Aggregator;
    use solana_pubkey::Pubkey;
    use std::sync::Arc;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    fn limits() -> VenueLimits {
        // Jupiter 10 a second and Titan 1 a second, at their full rate for round numbers.
        VenueLimits::new(&[(Venue::Jupiter, 10.0), (Venue::Titan, 1.0)], 1.0)
    }

    #[test]
    fn jupiter_serves_five_routes_a_second_and_titan_one() {
        let limits = limits();
        let start = Instant::now();
        // A route is two Jupiter requests, 100 ms apart: the first goes at once (and the second
        // route within the burst of one route), then one every 200 ms.
        let waits: Vec<u128> = (0..6)
            .map(|_| {
                limits
                    .reserve(Venue::Jupiter, 1, Duration::from_secs(5), start)
                    .unwrap()
                    .as_millis()
            })
            .collect();
        assert_eq!(waits, vec![0, 0, 200, 400, 600, 800]);

        // Titan: one a second, no burst.
        assert!(limits.try_reserve_now(Venue::Titan, 1, start));
        assert!(!limits.try_reserve_now(Venue::Titan, 1, start + ms(900)));
        assert!(limits.try_reserve_now(Venue::Titan, 1, start + ms(1000)));
    }

    #[test]
    fn a_reservation_over_the_longest_wait_is_turned_away_and_holds_nothing() {
        let limits = limits();
        let start = Instant::now();
        for _ in 0..3 {
            limits
                .reserve(Venue::Jupiter, 1, Duration::from_secs(5), start)
                .unwrap();
        }
        // The next slot is 400 ms out: over a 300 ms limit, refused without taking it.
        let would_wait = limits
            .reserve(Venue::Jupiter, 1, ms(300), start)
            .unwrap_err();
        assert_eq!(would_wait, ms(400));
        assert_eq!(
            limits.reserve(Venue::Jupiter, 1, ms(500), start).unwrap(),
            ms(400),
            "the refused one reserved nothing"
        );
    }

    #[test]
    fn an_unused_second_request_can_be_given_back() {
        let limits = limits();
        let start = Instant::now();
        limits
            .reserve(Venue::Jupiter, 1, Duration::from_secs(5), start)
            .unwrap();
        limits
            .reserve(Venue::Jupiter, 1, Duration::from_secs(5), start)
            .unwrap();
        limits
            .reserve(Venue::Jupiter, 1, Duration::from_secs(5), start)
            .unwrap();
        // The third waits 200 ms for its slot; the quote covered nothing and made one request.
        limits.refund(Venue::Jupiter, 1, start);
        assert_eq!(
            limits
                .reserve(Venue::Jupiter, 1, Duration::from_secs(5), start)
                .unwrap(),
            ms(300)
        );
    }

    #[test]
    fn background_work_waits_until_the_liquidations_have_been_quiet() {
        let limits = limits();
        let start = Instant::now();
        let costs = [(Venue::Jupiter, 1), (Venue::Titan, 1)];
        // Idle: it goes, and takes the slots.
        assert!(limits.reserve_background(&costs, ms(1000), start).is_ok());
        // A liquidation reserves on Jupiter after the background route's slots.
        let after = start + ms(300);
        limits
            .reserve(Venue::Jupiter, 1, Duration::from_secs(5), after)
            .unwrap();
        // The background wants Jupiter and Titan again: Titan is free again at 1 s, but Jupiter saw a
        // liquidation 300 ms ago and is not quiet for 700 ms more.
        let retry = limits
            .reserve_background(&costs, ms(1000), start + ms(1100))
            .unwrap_err();
        assert!(retry >= ms(1) && retry <= ms(1000), "{retry:?}");
        assert!(limits
            .reserve_background(&costs, ms(1000), start + ms(5000))
            .is_ok());
    }

    #[test]
    fn a_venue_with_no_limit_is_never_waited_for() {
        let limits = VenueLimits::new(&[(Venue::Jupiter, 10.0)], 1.0);
        let now = Instant::now();
        for _ in 0..100 {
            assert_eq!(
                limits
                    .reserve(Venue::Dflow, 1, Duration::ZERO, now)
                    .unwrap(),
                Duration::ZERO
            );
        }
        assert!(limits
            .take_report()
            .unwrap()
            .starts_with("VENUE_BUDGET jupiter("));
    }

    /// A venue that answers with a fixed result after a delay.
    struct Fake {
        delay: Duration,
        answer: Result<u64, String>,
    }

    impl QuoteSource for Fake {
        fn routes<'a>(
            &'a self,
            _request: &'a RouteRequest,
        ) -> impl Future<Output = Result<Vec<SwapInstructions>>> + Send + 'a {
            async move {
                tokio::time::sleep(self.delay).await;
                match &self.answer {
                    Ok(min_out) => Ok(vec![SwapInstructions {
                        aggregator: Aggregator::Jupiter,
                        in_amount: 1,
                        min_out_amount: *min_out,
                        instructions: vec![],
                        address_lookup_tables: vec![],
                    }]),
                    Err(text) => Err(anyhow!("{text}")),
                }
            }
        }
    }

    fn fake(delay_ms: u64, answer: Result<u64, &str>) -> Option<Fake> {
        Some(Fake {
            delay: ms(delay_ms),
            answer: answer.map_err(str::to_string),
        })
    }

    fn set(jupiter: Option<Fake>, titan: Option<Fake>) -> VenueSet<Fake> {
        VenueSet {
            jupiter,
            titan,
            dflow: None,
            nds: None,
            limits: Arc::new(limits()),
            config: VenueConfig {
                jupiter_wait: ms(600),
                fallback_wait: ms(800),
                grace: ms(100),
                parallel_min_profit_usd: 25.0,
            },
        }
    }

    fn request() -> RouteRequest {
        RouteRequest::new(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            1,
            1,
            50,
        )
    }

    #[tokio::test]
    async fn a_small_liquidation_leaves_titans_slot_alone() {
        let venues = set(fake(5, Ok(100)), fake(5, Ok(200)));
        let routed = venues.route(&request(), 3.0).await.unwrap();
        assert_eq!(routed.swap.min_out_amount, 100, "Jupiter alone");
        assert!(
            routed.detail.contains("titan not asked"),
            "{}",
            routed.detail
        );
        // Titan's slot is still there for the next one.
        assert!(venues
            .limits
            .try_reserve_now(Venue::Titan, 1, Instant::now()));
    }

    #[tokio::test]
    async fn a_large_liquidation_takes_the_better_route_of_both() {
        let venues = set(fake(5, Ok(100)), fake(20, Ok(150)));
        let routed = venues.route(&request(), 80.0).await.unwrap();
        // Titan answered within Jupiter's grace and guarantees more.
        assert_eq!(routed.swap.min_out_amount, 150);
    }

    #[tokio::test]
    async fn the_other_venue_gets_only_the_grace_to_beat_a_route() {
        let venues = set(fake(5, Ok(100)), fake(400, Ok(900)));
        let started = Instant::now();
        let routed = venues.route(&request(), 80.0).await.unwrap();
        assert_eq!(routed.swap.min_out_amount, 100);
        assert!(
            started.elapsed() < ms(300),
            "did not wait for the slow venue"
        );
    }

    #[tokio::test]
    async fn titan_is_the_fallback_when_jupiter_cannot_route() {
        let venues = set(
            fake(5, Err("No route pays 10 for at most 9")),
            fake(5, Ok(120)),
        );
        let routed = venues.route(&request(), 3.0).await.unwrap();
        assert_eq!(routed.swap.min_out_amount, 120);
        assert!(
            routed.detail.contains("titan asked as a fallback"),
            "{}",
            routed.detail
        );
    }

    #[tokio::test]
    async fn no_route_is_only_conclusive_when_every_venue_answered_it() {
        // Both venues answered: kept as they said it, so the executor holds the account.
        let both = set(
            fake(1, Err("No route pays 10 for at most 9")),
            fake(1, Err("No route pays 10 for at most 9")),
        );
        let error = both
            .route(&request(), 80.0)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(error.starts_with("No route pays"), "{error}");

        // Titan was left out for a small liquidation and then had no slot for the fallback: not
        // conclusive, so it reads as a failure to answer.
        let venues = set(
            fake(1, Err("No route pays 10 for at most 9")),
            fake(1, Ok(1)),
        );
        venues
            .limits
            .reserve(Venue::Titan, 1, Duration::from_secs(60), Instant::now())
            .unwrap();
        let error = venues
            .route(&request(), 3.0)
            .await
            .err()
            .unwrap()
            .to_string();
        assert!(
            error.starts_with("No aggregator returned a quote"),
            "{error}"
        );
        assert!(error.contains("left out"), "{error}");
    }

    #[tokio::test]
    async fn a_burst_is_paced_and_the_excess_turned_away_at_the_longest_wait() {
        let venues = set(fake(1, Ok(100)), None);
        let mut turned_away = 0;
        let started = Instant::now();
        // Ten liquidations at the same instant: Jupiter has 5 routes a second, and a wait over
        // 600 ms is refused.
        let requests: Vec<RouteRequest> = (0..10).map(|_| request()).collect();
        let results =
            futures::future::join_all(requests.iter().map(|request| venues.route(request, 3.0)))
                .await;
        for result in results {
            if let Err(e) = result {
                assert!(e.to_string().contains("over budget"), "{e}");
                turned_away += 1;
            }
        }
        assert!(turned_away >= 3, "{turned_away} turned away");
        assert!(started.elapsed() < ms(900));
        let report = venues.limits.take_report().unwrap();
        assert!(report.contains("over_budget="), "{report}");
        // The venue's round trips are measured too.
        assert!(report.contains("round_trip_ms p50/p90/max="), "{report}");
    }

    #[test]
    fn rate_limit_answers_are_recognised() {
        assert!(is_rate_limited(
            "Jupiter quote failed: HTTP 429 Too Many Requests"
        ));
        assert!(is_rate_limited("rate limit exceeded"));
        assert!(!is_rate_limited("No route pays 10 for at most 9"));
    }
}
