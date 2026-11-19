use crate::{
    cache::{
        pyth_prices::{Pricing, PushedPrices},
        Cache,
    },
    clock_manager,
    config::Eva01Config,
    execution::{
        activity::LiquidationActivity,
        executor::{
            apply_attempt_release, pushable_feed, AttemptOutcome, AttemptRelease, BundleLanding,
            Executor, LostRace,
        },
        inventory::InventoryStrategy,
        queue::{ExecutionQueue, QueuedIntent},
        reservations::TokenReservations,
        trace::{self, AttemptTrace, DetectionPath, Stage},
        venues,
    },
    geyser::{AccountType, GeyserUpdate},
    health_watch::{self, HealthWatch, Watch, WatchedPosition},
    hot_list::{HotConfig, HotList, HotMode, Standing},
    rebalancer::{liquidation_dex_gate, Rebalancer},
    tag_policy::{Balances, Candidate, TagPolicy},
    utils::{
        self,
        helius_sender::HeliusSender,
        jito::{JitoClient, TipEstimator},
        log_genuine_error,
        pyth_push::{PythPusher, MAX_PUSHED_FEEDS},
        rate_limiter::RateLimiter,
    },
    wrappers::{
        bank::BankWrapper,
        liquidator_account::{
            is_seizable_asset_tag, LiquidationPair, LiquidatorAccount, PreparedLiquidatableAccount,
        },
        marginfi_account::{MarginfiAccountWrapper, ObservationAccounts},
        oracle::{oracle_publish_time, stake_pool_is_empty, OracleWrapper, OracleWrapperTrait},
    },
};
use anchor_lang::AccountDeserialize;
use anyhow::{anyhow, Result};
use crossbeam::channel::{bounded, unbounded, Receiver, RecvTimeoutError, Sender};
use fixed::types::I80F48;
use fixed_macro::types::I80F48;
use log::{debug, error, info, warn};
use marginfi::{
    constants::{
        LIQUIDATION_BONUS_FEE_MINIMUM, LIQUIDATION_CLOSEOUT_DOLLAR_THRESHOLD,
        LIQUIDATION_TAG_DELAY_SECS, LIQUIDATION_TAG_FULL_PREMIUM_SECS,
    },
    state::{bank::BankImpl, marginfi_account::get_health_components},
};
use marginfi_type_crate::{
    constants::{ASSET_TAG_JUPLEND, ASSET_TAG_KAMINO, BANKRUPT_THRESHOLD},
    types::{
        reconcile_emode_configs, BalanceSide, Bank, BankOperationalState, EmodeConfig,
        HealthPriceMode, MarginfiAccount, MarginfiGroup, OracleSetup, ReconciledEmodeConfig,
        RequirementType,
    },
};
use rayon::iter::{IntoParallelRefIterator, ParallelIterator};
use solana_account::Account;
use solana_account_info::IntoAccountInfo;
use solana_client::rpc_client::RpcClient;
use solana_clock::Clock;
use solana_commitment_config::CommitmentConfig;
use solana_keypair::Keypair;
use solana_pubkey::Pubkey;
use std::sync::atomic::Ordering;
use std::{
    cmp::min,
    collections::{HashMap, HashSet},
    panic::{self, AssertUnwindSafe},
    sync::{
        atomic::{AtomicBool, AtomicU64},
        Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

mod ingest;
use ingest::{observe_results, Handoff, HotStats, Ingest};

/// Max liquidations executed concurrently. Bounds in-flight RPC/simulation load while still
/// letting a burst of liquidatable accounts (e.g. a price crash) be worked in parallel rather
/// than one-at-a-time.
const MAX_CONCURRENT_LIQUIDATIONS: usize = 8;
static NEXT_LIQUIDATION_ATTEMPT_ID: AtomicU64 = AtomicU64::new(1);

/// How often the rebalancer checks whether a pass is due (see `LiquidationActivity`).
const REBALANCE_CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Every borrower is evaluated at least this often, whatever the health watch says: a backstop
/// for what it can't see, such as an integration bank's exchange rate. The sweep is spread over
/// the interval in slices, a few milliseconds each, so it never holds up the cycle on a change.
const FULL_SCAN_INTERVAL: Duration = Duration::from_secs(30);

/// The full sweep, a slice at a time: every borrower once per [`FULL_SCAN_INTERVAL`], the share
/// of the pass that time has made due on each call.
#[derive(Default)]
struct Sweep {
    pass: Vec<Pubkey>,
    next: usize,
    /// Accounts due but not taken yet, as a fraction of one.
    owed: f64,
    last: Option<Instant>,
    /// When the borrowers were last collected, so an empty or failed collection is retried once
    /// an interval, not on every loop.
    collected: Option<Instant>,
}

impl Sweep {
    /// The accounts due by `now`; a new pass, from `borrowers`, once the last one is done.
    fn next_slice(&mut self, now: Instant, borrowers: impl FnOnce() -> Vec<Pubkey>) -> Vec<Pubkey> {
        if self.next >= self.pass.len() {
            let recently = self
                .collected
                .is_some_and(|at| now.saturating_duration_since(at) < FULL_SCAN_INTERVAL);
            if self.pass.is_empty() && recently {
                self.last = Some(now);
                return Vec::new();
            }
            self.pass = borrowers();
            self.next = 0;
            self.collected = Some(now);
        }
        let elapsed = self
            .last
            .map_or(Duration::ZERO, |last| now.saturating_duration_since(last));
        self.last = Some(now);
        self.owed +=
            self.pass.len() as f64 * elapsed.as_secs_f64() / FULL_SCAN_INTERVAL.as_secs_f64();
        let left = self.pass.len() - self.next;
        let take = (self.owed.floor() as usize).min(left);
        // A stall longer than the interval owes no more than the rest of this pass.
        self.owed = (self.owed - take as f64).min((left - take) as f64);
        let slice = self.pass[self.next..self.next + take].to_vec();
        self.next += take;
        slice
    }
}

/// How often the scan statistics are reported.
const SCAN_REPORT_INTERVAL: Duration = Duration::from_secs(60);

/// A liquidation the update path and the background path both find within this long is one event:
/// how far ahead the update path was is measured, and the background path's repeat sightings (its
/// sweep, the next change) are not counted again.
const HOT_EVENT_WINDOW: Duration = Duration::from_secs(60);

/// How many of the accounts that could earn `min_profit` fall in each margin band, for choosing the
/// hot list's cutoff: under 0 (unhealthy), 0.5%, 1%, 2%, 4%, 8%, 16%, and above. And how many could
/// earn it at all.
fn sizing_buckets(standings: &HashMap<Pubkey, Standing>, min_profit: f64) -> ([usize; 8], usize) {
    const EDGES: [f64; 7] = [0.0, 0.005, 0.01, 0.02, 0.04, 0.08, 0.16];
    let mut buckets = [0usize; 8];
    let mut could_pay = 0;
    for standing in standings.values() {
        if standing.potential_profit < min_profit {
            continue;
        }
        could_pay += 1;
        let band = EDGES
            .iter()
            .position(|edge| standing.margin < *edge)
            .unwrap_or(EDGES.len());
        buckets[band] += 1;
    }
    (buckets, could_pay)
}

/// What a batch of updates changed.
#[derive(Default)]
struct Changes {
    /// When the oldest update in the batch arrived from the network.
    oldest: Option<Instant>,
    /// Accounts to evaluate whatever their watch says: their own state changed, or their bank's
    /// configuration did.
    accounts: HashSet<Pubkey>,
    /// Banks whose price or share value may have moved.
    moved_banks: HashSet<Pubkey>,
}

impl Changes {
    fn is_empty(&self) -> bool {
        self.accounts.is_empty() && self.moved_banks.is_empty()
    }

    /// Adds what `other` changed to this batch.
    fn merge(&mut self, other: Changes) {
        self.oldest = match (self.oldest, other.oldest) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        self.accounts.extend(other.accounts);
        self.moved_banks.extend(other.moved_banks);
    }
}

/// Evaluation work since the last report.
struct ScanStats {
    since: Instant,
    evaluated: usize,
    skipped: usize,
    /// How long each cycle on a change took, apart from the sweep's slices.
    cycle_times: Vec<Duration>,
    /// How long the oldest update of each batch had waited, from arriving off the network to its
    /// cycle starting: queueing ahead of evaluation.
    waits: Vec<Duration>,
    /// The same, to the cycle finishing: the whole time from a change to every affected account
    /// being evaluated and any liquidation queued.
    detections: Vec<Duration>,
    /// For each liquidation this path queued after an update, how long after the oldest update behind
    /// it arrived: the number to set beside the hot path's own arrival-to-queue time.
    liquidation_latencies: Vec<Duration>,
    sweeps: usize,
    slowest_sweep: Duration,
}

/// The median, 90th percentile and slowest of `times`, in ms.
fn percentiles(times: &[Duration]) -> (u128, u128, u128) {
    let mut times = times.to_vec();
    times.sort();
    let at = |q: f64| {
        times
            .get(((times.len() as f64 - 1.0) * q).round() as usize)
            .map_or(0, Duration::as_millis)
    };
    (at(0.5), at(0.9), at(1.0))
}

impl ScanStats {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            evaluated: 0,
            skipped: 0,
            cycle_times: Vec::new(),
            waits: Vec::new(),
            detections: Vec::new(),
            liquidation_latencies: Vec::new(),
            sweeps: 0,
            slowest_sweep: Duration::ZERO,
        }
    }

    /// The median, 90th percentile and slowest of the cycles on a change, in ms.
    fn cycle_percentiles(&self) -> (u128, u128, u128) {
        percentiles(&self.cycle_times)
    }
}

/// Taken off the premium `end_liquidation` allows, so price moves between evaluation and landing
/// cannot push the seizure over the cap.
const PREMIUM_SAFETY_MARGIN: I80F48 = I80F48!(0.005);

/// Liquidations are sized to this share of the most the program allows, leaving room for what can
/// still move before landing (feeds not pushed, interest, reserve rates; an oversized piece ends the
/// account healthy, 6068); profit is judged on the same share.
const LIQUIDATION_SIZE_HAIRCUT: I80F48 = I80F48!(0.95);

/// Equity under this share of liabilities is close enough to bad debt that the program may judge
/// the account insolvent on its prices: no tag premium is counted on.
const NEAR_BAD_DEBT: I80F48 = I80F48!(1.01);

/// Room left under `end_liquidation`'s rule that health may fall by no more than the premium
/// extracted, per dollar repaid.
const HEALTH_FALL_MARGIN: I80F48 = I80F48!(0.002);

/// The premium a tag grows to. marginfi lowered it from 100% to 25% after the version eva builds
/// against; the lower one is safe on either program, since seizing less than the cap allows always
/// passes it.
const TAG_MAX_PREMIUM: I80F48 = I80F48!(0.25);

/// The premium `end_liquidation` allows on an account tagged at `tagged_at`, as the program
/// computes it (`liquidation_record::tag_adjusted_premium`), with [`TAG_MAX_PREMIUM`].
fn tag_premium(base_premium: I80F48, tagged_at: i64, now: i64) -> I80F48 {
    if tagged_at == 0 || base_premium >= TAG_MAX_PREMIUM {
        return base_premium;
    }
    let elapsed = now.saturating_sub(tagged_at);
    if elapsed <= LIQUIDATION_TAG_DELAY_SECS {
        return base_premium;
    }
    let growth_secs = elapsed.min(LIQUIDATION_TAG_FULL_PREMIUM_SECS) - LIQUIDATION_TAG_DELAY_SECS;
    let growth_window = LIQUIDATION_TAG_FULL_PREMIUM_SECS - LIQUIDATION_TAG_DELAY_SECS;
    let progress = I80F48::from_num(growth_secs) / I80F48::from_num(growth_window);
    base_premium + (TAG_MAX_PREMIUM - base_premium) * progress
}

/// A hot account is re-evaluated once any of its positions' unit values has moved this share of its
/// margin since the last evaluation (the margin is what a move against it has to use up, so a quarter
/// of it is checked well before the line).
const HOT_TOLERANCE_SHARE: f64 = 0.25;

/// A hot account counts as having at least this margin for its tolerance, so one almost on the line
/// is not evaluated on every rounding.
const HOT_MIN_MARGIN: f64 = 0.002;

/// The relative move that re-evaluates an account already past the line: a liquidation of it is
/// sized on prices, so it follows them closely.
const HOT_UNHEALTHY_TOLERANCE: f64 = 0.0005;

/// Accounts further than this from the line never get a hot watch.
const HOT_WATCH_MARGIN_LIMIT: f64 = 0.25;

/// Debt and collateral are valued here at maintenance prices and a liquidation at equity prices
/// (time-weighted), which can differ by a few percent: the profit bound leaves this much room.
const PRICE_SLACK: f64 = 1.2;

/// Below this margin on eva's prices, an account is also priced on the chain's, which can lag
/// eva's by about a price update and still allow a liquidation eva's prices don't.
const CHAIN_CHECK_MARGIN: f64 = 0.02;

/// A liquidation the chain's prices already allow goes behind a push only when the push lets it
/// earn this much more: a push takes a bundle slot and can lose to the chain catching up.
const PUSH_PROFIT_GAIN: I80F48 = I80F48!(1.2);

/// A pair that is cheaper to execute wins unless the heavier one earns more than this share more:
/// landing first matters far more than the last few cents of premium.
const PAIR_PROFIT_TIE_MARGIN: I80F48 = I80F48!(0.30);

/// How much heavier seizing from a bank makes the liquidation transaction. An integration bank
/// (Kamino, JupLend) withdraws through its venue, adding the accounts an in-tx swap would need to
/// fit under the account-lock limit.
fn seize_weight(asset_tag: u8) -> u8 {
    u8::from(matches!(asset_tag, ASSET_TAG_KAMINO | ASSET_TAG_JUPLEND))
}

/// Whether a pair earning `profit` and seizing at `weight` should replace the best so far: a
/// lighter pair wins unless the heavier one earns more than [`PAIR_PROFIT_TIE_MARGIN`] more.
fn is_better_pair(profit: I80F48, weight: u8, best_profit: I80F48, best_weight: u8) -> bool {
    let margin = I80F48::ONE + PAIR_PROFIT_TIE_MARGIN;
    match weight.cmp(&best_weight) {
        std::cmp::Ordering::Equal => profit > best_profit,
        std::cmp::Ordering::Less => profit * margin >= best_profit,
        std::cmp::Ordering::Greater => profit > best_profit * margin,
    }
}

