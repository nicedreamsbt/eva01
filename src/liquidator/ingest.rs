//! The update path: applies every Geyser and Hermes update to the cache the moment it arrives and
//! evaluates only the hot accounts the update touches, so a liquidation that an update makes
//! possible is queued within milliseconds, not after the background evaluation (the health watch,
//! its cycles and the sweep) has worked through everyone else.
//!
//! The background path still sees every change: what the update path applied is handed over,
//! coalesced, for it to evaluate as before, and the accounts it evaluates tell the hot list who
//! belongs on it.

use super::*;
use crate::hot_list::{HotList, HotMode, Standing};
/// Applied updates the background path has not taken yet, merged into one batch.
pub(super) struct Handoff {
    pending: Mutex<Changes>,
    wake_tx: Sender<()>,
    wake_rx: Receiver<()>,
}

impl Handoff {
    pub(super) fn new() -> Self {
        let (wake_tx, wake_rx) = bounded(1);
        Self {
            pending: Mutex::new(Changes::default()),
            wake_tx,
            wake_rx,
        }
    }

    fn push(&self, changes: Changes) {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .merge(changes);
        // A wake-up already waiting is enough: the batch it announces is this one too.
        let _ = self.wake_tx.try_send(());
    }

    /// Everything handed over since the last call, waiting up to `timeout` for something to arrive.
    pub(super) fn take(&self, timeout: Duration) -> Changes {
        let _ = self.wake_rx.recv_timeout(timeout);
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

/// What the update path did since the last report, and what it found that the background path is
/// still to confirm.
#[derive(Default)]
pub(super) struct HotStats {
    /// Update batches that touched hot accounts, and the accounts evaluated in them.
    pub batches: usize,
    pub evaluated: usize,
    /// How long each such batch took to evaluate.
    pub check_times: Vec<Duration>,
    /// For each liquidation found, how long after the oldest update behind it arrived.
    pub arrival_to_queue: Vec<Duration>,
    pub found: usize,
    /// The accounts the update path found liquidatable and when, to tell how much earlier it was
    /// than the background path, and which accounts only the background path found.
    pub found_at: HashMap<Pubkey, Instant>,
    /// When the background path last found each account liquidatable, so a repeat sighting of the
    /// same event is not counted again.
    pub background_seen: HashMap<Pubkey, Instant>,
    /// How much earlier than the background path the update path found each liquidation.
    pub leads: Vec<Duration>,
    /// Liquidations the background path found that the update path had not.
    pub misses: usize,
}

pub(super) struct Ingest {
    pub(super) cache: Arc<Cache>,
    pub(super) queue: Arc<ExecutionQueue>,
    pub(super) liquidator_account: Arc<LiquidatorAccount>,
    pub(super) evaluator: Arc<Evaluator>,
    pub(super) geyser_rx: Receiver<GeyserUpdate>,
    pub(super) stop_liquidator: Arc<AtomicBool>,
    pub(super) handoff: Arc<Handoff>,
    pub(super) hot: Arc<Mutex<HotList>>,
    pub(super) hot_mode: HotMode,
    pub(super) hot_stats: Arc<Mutex<HotStats>>,
    /// The threads the hot path evaluates on, apart from the pool the background cycles use: work
    /// injected into a pool that is busy with a cycle waits for it, and the hot path must not.
    pub(super) pool: rayon::ThreadPool,
}

/// What an evaluation found, for the hot list: every account it returned a result for.
pub(super) fn observe_results(
    hot: &mut HotList,
    liquidations: &[PreparedLiquidatableAccount],
    unhealthy: &[UnhealthyAccount],
    healthy: &[HealthyAccount],
    now: Instant,
    checked_at: Option<Instant>,
    skip: &HashSet<Pubkey>,
) {
    for intent in liquidations
        .iter()
        .filter(|intent| !skip.contains(&intent.liquidatee_account.address))
    {
        let standing = Standing {
            margin: -1.0,
            potential_profit: intent.profit.to_num::<f64>(),
        };
        hot.observe(
            intent.liquidatee_account.address,
            standing,
            intent.observation_accounts.bank_pks.clone(),
            None,
            now,
            checked_at,
        );
    }
    for account in unhealthy
        .iter()
        .filter(|account| !skip.contains(&account.address))
    {
        let standing = Standing {
            margin: -1.0,
            potential_profit: account.potential_profit,
        };
        hot.observe(
            account.address,
            standing,
            account.observation_accounts.bank_pks.clone(),
            account.hot_watch.clone(),
            now,
            checked_at,
        );
    }
    for account in healthy
        .iter()
        .filter(|account| !skip.contains(&account.address))
    {
        hot.observe(
            account.address,
            account.standing,
            account.banks.clone(),
            account.hot_watch.clone(),
            now,
            checked_at,
        );
    }
}

impl Ingest {
    /// Applies updates and checks the hot accounts they touch, until stopped.
    pub(super) fn run(&self) -> Result<()> {
        while !self.stop_liquidator.load(Ordering::Relaxed) {
            let changes = self.receive_geyser_updates()?;
            if changes.is_empty() {
                continue;
            }
            // The hot accounts first: the background path gets the batch afterwards, and takes
            // as long as it takes.
            self.check_hot(&changes);
            self.handoff.push(changes);
        }
        Ok(())
    }

    /// Applies whatever is queued without checking anything: what arrived while the cache loaded.
    pub(super) fn drain_backlog(&self) -> Changes {
        let mut backlog = Changes::default();
        while let Ok(update) = self.geyser_rx.try_recv() {
            self.process_geyser_update(update, &mut backlog);
        }
        backlog
    }

    /// The hot accounts a batch of changes could matter to: those that changed themselves, and the
    /// hot holders of the banks that moved whose watch says the move was far enough.
    fn affected_hot_accounts(&self, changes: &Changes) -> Vec<Pubkey> {
        let clock = match clock_manager::get_clock(&self.cache.clock) {
            Ok(clock) => clock,
            Err(e) => {
                error!("Failed to read the clock for the hot path: {e}");
                return Vec::new();
            }
        };
        // Each moved bank's unit values now, once per batch.
        let mut current = HashMap::new();
        let mut unpriced = Vec::new();
        for bank in &changes.moved_banks {
            match (
                unit_value(&self.cache, &clock, bank, false),
                unit_value(&self.cache, &clock, bank, true),
            ) {
                (Some(asset), Some(liability)) => {
                    current.insert((*bank, false), asset);
                    current.insert((*bank, true), liability);
                }
                _ => unpriced.push(*bank),
            }
        }
        let hot = self.hot.lock().unwrap_or_else(|e| e.into_inner());
        let mut affected =
            hot.needing_evaluation(changes.moved_banks.iter(), &current, unpriced.iter());
        affected.extend(hot.among(&changes.accounts));
        affected.into_iter().collect()
    }

    /// Evaluates the hot accounts `changes` touches (the hot holders of every bank that moved, and
    /// the hot accounts that changed themselves) and queues the liquidations it finds.
    fn check_hot(&self, changes: &Changes) {
        if self.hot_mode == HotMode::Off {
            return;
        }
        let started = Instant::now();
        let affected = self.affected_hot_accounts(changes);
        if affected.is_empty() {
            return;
        }

        let (liquidations, unhealthy, healthy) = match self
            .pool
            .install(|| self.evaluator.check_accounts(&affected))
        {
            Ok(results) => results,
            Err(e) => {
                warn!(
                    "The hot path could not evaluate {} account(s): {e}",
                    affected.len()
                );
                return;
            }
        };
        let now = Instant::now();

        // What the evaluation found is the list's newest word on these accounts; an account it
        // could not evaluate at all (nothing left to liquidate, or no price) leaves the list.
        {
            let mut hot = self.hot.lock().unwrap_or_else(|e| e.into_inner());
            let answered: HashSet<Pubkey> = liquidations
                .iter()
                .map(|intent| intent.liquidatee_account.address)
                .chain(unhealthy.iter().map(|account| account.address))
                .chain(healthy.iter().map(|account| account.address))
                .collect();
            for account in &affected {
                if !answered.contains(account) {
                    hot.remove(account);
                }
            }
            observe_results(
                &mut hot,
                &liquidations,
                &unhealthy,
                &healthy,
                now,
                Some(now),
                &HashSet::new(),
            );
        }

        let elapsed = started.elapsed();
        {
            let mut stats = self.hot_stats.lock().unwrap_or_else(|e| e.into_inner());
            stats.batches += 1;
            stats.evaluated += affected.len();
            stats.check_times.push(elapsed);
            for intent in &liquidations {
                stats.found += 1;
                // The first sighting of an event stands; a later tick finding the same account again
                // is the same event.
                let address = intent.liquidatee_account.address;
                if stats
                    .found_at
                    .get(&address)
                    .is_none_or(|at| now.saturating_duration_since(*at) >= HOT_EVENT_WINDOW)
                {
                    stats.found_at.insert(address, now);
                }
                if let Some(oldest) = changes.oldest {
                    stats
                        .arrival_to_queue
                        .push(now.saturating_duration_since(oldest));
                }
            }
        }
        for intent in &liquidations {
            info!(
                "HOT_PATH_FOUND mode={:?} attempt_id={} account={} arrival_to_queue_ms={} evaluated_ms={} estimated_profit_usd={:.4}",
                self.hot_mode,
                intent.attempt_id,
                intent.liquidatee_account.address,
                changes
                    .oldest
                    .map_or(0, |oldest| now.saturating_duration_since(oldest).as_millis()),
                elapsed.as_millis(),
                intent.profit.to_num::<f64>()
            );
        }
        if self.hot_mode == HotMode::On {
            // The evaluated accounts' older intents are replaced, or withdrawn when they are no
            // longer liquidatable.
            self.queue.offer(&affected, liquidations);
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

    fn receive_geyser_updates(&self) -> Result<Changes> {
        let first_update = match self.geyser_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(update) => update,
            Err(RecvTimeoutError::Timeout) => return Ok(Changes::default()),
            Err(RecvTimeoutError::Disconnected) if self.stop_liquidator.load(Ordering::Relaxed) => {
                return Ok(Changes::default())
            }
            Err(RecvTimeoutError::Disconnected) => {
                return Err(anyhow!("Geyser update channel disconnected"))
            }
        };

        let mut changes = Changes::default();
        self.process_geyser_update(first_update, &mut changes);
        while let Ok(update) = self.geyser_rx.try_recv() {
            self.process_geyser_update(update, &mut changes);
        }
        Ok(changes)
    }

    fn process_geyser_update(&self, update: GeyserUpdate, changes: &mut Changes) {
        if let Err(error) = self.handle_geyser_update(update, changes) {
            log_genuine_error("Failed to process Geyser update", error);
        }
    }

    /// Our clock lags the chain, so a fresh Scope price can look "published in the future" and get
    /// rejected. Moving the clock up to each new publish time prevents that.
    fn advance_clock_to_oracle(&self, address: &Pubkey, account: &Account, banks: &[Pubkey]) {
        let scope_entries: Vec<u16> = banks
            .iter()
            .filter_map(|bank| self.cache.banks.try_get_bank(bank).ok())
            .filter(|wrapper| {
                matches!(
                    wrapper.bank.config.oracle_setup,
                    OracleSetup::Scope | OracleSetup::ScopeKamino | OracleSetup::ScopeJuplend
                ) && wrapper.bank.config.oracle_keys[0] == *address
            })
            .map(|wrapper| wrapper.bank.config.scope_entry_index)
            .collect();

        if let Some(publish_time) = oracle_publish_time(account, &scope_entries) {
            if let Err(e) = clock_manager::advance_to(&self.cache.clock, publish_time) {
                error!("Failed to advance the clock to the {address} publish time: {e}");
            }
        }
    }

    fn handle_geyser_update(&self, update: GeyserUpdate, changes: &mut Changes) -> Result<()> {
        let GeyserUpdate {
            account_type,
            address,
            account,
            slot,
            received_at,
        } = update;

        debug!("Processing the {:?} {:?} update.", account_type, address);
        // The oldest update's arrival is what a detection is timed from; updates that cannot move
        // an evaluation (a wallet token balance) don't count.
        if !matches!(account_type, AccountType::Token) {
            changes.oldest = Some(
                changes
                    .oldest
                    .map_or(received_at, |oldest| oldest.min(received_at)),
            );
        }

        match account_type {
            AccountType::Oracle | AccountType::HermesPrice => {
                if matches!(account_type, AccountType::Oracle) {
                    self.cache.pyth_prices.record_onchain(address, &account);
                }
                let banks = self.cache.banks.get_banks_for_oracle(&address)?;
                self.advance_clock_to_oracle(&address, &account, &banks);
                // An older price than the one cached changes nothing to evaluate.
                if !self.cache.oracles.try_update(&address, account)? {
                    return Ok(());
                }

                changes.moved_banks.extend(banks);
            }
            AccountType::Marginfi => {
                let mut data = account.data.as_slice();
                let marginfi_account = MarginfiAccount::try_deserialize(&mut data)?;
                let has_liabilities = self
                    .cache
                    .marginfi_accounts
                    .try_insert(MarginfiAccountWrapper::new(address, marginfi_account))?;

                // Only queue accounts that carry debt — a debt-free account can't be liquidated, so
                // evaluating it is wasted work. It rejoins the queue the moment it borrows (that
                // update will report liabilities).
                self.queue.account_updated(address, slot);
                if has_liabilities {
                    changes.accounts.insert(address);
                }
            }
            AccountType::Bank => {
                let bank = utils::decode_bank(&account.data)?;
                let previous = self.cache.banks.try_get_bank(&address).ok();
                let reconfigured = previous.as_ref().is_none_or(|previous| {
                    previous.bank.config != bank.config || previous.bank.emode != bank.emode
                });
                self.cache.banks.try_insert(address, bank, account)?;
                if previous.is_none() {
                    self.liquidator_account.cover_bank_in_background(&address);
                }

                // Only the share values move on most updates, which the health watch measures; a
                // new weight, limit or oracle can change any holder's health.
                if reconfigured {
                    self.add_accounts_for_bank(&address, &mut changes.accounts)?;
                } else {
                    changes.moved_banks.insert(address);
                }
            }
            AccountType::Token => {
                self.cache
                    .tokens
                    .try_update_account(address, account, slot)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batches_handed_over_are_merged_and_taken_once() {
        let handoff = Handoff::new();
        let (a, b, bank) = (
            Pubkey::new_unique(),
            Pubkey::new_unique(),
            Pubkey::new_unique(),
        );
        let first = Instant::now();
        let mut one = Changes::default();
        one.accounts.insert(a);
        one.oldest = Some(first);
        let mut two = Changes::default();
        two.accounts.insert(b);
        two.moved_banks.insert(bank);
        two.oldest = Some(first + Duration::from_millis(5));
        handoff.push(one);
        handoff.push(two);

        let taken = handoff.take(Duration::from_millis(50));
        assert_eq!(taken.accounts, HashSet::from([a, b]));
        assert_eq!(taken.moved_banks, HashSet::from([bank]));
        assert_eq!(taken.oldest, Some(first), "the oldest arrival is kept");
        assert!(handoff.take(Duration::from_millis(1)).is_empty());
    }
}