/// Of pairs earning `profit`, seizing at `weight`, and earning `funded` of that from what the
/// wallet holds of their debt, in order: the pair for the wallet repay, the one the wallet funds
/// the most profit of (then the most profitable, then the lighter), and the pair for a sandwich
/// when that differs and earns more (a lighter one unless the heavier earns more than
/// [`PAIR_PROFIT_TIE_MARGIN`] more). The executor weighs the two (`wallet_goes_first`). Indexes
/// into the pairs; `None` when there are none.
fn choose_pairs(
    pairs: impl Iterator<Item = (I80F48, u8, I80F48)>,
) -> Option<(usize, Option<usize>)> {
    // (index, funded, profit, weight)
    let mut main: Option<(usize, I80F48, I80F48, u8)> = None;
    let mut sandwich: Option<(usize, I80F48, u8)> = None;
    for (index, (profit, weight, funded)) in pairs.enumerate() {
        if main.is_none_or(|(_, best_funded, best_profit, best_weight)| {
            (funded, profit, std::cmp::Reverse(weight))
                > (best_funded, best_profit, std::cmp::Reverse(best_weight))
        }) {
            main = Some((index, funded, profit, weight));
        }
        if sandwich.is_none_or(|(_, best_profit, best_weight)| {
            is_better_pair(profit, weight, best_profit, best_weight)
        }) {
            sandwich = Some((index, profit, weight));
        }
    }
    let (main, _, main_profit, main_weight) = main?;
    // Only when it beats the main pair for an in-tx swap.
    let sandwich = sandwich
        .filter(|(index, profit, weight)| {
            *index != main && is_better_pair(*profit, *weight, main_profit, main_weight)
        })
        .map(|(index, _, _)| index);
    Some((main, sandwich))
}

/// Where an SPL token account's `amount: u64` sits, after its mint and owner.
const TOKEN_AMOUNT: std::ops::Range<usize> = 64..72;

/// The share of an `amount` (native units) repay of `liab_bank`'s mint that the wallet holds, as
/// last read, at most 1. Claims by liquidations in flight aren't counted: the executor checks them
/// when it plans.
fn wallet_share(cache: &Cache, liab_bank: &Pubkey, amount: I80F48) -> I80F48 {
    if amount <= I80F48::ZERO {
        return I80F48::ZERO;
    }
    let held = cache
        .banks
        .try_get_bank(liab_bank)
        .and_then(|bank| cache.tokens.try_get_token_for_mint(&bank.bank.mint))
        .and_then(|token| cache.tokens.try_get_account(&token))
        .ok()
        .and_then(|token| token.data.get(TOKEN_AMOUNT)?.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0);
    I80F48::min(I80F48::ONE, I80F48::from_num(held) / amount)
}

/// A cycle's results: the liquidations, the unhealthy accounts not worth liquidating, and each
/// healthy account's watch.
type Evaluations = (
    Vec<PreparedLiquidatableAccount>,
    Vec<UnhealthyAccount>,
    Vec<HealthyAccount>,
);

/// What one share of `bank` on the given side is worth at its maintenance price, the unit a
/// position's weighted value scales with; `None` when it can't be priced.
fn unit_value(cache: &Cache, clock: &Clock, bank: &Pubkey, liability: bool) -> Option<f64> {
    let bank_wrapper = cache.banks.try_get_bank(bank).ok()?;
    let oracle = OracleWrapper::build(cache, clock, bank).ok()?;
    let (side, share_value) = if liability {
        (
            BalanceSide::Liabilities,
            bank_wrapper.bank.liability_share_value,
        )
    } else {
        (BalanceSide::Assets, bank_wrapper.bank.asset_share_value)
    };
    let price = bank_wrapper
        .price(&oracle, side, RequirementType::Maintenance)
        .ok()?;
    Some((price * I80F48::from(share_value)).to_num())
}

/// Unit values computed once per evaluation cycle: every account in it shares the same banks.
struct UnitValues<'a> {
    cache: &'a Cache,
    clock: &'a Clock,
    memo: std::sync::RwLock<HashMap<(Pubkey, bool), Option<f64>>>,
}

impl<'a> UnitValues<'a> {
    fn new(cache: &'a Cache, clock: &'a Clock) -> Self {
        Self {
            cache,
            clock,
            memo: Default::default(),
        }
    }

    fn get(&self, bank: &Pubkey, liability: bool) -> Option<f64> {
        let key = (*bank, liability);
        if let Some(value) = self
            .memo
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            return *value;
        }
        let value = unit_value(self.cache, self.clock, bank, liability);
        self.memo
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, value);
        value
    }
}

/// A tag the tag policy found due.
struct DueTag {
    address: Pubkey,
    observation_accounts: ObservationAccounts,
    waited: Duration,
}

/// Sends the tags it is handed, one after another; it stops once the sender is dropped.
fn spawn_tagger(liquidator_account: Arc<LiquidatorAccount>) -> Result<Sender<DueTag>> {
    let (tags, due) = unbounded::<DueTag>();
    thread::Builder::new()
        .name("tagger".to_string())
        .spawn(move || {
            for tag in due {
                match liquidator_account.send_tag_tx(tag.address, &tag.observation_accounts) {
                    Ok(signature) => info!(
                        "Sent a tag for {} after {} min unhealthy with nobody liquidating it, so its premium grows: {}",
                        tag.address,
                        tag.waited.as_secs() / 60,
                        signature
                    ),
                    Err(err) => error!("Failed to tag {}: {:?}", tag.address, err),
                }
            }
        })?;
    Ok(tags)
}

/// How often each unhealthy account we are not liquidating is reported.
const SKIP_REPORT_INTERVAL: Duration = Duration::from_secs(1200);

/// After an attempt the same account is left alone this long, unless it lost a race. After a
/// landing it goes again as soon as Geyser delivers the account at the slot it landed at, so the
/// next attempt is sized on the state the landing produced; this is the most it waits. It also
/// spaces out retries of a failure that would repeat (a refused bundle, an account on hold). A lost
/// race changed nothing, so the account may go again at once, a few times in a row.
const ATTEMPT_COOLDOWN: Duration = Duration::from_secs(3);

pub struct Liquidator {
    /// Moved onto its own thread by [`Liquidator::start`].
    rebalancer: Option<Rebalancer>,
    executor: Arc<Executor>,
    strategy: Arc<InventoryStrategy>,
    evaluator: Arc<Evaluator>,
    queue: Arc<ExecutionQueue>,
    /// The liquidation attempts the rebalancer stays out of the way of.
    activity: Arc<LiquidationActivity>,
    threads: Vec<JoinHandle<()>>,
    /// Watches bundles after the relays accept them and releases those accounts.
    settlement: Vec<JoinHandle<()>>,
    tag_policy: TagPolicy,
    /// Tags due, sent from a thread of their own so the scan never waits on the RPC.
    tags: Sender<DueTag>,
    skip_reports: RateLimiter<Pubkey>,
    /// Which borrowers updates can skip.
    health_watch: HealthWatch,
    sweep: Sweep,
    scan_stats: ScanStats,
    /// The update path (applies Geyser and Hermes updates, checks the hot list); moved onto its own
    /// thread by [`Liquidator::start`].
    ingest: Option<Ingest>,
    ingest_thread: Option<JoinHandle<()>>,
    /// What the update path has applied that this background path has not taken yet.
    handoff: Arc<Handoff>,
    hot: Arc<Mutex<HotList>>,
    hot_config: HotConfig,
    hot_stats: Arc<Mutex<HotStats>>,
    /// The swap venues' rate limits, reported each minute.
    venue_limits: Arc<venues::VenueLimits>,
    /// Where each account last stood, to size the hot list in the minute report.
    standings: HashMap<Pubkey, Standing>,
    stop_liquidator: Arc<AtomicBool>,
    cache: Arc<Cache>,
}

/// Finds the liquidatable accounts in the cache and sizes their liquidations. Shared by the
/// liquidator loop (which evaluates every updated account) and the execution workers (which
/// re-evaluate an account on the current state right before liquidating it).
struct Evaluator {
    cache: Arc<Cache>,
    /// Builds price pushes, to check which a liquidation can go behind; `None` with bundles off,
    /// when nothing is pushed.
    pusher: Option<Arc<PythPusher>>,
    min_profit: f64,
    /// Liquidations earning more than this are left to another instance (a dust instance's cap).
    max_profit: Option<f64>,
}

#[derive(Debug, Clone, Copy)]
struct LiquidationAmounts {
    max_liquidatable_asset_amount: I80F48,
    max_liquidatable_liab_amount: I80F48,
    /// The most `end_liquidation` lets this pair repay (USD), whether or not it is worth it.
    max_repay: I80F48,
    /// The premium the liquidation is sized at.
    premium: I80F48,
    liquidator_profit: I80F48,
    /// Unhealthy, untagged, solvent and not e-mode boosted: tagging would let the premium grow.
    taggable: bool,
    caps: RepayCapBreakdown,
}

#[derive(Debug, Clone, Copy, Default)]
struct RepayCapBreakdown {
    debt_usd: f64,
    collateral_usd: f64,
    equity_usd: Option<f64>,
    health_usd: Option<f64>,
    binding: &'static str,
}

impl LiquidationAmounts {
    fn none() -> Self {
        Self {
            max_liquidatable_asset_amount: I80F48::ZERO,
            max_liquidatable_liab_amount: I80F48::ZERO,
            max_repay: I80F48::ZERO,
            premium: I80F48::ZERO,
            liquidator_profit: I80F48::ZERO,
            taggable: false,
            caps: RepayCapBreakdown::default(),
        }
    }

    fn is_liquidatable(&self) -> bool {
        !self.max_liquidatable_asset_amount.is_zero()
    }
}

/// What the pair search finds for an account. Short-lived, so the sizing stays inline.
#[allow(clippy::large_enum_variant)]
enum PairSearch {
    /// Healthy with this margin (`health_watch::margin`).
    Healthy(f64),
    /// Unhealthy, with no pair to size.
    None,
    Sized(Sizing),
}

/// The best bank pair for an unhealthy account and what it allows.
struct Sizing {
    /// The most profitable pair.
    asset_bank: Pubkey,
    liab_bank: Pubkey,
    amounts: LiquidationAmounts,
    /// The pair an in-tx swap should take instead, when the most profitable one seizes from a
    /// Kamino or JupLend bank and a lighter pair earns nearly as much (`is_better_pair`).
    sandwich: Option<(Pubkey, Pubkey, LiquidationAmounts)>,
    maintenance_health: I80F48,
    /// The push to post first, whose prices the pairs are sized on.
    push: Option<PushedPrices>,
    /// The liquidation the chain's prices already allow, when a push lets this one earn more.
    without_push: Option<Box<Sizing>>,
}

/// A bank's maintenance price over its equity price on one side.
/// Of the oracles each paired with the account's health once only it is pushed, the ones that lower
/// it most below `on_chain`, as many as one push carries.
fn strongest_pushes(
    on_chain: I80F48,
    singles: impl Iterator<Item = (I80F48, Pubkey)>,
) -> Vec<Pubkey> {
    let mut lowering: Vec<(I80F48, Pubkey)> = singles.filter(|(h, _)| *h < on_chain).collect();
    lowering.sort_by_key(|(h, _)| *h);
    lowering
        .into_iter()
        .take(MAX_PUSHED_FEEDS)
        .map(|(_, oracle)| oracle)
        .collect()
}

/// The bank and oracle accounts behind `banks_and_oracles`, the oracles on `pricing`'s prices.
fn program_accounts(
    cache: &Cache,
    banks_and_oracles: &[Pubkey],
    pricing: Pricing,
) -> Result<Vec<Account>> {
    banks_and_oracles
        .iter()
        .map(|pk| {
            if let Ok(wrapper) = cache.banks.try_get_bank(pk) {
                return Ok(wrapper.account);
            }
            let cached = cache.oracles.try_get_account(pk)?;
            Ok(pricing.account(&cache.pyth_prices, pk, cached))
        })
        .collect()
}

/// An account's weighted assets and liabilities on `accounts`, the bank and oracle accounts behind
/// `banks_and_oracles`.
fn health_components(
    account: &MarginfiAccountWrapper,
    group: &MarginfiGroup,
    clock: &Clock,
    banks_and_oracles: &[Pubkey],
    accounts: &mut [Account],
    requirement: RequirementType,
) -> Result<(I80F48, I80F48)> {
    let remaining_ais: Vec<_> = accounts
        .iter_mut()
        .zip(banks_and_oracles.iter())
        .map(|(account, pk)| (pk, account).into_account_info())
        .collect();
    get_health_components(
        &account.account,
        group,
        &remaining_ais,
        requirement,
        &mut None,
        HealthPriceMode::Client(clock.clone()),
        &mut None,
    )
    .map_err(|e| anyhow!("{e:?}"))
}

fn maintenance_to_equity_price(
    bank: &BankWrapper,
    oracle: &OracleWrapper,
    side: impl Fn() -> BalanceSide,
) -> Result<I80F48> {
    let equity = bank.price(oracle, side(), RequirementType::Equity)?;
    bank.price(oracle, side(), RequirementType::Maintenance)?
        .checked_div(equity)
        .ok_or_else(|| anyhow!("no equity price for the bank {}", bank.address))
}

/// Whether the bank's deposits cover its borrows. The program's `check_utilization_ratio` answers
/// the same question but logs every failing call, which for a bank in bad debt is once per holder
/// per evaluation cycle.
fn bank_covers_its_borrows(bank: &Bank) -> Result<bool> {
    let total_assets = bank
        .get_asset_amount(bank.total_asset_shares.into())
        .map_err(|e| anyhow!("Couldn't calculate the bank's total assets: {}", e))?;
    let total_liabilities = bank
        .get_liability_amount(bank.total_liability_shares.into())
        .map_err(|e| anyhow!("Couldn't calculate the bank's total liabilities: {}", e))?;
    Ok(total_assets >= total_liabilities)
}

/// What `end_liquidation` bounds a receivership by: USD values at equity prices, health at
/// maintenance weights.
struct RepayBounds {
    liab_value: I80F48,
    asset_value: I80F48,
    /// Equity assets minus equity liabilities; negative in bad debt.
    equity: I80F48,
    maintenance_health: I80F48,
    asset_weight_maint: I80F48,
    liab_weight_maint: I80F48,
    /// Maintenance price over equity price, per side: turns equity dollars into health dollars.
    asset_price_ratio: I80F48,
    liab_price_ratio: I80F48,
    above_closeout: bool,
}

impl RepayBounds {
    fn breakdown(&self, premium: I80F48) -> RepayCapBreakdown {
        let one_plus_premium = I80F48::ONE + premium;
        let debt = self.liab_value;
        let collateral = self.asset_value / one_plus_premium;
        let equity = (self.equity >= I80F48::ZERO && premium > I80F48::ZERO)
            .then_some(self.equity / premium);
        let health_gain = self.liab_weight_maint * self.liab_price_ratio
            - self.asset_weight_maint * one_plus_premium * self.asset_price_ratio;
        let health = (self.above_closeout && health_gain > I80F48::ZERO)
            .then_some(-self.maintenance_health / health_gain);
        let mut binding = ("debt", debt);
        if self.health_falls_too_far(premium, health_gain) {
            return RepayCapBreakdown {
                debt_usd: debt.to_num::<f64>(),
                collateral_usd: collateral.to_num::<f64>(),
                equity_usd: equity.map(|v| v.to_num::<f64>()),
                health_usd: health.map(|v| v.to_num::<f64>()),
                binding: "health_fall",
            };
        }
        for (name, cap) in [
            ("collateral", Some(collateral)),
            ("equity", equity),
            ("health", health),
        ] {
            if let Some(cap) = cap {
                if cap < binding.1 {
                    binding = (name, cap);
                }
            }
        }
        RepayCapBreakdown {
            debt_usd: debt.to_num::<f64>(),
            collateral_usd: collateral.to_num::<f64>(),
            equity_usd: equity.map(|v| v.to_num::<f64>()),
            health_usd: health.map(|v| v.to_num::<f64>()),
            binding: binding.0,
        }
    }

    /// Largest repayment (USD) `end_liquidation` accepts when seizing `repaid * (1 + premium)`:
    /// * the seizure fits the collateral and the repayment fits the debt;
    /// * an account that started solvent must stay solvent, i.e. the premium taken cannot exceed
    ///   its equity;
    /// * above the closeout threshold the account must stay unhealthy. Each equity dollar repaid
    ///   raises maintenance health by `l * rl - w * (1 + premium) * ra`; once that is no longer
    ///   positive the liquidation cannot heal the account and this bound no longer applies.
    fn max_repay(&self, premium: I80F48) -> I80F48 {
        let one_plus_premium = I80F48::ONE + premium;
        let mut max_repay = min(self.liab_value, self.asset_value / one_plus_premium);
        if self.equity >= I80F48::ZERO && premium > I80F48::ZERO {
            max_repay = min(max_repay, self.equity / premium);
        }
        let health_gain_per_repaid = self.liab_weight_maint * self.liab_price_ratio
            - self.asset_weight_maint * one_plus_premium * self.asset_price_ratio;
        if self.above_closeout && health_gain_per_repaid > I80F48::ZERO {
            max_repay = min(max_repay, -self.maintenance_health / health_gain_per_repaid);
        }
        // `end_liquidation` lets health fall by no more than what was extracted, the premium. A
        // seizure that lowers health by more per dollar repaid fails at any size.
        if self.health_falls_too_far(premium, health_gain_per_repaid) {
            return I80F48::ZERO;
        }
        I80F48::max(I80F48::ZERO, max_repay)
    }

    /// Whether each dollar repaid lowers maintenance health by more than the premium it extracts,
    /// with [`HEALTH_FALL_MARGIN`] to spare for price drift until the program measures it.
    fn health_falls_too_far(&self, premium: I80F48, health_gain_per_repaid: I80F48) -> bool {
        -health_gain_per_repaid > premium - HEALTH_FALL_MARGIN
    }
}

#[allow(clippy::large_enum_variant)]
enum Evaluation {
    Healthy(HealthyAccount),
    Liquidate(PreparedLiquidatableAccount),
    /// Unhealthy but not worth liquidating at the current premium.
    Unhealthy(UnhealthyAccount),
}

/// A healthy account: the watch that lets updates skip it while it stays so, and where it stands
/// for the hot list.
struct HealthyAccount {
    address: Pubkey,
    watch: Option<Watch>,
    /// What re-evaluates it as a hot account; `None` when it is not near the line.
    hot_watch: Option<Watch>,
    standing: Standing,
    /// The banks it holds.
    banks: Vec<Pubkey>,
}

struct UnhealthyAccount {
    /// The most a liquidation of it could earn (USD), for the hot list.
    potential_profit: f64,
    /// What re-evaluates it as a hot account.
    hot_watch: Option<Watch>,
    attempt_id: u64,
    address: Pubkey,
    observation_accounts: ObservationAccounts,
    /// Tagging would let the premium grow enough to make the liquidation worth it.
    taggable: bool,
    tagged_at: i64,
    maintenance_health: I80F48,
    asset_bank: Pubkey,
    liab_bank: Pubkey,
    max_repay: I80F48,
    profit: I80F48,
    premium: I80F48,
    caps: RepayCapBreakdown,
    balances: Balances,
}

impl Liquidator {
    pub fn new(
        config: Eva01Config,
        liquidator_account: Arc<LiquidatorAccount>,
        geyser_rx: Receiver<GeyserUpdate>,
        stop_liquidator: Arc<AtomicBool>,
        cache: Arc<Cache>,
    ) -> Result<Self> {
        // Shared by the liquidation plans and the rebalancer, so neither spends tokens the other
        // has claimed.
        let reservations = TokenReservations::new();
        let activity = Arc::new(LiquidationActivity::default());
        let rebalancer = Rebalancer::new(
            config.clone(),
            cache.clone(),
            reservations.clone(),
            activity.clone(),
        )?;
        let venues = rebalancer.liquidation_venues();
        let venue_limits = rebalancer.venue_limits();
        // Not the rebalancer's gate: a sweep must not hold a slot a liquidation quote is waiting on.
        let dex_gate = Arc::new(liquidation_dex_gate());

        let rpc_client =
            RpcClient::new_with_commitment(config.rpc_url.clone(), CommitmentConfig::confirmed());
        let pusher = if config.price_push_enabled {
            Some(Arc::new(PythPusher::new(&config, cache.clone())?))
        } else {
            None
        };
        let bundles = if let Some(pusher) = &pusher {
            Some(BundleLanding {
                pusher: pusher.clone(),
                jito: JitoClient::new(
                    config.jito_block_engine_url.clone(),
                    config.bundle_api_key.clone(),
                ),
                tip_estimator: TipEstimator::new(
                    config.jito_tip_max_lamports,
                    config.bundle_tip_profit_share,
                ),
                signer: Keypair::try_from(config.wallet_keypair.as_slice())?,
                sender: config
                    .helius_sender
                    .as_ref()
                    .map(|sender| HeliusSender::new(sender.url.clone(), sender.api_key.clone())),
                cu_price_micro_lamports: config.compute_unit_price_micro_lamports,
            })
        } else {
            None
        };
        let executor = Arc::new(Executor::new(
            rpc_client,
            cache.clone(),
            bundles,
            I80F48::from_num(config.wallet_min_profit_share),
        ));
        let queue = Arc::new(ExecutionQueue::new(ATTEMPT_COOLDOWN));

        let strategy = InventoryStrategy::new(
            liquidator_account.clone(),
            venues,
            dex_gate,
            config.slippage_bps,
            config.min_profit,
            reservations,
        )?;
        let tags = spawn_tagger(liquidator_account.clone())?;
        let settlement = executor.start_settlement(Arc::clone(&queue));

        let evaluator = Arc::new(Evaluator {
            cache: cache.clone(),
            pusher,
            min_profit: config.min_profit,
            max_profit: config.max_profit,
        });
        let handoff = Arc::new(Handoff::new());
        let hot_config = HotConfig::from_env();
        info!(
            "Hot path: mode {:?}, margin cutoff {}, at most {} account(s), {} thread(s)",
            hot_config.mode, hot_config.margin_cutoff, hot_config.max_accounts, hot_config.threads
        );
        let hot = Arc::new(Mutex::new(HotList::new(&hot_config, config.min_profit)));
        let hot_stats = Arc::new(Mutex::new(HotStats::default()));
        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(hot_config.threads)
            .thread_name(|index| format!("hot-path-{index}"))
            .build()?;
        let ingest = Ingest {
            cache: cache.clone(),
            queue: queue.clone(),
            liquidator_account: liquidator_account.clone(),
            evaluator: evaluator.clone(),
            geyser_rx,
            stop_liquidator: stop_liquidator.clone(),
            handoff: handoff.clone(),
            hot: hot.clone(),
            hot_mode: hot_config.mode,
            hot_stats: hot_stats.clone(),
            pool,
        };

        Ok(Liquidator {
            rebalancer: Some(rebalancer),
            executor,
            strategy: Arc::new(strategy),
            evaluator,
            queue,
            activity,
            threads: Vec::new(),
            settlement,
            tags,
            tag_policy: TagPolicy::new(),
            skip_reports: RateLimiter::new(SKIP_REPORT_INTERVAL),
            health_watch: HealthWatch::default(),
            sweep: Sweep::default(),
            scan_stats: ScanStats::new(),
            ingest: Some(ingest),
            ingest_thread: None,
            handoff,
            hot,
            hot_config,
            hot_stats,
            venue_limits,
            standings: HashMap::new(),
            stop_liquidator,
            cache,
        })
    }

    pub fn start(&mut self) -> Result<()> {
        let mut rebalancer = self
            .rebalancer
            .take()
            .ok_or_else(|| anyhow!("the Liquidator was already started"))?;
        if let Err(e) = rebalancer.refill_native_sol() {
            error!("Refilling the native SOL failed: {:?}", e);
        }
        if rebalancer.run()?.trades > 0 {
            self.executor.refresh_wallet_balances();
        }
        self.spawn_threads(rebalancer)?;

        // Updates only reach the cache through the update path, so everything Geyser and Hermes sent
        // since the cache was loaded (the startup rebalance alone takes seconds) is still queued.
        // Apply it first, or the initial scan prices every account on the load-time snapshot.
        let ingest = self
            .ingest
            .take()
            .ok_or_else(|| anyhow!("the Liquidator was already started"))?;
        let backlog = ingest.drain_backlog();

        // Geyser only delivers changes, so an account that is already underwater at startup would
        // wait for an unrelated update to touch one of its banks. Sweep them all once up front.
        match self
            .cache
            .marginfi_accounts
            .try_get_accounts_with_liabilities()
        {
            Ok(accounts) => {
                info!(
                    "Running the initial Liquidation scan (updates queued during startup applied: {} account(s) and {} bank(s) touched)...",
                    backlog.accounts.len(),
                    backlog.moved_banks.len()
                );
                self.run_liquidation_cycle(accounts, false, None);
            }
            Err(error) => error!(
                "Failed to collect accounts for the initial scan: {:?}",
                error
            ),
        }

        // From here the update path applies updates and checks the hot list as they arrive; this
        // thread is the background path, which takes what it applied and evaluates everyone else.
        self.ingest_thread = Some(thread::Builder::new().name("ingest".to_string()).spawn(
            move || {
                if let Err(e) = ingest.run() {
                    // A service that dies leaves the process up but deaf: exit, so the
                    // supervisor restarts eva with a fresh cache.
                    error!("The update path failed, exiting: {:?}", e);
                    std::process::exit(1);
                }
            },
        )?);

        info!("Staring the Liquidator loop.");
        while !self.stop_liquidator.load(Ordering::Relaxed) {
            debug!("Waiting for any data change...");
            let changes = self.handoff.take(Duration::from_secs(1));
            let oldest = changes.oldest;
            let accounts_to_check = self.accounts_to_evaluate(changes);
            if !accounts_to_check.is_empty() {
                self.run_liquidation_cycle(accounts_to_check, false, oldest);
            }
            let slice = self.sweep.next_slice(Instant::now(), || {
                self.cache
                    .marginfi_accounts
                    .try_get_accounts_with_liabilities()
                    .inspect_err(|e| error!("Failed to collect accounts for the sweep: {:?}", e))
                    .unwrap_or_default()
            });
            if !slice.is_empty() {
                self.run_liquidation_cycle(slice, true, None);
            }
            self.report_scan_stats();
        }

        // Workers first, so an attempt still sending can hand its bundle to the watcher.
        // The watcher then exits once its channel is dropped.
        info!("Stopping the liquidation workers and the rebalancer...");
        self.queue.stop();
        for thread in self.threads.drain(..) {
            if thread.join().is_err() {
                error!("A liquidation worker or the rebalancer panicked.");
            }
        }
        if let Some(thread) = self.ingest_thread.take() {
            if thread.join().is_err() {
                error!("The update path panicked.");
            }
        }
        self.executor.stop_settlement();
        for thread in self.settlement.drain(..) {
            if thread.join().is_err() {
                error!("A settlement watcher panicked.");
            }
        }
        info!("The Liquidator loop is stopped.");

        Ok(())
    }

    /// Starts the execution workers and the rebalancer. Both outlive the evaluation cycles, so a
    /// send waiting for confirmation or a sweep selling seized collateral never holds up the loop
    /// that applies Geyser updates.
    fn spawn_threads(&mut self, rebalancer: Rebalancer) -> Result<()> {
        // Nothing is ever sent: the channel disconnects once every worker holding a sender has
        // exited, which stops the rebalancer.
        let (workers_alive, workers_gone) = bounded::<()>(1);
        for index in 0..MAX_CONCURRENT_LIQUIDATIONS {
            let queue = Arc::clone(&self.queue);
            let evaluator = Arc::clone(&self.evaluator);
            let executor = Arc::clone(&self.executor);
            let strategy = Arc::clone(&self.strategy);
            let activity = Arc::clone(&self.activity);
            let alive = workers_alive.clone();
            self.threads.push(
                thread::Builder::new()
                    .name(format!("liquidation-worker-{index}"))
                    .spawn(move || {
                        let _alive = alive;
                        run_worker(&queue, &evaluator, &executor, &strategy, &activity)
                    })?,
            );
        }
        drop(workers_alive);

        let executor = Arc::clone(&self.executor);
        let activity = Arc::clone(&self.activity);
        self.threads.push(
            thread::Builder::new()
                .name("rebalancer".to_string())
                .spawn(move || run_rebalancer(rebalancer, &executor, &activity, &workers_gone))?,
        );
        Ok(())
    }

    /// The accounts a batch of updates requires evaluating: those changed outright, and those in a
    /// moved bank whose watch says the move may have reached their margin.
    fn accounts_to_evaluate(&mut self, changes: Changes) -> Vec<Pubkey> {
        let Changes {
            oldest: _,
            mut accounts,
            moved_banks,
        } = changes;
        if moved_banks.is_empty() {
            return accounts.into_iter().collect();
        }
        let clock = match clock_manager::get_clock(&self.cache.clock) {
            Ok(clock) => clock,
            Err(e) => {
                error!("Failed to read the clock for the health watch: {e}");
                for bank in &moved_banks {
                    let _ = self.add_accounts_for_bank(bank, &mut accounts);
                }
                return accounts.into_iter().collect();
            }
        };

        // Each moved bank's unit values now, once per batch. A bank that can't be priced has all
        // its accounts evaluated: the watch can't tell how far it moved.
        let mut current = HashMap::new();
        let mut priced = Vec::with_capacity(moved_banks.len());
        for bank in moved_banks {
            match (
                unit_value(&self.cache, &clock, &bank, false),
                unit_value(&self.cache, &clock, &bank, true),
            ) {
                (Some(asset), Some(liability)) => {
                    current.insert((bank, false), asset);
                    current.insert((bank, true), liability);
                    priced.push(bank);
                }
                _ => {
                    if let Err(e) = self.add_accounts_for_bank(&bank, &mut accounts) {
                        error!("Failed to collect the accounts of the bank {bank}: {e}");
                    }
                }
            }
        }
        for bank in priced {
            let holders = match self
                .cache
                .marginfi_accounts
                .try_get_accounts_for_bank_with_liabilities(&bank)
            {
                Ok(holders) => holders,
                Err(e) => {
                    error!("Failed to collect the accounts of the bank {bank}: {e}");
                    continue;
                }
            };
            for holder in holders {
                if accounts.contains(&holder) {
                    continue;
                }
                if self.health_watch.needs_evaluation(&holder, &current) {
                    accounts.insert(holder);
                } else {
                    self.scan_stats.skipped += 1;
                }
            }
        }
        accounts.into_iter().collect()
    }

    /// Evaluates `accounts_to_check`. A full sweep (`full`) also audits the health watch: an
    /// account it still watches hasn't needed evaluating since it was last healthy, so finding it
    /// unhealthy now means the watch missed it.
    ///
    /// `oldest` is when the oldest update behind this cycle arrived from the network. The accounts
    /// the cycle evaluates tell the hot list who belongs on it; the ones the update path evaluated
    /// since the cycle began are left to it, whose result is newer.
    fn run_liquidation_cycle(
        &mut self,
        accounts_to_check: Vec<Pubkey>,
        full: bool,
        oldest: Option<Instant>,
    ) {
        let started = Instant::now();
        if let Some(oldest) = oldest {
            self.scan_stats
                .waits
                .push(started.saturating_duration_since(oldest));
        }
        match self.evaluator.check_accounts(&accounts_to_check) {
            Ok((mut liquidations, unhealthy, healthy)) => {
                if full {
                    let missed: Vec<Pubkey> = liquidations
                        .iter()
                        .map(|intent| intent.liquidatee_account.address)
                        .chain(unhealthy.iter().map(|account| account.address))
                        .filter(|address| self.health_watch.is_watched(address))
                        .collect();
                    if !missed.is_empty() {
                        warn!(
                            "The health watch missed {} unhealthy account(s): {:?}",
                            missed.len(),
                            missed
                        );
                    }
                }
                // Only a healthy account with room to spare is watched; any other is evaluated on
                // every update of its banks.
                for account in &accounts_to_check {
                    self.health_watch.set(*account, None);
                }
                for account in &healthy {
                    self.health_watch
                        .set(account.address, account.watch.clone());
                }

                // The hot list: what this cycle found out about each account, except those the
                // update path has evaluated since the cycle began.
                let hot_checked: HashSet<Pubkey> = {
                    let hot = self.hot.lock().unwrap_or_else(|e| e.into_inner());
                    accounts_to_check
                        .iter()
                        .filter(|account| hot.checked_since(account, started))
                        .copied()
                        .collect()
                };
                self.compare_with_hot_path(&liquidations, started);
                self.update_hot_list(
                    &accounts_to_check,
                    &liquidations,
                    &unhealthy,
                    &healthy,
                    &hot_checked,
                );

                // With the update path acting, an account it has checked since this cycle began
                // keeps what it queued: this cycle's result for it is older.
                let skip_queue: HashSet<Pubkey> = if self.hot_config.mode == HotMode::On {
                    hot_checked
                } else {
                    HashSet::new()
                };
                let checked: Vec<Pubkey> = accounts_to_check
                    .iter()
                    .filter(|account| !skip_queue.contains(account))
                    .copied()
                    .collect();
                liquidations
                    .retain(|intent| !skip_queue.contains(&intent.liquidatee_account.address));
                let liquidatable = liquidations.len();
                // Which path found each liquidation travels with it to the end of its attempt.
                let path = if full {
                    DetectionPath::Sweep
                } else if oldest.is_none() {
                    DetectionPath::Startup
                } else {
                    DetectionPath::Background
                };
                let liquidations: Vec<PreparedLiquidatableAccount> = liquidations
                    .into_iter()
                    .map(|intent| {
                        let trace = AttemptTrace::new(path, oldest, intent.attempt_id);
                        intent.with_trace(trace)
                    })
                    .collect();
                if let Some(oldest) = oldest {
                    let queued_at = Instant::now();
                    for intent in &liquidations {
                        let latency = queued_at.saturating_duration_since(oldest);
                        self.scan_stats.liquidation_latencies.push(latency);
                        info!(
                            "BACKGROUND_FOUND attempt_id={} account={} arrival_to_queue_ms={} estimated_profit_usd={:.4}",
                            intent.attempt_id,
                            intent.liquidatee_account.address,
                            latency.as_millis(),
                            intent.profit.to_num::<f64>()
                        );
                    }
                }
                self.queue.offer(&checked, liquidations);
                self.handle_unhealthy(&accounts_to_check, &unhealthy);
                let elapsed = started.elapsed();
                let stats = &mut self.scan_stats;
                stats.evaluated += accounts_to_check.len();
                if full {
                    stats.sweeps += 1;
                    stats.slowest_sweep = stats.slowest_sweep.max(elapsed);
                } else {
                    stats.cycle_times.push(elapsed);
                    if let Some(oldest) = oldest {
                        stats.detections.push(oldest.elapsed());
                    }
                }
                debug!(
                    "Evaluated {} account(s) in {} ms: {} queued for liquidation, {} unhealthy but not worth liquidating.",
                    accounts_to_check.len(),
                    elapsed.as_millis(),
                    liquidatable,
                    unhealthy.len()
                );
            }
            Err(err) => {
                error!("Failed to evaluate accounts in liquidation: {}", err);
            }
        }
    }

    /// Reports the evaluation work done since the last report, once a minute.
    fn report_scan_stats(&mut self) {
        let stats = &self.scan_stats;
        if stats.since.elapsed() < SCAN_REPORT_INTERVAL {
            return;
        }
        let (median, p90, slowest) = stats.cycle_percentiles();
        info!(
            "Scans in the last {} s: {} cycle(s) on a change taking {} ms median, {} ms p90, {} ms at most; {} sweep slice(s), the slowest {} ms; {} evaluation(s), {} skipped by the health watch ({} account(s) watched).",
            stats.since.elapsed().as_secs(),
            stats.cycle_times.len(),
            median,
            p90,
            slowest,
            stats.sweeps,
            stats.slowest_sweep.as_millis(),
            stats.evaluated,
            stats.skipped,
            self.health_watch.len()
        );
        let (wait_median, wait_p90, wait_max) = percentiles(&stats.waits);
        let (detect_median, detect_p90, detect_max) = percentiles(&stats.detections);
        let (found_median, found_p90, found_max) = percentiles(&stats.liquidation_latencies);
        info!(
            "DETECTION_TIMING background path, {} batch(es): an update waited for its cycle {} ms median, {} ms p90, {} ms at most; arrival to every affected account evaluated {} ms median, {} ms p90, {} ms at most; {} liquidation(s) queued, arrival to queue {} ms median, {} ms p90, {} ms at most.",
            stats.detections.len(),
            wait_median,
            wait_p90,
            wait_max,
            detect_median,
            detect_p90,
            detect_max,
            stats.liquidation_latencies.len(),
            found_median,
            found_p90,
            found_max
        );
        self.scan_stats = ScanStats::new();
        self.report_hot_path();
        for line in trace::take_summary() {
            info!("{line}");
        }
        if let Some(line) = self.venue_limits.take_report() {
            info!("{line}");
        }
    }

    /// The update path's minute: how big the hot list is, how long its checks took, how much earlier
    /// than the background path it found liquidations, and what only the background path found. And
    /// the sizing of the whole book by margin, to choose the cutoff.
    fn report_hot_path(&mut self) {
        let now = Instant::now();
        let (batches, evaluated, check_times, arrivals, found, leads, misses) = {
            let mut stats = self.hot_stats.lock().unwrap_or_else(|e| e.into_inner());
            stats
                .found_at
                .retain(|_, at| now.saturating_duration_since(*at) < HOT_EVENT_WINDOW * 2);
            stats
                .background_seen
                .retain(|_, at| now.saturating_duration_since(*at) < HOT_EVENT_WINDOW * 2);
            (
                std::mem::take(&mut stats.batches),
                std::mem::take(&mut stats.evaluated),
                std::mem::take(&mut stats.check_times),
                std::mem::take(&mut stats.arrival_to_queue),
                std::mem::take(&mut stats.found),
                std::mem::take(&mut stats.leads),
                std::mem::take(&mut stats.misses),
            )
        };
        let (len, range, overflowed) = {
            let mut hot = self.hot.lock().unwrap_or_else(|e| e.into_inner());
            (hot.len(), hot.margin_range(), hot.take_overflowed())
        };
        let (check_median, check_p90, check_max) = percentiles(&check_times);
        let (arrival_median, arrival_p90, arrival_max) = percentiles(&arrivals);
        let (lead_median, lead_p90, lead_max) = percentiles(&leads);
        info!(
            "HOT_PATH mode={:?} list={} margin(min,median,max)={:?} overflowed={}; {} batch(es) checked {} account(s) in {} ms median, {} ms p90, {} ms at most; {} liquidation(s) found, arrival to queue {} ms median, {} ms p90, {} ms at most; ahead of the background path by {} ms median, {} ms p90, {} ms at most ({} compared); {} found only by the background path.",
            self.hot_config.mode,
            len,
            range.map(|(low, median, high)| (
                (low * 1e4).round() / 1e4,
                (median * 1e4).round() / 1e4,
                (high * 1e4).round() / 1e4
            )),
            overflowed,
            batches,
            evaluated,
            check_median,
            check_p90,
            check_max,
            found,
            arrival_median,
            arrival_p90,
            arrival_max,
            lead_median,
            lead_p90,
            lead_max,
            leads.len(),
            misses
        );

        // Where the whole book stands, for choosing the margin cutoff and the list size.
        let (buckets, could_pay) = sizing_buckets(&self.standings, self.evaluator.min_profit);
        info!(
            "HOT_SIZING {} account(s) evaluated, {} could earn the minimum; by margin (<0, <0.5%, <1%, <2%, <4%, <8%, <16%, >=16%) those that could: {:?}; cutoff {}.",
            self.standings.len(),
            could_pay,
            buckets,
            self.hot_config.margin_cutoff
        );
    }

    /// Compares the liquidations this cycle found with what the update path found: how much earlier
    /// the update path was, and the ones only this cycle found.
    fn compare_with_hot_path(
        &self,
        liquidations: &[PreparedLiquidatableAccount],
        started: Instant,
    ) {
        if self.hot_config.mode == HotMode::Off || liquidations.is_empty() {
            return;
        }
        let now = Instant::now();
        let hot = self.hot.lock().unwrap_or_else(|e| e.into_inner());
        let mut stats = self.hot_stats.lock().unwrap_or_else(|e| e.into_inner());
        for intent in liquidations {
            let address = intent.liquidatee_account.address;
            // Seen by this path a moment ago: the same event, not a new one.
            let repeat = stats
                .background_seen
                .get(&address)
                .is_some_and(|at| now.saturating_duration_since(*at) < HOT_EVENT_WINDOW);
            stats.background_seen.insert(address, now);
            if repeat {
                continue;
            }
            match stats.found_at.get(&address).copied() {
                Some(found) if now.saturating_duration_since(found) < HOT_EVENT_WINDOW => {
                    let lead = now.saturating_duration_since(found);
                    stats.leads.push(lead);
                    info!(
                        "HOT_LEAD account={} hot_path_ahead_ms={}",
                        address,
                        lead.as_millis()
                    );
                }
                _ => {
                    stats.misses += 1;
                    warn!(
                        "HOT_MISS account={} on_list_when_the_cycle_began={} mode={:?}",
                        address,
                        hot.was_hot_at(&address, started),
                        self.hot_config.mode
                    );
                }
            }
        }
    }

    /// Records what a cycle found out about each account it evaluated: in the hot list (except the
    /// accounts in `skip`, whose newer result the update path has), and in the standings that size
    /// the book. An account it could not evaluate at all leaves both.
    fn update_hot_list(
        &mut self,
        checked: &[Pubkey],
        liquidations: &[PreparedLiquidatableAccount],
        unhealthy: &[UnhealthyAccount],
        healthy: &[HealthyAccount],
        skip: &HashSet<Pubkey>,
    ) {
        let answered: HashSet<Pubkey> = liquidations
            .iter()
            .map(|intent| intent.liquidatee_account.address)
            .chain(unhealthy.iter().map(|account| account.address))
            .chain(healthy.iter().map(|account| account.address))
            .collect();
        if self.hot_config.mode != HotMode::Off {
            let mut hot = self.hot.lock().unwrap_or_else(|e| e.into_inner());
            for account in checked {
                if !answered.contains(account) && !skip.contains(account) {
                    hot.remove(account);
                }
            }
            observe_results(
                &mut hot,
                liquidations,
                unhealthy,
                healthy,
                Instant::now(),
                None,
                skip,
            );
        }
        for account in checked {
            if !answered.contains(account) {
                self.standings.remove(account);
            }
        }
        for intent in liquidations {
            self.standings.insert(
                intent.liquidatee_account.address,
                Standing {
                    margin: -1.0,
                    potential_profit: intent.profit.to_num::<f64>(),
                },
            );
        }
        for account in unhealthy {
            self.standings.insert(
                account.address,
                Standing {
                    margin: -1.0,
                    potential_profit: account.potential_profit,
                },
            );
        }
        for account in healthy {
            self.standings.insert(account.address, account.standing);
        }
    }

    fn add_accounts_for_bank(
        &self,
        bank: &Pubkey,
        account_addresses: &mut HashSet<Pubkey>,
    ) -> Result<()> {
        account_addresses.extend(
            self.cache
                .marginfi_accounts
                .try_get_accounts_for_bank_with_liabilities(bank)?,
        );
        Ok(())
    }

    /// Reports the unhealthy accounts we are not liquidating and tags the ones the tag policy says
    /// are due.
    fn handle_unhealthy(&mut self, checked: &[Pubkey], unhealthy: &[UnhealthyAccount]) {
        let now = Instant::now();
        self.skip_reports.prune(now);
        for account in unhealthy {
            if self.skip_reports.allow(account.address, now) {
                self.report_skipped(account);
            }
        }

        let candidates = unhealthy.iter().map(|account| Candidate {
            address: account.address,
            taggable: account.taggable,
            balances: &account.balances,
        });
        let due = self.tag_policy.observe(checked, candidates, now);
        if due.is_empty() {
            return;
        }
        let by_address: HashMap<Pubkey, &UnhealthyAccount> =
            unhealthy.iter().map(|a| (a.address, a)).collect();
        for (address, waited) in due {
            let tag = DueTag {
                address,
                observation_accounts: by_address[&address].observation_accounts.clone(),
                waited,
            };
            if self.tags.send(tag).is_err() {
                error!("Failed to tag {address}: the tagging thread is gone");
            }
        }
    }

    fn report_skipped(&self, account: &UnhealthyAccount) {
        info!(
            "LIQUIDATION_SIZING attempt_id={} outcome=below_min_profit account={} asset_bank={} liab_bank={} maintenance_health_usd={:.6} max_repay_usd={:.6} planned_repay_usd={:.6} estimated_profit_usd={:.6} premium_pct={:.4} binding_cap={} cap_debt_usd={:.4} cap_collateral_usd={:.4} cap_equity_usd={:?} cap_health_usd={:?} min_profit_usd={:.4} tagged_at={} taggable={}",
            account.attempt_id,
            account.address,
            account.asset_bank,
            account.liab_bank,
            account.maintenance_health.to_num::<f64>(),
            account.max_repay.to_num::<f64>(),
            (account.max_repay * LIQUIDATION_SIZE_HAIRCUT).to_num::<f64>(),
            account.profit.to_num::<f64>(),
            account.premium.to_num::<f64>() * 100.0,
            account.caps.binding,
            account.caps.debt_usd,
            account.caps.collateral_usd,
            account.caps.equity_usd,
            account.caps.health_usd,
            self.evaluator.min_profit,
            account.tagged_at,
            account.taggable
        );
    }
}

/// Takes intents off the queue and lands them until the queue stops.
fn run_worker(
    queue: &ExecutionQueue,
    evaluator: &Evaluator,
    executor: &Executor,
    strategy: &InventoryStrategy,
    activity: &LiquidationActivity,
) {
    while let Some(queued) = queue.take() {
        let liquidatee = queued.intent.liquidatee_account.address;
        let attempt_id = queued.intent.attempt_id;
        // A panic would otherwise end the worker and leave the account marked in flight for good.
        let attempt = panic::catch_unwind(AssertUnwindSafe(|| {
            attempt_liquidation(queued, queue, evaluator, executor, strategy, activity)
        }));
        // A panic cools down too, so it can't repeat in a tight loop.
        let release = attempt.unwrap_or_else(|_| {
            error!("The liquidation attempt on {} panicked.", liquidatee);
            AttemptRelease::Finished { landed_slot: None }
        });
        let release = match release {
            AttemptRelease::Finished { landed_slot: None } => AttemptRelease::Finished {
                landed_slot: executor.landed_slot(attempt_id),
            },
            other => other,
        };
        apply_attempt_release(queue, liquidatee, release);
    }
}

/// How the queue should treat the account. `WatcherOwns` means the bundle was accepted and the
/// settlement thread releases it.
fn attempt_liquidation(
    QueuedIntent { intent, queued_at }: QueuedIntent,
    queue: &ExecutionQueue,
    evaluator: &Evaluator,
    executor: &Executor,
    strategy: &InventoryStrategy,
    activity: &LiquidationActivity,
) -> AttemptRelease {
    let liquidatee = intent.liquidatee_account.address;
    intent.trace.mark(Stage::Taken);
    // The intent was sized on the state at evaluation time. By now another liquidator may have
    // taken a chunk of the account or prices may have moved, so it is re-sized on the current
    // state and dropped if it is no longer worth it, instead of sending a transaction the program
    // would reject.
    // One id for the whole attempt: the re-evaluation sizes it afresh but it is the same attempt.
    let fresh = evaluator.re_evaluate(&liquidatee).map(|fresh| {
        fresh.map(|fresh| {
            fresh
                .for_attempt(intent.attempt_id)
                .with_trace(intent.trace.clone())
        })
    });
    intent.trace.mark(Stage::Evaluated);
    match fresh {
        Ok(Some(fresh)) if executor.is_on_hold(&fresh).unwrap_or(false) => {
            info!(
                "LIQUIDATION_ATTEMPT_END attempt_id={} account={} outcome=skipped reason=on_hold",
                fresh.attempt_id, liquidatee
            );
            intent
                .trace
                .report(intent.attempt_id, &liquidatee, "on_hold");
        }
        Ok(Some(fresh)) => {
            // The sandwich takes its own pair when the main one seizes from Kamino or JupLend.
            let sandwich = fresh.sandwich_pair.as_ref().map_or_else(
                || "same".to_string(),
                |pair| {
                    format!(
                        "{}->{} profit_usd={:.4}",
                        pair.asset_bank,
                        pair.liab_bank,
                        pair.profit.to_num::<f64>()
                    )
                },
            );
            info!("LIQUIDATION_QUEUE attempt_id={} path={} account={} queue_delay_ms={} asset_bank={} liab_bank={} estimated_repay_usd={:.4} estimated_profit_usd={:.4} binding_cap={} push_feed_count={} sandwich_pair={}",
                fresh.attempt_id, fresh.trace.path.as_str(), liquidatee, queued_at.elapsed().as_millis(), fresh.asset_bank,
                fresh.liab_bank, fresh.estimated_repay_usd, fresh.profit.to_num::<f64>(), fresh.binding_cap, fresh.push.as_ref().map_or(0, |push| push.feeds.len()), sandwich);
            queue.resized(&liquidatee, fresh.profit);
            let mut attempt = activity.attempt(liquidatee);
            match executor.try_execute(strategy, &fresh) {
                // An accepted bundle is a liquidation attempt that went out: it counts as activity
                // (the rebalancer stays out of the way) and not as a failed retry, whatever the
                // watcher later finds.
                Ok(AttemptOutcome::Settling) => {
                    attempt.succeeded();
                    return AttemptRelease::WatcherOwns;
                }
                Ok(AttemptOutcome::Finished) => attempt.succeeded(),
                Err(e) => {
                    error!(
                        "LIQUIDATION_EXECUTION_FAILURE attempt_id={} account={} error={:#}",
                        fresh.attempt_id, liquidatee, e
                    );
                    return if e.downcast_ref::<LostRace>().is_some() {
                        AttemptRelease::LostRace
                    } else {
                        AttemptRelease::Finished { landed_slot: None }
                    };
                }
            }
        }
        Ok(None) => {
            info!(
                "LIQUIDATION_ATTEMPT_END attempt_id={} account={} outcome=no_longer_liquidatable elapsed_since_queue_ms={}",
                intent.attempt_id, liquidatee, queued_at.elapsed().as_millis()
            );
            intent
                .trace
                .report(intent.attempt_id, &liquidatee, "no_longer_liquidatable");
        }
        Err(e) => {
            error!(
                "LIQUIDATION_REEVALUATION_FAILURE attempt_id={} account={} error={:#}",
                intent.attempt_id, liquidatee, e
            );
            intent
                .trace
                .report(intent.attempt_id, &liquidatee, "reevaluation_failed");
        }
    }
    AttemptRelease::Finished { landed_slot: None }
}

/// Sells leftover tokens back to the swap mint and refills the inventory whenever a pass is due
/// (see `LiquidationActivity`), until every worker is gone.
fn run_rebalancer(
    mut rebalancer: Rebalancer,
    executor: &Executor,
    activity: &LiquidationActivity,
    workers_gone: &Receiver<()>,
) {
    // The startup pass just ran.
    let mut last_pass = Instant::now();
    while let Err(RecvTimeoutError::Timeout) = workers_gone.recv_timeout(REBALANCE_CHECK_INTERVAL) {
        // Whatever the liquidations are doing: out of SOL, none of them lands.
        match panic::catch_unwind(AssertUnwindSafe(|| rebalancer.refill_native_sol())) {
            // The claim is released once the balances show what the refill spent.
            Ok(Ok(Some(claim))) => {
                executor.refresh_wallet_balances();
                drop(claim);
            }
            Ok(Ok(None)) => {}
            Ok(Err(error)) => error!("Refilling the native SOL failed: {:?}", error),
            Err(_) => error!("Refilling the native SOL panicked"),
        }
        if !activity.rebalance_due(last_pass, Instant::now()) {
            continue;
        }
        last_pass = Instant::now();
        match panic::catch_unwind(AssertUnwindSafe(|| rebalancer.run())) {
            // Liquidations size from the cached balances; the sold tokens stay claimed until the
            // balances show them gone.
            Ok(Ok(pass)) => {
                if pass.trades > 0 {
                    executor.refresh_wallet_balances();
                }
                drop(pass.claim);
            }
            Ok(Err(error)) => error!("Rebalancing failed: {:?}", error),
            Err(_) => {
                error!("Rebalancing panicked.");
                executor.refresh_wallet_balances();
            }
        }
    }
}

impl Evaluator {
    /// A healthy account's watch: its margin and its positions' unit values now. `None` when a
    /// unit value can't be priced, so every update of its banks evaluates it.
    fn watch_for(
        &self,
        account: &MarginfiAccountWrapper,
        unit_values: &UnitValues,
        margin: f64,
    ) -> Option<Watch> {
        Watch::new(margin, self.positions_for(account, unit_values)?)
    }

    /// The account's positions and their unit values now. `None` when one can't be priced.
    fn positions_for(
        &self,
        account: &MarginfiAccountWrapper,
        unit_values: &UnitValues,
    ) -> Option<Vec<WatchedPosition>> {
        let (deposits, liabilities) = account.get_deposits_and_liabilities_shares();
        deposits
            .iter()
            .map(|(_, bank)| (*bank, false))
            .chain(liabilities.iter().map(|(_, bank)| (*bank, true)))
            .map(|(bank, liability)| {
                Some(WatchedPosition {
                    bank,
                    liability,
                    unit_value: unit_values.get(&bank, liability)?,
                })
            })
            .collect()
    }

    /// The watch a hot account is re-evaluated on: a much tighter one than a healthy account's, so
    /// that a price move that could bring it to the line is always seen, and one that could not is
    /// not (a bank's share value moves on every interaction with it). `margin` is negative for an
    /// account already past the line. `None` when a position can't be priced, or the account is not
    /// near enough the line to be hot.
    fn hot_watch_for(
        &self,
        account: &MarginfiAccountWrapper,
        unit_values: &UnitValues,
        margin: f64,
    ) -> Option<Watch> {
        if margin >= HOT_WATCH_MARGIN_LIMIT {
            return None;
        }
        let tolerance = if margin < 0.0 {
            HOT_UNHEALTHY_TOLERANCE
        } else {
            margin.max(HOT_MIN_MARGIN) * HOT_TOLERANCE_SHARE
        };
        Some(Watch::with_tolerance(
            tolerance,
            self.positions_for(account, unit_values)?,
        ))
    }

    fn check_accounts(&self, account_addresses: &[Pubkey]) -> Result<Evaluations> {
        let clock = clock_manager::get_clock(&self.cache.clock)?;
        let unit_values = UnitValues::new(&self.cache, &clock);

        // Evaluation is CPU-bound (oracle parsing + health math) and independent per account, so a
        // full scan of every borrower parallelises cleanly across cores.
        let evaluations: Vec<Evaluation> = account_addresses
            .par_iter()
            .filter_map(|account_address| {
                let account = self
                    .cache
                    .marginfi_accounts
                    .try_get_account(account_address)
                    .ok()?;

                match self.process_account(&account, clock.clone(), &unit_values) {
                    Ok(acc_opt) => acc_opt,
                    Err(err) => {
                        debug!("Failed to process account {:?}: {:?}", account.address, err);
                        None
                    }
                }
            })
            .collect();

        let mut liquidations = Vec::new();
        let mut unhealthy = Vec::new();
        let mut healthy = Vec::new();
        for evaluation in evaluations {
            match evaluation {
                Evaluation::Healthy(account) => healthy.push(account),
                Evaluation::Liquidate(acc) => liquidations.push(acc),
                Evaluation::Unhealthy(acc) => unhealthy.push(acc),
            }
        }
        Ok((liquidations, unhealthy, healthy))
    }

    /// Sizes the liquidation of `address` on the current cache state; `None` when it is no longer
    /// worth liquidating.
    fn re_evaluate(&self, address: &Pubkey) -> Result<Option<PreparedLiquidatableAccount>> {
        let account = self.cache.marginfi_accounts.try_get_account(address)?;
        let clock = clock_manager::get_clock(&self.cache.clock)?;
        let unit_values = UnitValues::new(&self.cache, &clock);
        Ok(
            match self.process_account(&account, clock.clone(), &unit_values)? {
                Some(Evaluation::Liquidate(intent)) => Some(intent),
                _ => None,
            },
        )
    }

    fn process_account(
        &self,
        account: &MarginfiAccountWrapper,
        clock: Clock,
        unit_values: &UnitValues,
    ) -> Result<Option<Evaluation>> {
        let (deposit_shares, liab_shares) = account.get_deposits_and_liabilities_shares();
        if deposit_shares.is_empty() || liab_shares.is_empty() {
            return Ok(None);
        }
        // The program can't price such an account at all, so nobody can liquidate it.
        let active = MarginfiAccountWrapper::get_active_banks(&account.account.lending_account);
        if active.iter().any(|bank| {
            self.cache
                .banks
                .try_get_bank(bank)
                .is_ok_and(|bank| stake_pool_is_empty(&self.cache, &bank.bank))
        }) {
            return Ok(None);
        }

        let deposit_values = self.get_value_of_shares(
            &clock,
            deposit_shares,
            &BalanceSide::Assets,
            RequirementType::Maintenance,
        )?;

        let liab_values = self.get_value_of_shares(
            &clock,
            liab_shares,
            &BalanceSide::Liabilities,
            RequirementType::Maintenance,
        )?;

        // The most a liquidation of this account could ever earn, for the hot list.
        let potential_profit = self.potential_profit_usd(
            liab_values
                .iter()
                .map(|(value, _)| value.to_num::<f64>())
                .sum(),
            deposit_values
                .iter()
                .map(|(value, _)| value.to_num::<f64>())
                .sum(),
        );

        let (asset_banks, liab_banks) =
            match self.find_liquidation_bank_candidates(deposit_values, liab_values)? {
                Some(banks) => banks,
                None => return Ok(None),
            };

        let banks_to_include: Vec<Pubkey> = vec![];
        let banks_to_exclude: Vec<Pubkey> = vec![];
        let observation_accounts = MarginfiAccountWrapper::get_observation_accounts::<OracleWrapper>(
            &account.account.lending_account,
            &banks_to_include,
            &banks_to_exclude,
            &self.cache,
            &clock,
        )?;

        // Calculated max liquidatable amount is the defining factor for liquidation.
        let sizing = match self.compute_max_liquidatable_amounts_with_banks(
            &self.cache,
            clock.clone(),
            account,
            &self.cache.marginfi_group,
            observation_accounts.observation_accounts.as_slice(),
            &asset_banks,
            &liab_banks,
        )? {
            PairSearch::Sized(sizing) => sizing,
            PairSearch::Healthy(margin) => {
                let watch = self.watch_for(account, unit_values, margin);
                let hot_watch = self.hot_watch_for(account, unit_values, margin);
                return Ok(Some(Evaluation::Healthy(HealthyAccount {
                    address: account.address,
                    watch,
                    hot_watch,
                    standing: Standing {
                        margin,
                        potential_profit,
                    },
                    banks: active,
                })));
            }
            PairSearch::None => return Ok(None),
        };
        Ok(match self.prepare(account, observation_accounts, sizing)? {
            Some(Evaluation::Unhealthy(unhealthy)) => {
                Some(Evaluation::Unhealthy(UnhealthyAccount {
                    potential_profit,
                    hot_watch: self.hot_watch_for(account, unit_values, -1.0),
                    ..unhealthy
                }))
            }
            other => other,
        })
    }

    /// The most a liquidation of an account with `debt_usd` of debt and `collateral_usd` of
    /// collateral (at maintenance prices) could earn: what it repays can't exceed the debt or the
    /// collateral less its premium, and the premium is at most the largest one the program or a tag
    /// allows. Liquidations are valued at equity prices, which can differ from the maintenance ones
    /// by a few percent, so the bound leaves room for it: it is an upper bound.
    fn potential_profit_usd(&self, debt_usd: f64, collateral_usd: f64) -> f64 {
        let premium =
            I80F48::max(TAG_MAX_PREMIUM, self.cache.liquidation_max_fee()).to_num::<f64>();
        let repay = debt_usd.min(collateral_usd / (1.0 + premium)) * PRICE_SLACK;
        repay * premium * LIQUIDATION_SIZE_HAIRCUT.to_num::<f64>()
    }

    /// The liquidation `sizing` allows, or the unhealthy account it leaves when none is worth it.
    fn prepare(
        &self,
        account: &MarginfiAccountWrapper,
        observation_accounts: ObservationAccounts,
        sizing: Sizing,
    ) -> Result<Option<Evaluation>> {
        let Sizing {
            asset_bank: asset_bank_pk,
            liab_bank: liab_bank_pk,
            amounts: liquidation_amounts,
            sandwich,
            maintenance_health,
            push,
            without_push,
        } = sizing;

        let attempt_id = NEXT_LIQUIDATION_ATTEMPT_ID.fetch_add(1, Ordering::Relaxed);

        if !liquidation_amounts.is_liquidatable() {
            return Ok(Some(Evaluation::Unhealthy(UnhealthyAccount {
                potential_profit: 0.0,
                hot_watch: None,
                attempt_id,
                address: account.address,
                observation_accounts,
                taggable: liquidation_amounts.taggable,
                tagged_at: account.account.liquidation_tagged_at,
                maintenance_health,
                asset_bank: asset_bank_pk,
                liab_bank: liab_bank_pk,
                max_repay: liquidation_amounts.max_repay,
                profit: liquidation_amounts.liquidator_profit * LIQUIDATION_SIZE_HAIRCUT,
                premium: liquidation_amounts.premium,
                caps: liquidation_amounts.caps,
                balances: account.get_deposits_and_liabilities_shares(),
            })));
        }

        let profit = liquidation_amounts.liquidator_profit * LIQUIDATION_SIZE_HAIRCUT;
        if self
            .max_profit
            .is_some_and(|max| profit > I80F48::from_num(max))
        {
            // The smaller liquidation the chain already allows may still be within the cap.
            if let Some(unpushed) = without_push {
                return self.prepare(account, observation_accounts, *unpushed);
            }
            debug!(
                "Account {}: ${:.4} profit is above this instance's ${:?} cap; left to another instance",
                account.address,
                profit.to_num::<f64>(),
                self.max_profit
            );
            return Ok(None);
        }

        let slippage_adjusted_asset_amount =
            liquidation_amounts.max_liquidatable_asset_amount * LIQUIDATION_SIZE_HAIRCUT;
        let slippage_adjusted_liab_amount =
            liquidation_amounts.max_liquidatable_liab_amount * LIQUIDATION_SIZE_HAIRCUT;

        debug!(
                "asset_amount_to_liquidate: {:?}, slippage_adjusted_asset_amount: {:?}, slippage_adjusted_liab_amount: {:?}",
                liquidation_amounts.max_liquidatable_asset_amount, slippage_adjusted_asset_amount, slippage_adjusted_liab_amount
            );

        // Sized like the main pair; a pair the program won't let liquidate is no alternative.
        let sandwich_pair = sandwich
            .filter(|(_, _, amounts)| amounts.is_liquidatable())
            .map(|(asset_bank, liab_bank, amounts)| LiquidationPair {
                asset_bank,
                liab_bank,
                asset_amount: amounts.max_liquidatable_asset_amount * LIQUIDATION_SIZE_HAIRCUT,
                liab_amount: amounts.max_liquidatable_liab_amount * LIQUIDATION_SIZE_HAIRCUT,
                profit: amounts.liquidator_profit * LIQUIDATION_SIZE_HAIRCUT,
            });

        let without_push = match without_push {
            Some(sizing) => match self.prepare(account, observation_accounts.clone(), *sizing)? {
                Some(Evaluation::Liquidate(intent)) => Some(Box::new(intent)),
                _ => None,
            },
            None => None,
        };

        Ok(Some(Evaluation::Liquidate(PreparedLiquidatableAccount {
            attempt_id,
            liquidatee_account: account.clone(),
            observation_accounts,
            asset_bank: asset_bank_pk,
            liab_bank: liab_bank_pk,
            asset_amount: slippage_adjusted_asset_amount,
            liab_amount: slippage_adjusted_liab_amount,
            profit,
            estimated_repay_usd: (liquidation_amounts.max_repay * LIQUIDATION_SIZE_HAIRCUT)
                .to_num::<f64>(),
            binding_cap: liquidation_amounts.caps.binding,
            push,
            without_push,
            sandwich_pair,
            trace: AttemptTrace::untraced(),
        })))
    }

    fn find_liquidation_bank_candidates(
        &self,
        deposit_values: Vec<(I80F48, Pubkey)>,
        liab_values: Vec<(I80F48, Pubkey)>,
    ) -> Result<Option<(Vec<Pubkey>, Vec<Pubkey>)>> {
        if deposit_values.is_empty() || liab_values.is_empty() {
            debug!(
                "No bank candidates: {} priced deposit(s), {} priced liability(-ies)",
                deposit_values.len(),
                liab_values.len()
            );
            return Ok(None);
        }

        let total_deposit_value = deposit_values
            .iter()
            .map(|(v, _)| v.to_num::<f64>())
            .sum::<f64>();
        if total_deposit_value < BANKRUPT_THRESHOLD {
            debug!(
                "No bank candidates: deposits worth ${total_deposit_value:.6} are below the \
                 bankruptcy threshold ${BANKRUPT_THRESHOLD}"
            );
            return Ok(None);
        }

        // Every deposit we can seize and every liability we can buy, so an unsupported or excluded
        // position doesn't block an otherwise liquidatable account.
        let usable = |bank_pk: &Pubkey, collateral: bool| {
            self.cache.banks.try_get_bank(bank_pk).is_ok_and(|bank| {
                !self.cache.mints.is_excluded(&bank.bank.mint)
                    && (!collateral || is_seizable_asset_tag(bank.bank.config.asset_tag))
            })
        };
        let asset_banks: Vec<Pubkey> = deposit_values
            .iter()
            .filter(|(_, bank_pk)| usable(bank_pk, true))
            .map(|(_, bank_pk)| *bank_pk)
            .collect();
        let liab_banks: Vec<Pubkey> = liab_values
            .iter()
            .filter(|(_, bank_pk)| usable(bank_pk, false))
            .map(|(_, bank_pk)| *bank_pk)
            .collect();

        match (asset_banks.is_empty(), liab_banks.is_empty()) {
            (false, false) => Ok(Some((asset_banks, liab_banks))),
            _ => {
                debug!(
                    "No bank candidates: no seizable deposit or no buyable liability \
                     (unsupported asset tag or excluded mint)"
                );
                Ok(None)
            }
        }
    }

    /// Sizes the account's pairs on the prices the program will read. Where eva prices Pyth feeds
    /// ahead of the chain, a liquidation goes behind a push of the few that lower the account's
    /// health the most, sized on exactly the prices the push posts with every other feed at the
    /// chain's price, unless the chain's prices already allow one earning nearly as much.
    #[allow(clippy::too_many_arguments)]
    fn compute_max_liquidatable_amounts_with_banks(
        &self,
        cache: &Cache,
        clock: Clock,
        account: &MarginfiAccountWrapper,
        group: &MarginfiGroup,
        banks_and_oracles: &[Pubkey],
        asset_banks: &[Pubkey],
        liab_banks: &[Pubkey],
    ) -> Result<PairSearch> {
        let size = |pricing: Pricing| {
            self.size_on_accounts(
                cache,
                clock.clone(),
                account,
                group,
                banks_and_oracles,
                pricing,
                asset_banks,
                liab_banks,
            )
        };
        let search = size(Pricing::Newest)?;
        let near_threshold = match &search {
            PairSearch::Healthy(margin) => *margin < CHAIN_CHECK_MARGIN,
            PairSearch::Sized(_) => true,
            PairSearch::None => false,
        };
        if !near_threshold {
            return Ok(search);
        }
        let mut behind: Vec<(Pubkey, [u8; 32])> = Vec::new();
        for oracle in banks_and_oracles {
            if let Some(feed_id) = pushable_feed(cache, oracle) {
                if !behind.iter().any(|(_, known)| *known == feed_id) {
                    behind.push((*oracle, feed_id));
                }
            }
        }
        if behind.is_empty() {
            return Ok(search);
        }
        match search {
            PairSearch::Sized(sizing) if sizing.amounts.is_liquidatable() => {}
            // Not liquidatable on eva's prices, but the chain may still hold older ones the
            // account fails on: a price that recovered on Hermes before the chain caught up.
            search => {
                return Ok(match size(Pricing::Chain) {
                    Ok(PairSearch::Sized(on_chain)) if on_chain.amounts.is_liquidatable() => {
                        PairSearch::Sized(on_chain)
                    }
                    _ => search,
                })
            }
        }

        let health = |pricing: Pricing| -> Result<I80F48> {
            let mut accounts = program_accounts(cache, banks_and_oracles, pricing)?;
            let (assets, liabilities) = health_components(
                account,
                group,
                &cache.clock_not_before(&clock),
                banks_and_oracles,
                &mut accounts,
                RequirementType::Maintenance,
            )?;
            Ok(assets - liabilities)
        };
        let on_chain = match health(Pricing::Chain) {
            Ok(health) => health,
            // The chain's price is unusable as it stands (too old for the program, say): only
            // pushing every feed can make this liquidation pass.
            Err(e) => {
                let push = self.pusher.as_ref().and_then(|pusher| {
                    cache
                        .pyth_prices
                        .push_of(&behind)
                        .filter(|push| pusher.check(push).is_ok())
                });
                debug!(
                    "Account {}: can't price it on the chain's Pyth prices ({e}); {}",
                    account.address,
                    if push.is_some() {
                        "pushing every feed"
                    } else {
                        "one bundle can't push every feed"
                    }
                );
                let Some(push) = push else {
                    return Ok(PairSearch::None);
                };
                return Ok(
                    match size(Pricing::Pushed(&push)).unwrap_or(PairSearch::None) {
                        PairSearch::Sized(sizing) => PairSearch::Sized(Sizing {
                            push: Some(push),
                            ..sizing
                        }),
                        _ => PairSearch::None,
                    },
                );
            }
        };

        // On the chain's prices as they stand, when those already find the account unhealthy.
        let unpushed = if on_chain < I80F48::ZERO {
            match size(Pricing::Chain).unwrap_or(PairSearch::None) {
                PairSearch::Sized(sizing) => Some(sizing),
                _ => None,
            }
        } else {
            None
        };
        // Behind a push of the feeds that lower its health the most, as many as a bundle carries.
        let pushed = self
            .strongest_fitting_push(cache, on_chain, &behind, |push| {
                health(Pricing::Pushed(push)).ok()
            })
            .and_then(
                |push| match size(Pricing::Pushed(&push)).unwrap_or(PairSearch::None) {
                    PairSearch::Sized(sizing) => Some(Sizing {
                        push: Some(push),
                        ..sizing
                    }),
                    _ => None,
                },
            );

        let earned = |sizing: &Sizing| {
            if sizing.amounts.is_liquidatable() {
                sizing.amounts.liquidator_profit
            } else {
                I80F48::ZERO
            }
        };
        Ok(match (unpushed, pushed) {
            // Kept, to take at once when no bundle can carry the push right away.
            (Some(unpushed), Some(pushed))
                if earned(&pushed) > earned(&unpushed) * PUSH_PROFIT_GAIN =>
            {
                PairSearch::Sized(Sizing {
                    without_push: Some(Box::new(unpushed)),
                    ..pushed
                })
            }
            (Some(unpushed), _) => PairSearch::Sized(unpushed),
            (None, Some(pushed)) => PairSearch::Sized(pushed),
            (None, None) => {
                debug!(
                    "Account {}: liquidatable only on Pyth prices one bundle can't push",
                    account.address
                );
                PairSearch::None
            }
        })
    }

    /// The push of the `behind` feeds (oracle and feed id) whose price alone (`health_with`) lowers
    /// the account's health below `on_chain` the most, as many as one bundle's push carries and one
    /// VAA covers. `None` with bundles off, when nothing is pushed, or when no push lowers it.
    fn strongest_fitting_push(
        &self,
        cache: &Cache,
        on_chain: I80F48,
        behind: &[(Pubkey, [u8; 32])],
        health_with: impl Fn(&PushedPrices) -> Option<I80F48>,
    ) -> Option<PushedPrices> {
        let pusher = self.pusher.as_ref()?;
        let singles = behind.iter().filter_map(|feed| {
            let alone = cache.pyth_prices.push_of(std::slice::from_ref(feed))?;
            health_with(&alone).map(|h| (h, feed.0))
        });
        let mut strongest = strongest_pushes(on_chain, singles);
        // Dropping the weakest first.
        while !strongest.is_empty() {
            let feeds: Vec<(Pubkey, [u8; 32])> = strongest
                .iter()
                .filter_map(|oracle| behind.iter().find(|(known, _)| known == oracle).copied())
                .collect();
            if let Some(push) = cache
                .pyth_prices
                .push_of(&feeds)
                .filter(|push| pusher.check(push).is_ok())
            {
                return Some(push);
            }
            strongest.pop();
        }
        None
    }

    /// Sizes every pair on `accounts`, the bank and oracle accounts behind `banks_and_oracles`.
    #[allow(clippy::too_many_arguments)]
    fn size_on_accounts(
        &self,
        cache: &Cache,
        clock: Clock,
        account: &MarginfiAccountWrapper,
        group: &MarginfiGroup,
        banks_and_oracles: &[Pubkey],
        pricing: Pricing,
        asset_banks: &[Pubkey],
        liab_banks: &[Pubkey],
    ) -> Result<PairSearch> {
        let mut accounts = program_accounts(cache, banks_and_oracles, pricing)?;
        // Read after the oracle accounts were copied, so it covers every price in them.
        let clock = cache.clock_not_before(&clock);
        let mut bank_to_emode_config = HashMap::<Pubkey, EmodeConfig>::new();
        for pk in banks_and_oracles {
            if let Ok(wrapper) = cache.banks.try_get_bank(pk) {
                bank_to_emode_config.insert(*pk, wrapper.bank.emode.emode_config);
            }
        }

        let remaining_ais: Vec<_> = accounts
            .iter_mut()
            .zip(banks_and_oracles.iter())
            .map(|(account, pk)| (pk, account).into_account_info())
            .collect();

        let (total_weighted_assets, total_weighted_liabilities) = get_health_components(
            &account.account,
            group,
            &remaining_ais,
            RequirementType::Maintenance,
            &mut None,
            HealthPriceMode::Client(clock.clone()),
            &mut None,
        )?;

        let emode_configs = account
            .account
            .lending_account
            .get_active_balances_iter()
            .filter(|b| !b.is_empty(BalanceSide::Liabilities))
            .map(|b| {
                bank_to_emode_config
                    .remove(&b.bank_pk)
                    .expect("every liability bank is in the observation accounts")
            });
        let reconciled_emode_config =
            reconcile_emode_configs(emode_configs, RequirementType::Maintenance);

        let maintenance_health = total_weighted_assets - total_weighted_liabilities;
        debug!(
            "Account {} maintenance_health = {:?} (assets {:?}, liabilities {:?})",
            account.address, maintenance_health, total_weighted_assets, total_weighted_liabilities
        );
        if maintenance_health >= I80F48::ZERO {
            return Ok(PairSearch::Healthy(health_watch::margin(
                total_weighted_assets.to_num(),
                total_weighted_liabilities.to_num(),
            )));
        }
        let (total_assets_equity, total_liabs_equity) = get_health_components(
            &account.account,
            group,
            &remaining_ais,
            RequirementType::Equity,
            &mut None,
            HealthPriceMode::Client(clock.clone()),
            &mut None,
        )?;

        // Which collateral is seized and which debt is repaid decides how much the program lets a
        // liquidation take, so every pair is sized. The most profitable one wins for a repay from
        // the wallet; an in-tx swap needs room for its route, so for it a lighter pair wins unless
        // the heavier earns much more.
        let mut candidates: Vec<(Pubkey, Pubkey, LiquidationAmounts, u8)> = Vec::new();
        let mut any_taggable = false;
        for asset_bank in asset_banks {
            let weight = cache
                .banks
                .try_get_bank(asset_bank)
                .map(|bank| seize_weight(bank.bank.config.asset_tag))
                .unwrap_or(0);
            for liab_bank in liab_banks {
                let amounts = cache
                    .banks
                    .try_get_bank(asset_bank)
                    .and_then(|asset_bank_wrapper| {
                        let liab_bank_wrapper = cache.banks.try_get_bank(liab_bank)?;
                        self.size_pair(
                            cache,
                            clock.clone(),
                            account,
                            &asset_bank_wrapper,
                            &liab_bank_wrapper,
                            &reconciled_emode_config,
                            maintenance_health,
                            total_assets_equity,
                            total_liabs_equity,
                            pricing,
                        )
                    });
                let amounts = match amounts {
                    Ok(amounts) => amounts,
                    Err(e) => {
                        debug!(
                            "Account {}: can't size seizing {} for {}: {}",
                            account.address, asset_bank, liab_bank, e
                        );
                        continue;
                    }
                };
                any_taggable |= amounts.taggable;
                candidates.push((*asset_bank, *liab_bank, amounts, weight));
            }
        }
        let Some((main, sandwich)) =
            choose_pairs(candidates.iter().map(|(_, liab_bank, amounts, weight)| {
                // Only a pair worth liquidating can be funded: one below the minimum is sized at
                // nothing.
                let funded = if amounts.is_liquidatable() {
                    amounts.liquidator_profit
                        * wallet_share(
                            cache,
                            liab_bank,
                            amounts.max_liquidatable_liab_amount * LIQUIDATION_SIZE_HAIRCUT,
                        )
                } else {
                    I80F48::ZERO
                };
                (amounts.liquidator_profit, *weight, funded)
            }))
        else {
            return Ok(PairSearch::None);
        };
        let (asset_bank, liab_bank, amounts, _) = candidates[main];
        let sandwich = sandwich.map(|index| {
            let (asset_bank, liab_bank, amounts, _) = candidates[index];
            (asset_bank, liab_bank, amounts)
        });
        // Tagging helps every pair, so it is worth it when any pair would profit from it.
        Ok(PairSearch::Sized(Sizing {
            asset_bank,
            liab_bank,
            amounts: LiquidationAmounts {
                taggable: any_taggable,
                ..amounts
            },
            sandwich,
            maintenance_health,
            push: None,
            without_push: None,
        }))
    }

    /// How much seizing `asset_bank_wrapper` for `liab_bank_wrapper` may repay and earn, as
    /// `end_liquidation` measures it.
    #[allow(clippy::too_many_arguments)]
    fn size_pair(
        &self,
        cache: &Cache,
        clock: Clock,
        account: &MarginfiAccountWrapper,
        asset_bank_wrapper: &BankWrapper,
        liab_bank_wrapper: &BankWrapper,
        reconciled_emode_config: &ReconciledEmodeConfig,
        maintenance_health: I80F48,
        total_assets_equity: I80F48,
        total_liabs_equity: I80F48,
        pricing: Pricing,
    ) -> Result<LiquidationAmounts> {
        let in_bad_debt = total_assets_equity < total_liabs_equity;
        // Valued and converted on the prices the program reads, like the health above.
        let asset_oracle_wrapper =
            OracleWrapper::build_on(&self.cache, &clock, &asset_bank_wrapper.address, pricing)?;

        let liab_oracle_wrapper =
            OracleWrapper::build_on(&self.cache, &clock, &liab_bank_wrapper.address, pricing)?;

        let asset_weight_maint: I80F48 = asset_bank_wrapper
            .bank
            .get_asset_weight(RequirementType::Maintenance, reconciled_emode_config);
        let liab_weight_maint: I80F48 = liab_bank_wrapper.bank.config.liability_weight_maint.into();

        // `end_liquidation` holds the premium at its base for e-mode-boosted accounts and for
        // accounts already in bad debt; otherwise a tag grows it over time.
        let own_asset_weight_maint: I80F48 =
            asset_bank_wrapper.bank.config.asset_weight_maint.into();
        let emode_boosted = account.account.health_cache.is_emode_boosted()
            || asset_weight_maint > own_asset_weight_maint;
        let tagged_at = account.account.liquidation_tagged_at;
        let base_premium = I80F48::max(cache.liquidation_max_fee(), LIQUIDATION_BONUS_FEE_MINIMUM);
        // The program drops the tag's premium for an account in bad debt, judged on the prices it
        // caches at the start: near that line the two can disagree, so the base premium is safe.
        let near_bad_debt = total_assets_equity < total_liabs_equity * NEAR_BAD_DEBT;
        let premium = if emode_boosted || in_bad_debt || near_bad_debt {
            base_premium
        } else {
            tag_premium(base_premium, tagged_at, clock.unix_timestamp)
        };
        let premium_taken = I80F48::max(I80F48::ZERO, premium - PREMIUM_SAFETY_MARGIN);

        let (asset_amount, _) = account.get_balance_for_bank(asset_bank_wrapper, &clock)?;
        let (_, liab_amount) = account.get_balance_for_bank(liab_bank_wrapper, &clock)?;

        let asset_value = asset_bank_wrapper.calc_value(
            &asset_oracle_wrapper,
            asset_amount,
            BalanceSide::Assets,
            RequirementType::Equity,
        )?;

        let liab_value = liab_bank_wrapper.calc_value(
            &liab_oracle_wrapper,
            liab_amount,
            BalanceSide::Liabilities,
            RequirementType::Equity,
        )?;

        let bounds = RepayBounds {
            liab_value,
            asset_value,
            equity: total_assets_equity - total_liabs_equity,
            maintenance_health,
            asset_weight_maint,
            liab_weight_maint,
            asset_price_ratio: maintenance_to_equity_price(
                asset_bank_wrapper,
                &asset_oracle_wrapper,
                || BalanceSide::Assets,
            )?,
            liab_price_ratio: maintenance_to_equity_price(
                liab_bank_wrapper,
                &liab_oracle_wrapper,
                || BalanceSide::Liabilities,
            )?,
            above_closeout: total_assets_equity >= LIQUIDATION_CLOSEOUT_DOLLAR_THRESHOLD,
        };
        let max_repay = bounds.max_repay(premium_taken);
        let caps = bounds.breakdown(premium_taken);
        let one_plus_premium = I80F48::ONE + premium_taken;
        let liquidator_profit = max_repay * premium_taken;
        // Tag only when the premium a tag can grow to would make the liquidation worth it.
        let taggable =
            tagged_at == 0 && !in_bad_debt && !emode_boosted && base_premium < TAG_MAX_PREMIUM && {
                let max_premium = TAG_MAX_PREMIUM - PREMIUM_SAFETY_MARGIN;
                bounds.max_repay(max_premium) * max_premium * LIQUIDATION_SIZE_HAIRCUT
                    > self.min_profit
            };

        if liquidator_profit * LIQUIDATION_SIZE_HAIRCUT <= self.min_profit {
            debug!(
                "Account {} profit ${:.6} <= min profit ${} at premium {:.4} (tagged_at {}, taggable {})",
                account.address,
                liquidator_profit.to_num::<f64>(),
                self.min_profit,
                premium_taken.to_num::<f64>(),
                tagged_at,
                taggable
            );
            return Ok(LiquidationAmounts {
                max_repay,
                premium: premium_taken,
                liquidator_profit,
                taggable,
                caps,
                ..LiquidationAmounts::none()
            });
        }

        let max_liquidatable_asset_amount = asset_bank_wrapper.calc_amount(
            &asset_oracle_wrapper,
            max_repay * one_plus_premium,
            BalanceSide::Assets,
            RequirementType::Equity,
        )?;

        let max_liquidatable_liab_amount = liab_bank_wrapper.calc_amount(
            &liab_oracle_wrapper,
            max_repay,
            BalanceSide::Liabilities,
            RequirementType::Equity,
        )?;

        debug!("Account {:?} liquidability evaluation:\nMaintenance health {:?}\n\
            Equity assets {:?}\nEquity liabilities {:?}\nPremium {:?} (base {:?}, tagged_at {}, emode boosted {})\n\
            Asset Bank {:?}\nAsset maint weight: {:?}\nAsset Amount {:?}\nAsset Value (USD) {:?}\n\
            Liab Bank {:?}\nLiab maint weight: {:?}\nLiab Amount {:?}\nLiab Value (USD) {:?}\n\
            Max Repay (USD) {:?}\nMax Liquidatable Asset Amount {:?}\nMax Liquidatable Liab Amount {:?}\nLiquidator profit (USD) {:?}",
            account.address, maintenance_health,
            total_assets_equity, total_liabs_equity, premium_taken, base_premium, tagged_at, emode_boosted,
            asset_bank_wrapper.address, asset_weight_maint, asset_amount, asset_value,
            liab_bank_wrapper.address, liab_weight_maint, liab_amount, liab_value,
            max_repay, max_liquidatable_asset_amount, max_liquidatable_liab_amount, liquidator_profit);

        Ok(LiquidationAmounts {
            max_liquidatable_asset_amount,
            max_liquidatable_liab_amount,
            max_repay,
            premium: premium_taken,
            liquidator_profit,
            taggable,
            caps,
        })
    }

    fn get_value_of_shares(
        &self,
        clock: &Clock,
        shares: Vec<(I80F48, Pubkey)>,
        balance_side: &BalanceSide,
        requirement_type: RequirementType,
    ) -> Result<Vec<(I80F48, Pubkey)>> {
        let mut values: Vec<(I80F48, Pubkey)> = Vec::new();

        for (shares_amount, bank_pk) in shares {
            let bank_wrapper = self.cache.banks.try_get_bank(&bank_pk)?;
            let oracle_wrapper = OracleWrapper::build(&self.cache, clock, &bank_pk)?;

            if !matches!(
                bank_wrapper.bank.config.operational_state,
                BankOperationalState::Operational | BankOperationalState::ReduceOnly
            ) {
                continue;
            }

            if !bank_covers_its_borrows(&bank_wrapper.bank).unwrap_or(false) {
                debug!("Skipping bankrupt bank from evaluation: {}", bank_pk);
                continue;
            }

            let value = match balance_side {
                BalanceSide::Liabilities => {
                    let liabilities = bank_wrapper
                        .bank
                        .get_liability_amount(shares_amount)
                        .map_err(|e| anyhow!("Couldn't calculate liability amount for: {}", e))?;
                    bank_wrapper.calc_value(
                        &oracle_wrapper,
                        liabilities,
                        BalanceSide::Liabilities,
                        requirement_type,
                    )?
                }
                BalanceSide::Assets => {
                    let assets = bank_wrapper
                        .bank
                        .get_asset_amount(shares_amount)
                        .map_err(|e| anyhow!("Couldn't calculate asset amount for: {}", e))?;
                    bank_wrapper.calc_value(
                        &oracle_wrapper,
                        assets,
                        BalanceSide::Assets,
                        requirement_type,
                    )?
                }
            };

            values.push((value, bank_pk));
        }

        Ok(values)
    }
}

#[cfg(test)]
mod repay_bounds_tests {
    use super::*;

    #[test]
    fn a_seizure_that_lowers_health_more_than_the_premium_is_not_sized() {
        // A boosted pair: collateral weight 0.97, its price 3.5% richer in maintenance terms.
        let mut bounds = example();
        bounds.asset_weight_maint = I80F48!(0.97);
        bounds.asset_price_ratio = I80F48!(1.035);
        let premium = I80F48!(0.05);
        // Each dollar repaid moves health by 1 - 0.97 × 1.05 × 1.035 = -0.054, more than 0.05.
        assert_eq!(bounds.max_repay(premium), I80F48::ZERO);
        assert_eq!(bounds.breakdown(premium).binding, "health_fall");
        // With the prices level, health falls by 1 - 0.97 × 1.05 = 0.0185 a dollar, less than the
        // premium extracts, and the usual caps apply.
        bounds.asset_price_ratio = I80F48::ONE;
        assert!(bounds.max_repay(premium) > I80F48::ZERO);
    }

    /// The 0.1.12 patch notes' example: $100 SOL at maintenance weight 0.8, $85 USDC debt.
    fn example() -> RepayBounds {
        RepayBounds {
            liab_value: I80F48!(85),
            asset_value: I80F48!(100),
            equity: I80F48!(15),
            maintenance_health: I80F48!(-5),
            asset_weight_maint: I80F48!(0.8),
            liab_weight_maint: I80F48!(1),
            asset_price_ratio: I80F48::ONE,
            liab_price_ratio: I80F48::ONE,
            above_closeout: true,
        }
    }

    #[test]
    fn collateral_twap_above_spot_shrinks_the_stay_unhealthy_bound() {
        // SOL falling: its time-weighted price sits 1% above spot, so each equity dollar seized
        // removes only 0.99 health dollars: 1 - 0.8 * 1.05 * 0.99 = 0.1684 health per dollar.
        let bounds = RepayBounds {
            asset_price_ratio: I80F48!(0.99),
            ..example()
        };
        let repay = bounds.max_repay(I80F48!(0.05));
        assert!((repay - I80F48!(29.6912)).abs() < I80F48!(0.001));
    }

    #[test]
    fn base_premium_is_held_by_the_stay_unhealthy_bound() {
        // Each dollar repaid at 5% gains 1 - 0.8 * 1.05 = 0.16 health: 5 / 0.16.
        let repay = example().max_repay(I80F48!(0.05));
        assert!((repay - I80F48!(31.25)).abs() < I80F48!(0.0001));
    }

    #[test]
    fn max_premium_is_held_by_solvency() {
        // At 100% repaying lowers health, so only "stay solvent" binds: $15 equity / 1.0.
        assert_eq!(example().max_repay(I80F48!(1)), I80F48!(15));
    }

    #[test]
    fn bad_debt_has_no_solvency_bound() {
        let bounds = RepayBounds {
            equity: I80F48!(-5),
            ..example()
        };
        assert_eq!(bounds.max_repay(I80F48!(1)), I80F48!(50));
    }
}

#[cfg(test)]
mod bank_solvency_tests {
    use super::*;

    fn bank(assets: i32, liabilities: i32) -> Bank {
        let mut bank: Bank = bytemuck::Zeroable::zeroed();
        bank.asset_share_value = I80F48::ONE.into();
        bank.liability_share_value = I80F48::ONE.into();
        bank.total_asset_shares = I80F48::from_num(assets).into();
        bank.total_liability_shares = I80F48::from_num(liabilities).into();
        bank
    }

    #[test]
    fn a_bank_whose_deposits_cover_its_borrows_is_solvent() {
        assert!(bank_covers_its_borrows(&bank(100, 90)).unwrap());
        assert!(bank_covers_its_borrows(&bank(100, 100)).unwrap());
    }

    #[test]
    fn a_bank_borrowed_beyond_its_deposits_is_not() {
        assert!(!bank_covers_its_borrows(&bank(100, 101)).unwrap());
    }

    #[test]
    fn share_values_are_applied() {
        let mut in_bad_debt = bank(100, 90);
        in_bad_debt.liability_share_value = I80F48::from_num(1.2).into();
        assert!(!bank_covers_its_borrows(&in_bad_debt).unwrap());
    }
}

#[cfg(test)]
mod pair_preference_tests {
    use super::*;

    #[test]
    fn a_lighter_pair_wins_within_the_margin() {
        // Plain SOL earning 20% less than Kamino SOL is still the better pair.
        assert!(is_better_pair(I80F48!(0.8), 0, I80F48!(1), 1));
        // Kamino SOL needs more than 30% more to beat plain SOL.
        assert!(!is_better_pair(I80F48!(1.25), 1, I80F48!(1), 0));
        assert!(is_better_pair(I80F48!(1.31), 1, I80F48!(1), 0));
    }

    #[test]
    fn a_much_better_heavier_pair_still_wins() {
        assert!(!is_better_pair(I80F48!(0.5), 0, I80F48!(1), 1));
    }

    #[test]
    fn equal_weights_compare_on_profit() {
        assert!(is_better_pair(I80F48!(1.001), 0, I80F48!(1), 0));
        assert!(!is_better_pair(I80F48!(1), 0, I80F48!(1), 0));
    }

    #[test]
    fn a_heavy_most_profitable_pair_gets_a_native_one_for_the_sandwich() {
        let pairs = |list: &[(f64, u8)]| {
            choose_pairs(
                list.iter()
                    .map(|(p, w)| (I80F48::from_num(*p), *w, I80F48::ZERO)),
            )
        };
        // A Kamino pair earning 1.2 against a native one earning 1: the wallet repay takes the
        // Kamino one, the sandwich the native one (within the 30% margin).
        assert_eq!(pairs(&[(1.0, 0), (1.2, 1)]), Some((1, Some(0))));
        // A native pair is the most profitable: one pair suffices.
        assert_eq!(pairs(&[(1.2, 0), (1.0, 1)]), Some((0, None)));
        // A Kamino pair earning far more: the sandwich takes it too.
        assert_eq!(pairs(&[(1.0, 0), (2.0, 1)]), Some((1, None)));
        // Only heavy pairs: the sandwich has nothing lighter.
        assert_eq!(pairs(&[(1.0, 1), (1.1, 1)]), Some((1, None)));
        assert_eq!(pairs(&[]), None);
    }

    #[test]
    fn the_pair_the_wallet_funds_most_of_wins() {
        let pairs = |list: &[(f64, u8, f64)]| {
            choose_pairs(
                list.iter()
                    .map(|(p, w, share)| (I80F48::from_num(*p), *w, I80F48::from_num(*p * *share))),
            )
        };
        // The same seizure against two stable debts: the one the wallet holds wins.
        assert_eq!(pairs(&[(1.0, 0, 0.0), (1.0, 0, 1.0)]), Some((1, None)));
        // A debt the wallet holds 99% of counts nearly in full: no all-or-nothing.
        assert_eq!(pairs(&[(1.0, 0, 0.99), (1.0, 0, 0.5)]), Some((0, None)));
        // A more profitable pair the wallet doesn't hold: the sandwich takes it.
        assert_eq!(pairs(&[(1.0, 0, 1.0), (3.0, 0, 0.0)]), Some((0, Some(1))));
        // The wallet funds none: the most profitable pair.
        assert_eq!(pairs(&[(1.0, 0, 0.0), (1.2, 0, 0.0)]), Some((1, None)));
    }

    #[test]
    fn integration_banks_are_heavier() {
        assert_eq!(seize_weight(ASSET_TAG_KAMINO), 1);
        assert_eq!(seize_weight(ASSET_TAG_JUPLEND), 1);
        assert_eq!(seize_weight(0), 0);
    }
}

#[cfg(test)]
mod tag_premium_tests {
    use super::*;

    #[test]
    fn a_tag_grows_the_premium_to_a_quarter_over_a_week() {
        let base = I80F48!(0.06);
        let tagged = 1_000_000;
        assert_eq!(tag_premium(base, 0, tagged + 1), base, "untagged");
        assert_eq!(
            tag_premium(base, tagged, tagged + 3600),
            base,
            "within the delay"
        );
        let full = tag_premium(base, tagged, tagged + LIQUIDATION_TAG_FULL_PREMIUM_SECS);
        assert_eq!(full, TAG_MAX_PREMIUM);
        let later = tag_premium(base, tagged, tagged + 2 * LIQUIDATION_TAG_FULL_PREMIUM_SECS);
        assert_eq!(later, TAG_MAX_PREMIUM, "never past the cap");
        let half = tag_premium(
            base,
            tagged,
            tagged
                + LIQUIDATION_TAG_DELAY_SECS
                + (LIQUIDATION_TAG_FULL_PREMIUM_SECS - LIQUIDATION_TAG_DELAY_SECS) / 2,
        );
        assert!((half - I80F48!(0.155)).abs() < I80F48!(0.0001));
    }
}

#[cfg(test)]
mod sweep_tests {
    use super::*;

    #[test]
    fn every_borrower_is_swept_once_per_interval_in_slices() {
        // The interval in 60 steps: each step makes a sixtieth of the pass due.
        let step = FULL_SCAN_INTERVAL / 60;
        let borrowers: Vec<Pubkey> = (0..600).map(|_| Pubkey::new_unique()).collect();
        let mut sweep = Sweep::default();
        let start = Instant::now();
        assert!(
            sweep.next_slice(start, || borrowers.clone()).is_empty(),
            "nothing due yet"
        );

        let slice = sweep.next_slice(start + step, || unreachable!());
        assert_eq!(slice, borrowers[..10]);

        // The rest of the interval sweeps the rest, and no account twice.
        let mut swept = slice;
        for n in 2..=60 {
            swept.extend(sweep.next_slice(start + step * n, || unreachable!()));
        }
        assert_eq!(swept, borrowers);

        // Then a new pass starts.
        let next: Vec<Pubkey> = (0..60).map(|_| Pubkey::new_unique()).collect();
        let slice = sweep.next_slice(start + step * 61, || next.clone());
        assert_eq!(slice, next[..1]);
    }
}

#[cfg(test)]
mod push_choice_tests {
    use super::*;
    use crate::cache::pyth_prices::test_price_update_account;
    use crate::utils::pyth_cranker::price_feed_address;
    use std::sync::Mutex;

    #[test]
    fn the_feeds_that_lower_health_most_are_pushed() {
        let (sol, eth, usdc, jup) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        // Healthy by $5 on the chain's prices: SOL's newer price takes $40 off, ETH's $10, JUP's
        // $1, and USDC's adds $1.
        let singles = [
            (I80F48!(-5), eth),
            (I80F48!(6), usdc),
            (I80F48!(-35), sol),
            (I80F48!(4), jup),
        ];
        assert_eq!(
            strongest_pushes(I80F48!(5), singles.into_iter()),
            vec![sol, eth, jup]
        );
        // A feed that moved in the account's favour is never pushed, even with room to spare.
        assert_eq!(
            strongest_pushes(I80F48!(5), [(I80F48!(6), usdc)].into_iter()),
            Vec::<Pubkey>::new()
        );
    }

    #[test]
    fn no_more_feeds_are_ranked_than_a_push_carries() {
        let singles: Vec<(I80F48, Pubkey)> = (0..MAX_PUSHED_FEEDS + 3)
            .map(|i| (I80F48::from_num(-(i as i32)), Pubkey::new_unique()))
            .collect();
        let ranked = strongest_pushes(I80F48!(1), singles.clone().into_iter());
        assert_eq!(ranked.len(), MAX_PUSHED_FEEDS);
        // The one lowering health most comes first, so dropping from the end drops the weakest.
        assert_eq!(ranked[0], singles.last().unwrap().1);
    }

    #[test]
    fn unpushed_feeds_are_read_at_the_chains_price() {
        let mut cache = Cache::new(
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Arc::new(Mutex::new(Clock::default())),
            HashSet::new(),
        );
        let mut add = |feed: [u8; 32]| {
            let address = price_feed_address(&feed);
            cache
                .oracles
                .try_insert(address, test_price_update_account(feed, 110, 2))
                .unwrap();
            cache
                .pyth_prices
                .record_onchain(address, &test_price_update_account(feed, 100, 1));
            address
        };
        let (sol, eth) = (add([1; 32]), add([2; 32]));
        let price = |account: &Account| {
            crate::cache::pyth_prices::price_update(account)
                .unwrap()
                .price_message
                .price
        };
        let oracles = [sol, eth];

        let ours = program_accounts(&cache, &oracles, Pricing::Newest).unwrap();
        assert_eq!(ours.iter().map(price).collect::<Vec<_>>(), vec![2, 2]);
        let chain = program_accounts(&cache, &oracles, Pricing::Chain).unwrap();
        assert_eq!(chain.iter().map(price).collect::<Vec<_>>(), vec![1, 1]);
        // SOL at the price its push posts, which need not be the newest eva holds.
        let mut message =
            crate::cache::pyth_prices::price_update(&cache.oracles.try_get_account(&sol).unwrap())
                .unwrap()
                .price_message;
        message.price = 3;
        message.publish_time = 105;
        let vaa = Arc::new(vec![1u8]);
        let push = PushedPrices {
            vaa: Arc::clone(&vaa),
            feeds: vec![(
                sol,
                crate::cache::pyth_prices::SignedUpdate {
                    message,
                    vaa,
                    update: pythnet_sdk::wire::v1::MerklePriceUpdate {
                        message: vec![0u8].into(),
                        proof: pythnet_sdk::accumulators::merkle::MerklePath::new(vec![]),
                    },
                },
            )],
        };
        let sol_pushed = program_accounts(&cache, &oracles, Pricing::Pushed(&push)).unwrap();
        assert_eq!(sol_pushed.iter().map(price).collect::<Vec<_>>(), vec![3, 1]);
    }
}

#[cfg(test)]
mod hot_path_tests {
    use super::*;

    #[test]
    fn the_book_is_sized_by_margin_among_the_accounts_that_could_pay() {
        let standing = |margin: f64, potential_profit: f64| Standing {
            margin,
            potential_profit,
        };
        let standings: HashMap<Pubkey, Standing> = [
            standing(-1.0, 5.0),  // unhealthy
            standing(0.003, 5.0), // under 0.5%
            standing(0.03, 5.0),  // under 4%
            standing(0.03, 0.5),  // could never pay the minimum
            standing(0.50, 5.0),  // far from the line
        ]
        .into_iter()
        .map(|standing| (Pubkey::new_unique(), standing))
        .collect();

        let (buckets, could_pay) = sizing_buckets(&standings, 1.0);
        assert_eq!(could_pay, 4);
        assert_eq!(buckets, [1, 1, 0, 0, 1, 0, 0, 1]);
    }

    #[test]
    fn batches_merge_keeping_the_oldest_arrival() {
        let (a, b) = (Pubkey::new_unique(), Pubkey::new_unique());
        let now = Instant::now();
        let mut one = Changes::default();
        assert!(one.is_empty());
        one.accounts.insert(a);

        let mut two = Changes::default();
        two.oldest = Some(now);
        two.moved_banks.insert(b);

        one.merge(two);
        assert_eq!(one.oldest, Some(now), "an undated batch takes the other's");
        assert!(!one.is_empty());
        assert!(one.accounts.contains(&a) && one.moved_banks.contains(&b));
    }
}
